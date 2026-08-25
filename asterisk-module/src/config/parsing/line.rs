use super::super::*;

pub(in crate::config) fn parse_line(
    section: &RawSection,
    general: &GeneralConfig,
) -> Result<ParsedLine, ConfigError> {
    let mut draft = LineSectionDraft::default();

    for entry in deserialize_entries::<LineOption>(section)? {
        let key = &entry.source.key;
        let raw = &entry.source.value;
        let diagnostic = entry.source.diagnostic_key();
        match entry.key {
            LineOption::Type | LineOption::Label | LineOption::Context | LineOption::CallerId => {}
            LineOption::IncomingLimit => {
                set_once(
                    &mut draft.incoming_limit,
                    section,
                    key,
                    raw,
                    raw.trim()
                        .parse::<u32>()
                        .ok()
                        .filter(|limit| *limit <= 255)
                        .ok_or_else(|| {
                            invalid_option(&diagnostic, raw, "incoming call limit 0..255", false)
                        })?,
                )?;
            }
            LineOption::Language => set_once(
                &mut draft.language,
                section,
                key,
                raw,
                parse_metadata_required(&diagnostic, raw, MAX_LANGUAGE_BYTES, false)?,
            )?,
            LineOption::AccountCode => set_once(
                &mut draft.account_code,
                section,
                key,
                "<redacted>",
                parse_metadata_optional(&diagnostic, raw, MAX_ACCOUNT_CODE_BYTES, true)?,
            )?,
            LineOption::SetVariable => {
                push_channel_variable(&mut draft.channel_variables, &diagnostic, raw)?
            }
            LineOption::Mailbox => set_once(
                &mut draft.mailbox,
                section,
                key,
                raw,
                parse_mailbox(&diagnostic, raw)?,
            )?,
            LineOption::VoicemailNumber => set_once(
                &mut draft.voicemail_number,
                section,
                key,
                "<redacted>",
                parse_optional_voicemail_destination(&diagnostic, raw)?,
            )?,
            LineOption::VoicemailTransfer => set_once(
                &mut draft.voicemail_transfer,
                section,
                key,
                "<redacted>",
                parse_optional_voicemail_destination(&diagnostic, raw)?,
            )?,
            LineOption::CallGroup => set_once(
                &mut draft.call_groups,
                section,
                key,
                raw,
                parse_numeric_groups(&diagnostic, raw)?,
            )?,
            LineOption::PickupGroup => set_once(
                &mut draft.pickup_groups,
                section,
                key,
                raw,
                parse_numeric_groups(&diagnostic, raw)?,
            )?,
            LineOption::NamedCallGroup => set_once(
                &mut draft.named_call_groups,
                section,
                key,
                raw,
                parse_named_groups(&diagnostic, raw)?,
            )?,
            LineOption::NamedPickupGroup => set_once(
                &mut draft.named_pickup_groups,
                section,
                key,
                raw,
                parse_named_groups(&diagnostic, raw)?,
            )?,
            LineOption::DirectedPickup => set_once(
                &mut draft.directed_pickup,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            LineOption::DirectedPickupContext => set_once(
                &mut draft.directed_pickup_context,
                section,
                key,
                raw,
                parse_optional_setting(&diagnostic, raw)?,
            )?,
            LineOption::PickupModeAnswer => set_once(
                &mut draft.pickup_mode_answer,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            LineOption::ParkingLot => set_once(
                &mut draft.parking_lot,
                section,
                key,
                raw,
                parse_empty_optional_setting(&diagnostic, raw)?,
            )?,
            LineOption::ConferenceEnabled => set_once(
                &mut draft.conference_enabled,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            LineOption::ConferenceNumber => set_once(
                &mut draft.conference_destination,
                section,
                key,
                raw,
                parse_empty_optional_setting(&diagnostic, raw)?,
            )?,
            LineOption::ConferenceOptions => set_once(
                &mut draft.conference_options,
                section,
                key,
                raw,
                parse_application_options(&diagnostic, raw)?,
            )?,
            LineOption::AdhocNumber => set_once(
                &mut draft.hotline_destination,
                section,
                key,
                "<redacted>",
                parse_optional_hotline_destination(&diagnostic, raw)?,
            )?,
            LineOption::InitialDialtoneTone => {
                set_once(
                    &mut draft.initial_dialtone_tone,
                    section,
                    key,
                    raw,
                    parse_tone(&diagnostic, raw)?,
                )?;
            }
            LineOption::SecondaryDialtoneDigits => {
                set_once(
                    &mut draft.secondary_dialtone_digits,
                    section,
                    key,
                    raw,
                    parse_secondary_dialtone_digits(&diagnostic, raw)?,
                )?;
            }
            LineOption::SecondaryDialtoneTone => {
                set_once(
                    &mut draft.secondary_dialtone_tone,
                    section,
                    key,
                    raw,
                    parse_tone(&diagnostic, raw)?,
                )?;
            }
            LineOption::Pin => {
                let pin = parse_mobility_pin(&diagnostic, raw)?;
                set_once(&mut draft.mobility_pin, section, key, "<redacted>", pin)?;
            }
            LineOption::RegistrationExtension => {
                set_once(
                    &mut draft.registration_extensions,
                    section,
                    key,
                    raw,
                    parse_registration_extensions(&diagnostic, raw)?,
                )?;
            }
            LineOption::Allow => draft.codec_settings.push((true, raw.as_str())),
            LineOption::Disallow => draft.codec_settings.push((false, raw.as_str())),
            LineOption::VideoMode => set_once(
                &mut draft.video_mode,
                section,
                key,
                raw,
                parse_video_mode(&diagnostic, raw)?,
            )?,
            LineOption::AudioEncryption => set_once(
                &mut draft.audio_encryption,
                section,
                key,
                raw,
                parse_media_encryption_policy(&diagnostic, raw)?,
            )?,
            LineOption::EchoCancel => set_once(
                &mut draft.echo_cancellation,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            LineOption::SilenceSuppression => set_once(
                &mut draft.silence_suppression,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
        }
    }

    let number = section.name.clone();
    let registration_extensions = draft.registration_extensions.take().flatten();
    if registration_extensions.is_some() && general.registration.contexts.is_empty() {
        return Err(invalid_option(
            section.diagnostic_key("regexten"),
            value(section, "regexten").unwrap_or_default(),
            "at least one general regcontext when regexten is configured",
            false,
        ));
    }
    if registration_extensions.is_none() && !general.registration.contexts.is_empty() {
        validate_registration_identifier(
            &section.section_location(),
            &number,
            "a logical line name usable as a registration extension",
        )?;
    }
    let registration_extensions = registration_extensions.unwrap_or_else(|| {
        vec![RegistrationExtension {
            extension: number.clone(),
            context: None,
        }]
    });
    let conference_destination = draft.conference_destination.take().unwrap_or(None);
    if draft.conference_enabled == Some(false)
        && (conference_destination.is_some() || draft.conference_options.is_some())
    {
        return Err(ConfigError::InvalidValue {
            key: format!("{}.meetme", section.name),
            value: "disabled with conference destination or options".into(),
        });
    }
    if draft.conference_enabled == Some(true) && conference_destination.is_none() {
        return Err(ConfigError::InvalidValue {
            key: format!("{}.meetmenum", section.name),
            value: "conference dialing is enabled without a destination".into(),
        });
    }
    let codecs = if draft.codec_settings.is_empty() {
        general.codecs.clone()
    } else {
        apply_codec_settings(
            Vec::new(),
            &draft.codec_settings,
            &format!("{}.codecs", section.name),
        )?
    };
    let (caller_name, caller_number) = value(section, "callerid")
        .map(parse_caller_id)
        .unwrap_or_else(|| (number.clone(), number.clone()));
    Ok(ParsedLine {
        line: LineConfig {
            number: number.clone(),
            label: value(section, "label").unwrap_or(&number).to_owned(),
            context: parse_required_setting(
                &format!("{}.context", section.name),
                value(section, "context").unwrap_or("from-sccp"),
            )?,
            caller_name,
            caller_number,
            mailbox: draft.mailbox.unwrap_or(None),
            language: draft.language.unwrap_or_else(|| general.language.clone()),
            account_code: draft
                .account_code
                .unwrap_or_else(|| general.account_code.clone()),
            channel_variables: draft.channel_variables,
        },
        features: LineFeatureConfig {
            incoming_limit: draft.incoming_limit.unwrap_or(6),
            voicemail: VoicemailDefaults {
                number: draft.voicemail_number.unwrap_or(None),
                transfer_destination: draft.voicemail_transfer.unwrap_or(None),
            },
            pickup: PickupConfig {
                call_groups: draft.call_groups.unwrap_or_default(),
                pickup_groups: draft.pickup_groups.unwrap_or_default(),
                named_call_groups: draft.named_call_groups.unwrap_or_default(),
                named_pickup_groups: draft.named_pickup_groups.unwrap_or_default(),
                directed: draft.directed_pickup.unwrap_or(true),
                directed_context: draft.directed_pickup_context.unwrap_or(None),
                answer_directed: draft.pickup_mode_answer.unwrap_or(true),
            },
            parking: LineParkingConfig {
                lot: draft.parking_lot.unwrap_or(None),
            },
            conference: LineConferenceConfig {
                enabled: draft.conference_enabled,
                destination: conference_destination,
                application_options: draft.conference_options,
            },
            hotline: LineHotlineConfig {
                destination: draft.hotline_destination.unwrap_or(None),
            },
            dial_tones: LineDialToneConfig {
                initial: draft.initial_dialtone_tone.unwrap_or(Tone::InsideDial),
                secondary_prefix: draft.secondary_dialtone_digits.unwrap_or(None),
                secondary: draft.secondary_dialtone_tone.unwrap_or(Tone::OutsideDial),
            },
            mobility: LineMobilityConfig {
                pin: draft.mobility_pin.flatten(),
            },
            registration: LineRegistrationConfig {
                extensions: registration_extensions,
            },
            media: LineMediaConfig {
                codecs,
                audio_encryption: draft
                    .audio_encryption
                    .unwrap_or_else(|| general.audio_encryption.clone()),
                video_mode: draft.video_mode.unwrap_or(VideoMode::Auto),
                audio_processing: AudioProcessingPolicy {
                    echo_cancellation: draft
                        .echo_cancellation
                        .map(|enabled| {
                            if enabled {
                                EchoCancellation::On
                            } else {
                                EchoCancellation::Off
                            }
                        })
                        .unwrap_or(general.audio_processing.echo_cancellation),
                    silence_suppression: draft
                        .silence_suppression
                        .map(|enabled| {
                            if enabled {
                                SilenceSuppression::On
                            } else {
                                SilenceSuppression::Off
                            }
                        })
                        .unwrap_or(general.audio_processing.silence_suppression),
                },
            },
        },
    })
}
