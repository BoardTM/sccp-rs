//! Channel format capability and RTP endpoint operations.

use std::ffi::{CStr, CString, c_char, c_int};
use std::net::{IpAddr, SocketAddr};
use std::ptr::NonNull;

use crate::asterisk::boundary::optional_c_text;
use crate::asterisk::raw::handles::{Ao2Object, BorrowedChannelLock as ChannelLock};
use crate::asterisk::sys;
use crate::media::formats::PbxVideoFormat;

use super::allocation::{
    NativeAudioFormat, audio_format, channel_private, configure_audio_payload, format_cap_alloc,
    format_cap_append, private_rtp, private_video_format, private_video_rtp,
    set_private_audio_format, video_format,
};

const SOURCE_FILE: &CStr = c"asterisk/native/channel/media.rs";
const SOURCE_FUNCTION: &CStr = c"channel_media";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaOperationError {
    CapabilitiesUnavailable,
    Rejected,
}

unsafe fn with_locked_rtp<T>(
    channel: NonNull<sys::ast_channel>,
    operation: impl FnOnce(NonNull<sys::ast_rtp_instance>) -> Result<T, MediaOperationError>,
) -> Result<T, MediaOperationError> {
    let _lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| MediaOperationError::Rejected)?;
    let private =
        unsafe { channel_private(channel.as_ptr()) }.ok_or(MediaOperationError::Rejected)?;
    operation(unsafe { private_rtp(private) })
}

pub unsafe fn send_digit_begin(
    channel: NonNull<sys::ast_channel>,
    digit: u8,
) -> Result<(), MediaOperationError> {
    unsafe {
        with_locked_rtp(channel, |rtp| {
            (sys::ast_rtp_instance_dtmf_begin(rtp.as_ptr(), c_char::from_ne_bytes([digit])) == 0)
                .then_some(())
                .ok_or(MediaOperationError::Rejected)
        })
    }
}

pub unsafe fn send_digit_end(
    channel: NonNull<sys::ast_channel>,
    digit: u8,
    duration_ms: u32,
) -> Result<(), MediaOperationError> {
    unsafe {
        with_locked_rtp(channel, |rtp| {
            (sys::ast_rtp_instance_dtmf_end_with_duration(
                rtp.as_ptr(),
                c_char::from_ne_bytes([digit]),
                duration_ms,
            ) == 0)
                .then_some(())
                .ok_or(MediaOperationError::Rejected)
        })
    }
}

pub unsafe fn update_source(channel: NonNull<sys::ast_channel>) -> Result<(), MediaOperationError> {
    let _lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| MediaOperationError::Rejected)?;
    let private =
        unsafe { channel_private(channel.as_ptr()) }.ok_or(MediaOperationError::Rejected)?;
    unsafe { sys::ast_rtp_instance_update_source(private_rtp(private).as_ptr()) };
    if let Some(video) = unsafe { private_video_rtp(private) } {
        unsafe { sys::ast_rtp_instance_update_source(video.as_ptr()) };
    }
    Ok(())
}

