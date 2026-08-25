use super::super::*;

pub(in crate::config) fn parse_line_button(
    raw: &str,
    device: &DeviceId,
    lines: &HashMap<String, LineConfig>,
    instances: &mut ButtonInstances,
) -> Result<ParsedButton, ConfigError> {
    let fields: Vec<_> = raw.split(',').map(str::trim).collect();
    let number = required_button_field(fields[0], "line")?;
    let line = lines.get(number).ok_or_else(|| ConfigError::UnknownLine {
        device: device.clone(),
        line: number.to_owned(),
    })?;
    let instance = ButtonInstances::next(&mut instances.line);
    let mut appearance = LineAppearance::new(
        instance,
        LineDefinition {
            number: line.number.clone(),
            display_name: line.label.clone(),
        },
    );
    let mut options = HashSet::new();
    for option in &fields[1..] {
        let Some((key, value)) = option.split_once('=') else {
            return Err(invalid_button(raw));
        };
        let key = normalize_name(required_button_field(key, raw)?);
        let value = required_button_field(value, raw)?;
        if !options.insert(key.clone()) {
            return Err(ConfigError::InvalidValue {
                key: format!("button.line.{key}"),
                value: raw.into(),
            });
        }
        match key.as_str() {
            "label" => appearance.label = Some(value.into()),
            "callername" => appearance.caller_id.name = Some(value.into()),
            "callernumber" => appearance.caller_id.number = Some(value.into()),
            "ring" | "ringmode" => {
                appearance.ring_mode = match normalize_name(value).as_str() {
                    "normal" => AppearanceRingMode::Normal,
                    "silent" => AppearanceRingMode::Silent,
                    "disabled" | "off" => AppearanceRingMode::Disabled,
                    _ => {
                        return Err(ConfigError::InvalidValue {
                            key: "button.line.ring".into(),
                            value: value.into(),
                        });
                    }
                }
            }
            "subscription" | "subscriptionidentity" => {
                appearance.subscription_identity = Some(value.into())
            }
            "privacy" => appearance.privacy = parse_bool("button.line.privacy", value)?,
            _ => {
                return Err(ConfigError::InvalidValue {
                    key: format!("button.line.{key}"),
                    value: value.into(),
                });
            }
        }
    }
    Ok(ParsedButton {
        definition: ButtonDefinition::Line(appearance),
        feature_argument: None,
        blf_target: None,
    })
}

pub(in crate::config) fn parse_button(
    raw: &str,
    device: &DeviceId,
    lines: &HashMap<String, LineConfig>,
    instances: &mut ButtonInstances,
) -> Result<ParsedButton, ConfigError> {
    let fields: Vec<_> = raw.split(',').map(str::trim).collect();
    let Some(kind) = fields.first().copied().filter(|kind| !kind.is_empty()) else {
        return Err(invalid_button(raw));
    };

    match normalize_name(kind).as_str() {
        "line" if fields.len() >= 2 => {
            parse_line_button(&fields[1..].join(","), device, lines, instances)
        }
        "speeddial" if matches!(fields.len(), 3 | 4) => {
            let label = required_button_field(fields[1], raw)?;
            let number = required_button_field(fields[2], raw)?;
            if fields.len() == 4 {
                let target = parse_blf_hint(fields[3], raw)?;
                let instance = ButtonInstances::next(&mut instances.feature);
                Ok(ParsedButton {
                    definition: ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                        instance,
                        number: number.to_owned(),
                        display_name: label.to_owned(),
                    }),
                    feature_argument: None,
                    blf_target: Some((instance, target)),
                })
            } else {
                let instance = ButtonInstances::next(&mut instances.speed_dial);
                Ok(ParsedButton {
                    definition: ButtonDefinition::SpeedDial(SpeedDialDefinition {
                        instance,
                        number: number.to_owned(),
                        display_name: label.to_owned(),
                    }),
                    feature_argument: None,
                    blf_target: None,
                })
            }
        }
        "blf" | "blfspeeddial" if fields.len() == 4 => {
            let label = required_button_field(fields[1], raw)?;
            let number = required_button_field(fields[2], raw)?;
            let target = parse_blf_hint(fields[3], raw)?;
            let instance = ButtonInstances::next(&mut instances.feature);
            Ok(ParsedButton {
                definition: ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                    instance,
                    number: number.to_owned(),
                    display_name: label.to_owned(),
                }),
                feature_argument: None,
                blf_target: Some((instance, target)),
            })
        }
        "feature" if fields.len() >= 3 => {
            let label = required_button_field(fields[1], raw)?;
            let feature_name = required_button_field(fields[2], raw)?;
            let feature = parse_feature(feature_name)?;
            let instance = ButtonInstances::next(&mut instances.feature);
            let feature_argument = if fields.len() > 3 {
                let argument = fields[3..].join(",");
                Some((instance, required_button_field(&argument, raw)?.to_owned()))
            } else {
                None
            };
            Ok(ParsedButton {
                definition: ButtonDefinition::Feature(FeatureDefinition {
                    instance,
                    label: label.to_owned(),
                    feature,
                }),
                feature_argument,
                blf_target: None,
            })
        }
        "service" if fields.len() >= 3 => {
            let label = required_button_field(fields[1], raw)?;
            let url = fields[2..].join(",");
            let url = required_button_field(&url, raw)?;
            let instance = ButtonInstances::next(&mut instances.service);
            Ok(ParsedButton {
                definition: ButtonDefinition::Service(ServiceDefinition {
                    instance,
                    label: label.to_owned(),
                    url: url.to_owned(),
                }),
                feature_argument: None,
                blf_target: None,
            })
        }
        "empty" | "unused" if fields.len() == 1 => Ok(ParsedButton {
            definition: ButtonDefinition::Unused,
            feature_argument: None,
            blf_target: None,
        }),
        "addon" | "addonmodule" if fields.len() == 3 => {
            let slot = parse::<u32>("button.addon.slot", required_button_field(fields[1], raw)?)?;
            if !(1..=56).contains(&slot) {
                return Err(ConfigError::InvalidValue {
                    key: "button.addon.slot".into(),
                    value: slot.to_string(),
                });
            }
            let device_type = parse_addon_type(required_button_field(fields[2], raw)?)?;
            Ok(ParsedButton {
                definition: ButtonDefinition::AddonModule(AddonModuleDefinition {
                    slot,
                    device_type,
                }),
                feature_argument: None,
                blf_target: None,
            })
        }
        _ => Err(invalid_button(raw)),
    }
}
