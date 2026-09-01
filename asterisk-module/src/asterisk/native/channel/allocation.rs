//! Channel and RTP allocation ownership.

use std::ffi::{CStr, c_int, c_ulonglong};
use std::fmt;
use std::io;
use std::mem;
use std::os::fd::BorrowedFd;
use std::ptr::{self, NonNull};

use sccp_protocol::{
    SocketQosFailure, SocketQosPolicy, apply_socket_qos as apply_platform_socket_qos,
};

use crate::asterisk::direct::channel_driver::{rtp_scheduler, technology_ptr};
use crate::asterisk::direct::module_info::module_self;
use crate::asterisk::raw::handles::{
    Ao2Object, BorrowedChannelLock, ModuleReference, NativeStatus,
};
use crate::asterisk::sys;
use crate::media::formats::PbxVideoFormat;

use super::ownership::NativeChannelOwnership;
use super::video::{VideoRtpConfiguration, VideoRtpError, prepare_video};

const SOURCE_FILE: &CStr = c"asterisk/native/channel/allocation.rs";
const SOURCE_FUNCTION: &CStr = c"allocate_channel";
const CAPABILITIES_TAG: &CStr = c"SCCP channel capabilities";
const TELEPHONE_EVENT_PAYLOAD: c_int = 101;
const TELEPHONE_EVENT_SAMPLE_RATE: c_int = 8_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeAudioFormat {
    G711Ulaw,
    G711Alaw,
    G722,
    G723,
    G729,
    G726Aal2,
    Gsm,
    Slin16,
    Ilbc,
    Siren7,
    Opus,
}