pub unsafe fn change_source(channel: NonNull<sys::ast_channel>) -> Result<(), MediaOperationError> {
    let _lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| MediaOperationError::Rejected)?;
    let private =
        unsafe { channel_private(channel.as_ptr()) }.ok_or(MediaOperationError::Rejected)?;
    unsafe { sys::ast_rtp_instance_change_source(private_rtp(private).as_ptr()) };
    if let Some(video) = unsafe { private_video_rtp(private) } {
        unsafe { sys::ast_rtp_instance_change_source(video.as_ptr()) };
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalMediaEndpoint {
    pub address: IpAddr,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioCapabilityMask(u32);

impl AudioCapabilityMask {
    const ULAW: u32 = 1 << 0;
    const ALAW: u32 = 1 << 1;
    const G722: u32 = 1 << 2;
    const G723: u32 = 1 << 3;
    const G729: u32 = 1 << 4;
    const G726_AAL2: u32 = 1 << 5;
    const GSM: u32 = 1 << 6;
    const SLIN16: u32 = 1 << 7;
    const ILBC: u32 = 1 << 8;
    const SIREN7: u32 = 1 << 9;
    const OPUS: u32 = 1 << 10;

    pub const fn all() -> Self {
        Self(
            Self::ULAW
                | Self::ALAW
                | Self::G722
                | Self::G723
                | Self::G729
                | Self::G726_AAL2
                | Self::GSM
                | Self::SLIN16
                | Self::ILBC
                | Self::SIREN7
                | Self::OPUS,
        )
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoCapabilityMask(u32);

impl VideoCapabilityMask {
    pub fn all() -> Self {
        Self(
            PbxVideoFormat::ALL
                .into_iter()
                .fold(0, |mask, format| mask | format.native_mask()),
        )
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

pub unsafe fn audio_capability_mask(
    capabilities: Option<NonNull<sys::ast_format_cap>>,
) -> AudioCapabilityMask {
    let Some(capabilities) = capabilities else {
        return AudioCapabilityMask::all();
    };
    let mut mask = 0;
    for (format, bit) in unsafe {
        [
            (sys::ast_format_ulaw, AudioCapabilityMask::ULAW),
            (sys::ast_format_alaw, AudioCapabilityMask::ALAW),
            (sys::ast_format_g722, AudioCapabilityMask::G722),
            (sys::ast_format_g723, AudioCapabilityMask::G723),
            (sys::ast_format_g729, AudioCapabilityMask::G729),
            (sys::ast_format_g726_aal2, AudioCapabilityMask::G726_AAL2),
            (sys::ast_format_gsm, AudioCapabilityMask::GSM),
            (sys::ast_format_slin16, AudioCapabilityMask::SLIN16),
            (sys::ast_format_ilbc, AudioCapabilityMask::ILBC),
            (sys::ast_format_siren7, AudioCapabilityMask::SIREN7),
            (sys::ast_format_opus, AudioCapabilityMask::OPUS),
        ]
    } {
        if unsafe { sys::ast_format_cap_iscompatible_format(capabilities.as_ptr(), format) }
            != sys::AST_FORMAT_CMP_NOT_EQUAL
        {
            mask |= bit;
        }
    }
    AudioCapabilityMask(mask)
}

pub unsafe fn identify_audio_format(format: NonNull<sys::ast_format>) -> Option<NativeAudioFormat> {
    [
        NativeAudioFormat::G711Ulaw,
        NativeAudioFormat::G711Alaw,
        NativeAudioFormat::G722,
        NativeAudioFormat::G723,
        NativeAudioFormat::G729,
        NativeAudioFormat::G726Aal2,
        NativeAudioFormat::Gsm,
        NativeAudioFormat::Slin16,
        NativeAudioFormat::Ilbc,
        NativeAudioFormat::Siren7,
        NativeAudioFormat::Opus,
    ]
    .into_iter()
    .find(|candidate| unsafe {
        sys::ast_format_cmp(audio_format(*candidate), format.as_ptr()) == sys::AST_FORMAT_CMP_EQUAL
    })
}

pub unsafe fn set_private_audio_codec(
    channel: NonNull<sys::ast_channel>,
    format: NativeAudioFormat,
) -> Result<(), MediaOperationError> {
    let _lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| MediaOperationError::Rejected)?;
    let private =
        unsafe { channel_private(channel.as_ptr()) }.ok_or(MediaOperationError::Rejected)?;
    unsafe { configure_audio_payload(private_rtp(private).as_ptr(), format) }
        .map_err(|_| MediaOperationError::Rejected)?;
    unsafe { set_private_audio_format(private, format) };
    Ok(())
}

/// Ask Asterisk's translator graph to choose the best station-native format
/// for an incoming set of source capabilities.
pub unsafe fn best_translated_audio_format(
    source: NonNull<sys::ast_format_cap>,
    destinations: &[NativeAudioFormat],
) -> Option<NativeAudioFormat> {
    let destination_capabilities = unsafe { format_cap_alloc() }?;
    for destination in destinations.iter().copied() {
        if unsafe { format_cap_append(&destination_capabilities, audio_format(destination)) }
            .is_err()
        {
            return None;
        }
    }
    let mut selected_destination = std::ptr::null_mut();
    let mut selected_source = std::ptr::null_mut();
    let result = unsafe {
        sys::ast_translator_best_choice(
            destination_capabilities.as_ptr(),
            source.as_ptr(),
            &mut selected_destination,
            &mut selected_source,
        )
    };
    let selected = (result == 0)
        .then(|| {
            destinations.iter().copied().find(|destination| unsafe {
                sys::ast_format_cmp(audio_format(*destination), selected_destination)
                    == sys::AST_FORMAT_CMP_EQUAL
            })
        })
        .flatten();
    let _selected_destination = unsafe { Ao2Object::from_owned(selected_destination) };
    let _selected_source = unsafe { Ao2Object::from_owned(selected_source) };
    selected
}

pub unsafe fn release_format_cap(capabilities: NonNull<sys::ast_format_cap>) {
    drop(unsafe { Ao2Object::from_owned(capabilities.as_ptr()) });
}

pub unsafe fn video_capability_mask(
    capabilities: Option<NonNull<sys::ast_format_cap>>,
) -> VideoCapabilityMask {
    let Some(capabilities) = capabilities else {
        return VideoCapabilityMask::all();
    };
    let mut mask = 0;
    for (format, video_format) in unsafe {
        [
            (sys::ast_format_h261, PbxVideoFormat::H261),
            (sys::ast_format_h263, PbxVideoFormat::H263),
            (sys::ast_format_h263p, PbxVideoFormat::H263Plus),
            (sys::ast_format_h264, PbxVideoFormat::H264),
            (sys::ast_format_h265, PbxVideoFormat::H265),
        ]
    } {
        if unsafe { sys::ast_format_cap_iscompatible_format(capabilities.as_ptr(), format) }
            != sys::AST_FORMAT_CMP_NOT_EQUAL
        {
            mask |= video_format.native_mask();
        }
    }
    VideoCapabilityMask(mask)
}

pub unsafe fn audio_framing(
    capabilities: Option<NonNull<sys::ast_format_cap>>,
    format: NativeAudioFormat,
) -> Option<u32> {
    let selected = unsafe { audio_format(format) };
    let framing = if let Some(capabilities) = capabilities {
        unsafe { sys::ast_format_cap_get_format_framing(capabilities.as_ptr(), selected) }
    } else {
        unsafe { sys::ast_format_get_default_ms(selected) }
    };
    (framing != 0).then_some(framing)
}

pub unsafe fn set_audio_format(
    channel: NonNull<sys::ast_channel>,
    format: NativeAudioFormat,
) -> Result<(), MediaOperationError> {
    let selected = unsafe { audio_format(format) };
    let _lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| MediaOperationError::Rejected)?;
    let private =
        unsafe { channel_private(channel.as_ptr()) }.ok_or(MediaOperationError::Rejected)?;
    let capabilities =
        unsafe { format_cap_alloc() }.ok_or(MediaOperationError::CapabilitiesUnavailable)?;
    if unsafe { format_cap_append(&capabilities, selected) }.is_err()
        || unsafe { private_video_format(private) }.is_some_and(|video| unsafe {
            format_cap_append(&capabilities, video_format(video)).is_err()
        })
    {
        return Err(MediaOperationError::CapabilitiesUnavailable);
    }
    unsafe { configure_audio_payload(private_rtp(private).as_ptr(), format) }
        .map_err(|_| MediaOperationError::Rejected)?;
    unsafe {
        sys::ast_channel_nativeformats_set(channel.as_ptr(), capabilities.as_ptr());
        sys::ast_channel_set_writeformat(channel.as_ptr(), selected);
        sys::ast_channel_set_rawwriteformat(channel.as_ptr(), selected);
        sys::ast_channel_set_readformat(channel.as_ptr(), selected);
        sys::ast_channel_set_rawreadformat(channel.as_ptr(), selected);
        set_private_audio_format(private, format);
    }
    Ok(())
}

pub unsafe fn set_remote_media(
    channel: NonNull<sys::ast_channel>,
    endpoint: SocketAddr,
) -> Result<(), MediaOperationError> {
    let endpoint = CString::new(endpoint.to_string()).map_err(|_| MediaOperationError::Rejected)?;
    let mut remote = unsafe { std::mem::zeroed::<sys::ast_sockaddr>() };
    if unsafe {
        sys::ast_sockaddr_parse(
            &mut remote,
            endpoint.as_ptr(),
            sys::PARSE_PORT_REQUIRE as c_int,
        )
    } == 0
    {
        return Err(MediaOperationError::Rejected);
    }
    unsafe {
        with_locked_rtp(channel, |rtp| {
            (sys::ast_rtp_instance_set_requested_target_address(rtp.as_ptr(), &remote) == 0)
                .then_some(())
                .ok_or(MediaOperationError::Rejected)
        })
    }
}

pub unsafe fn local_media_endpoint(
    channel: NonNull<sys::ast_channel>,
) -> Result<LocalMediaEndpoint, MediaOperationError> {
    unsafe {
        with_locked_rtp(channel, |rtp| {
            let mut local = std::mem::zeroed::<sys::ast_sockaddr>();
            sys::ast_rtp_instance_get_local_address(rtp.as_ptr(), &mut local);
            let address =
                sys::ast_sockaddr_stringify_fmt(&local, sys::AST_SOCKADDR_STR_ADDR as c_int);
            let address = optional_c_text(address, 64)
                .map_err(|_| MediaOperationError::Rejected)?
                .ok_or(MediaOperationError::Rejected)?
                .parse()
                .map_err(|_| MediaOperationError::Rejected)?;
            let port = sys::_ast_sockaddr_port(
                &local,
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            );
            (port != 0)
                .then_some(LocalMediaEndpoint { address, port })
                .ok_or(MediaOperationError::Rejected)
        })
    }
}
