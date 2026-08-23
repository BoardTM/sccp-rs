//! Independently owned video RTP for a native channel.

use std::ffi::{CStr, CString, c_int};
use std::mem;
use std::net::SocketAddr;
use std::ptr::NonNull;

use crate::asterisk::boundary::optional_c_text;
use crate::asterisk::raw::handles::BorrowedChannelLock as ChannelLock;
use crate::asterisk::sys;
use crate::media::formats::PbxVideoFormat;
use sccp_protocol::RtpPayloadNumber;

use super::allocation::{
    MediaSocketQosReport, OwnedRtpInstance, OwnedVideoRtp, RtpPolicy, apply_media_socket_qos,
    audio_format, channel_private, format_cap_alloc, format_cap_append, private_audio_format,
    private_video_rtp, take_private_video, video_format,
};
use super::media::LocalMediaEndpoint;

const SOURCE_FILE: &CStr = c"asterisk/native/channel/video.rs";
const SOURCE_FUNCTION: &CStr = c"channel_video";

#[derive(Clone, Copy, Debug)]
pub struct VideoRtpConfiguration<'a> {
    pub format: PbxVideoFormat,
    pub payload_type: RtpPayloadNumber,
    pub media_bind_address: &'a CStr,
    pub policy: RtpPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoRtpError {
    InvalidMediaAddress,
    RtpUnavailable,
    CapabilitiesUnavailable,
    ChannelUnavailable,
    VideoUnavailable,
    NativeRejected,
}

pub(super) struct PreparedVideoRtp {
    pub(super) media: OwnedVideoRtp,
    pub(super) qos: MediaSocketQosReport,
}

unsafe fn configure_format(
    rtp: &OwnedRtpInstance,
    format: PbxVideoFormat,
    payload_type: RtpPayloadNumber,
) -> Result<(), VideoRtpError> {
    let codecs = unsafe { sys::ast_rtp_instance_get_codecs(rtp.as_ptr()) };
    let selected = unsafe { video_format(format) };
    if codecs.is_null()
        || selected.is_null()
        || unsafe {
            sys::ast_rtp_codecs_payload_replace_format(
                codecs,
                c_int::from(payload_type.get()),
                selected,
            )
        } != 0
        || unsafe { sys::ast_rtp_codecs_set_preferred_format(codecs, selected) } != 0
    {
        return Err(VideoRtpError::RtpUnavailable);
    }
    unsafe { sys::ast_rtp_codecs_payloads_xover(codecs, codecs, rtp.as_ptr()) };
    Ok(())
}

pub(super) unsafe fn prepare_video(
    configuration: VideoRtpConfiguration<'_>,
) -> Result<PreparedVideoRtp, VideoRtpError> {
    let mut local_address = unsafe { mem::zeroed::<sys::ast_sockaddr>() };
    if unsafe {
        sys::ast_sockaddr_parse(
            &mut local_address,
            configuration.media_bind_address.as_ptr(),
            sys::PARSE_PORT_FORBID as c_int,
        )
    } == 0
    {
        return Err(VideoRtpError::InvalidMediaAddress);
    }
    let instance =
        unsafe { OwnedRtpInstance::create(&local_address) }.ok_or(VideoRtpError::RtpUnavailable)?;
    unsafe {
        sys::ast_rtp_instance_set_prop(instance.as_ptr(), sys::AST_RTP_PROPERTY_RTCP, 1);
        sys::ast_rtp_instance_set_prop(
            instance.as_ptr(),
            sys::AST_RTP_PROPERTY_NAT,
            c_int::from(configuration.policy.symmetric),
        );
        configure_format(&instance, configuration.format, configuration.payload_type)?;
    }
    let qos = unsafe { apply_media_socket_qos(&instance, configuration.policy) };
    Ok(PreparedVideoRtp {
        media: OwnedVideoRtp {
            instance,
            format: configuration.format,
        },
        qos,
    })
}