impl NativeAudioFormat {
    const fn skinny_rtp_payload(self) -> c_int {
        match self {
            Self::G711Ulaw => 0,
            Self::G711Alaw => 8,
            Self::G722 => 9,
            Self::G723 => 4,
            Self::G729 => 18,
            Self::G726Aal2 => 112,
            Self::Gsm => 3,
            Self::Slin16 => 25,
            Self::Ilbc => 97,
            Self::Siren7 => 102,
            Self::Opus => 107,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelAllocationError {
    InvalidMediaAddress,
    ModuleUnavailable,
    RtpUnavailable,
    ChannelUnavailable,
    CapabilitiesUnavailable,
}

pub struct ChannelAllocation<'a> {
    pub line: &'a CStr,
    pub context: &'a CStr,
    pub caller_number: &'a CStr,
    pub caller_name: &'a CStr,
    pub identity: ChannelIdentity,
    pub format: NativeAudioFormat,
    pub media_bind_address: &'a CStr,
    pub assigned_ids: Option<&'a sys::ast_assigned_ids>,
    pub assigned_uniqueid: Option<&'a CStr>,
    pub requestor: *const sys::ast_channel,
    pub rtp_policy: RtpPolicy,
    pub video: Option<VideoRtpConfiguration<'a>>,
    pub security: NativeChannelSecurity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelIdentity {
    pub pbx_id: u64,
    pub sccp_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpPolicy {
    pub symmetric: bool,
    pub dscp: u8,
    pub cos: u8,
}

impl SocketQosPolicy for RtpPolicy {
    fn dscp(self) -> u8 {
        self.dscp
    }

    fn cos(self) -> u8 {
        self.cos
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaSocketKind {
    Rtp,
    Rtcp,
}

impl fmt::Display for MediaSocketKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rtp => "RTP",
            Self::Rtcp => "RTCP",
        })
    }
}

#[derive(Debug)]
pub enum MediaSocketQosFailure {
    Unavailable {
        socket: MediaSocketKind,
    },
    Inspection {
        socket: MediaSocketKind,
        source: io::Error,
    },
    Marking {
        socket: MediaSocketKind,
        failure: SocketQosFailure,
    },
}

impl fmt::Display for MediaSocketQosFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { socket } => write!(formatter, "{socket} socket is unavailable"),
            Self::Inspection { socket, source } => {
                write!(formatter, "unable to inspect {socket} socket: {source}")
            }
            Self::Marking { socket, failure } => write!(formatter, "{socket}: {failure}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct MediaSocketQosReport {
    failures: Vec<MediaSocketQosFailure>,
}

impl MediaSocketQosReport {
    pub fn failures(&self) -> impl ExactSizeIterator<Item = &MediaSocketQosFailure> {
        self.failures.iter()
    }
}

pub struct AllocatedChannel {
    pub channel: NonNull<sys::ast_channel>,
    pub qos: MediaSocketQosReport,
    pub video: VideoRtpAllocation,
}

#[derive(Debug)]
pub enum VideoRtpAllocation {
    NotRequested,
    Active(MediaSocketQosReport),
    Unavailable(VideoRtpError),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeChannelSecurity {
    pub signaling: bool,
    pub media: bool,
}

pub struct ChannelPrivate {
    audio: OwnedAudioRtp,
    video: Option<OwnedVideoRtp>,
    _module: ModuleReference,
    owner: Option<NonNull<sys::ast_channel>>,
    identity: Option<ChannelIdentity>,
    security: NativeChannelSecurity,
    ownership: NativeChannelOwnership,
}

/// Owns an allocated, locked channel until all technology state is installed.
/// Returning early invokes the registered technology destructor through the
/// normal native hangup path.
struct UnpublishedChannel {
    channel: NonNull<sys::ast_channel>,
    lock: Option<BorrowedChannelLock>,
    published: bool,
}

impl UnpublishedChannel {
    unsafe fn new(channel: NonNull<sys::ast_channel>) -> Self {
        Self {
            channel,
            lock: Some(unsafe { BorrowedChannelLock::from_locked(channel) }),
            published: false,
        }
    }

    fn channel(&self) -> NonNull<sys::ast_channel> {
        self.channel
    }

    fn publish(mut self) -> NonNull<sys::ast_channel> {
        self.published = true;
        drop(self.lock.take());
        self.channel
    }
}

impl Drop for UnpublishedChannel {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        if let Some(private) = unsafe { channel_private(self.channel.as_ptr()) } {
            let _ = unsafe { take_private_identity(private) };
        }
        drop(self.lock.take());
        unsafe { sys::ast_hangup(self.channel.as_ptr()) };
    }
}

pub(super) struct OwnedRtpInstance(NonNull<sys::ast_rtp_instance>);

impl OwnedRtpInstance {
    pub(super) unsafe fn create(local_address: &sys::ast_sockaddr) -> Option<Self> {
        let scheduler = rtp_scheduler()?;
        NonNull::new(unsafe {
            sys::ast_rtp_instance_new(
                c"asterisk".as_ptr(),
                scheduler.as_ptr(),
                local_address,
                ptr::null_mut(),
            )
        })
        .map(Self)
    }

    pub(super) const fn as_ptr(&self) -> *mut sys::ast_rtp_instance {
        self.0.as_ptr()
    }

    pub(super) const fn as_non_null(&self) -> NonNull<sys::ast_rtp_instance> {
        self.0
    }
}

impl Drop for OwnedRtpInstance {
    fn drop(&mut self) {
        unsafe {
            sys::ast_rtp_instance_stop(self.as_ptr());
            sys::ast_rtp_instance_destroy(self.as_ptr());
        }
    }
}

pub(super) struct OwnedAudioRtp {
    pub(super) instance: OwnedRtpInstance,
    pub(super) format: NativeAudioFormat,
}

pub(super) struct OwnedVideoRtp {
    pub(super) instance: OwnedRtpInstance,
    pub(super) format: PbxVideoFormat,
}

pub(super) unsafe fn apply_media_socket_qos(
    rtp: &OwnedRtpInstance,
    policy: RtpPolicy,
) -> MediaSocketQosReport {
    let mut report = MediaSocketQosReport::default();
    let mut previous_fd = None;
    for (socket, rtcp) in [(MediaSocketKind::Rtp, 0), (MediaSocketKind::Rtcp, 1)] {
        let fd = unsafe { sys::ast_rtp_instance_fd(rtp.as_ptr(), rtcp) };
        if fd < 0 {
            report
                .failures
                .push(MediaSocketQosFailure::Unavailable { socket });
            continue;
        }
        if previous_fd == Some(fd) {
            continue;
        }
        previous_fd = Some(fd);
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        match apply_platform_socket_qos(&borrowed, policy) {
            Ok(socket_report) => {
                report.failures.extend(
                    socket_report
                        .into_failures()
                        .map(|failure| MediaSocketQosFailure::Marking { socket, failure }),
                );
            }
            Err(source) => report
                .failures
                .push(MediaSocketQosFailure::Inspection { socket, source }),
        }
    }
    report
}

pub unsafe fn channel_private(channel: *mut sys::ast_channel) -> Option<NonNull<ChannelPrivate>> {
    let channel = NonNull::new(channel)?;
    if unsafe { sys::ast_channel_tech(channel.as_ptr()) } != unsafe { technology_ptr() } {
        return None;
    }
    // SAFETY: the technology identity proves the private pointer's concrete type.
    NonNull::new(unsafe { sys::ast_channel_tech_pvt(channel.as_ptr()) }.cast())
}

pub unsafe fn destroy_channel_private(private: NonNull<ChannelPrivate>) {
    // SAFETY: channel technology owns this allocation exclusively and clears
    // the tech-pvt pointer before destruction.
    drop(unsafe { Box::from_raw(private.as_ptr()) });
}

pub unsafe fn prepare_channel_private_teardown(
    channel: NonNull<sys::ast_channel>,
    private: NonNull<ChannelPrivate>,
) {
    let rtp = unsafe { private.as_ref() }.audio.instance.as_ptr();
    unsafe {
        sys::ast_rtp_instance_set_stats_vars(channel.as_ptr(), rtp);
        sys::ast_channel_set_fd(channel.as_ptr(), 0, -1);
        sys::ast_channel_set_fd(channel.as_ptr(), 1, -1);
        sys::ast_channel_set_fd(channel.as_ptr(), 2, -1);
        sys::ast_channel_set_fd(channel.as_ptr(), 3, -1);
        (*private.as_ptr()).owner = None;
    }
}

pub unsafe fn private_rtp(private: NonNull<ChannelPrivate>) -> NonNull<sys::ast_rtp_instance> {
    unsafe { private.as_ref() }.audio.instance.as_non_null()
}

pub unsafe fn retain_private_rtp(private: NonNull<ChannelPrivate>) -> *mut sys::ast_rtp_instance {
    let rtp = unsafe { private_rtp(private) };
    unsafe { Ao2Object::from_borrowed(rtp) }.into_raw()
}

pub unsafe fn private_video_rtp(
    private: NonNull<ChannelPrivate>,
) -> Option<NonNull<sys::ast_rtp_instance>> {
    unsafe { private.as_ref() }
        .video
        .as_ref()
        .map(|video| video.instance.as_non_null())
}

pub unsafe fn retain_private_video_rtp(
    private: NonNull<ChannelPrivate>,
) -> Option<*mut sys::ast_rtp_instance> {
    let rtp = unsafe { private_video_rtp(private) }?;
    Some(unsafe { Ao2Object::from_borrowed(rtp) }.into_raw())
}

pub(super) unsafe fn private_video_format(
    private: NonNull<ChannelPrivate>,
) -> Option<PbxVideoFormat> {
    unsafe { private.as_ref() }
        .video
        .as_ref()
        .map(|video| video.format)
}

pub(super) unsafe fn take_private_video(private: NonNull<ChannelPrivate>) -> Option<OwnedVideoRtp> {
    unsafe { (*private.as_ptr()).video.take() }
}

pub(super) unsafe fn private_audio_format(private: NonNull<ChannelPrivate>) -> NativeAudioFormat {
    unsafe { private.as_ref() }.audio.format
}

pub(super) unsafe fn set_private_audio_format(
    private: NonNull<ChannelPrivate>,
    format: NativeAudioFormat,
) {
    unsafe { (*private.as_ptr()).audio.format = format };
}

pub unsafe fn reassign_private_owner(
    private: NonNull<ChannelPrivate>,
    owner: NonNull<sys::ast_channel>,
) {
    unsafe { (*private.as_ptr()).owner = Some(owner) };
}

pub unsafe fn private_owner(private: NonNull<ChannelPrivate>) -> Option<NonNull<sys::ast_channel>> {
    unsafe { private.as_ref() }.owner
}

pub unsafe fn private_security(private: NonNull<ChannelPrivate>) -> NativeChannelSecurity {
    unsafe { private.as_ref() }.security
}

pub(super) unsafe fn private_identity(private: NonNull<ChannelPrivate>) -> Option<ChannelIdentity> {
    unsafe { private.as_ref() }.identity
}

pub(super) unsafe fn take_private_identity(
    private: NonNull<ChannelPrivate>,
) -> Option<ChannelIdentity> {
    unsafe { (*private.as_ptr()).identity.take() }
}

pub unsafe fn handoff_channel_to_asterisk(channel: NonNull<sys::ast_channel>) -> Result<(), ()> {
    let private = unsafe { channel_private(channel.as_ptr()) }.ok_or(())?;
    unsafe { private.as_ref() }
        .ownership
        .handoff_to_asterisk()
        .map_err(|_| ())
}

pub(super) unsafe fn private_ownership(
    private: &NonNull<ChannelPrivate>,
) -> &NativeChannelOwnership {
    &unsafe { private.as_ref() }.ownership
}

pub(super) unsafe fn audio_format(format: NativeAudioFormat) -> *mut sys::ast_format {
    unsafe {
        match format {
            NativeAudioFormat::G711Ulaw => sys::ast_format_ulaw,
            NativeAudioFormat::G711Alaw => sys::ast_format_alaw,
            NativeAudioFormat::G722 => sys::ast_format_g722,
            NativeAudioFormat::G723 => sys::ast_format_g723,
            NativeAudioFormat::G729 => sys::ast_format_g729,
            NativeAudioFormat::G726Aal2 => sys::ast_format_g726_aal2,
            NativeAudioFormat::Gsm => sys::ast_format_gsm,
            NativeAudioFormat::Slin16 => sys::ast_format_slin16,
            NativeAudioFormat::Ilbc => sys::ast_format_ilbc,
            NativeAudioFormat::Siren7 => sys::ast_format_siren7,
            NativeAudioFormat::Opus => sys::ast_format_opus,
        }
    }
}

pub unsafe fn video_format(format: PbxVideoFormat) -> *mut sys::ast_format {
    unsafe {
        match format {
            PbxVideoFormat::H261 => sys::ast_format_h261,
            PbxVideoFormat::H263 => sys::ast_format_h263,
            PbxVideoFormat::H263Plus => sys::ast_format_h263p,
            PbxVideoFormat::H264 => sys::ast_format_h264,
            PbxVideoFormat::H265 => sys::ast_format_h265,
        }
    }
}

pub(super) unsafe fn format_cap_alloc() -> Option<Ao2Object<sys::ast_format_cap>> {
    unsafe {
        Ao2Object::from_owned(sys::__ast_format_cap_alloc(
            sys::AST_FORMAT_CAP_FLAG_DEFAULT,
            CAPABILITIES_TAG.as_ptr(),
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
        ))
    }
}

pub(super) unsafe fn format_cap_append(
    capabilities: &Ao2Object<sys::ast_format_cap>,
    format: *mut sys::ast_format,
) -> Result<(), ()> {
    let status = unsafe {
        sys::__ast_format_cap_append(
            capabilities.as_ptr(),
            format,
            0,
            CAPABILITIES_TAG.as_ptr(),
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
        )
    };
    NativeStatus::new(status).result(())
}

unsafe fn configure_rfc2833(rtp: *mut sys::ast_rtp_instance) -> Result<(), ()> {
    let codecs = unsafe { sys::ast_rtp_instance_get_codecs(rtp) };
    if codecs.is_null() {
        return Err(());
    }

    // OpenReceiveChannel and StartMediaTransmission advertise payload 101 to
    // RFC2833-capable stations. Register the same payload with Asterisk in
    // both directions; otherwise the RTP engine sees the phone's valid
    // telephone-event packets as an unknown dynamic payload while the SCCP
    // server suppresses their duplicate KeypadButton signaling copy.
    unsafe {
        sys::ast_rtp_instance_set_prop(rtp, sys::AST_RTP_PROPERTY_DTMF, 1);
    }
    if unsafe { sys::ast_rtp_instance_dtmf_mode_set(rtp, sys::AST_RTP_DTMF_MODE_RFC2833) } != 0 {
        return Err(());
    }
    if unsafe {
        sys::ast_rtp_codecs_payloads_set_rtpmap_type_rate(
            codecs,
            rtp,
            TELEPHONE_EVENT_PAYLOAD,
            c"audio".as_ptr().cast_mut(),
            c"telephone-event".as_ptr().cast_mut(),
            0 as sys::ast_rtp_options,
            TELEPHONE_EVENT_SAMPLE_RATE as u32,
        )
    } != 0
    {
        return Err(());
    }
    if unsafe {
        sys::ast_rtp_codecs_set_preferred_dtmf_format(
            codecs,
            TELEPHONE_EVENT_PAYLOAD,
            TELEPHONE_EVENT_SAMPLE_RATE,
        )
    } != 0
    {
        return Err(());
    }

    // RTP map setup populates the transmit map. SCCP does not have an SDP
    // negotiation step that would normally cross it into the receive map, so
    // do that explicitly after all station-specific payload overrides have
    // been installed and before the phone can send media.
    unsafe {
        sys::ast_rtp_codecs_payloads_xover(codecs, codecs, rtp);
    }
    Ok(())
}

pub(super) unsafe fn configure_audio_payload(
    rtp: *mut sys::ast_rtp_instance,
    format: NativeAudioFormat,
) -> Result<(), ()> {
    let codecs = unsafe { sys::ast_rtp_instance_get_codecs(rtp) };
    if codecs.is_null() {
        return Err(());
    }
    let selected = unsafe { audio_format(format) };
    if selected.is_null()
        || unsafe {
            sys::ast_rtp_codecs_payload_replace_format(
                codecs,
                format.skinny_rtp_payload(),
                selected,
            )
        } != 0
        || unsafe { sys::ast_rtp_codecs_set_preferred_format(codecs, selected) } != 0
    {
        return Err(());
    }
    Ok(())
}

pub unsafe fn allocate_channel(
    request: ChannelAllocation<'_>,
) -> Result<AllocatedChannel, ChannelAllocationError> {
    let selected = unsafe { audio_format(request.format) };
    let mut local_address = unsafe { mem::zeroed::<sys::ast_sockaddr>() };
    if unsafe {
        sys::ast_sockaddr_parse(
            &mut local_address,
            request.media_bind_address.as_ptr(),
            sys::PARSE_PORT_FORBID as c_int,
        )
    } == 0
    {
        return Err(ChannelAllocationError::InvalidMediaAddress);
    }

    let module = unsafe { ModuleReference::acquire(module_self()) }
        .ok_or(ChannelAllocationError::ModuleUnavailable)?;
    let rtp = unsafe { OwnedRtpInstance::create(&local_address) }
        .ok_or(ChannelAllocationError::RtpUnavailable)?;
    unsafe {
        sys::ast_rtp_instance_set_prop(rtp.as_ptr(), sys::AST_RTP_PROPERTY_RTCP, 1);
        sys::ast_rtp_instance_set_prop(
            rtp.as_ptr(),
            sys::AST_RTP_PROPERTY_NAT,
            c_int::from(request.rtp_policy.symmetric),
        );
    }
    let qos = unsafe { apply_media_socket_qos(&rtp, request.rtp_policy) };
    let (video, mut video_result) = match request
        .video
        .map(|configuration| unsafe { prepare_video(configuration) })
    {
        Some(Ok(prepared)) => (
            Some(prepared.media),
            VideoRtpAllocation::Active(prepared.qos),
        ),
        Some(Err(error)) => (None, VideoRtpAllocation::Unavailable(error)),
        None => (None, VideoRtpAllocation::NotRequested),
    };
    let private = Box::new(ChannelPrivate {
        audio: OwnedAudioRtp {
            instance: rtp,
            format: request.format,
        },
        video,
        _module: module,
        owner: None,
        identity: Some(request.identity),
        security: request.security,
        ownership: NativeChannelOwnership::module_owned(),
    });
    // A channel technology has no SDP parsing step to populate its RTP maps.
    // Install the exact Skinny payload selected for this call, including the
    // station-specific dynamic assignments, before crossing all mappings into
    // receive state in `configure_rfc2833`.
    if unsafe { configure_audio_payload(private.audio.instance.as_ptr(), request.format) }
        .and_then(|()| unsafe { configure_rfc2833(private.audio.instance.as_ptr()) })
        .is_err()
    {
        return Err(ChannelAllocationError::RtpUnavailable);
    }
    let mut assigned = unsafe { mem::zeroed::<sys::ast_assigned_ids>() };
    let assigned_ids = if let Some(uniqueid) = request.assigned_uniqueid {
        assigned.uniqueid = uniqueid.as_ptr();
        &assigned
    } else {
        request.assigned_ids.map_or(ptr::null(), std::ptr::from_ref)
    };
    let channel = NonNull::new(unsafe {
        sys::__ast_channel_alloc(
            1,
            sys::AST_STATE_DOWN as c_int,
            request.caller_number.as_ptr(),
            request.caller_name.as_ptr(),
            c"".as_ptr(),
            c"".as_ptr(),
            request.context.as_ptr(),
            assigned_ids,
            request.requestor,
            sys::AST_AMA_NONE,
            ptr::null_mut(),
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
            c"SCCP/%s-%08llx".as_ptr(),
            request.line.as_ptr(),
            request.identity.sccp_id as c_ulonglong,
        )
    });
    let Some(channel) = channel else {
        return Err(ChannelAllocationError::ChannelUnavailable);
    };
    let private = unsafe { NonNull::new_unchecked(Box::into_raw(private)) };

    unsafe {
        (*private.as_ptr()).owner = Some(channel);
        sys::ast_rtp_instance_set_channel_id(
            (*private.as_ptr()).audio.instance.as_ptr(),
            sys::ast_channel_uniqueid(channel.as_ptr()),
        );
        sys::ast_channel_set_fd(
            channel.as_ptr(),
            0,
            sys::ast_rtp_instance_fd((*private.as_ptr()).audio.instance.as_ptr(), 0),
        );
        sys::ast_channel_set_fd(
            channel.as_ptr(),
            1,
            sys::ast_rtp_instance_fd((*private.as_ptr()).audio.instance.as_ptr(), 1),
        );
        sys::ast_channel_set_fd(channel.as_ptr(), 2, -1);
        sys::ast_channel_set_fd(channel.as_ptr(), 3, -1);
        sys::ast_channel_tech_set(channel.as_ptr(), technology_ptr());
        sys::ast_channel_tech_pvt_set(channel.as_ptr(), private.as_ptr().cast());
    }
    let pending = unsafe { UnpublishedChannel::new(channel) };

    let Some(capabilities) = (unsafe { format_cap_alloc() }) else {
        return Err(ChannelAllocationError::CapabilitiesUnavailable);
    };
    if unsafe { format_cap_append(&capabilities, selected) }.is_err() {
        return Err(ChannelAllocationError::CapabilitiesUnavailable);
    }
    if let Some(video) = unsafe { (*private.as_ptr()).video.take() } {
        if unsafe { format_cap_append(&capabilities, video_format(video.format)) }.is_err() {
            video_result = VideoRtpAllocation::Unavailable(VideoRtpError::CapabilitiesUnavailable);
        } else {
            unsafe {
                sys::ast_rtp_instance_set_channel_id(
                    video.instance.as_ptr(),
                    sys::ast_channel_uniqueid(channel.as_ptr()),
                );
                sys::ast_channel_set_fd(
                    channel.as_ptr(),
                    2,
                    sys::ast_rtp_instance_fd(video.instance.as_ptr(), 0),
                );
                sys::ast_channel_set_fd(
                    channel.as_ptr(),
                    3,
                    sys::ast_rtp_instance_fd(video.instance.as_ptr(), 1),
                );
                (*private.as_ptr()).video = Some(video);
            }
        }
    }
    unsafe {
        sys::ast_channel_nativeformats_set(pending.channel().as_ptr(), capabilities.as_ptr());
        sys::ast_channel_set_writeformat(pending.channel().as_ptr(), selected);
        sys::ast_channel_set_rawwriteformat(pending.channel().as_ptr(), selected);
        sys::ast_channel_set_readformat(pending.channel().as_ptr(), selected);
        sys::ast_channel_set_rawreadformat(pending.channel().as_ptr(), selected);
    }
    Ok(AllocatedChannel {
        channel: pending.publish(),
        qos,
        video: video_result,
    })
}
