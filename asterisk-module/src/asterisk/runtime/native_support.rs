use super::super::*;

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

pub fn c_string(value: &str) -> CString {
    let bytes = value
        .as_bytes()
        .iter()
        .copied()
        .filter(|byte| *byte != 0)
        .collect::<Vec<_>>();
    // SAFETY: the filtering step above removes every interior NUL byte.
    unsafe { CString::from_vec_unchecked(bytes) }
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
}

pub fn read_party_snapshot(channel: *mut sys::ast_channel) -> Option<PartySnapshot> {
    if channel.is_null() {
        return None;
    }
    let channel = match unsafe { AsteriskChannel::from_raw(channel.cast()) } {
        Ok(channel) => channel,
        Err(error) => {
            ast_log(
                LogLevel::Warning,
                &format!("unable to borrow channel party metadata: {error}"),
            );
            return None;
        }
    };
    match AsteriskPartyUpdates::new().snapshot(&channel) {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            ast_log(
                LogLevel::Warning,
                &format!("unable to copy channel party metadata: {error}"),
            );
            None
        }
    }
}

pub fn read_channel_metadata(channel: *mut sys::ast_channel) -> Option<CallMetadata> {
    if channel.is_null() {
        return None;
    }
    let channel = match unsafe { AsteriskChannel::from_raw(channel.cast()) } {
        Ok(channel) => channel,
        Err(error) => {
            ast_log(
                LogLevel::Warning,
                &format!("unable to borrow PBX channel metadata: {error}"),
            );
            return None;
        }
    };
    match AsteriskChannelMetadata::new().snapshot(&channel) {
        Ok(metadata) => Some(metadata),
        Err(error) => {
            ast_log(
                LogLevel::Warning,
                &format!("unable to copy PBX channel metadata: {error}"),
            );
            None
        }
    }
}
