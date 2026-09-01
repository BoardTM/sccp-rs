use super::{
    AsteriskChannel, AsteriskChannelMetadata, AsteriskPartyUpdates, AutoAnswerMode, CString,
    CallMetadata, ChannelState, Codec, Digit, LogLevel, ModuleConfig, NonNull, PartySnapshot,
    PbxAudioFormat, native_channel, parse_requestor_mode, pbx_audio_format, raw, sys,
};

pub fn format_for(codec: Codec) -> Option<native_channel::NativeAudioFormat> {
    Some(native_audio_format(pbx_audio_format(codec).ok()?))
}

pub fn dial_terminator_digit(character: char) -> Result<Digit, String> {
    match character {
        '0'..='9' => Ok(Digit::Number(character as u8 - b'0')),
        '*' => Ok(Digit::Star),
        '#' => Ok(Digit::Pound),
        'A' => Ok(Digit::A),
        'B' => Ok(Digit::B),
        'C' => Ok(Digit::C),
        'D' => Ok(Digit::D),
        _ => Err(format!("invalid configured dial terminator {character:?}")),
    }
}

pub fn anonymous_hotline_definition(
    config: &ModuleConfig,
) -> Result<Option<sccp_protocol::AnonymousHotlineDefinition>, String> {
    config
        .guest_hotline()
        .enabled
        .then(|| {
            sccp_protocol::AnonymousHotlineDefinition::new(config.guest_hotline().label.clone())
        })
        .transpose()
        .map_err(|error| format!("invalid guest-hotline handset definition: {error}"))
}

pub const fn native_audio_format(format: PbxAudioFormat) -> native_channel::NativeAudioFormat {
    match format {
        PbxAudioFormat::G711Ulaw => native_channel::NativeAudioFormat::G711Ulaw,
        PbxAudioFormat::G711Alaw => native_channel::NativeAudioFormat::G711Alaw,
        PbxAudioFormat::G722 => native_channel::NativeAudioFormat::G722,
        PbxAudioFormat::G723 => native_channel::NativeAudioFormat::G723,
        PbxAudioFormat::G729 => native_channel::NativeAudioFormat::G729,
        PbxAudioFormat::G726Aal2 => native_channel::NativeAudioFormat::G726Aal2,
        PbxAudioFormat::Gsm => native_channel::NativeAudioFormat::Gsm,
        PbxAudioFormat::Slin16 => native_channel::NativeAudioFormat::Slin16,
        PbxAudioFormat::Ilbc => native_channel::NativeAudioFormat::Ilbc,
        PbxAudioFormat::Siren7 => native_channel::NativeAudioFormat::Siren7,
        PbxAudioFormat::Opus => native_channel::NativeAudioFormat::Opus,
    }
}

pub const fn pbx_audio_format_from_native(
    format: native_channel::NativeAudioFormat,
) -> PbxAudioFormat {
    match format {
        native_channel::NativeAudioFormat::G711Ulaw => PbxAudioFormat::G711Ulaw,
        native_channel::NativeAudioFormat::G711Alaw => PbxAudioFormat::G711Alaw,
        native_channel::NativeAudioFormat::G722 => PbxAudioFormat::G722,
        native_channel::NativeAudioFormat::G723 => PbxAudioFormat::G723,
        native_channel::NativeAudioFormat::G729 => PbxAudioFormat::G729,
        native_channel::NativeAudioFormat::G726Aal2 => PbxAudioFormat::G726Aal2,
        native_channel::NativeAudioFormat::Gsm => PbxAudioFormat::Gsm,
        native_channel::NativeAudioFormat::Slin16 => PbxAudioFormat::Slin16,
        native_channel::NativeAudioFormat::Ilbc => PbxAudioFormat::Ilbc,
        native_channel::NativeAudioFormat::Siren7 => PbxAudioFormat::Siren7,
        native_channel::NativeAudioFormat::Opus => PbxAudioFormat::Opus,
    }
}

pub unsafe fn state_from_channel(channel: *mut sys::ast_channel) -> Option<ChannelState> {
    let channel = NonNull::new(channel)?;
    unsafe { native_channel::channel_identity(channel) }.map(ChannelState::from)
}

pub unsafe fn take_state_from_channel(channel: *mut sys::ast_channel) -> Option<ChannelState> {
    let channel = NonNull::new(channel)?;
    unsafe { native_channel::take_channel_identity(channel) }.map(ChannelState::from)
}

pub fn c_string(value: &str) -> Result<CString, crate::asterisk::boundary::NativeTextError> {
    crate::asterisk::boundary::native_c_string(value)
}

pub fn requestor_auto_answer_mode(
    requestor: *const sys::ast_channel,
) -> Result<Option<AutoAnswerMode>, ()> {
    if requestor.is_null() {
        return Ok(None);
    }
    let channel =
        unsafe { AsteriskChannel::from_raw(requestor.cast_mut().cast()) }.map_err(|_| ())?;
    let value =
        raw::channel::copy_channel_variable(&channel, c"AUTO_ANSWER", 16).map_err(|_| ())?;
    parse_requestor_mode(value.as_deref()).map_err(|_| ())
}