unsafe fn with_locked_video_rtp<T>(
    channel: NonNull<sys::ast_channel>,
    operation: impl FnOnce(NonNull<sys::ast_rtp_instance>) -> Result<T, VideoRtpError>,
) -> Result<T, VideoRtpError> {
    let _lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| VideoRtpError::ChannelUnavailable)?;
    let private =
        unsafe { channel_private(channel.as_ptr()) }.ok_or(VideoRtpError::ChannelUnavailable)?;
    let rtp = unsafe { private_video_rtp(private) }.ok_or(VideoRtpError::VideoUnavailable)?;
    operation(rtp)
}

pub unsafe fn set_remote_video(
    channel: NonNull<sys::ast_channel>,
    endpoint: SocketAddr,
) -> Result<(), VideoRtpError> {
    if endpoint.port() == 0 || endpoint.ip().is_unspecified() || endpoint.ip().is_multicast() {
        return Err(VideoRtpError::InvalidMediaAddress);
    }
    let endpoint =
        CString::new(endpoint.to_string()).map_err(|_| VideoRtpError::InvalidMediaAddress)?;
    let mut remote = unsafe { mem::zeroed::<sys::ast_sockaddr>() };
    if unsafe {
        sys::ast_sockaddr_parse(
            &mut remote,
            endpoint.as_ptr(),
            sys::PARSE_PORT_REQUIRE as c_int,
        )
    } == 0
    {
        return Err(VideoRtpError::InvalidMediaAddress);
    }
    unsafe {
        with_locked_video_rtp(channel, |rtp| {
            (sys::ast_rtp_instance_set_requested_target_address(rtp.as_ptr(), &remote) == 0)
                .then_some(())
                .ok_or(VideoRtpError::NativeRejected)
        })
    }
}

pub unsafe fn disable_video(channel: NonNull<sys::ast_channel>) -> Result<(), VideoRtpError> {
    let lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| VideoRtpError::ChannelUnavailable)?;
    let private =
        unsafe { channel_private(channel.as_ptr()) }.ok_or(VideoRtpError::ChannelUnavailable)?;
    let capabilities =
        unsafe { format_cap_alloc() }.ok_or(VideoRtpError::CapabilitiesUnavailable)?;
    let audio = unsafe { audio_format(private_audio_format(private)) };
    unsafe { format_cap_append(&capabilities, audio) }
        .map_err(|_| VideoRtpError::CapabilitiesUnavailable)?;
    let video = unsafe { take_private_video(private) };
    unsafe {
        sys::ast_channel_set_fd(channel.as_ptr(), 2, -1);
        sys::ast_channel_set_fd(channel.as_ptr(), 3, -1);
        sys::ast_channel_nativeformats_set(channel.as_ptr(), capabilities.as_ptr());
    }
    drop(lock);
    drop(video);
    Ok(())
}

pub unsafe fn local_video_endpoint(
    channel: NonNull<sys::ast_channel>,
) -> Result<LocalMediaEndpoint, VideoRtpError> {
    unsafe {
        with_locked_video_rtp(channel, |rtp| {
            let mut local = mem::zeroed::<sys::ast_sockaddr>();
            sys::ast_rtp_instance_get_local_address(rtp.as_ptr(), &mut local);
            let address = optional_c_text(
                sys::ast_sockaddr_stringify_fmt(&local, sys::AST_SOCKADDR_STR_ADDR as c_int),
                64,
            )
            .map_err(|_| VideoRtpError::NativeRejected)?
            .ok_or(VideoRtpError::NativeRejected)?
            .parse()
            .map_err(|_| VideoRtpError::NativeRejected)?;
            let port = sys::_ast_sockaddr_port(
                &local,
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            );
            (port != 0)
                .then_some(LocalMediaEndpoint { address, port })
                .ok_or(VideoRtpError::NativeRejected)
        })
    }
}