pub fn ast_log(level: LogLevel, message: &str) {
    raw::system::log_message(level, message);
    #[cfg(feature = "telemetry")]
    crate::asterisk::telemetry::record_log(level, message);
}

fn read_snapshot_with<T, Borrowed, BorrowError, CopyError>(
    channel: *mut sys::ast_channel,
    description: &'static str,
    borrow: impl FnOnce(*mut sys::ast_channel) -> Result<Borrowed, BorrowError>,
    copy: impl FnOnce(&Borrowed) -> Result<T, CopyError>,
    warn: impl Fn(&str),
) -> Option<T>
where
    BorrowError: std::fmt::Display,
    CopyError: std::fmt::Display,
{
    if channel.is_null() {
        return None;
    }
    let channel = match borrow(channel) {
        Ok(channel) => channel,
        Err(error) => {
            warn(&format!("unable to borrow {description}: {error}"));
            return None;
        }
    };
    match copy(&channel) {
        Ok(value) => Some(value),
        Err(error) => {
            warn(&format!("unable to copy {description}: {error}"));
            None
        }
    }
}

fn read_native_channel_snapshot<T, E>(
    channel: *mut sys::ast_channel,
    description: &'static str,
    copy: impl for<'channel> FnOnce(&AsteriskChannel<'channel>) -> Result<T, E>,
) -> Option<T>
where
    E: std::fmt::Display,
{
    read_snapshot_with(
        channel,
        description,
        |channel| unsafe { AsteriskChannel::from_raw(channel.cast()) },
        copy,
        |message| ast_log(LogLevel::Warning, message),
    )
}

pub fn read_party_snapshot(channel: *mut sys::ast_channel) -> Option<PartySnapshot> {
    read_native_channel_snapshot(channel, "channel party metadata", |channel| {
        AsteriskPartyUpdates::new().snapshot(channel)
    })
}

pub fn read_channel_metadata(channel: *mut sys::ast_channel) -> Option<CallMetadata> {
    read_native_channel_snapshot(channel, "PBX channel metadata", |channel| {
        AsteriskChannelMetadata::new().snapshot(channel)
    })
}

#[cfg(test)]
mod snapshot_tests {
    use std::cell::RefCell;

    use super::*;

    fn non_null_channel() -> *mut sys::ast_channel {
        NonNull::<sys::ast_channel>::dangling().as_ptr()
    }

    #[test]
    fn both_snapshot_families_reject_null_before_borrowing() {
        for description in ["channel party metadata", "PBX channel metadata"] {
            let borrowed = RefCell::new(false);
            let result = read_snapshot_with(
                ptr::null_mut(),
                description,
                |_| {
                    *borrowed.borrow_mut() = true;
                    Ok::<_, &'static str>(())
                },
                |_| Ok::<_, &'static str>(()),
                |_| {},
            );
            assert_eq!(result, None);
            assert!(!*borrowed.borrow());
        }
    }

    #[test]
    fn party_snapshot_reports_borrow_and_copy_failures() {
        for copy_fails in [false, true] {
            let warnings = RefCell::new(Vec::new());
            let result = read_snapshot_with(
                non_null_channel(),
                "channel party metadata",
                |_| {
                    if copy_fails {
                        Ok(())
                    } else {
                        Err("borrow failed")
                    }
                },
                |_| Err::<(), _>("copy failed"),
                |message| warnings.borrow_mut().push(message.to_owned()),
            );
            assert_eq!(result, None);
            assert_eq!(warnings.borrow().len(), 1);
        }
    }

    #[test]
    fn metadata_snapshot_reports_borrow_and_copy_failures() {
        for borrow_fails in [true, false] {
            let warnings = RefCell::new(Vec::new());
            let result = read_snapshot_with(
                non_null_channel(),
                "PBX channel metadata",
                |_| {
                    if borrow_fails {
                        Err("borrow failed")
                    } else {
                        Ok(())
                    }
                },
                |_| Err::<(), _>("copy failed"),
                |message| warnings.borrow_mut().push(message.to_owned()),
            );
            assert_eq!(result, None);
            assert_eq!(warnings.borrow().len(), 1);
        }
    }

    #[test]
    fn both_snapshot_families_copy_owned_values_without_leaking_the_borrow() {
        for description in ["channel party metadata", "PBX channel metadata"] {
            let warnings = RefCell::new(Vec::new());
            let result = read_snapshot_with(
                non_null_channel(),
                description,
                |_| Ok::<_, &'static str>("borrowed"),
                |value| Ok::<_, &'static str>(value.to_uppercase()),
                |message| warnings.borrow_mut().push(message.to_owned()),
            );
            assert_eq!(result.as_deref(), Some("BORROWED"));
            assert!(warnings.borrow().is_empty());
        }
    }
}
