//! Conference phone XML document family.

use super::*;

/// Display schema used when rendering conference workflows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceMenuFamily {
    Menu,
    IconMenu,
}

/// Participant state needed to render conference menus and allowed actions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceListEntry {
    pub participant_id: ParticipantId,
    pub name: String,
    pub number: String,
    pub moderator: bool,
    pub muted: bool,
}

/// Typed callback encoded into conference menu action URLs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceListAction {
    /// Open the action menu for one participant.
    Participant {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Mute {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Unmute {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Remove {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Promote {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Demote {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    End {
        conference_id: ConferenceId,
    },
}

impl ConferenceListAction {
    /// Application identifier embedded in generated callback URLs.
    pub const APPLICATION_ID: u32 = 9091;

    /// Encodes the action as a device-local callback URL.
    pub fn url(self) -> String {
        match self {
            Self::Participant {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Mute {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/mute",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Unmute {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/unmute",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Remove {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/remove",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Promote {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/promote",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Demote {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/demote",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::End { conference_id } => format!(
                "UserData:{}:0:conference/{}/end",
                Self::APPLICATION_ID,
                conference_id.get()
            ),
        }
    }

    /// Parses a complete callback URL or its bare `conference/...` path.
    pub fn parse(value: &str) -> Option<Self> {
        let path = value
            .trim_matches(['\0', ' ', '\r', '\n'])
            .strip_prefix(&format!("UserData:{}:0:", Self::APPLICATION_ID))
            .unwrap_or(value)
            .strip_prefix("conference/")?;
        let segments: Vec<_> = path.split('/').collect();
        let [conference_id, action, rest @ ..] = segments.as_slice() else {
            return None;
        };
        let conference_id = ConferenceId::new(conference_id.parse().ok()?);
        match (*action, rest) {
            ("participant", [participant]) => Some(Self::Participant {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "mute"]) => Some(Self::Mute {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "unmute"]) => Some(Self::Unmute {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "remove"]) => Some(Self::Remove {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "promote"]) => Some(Self::Promote {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "demote"]) => Some(Self::Demote {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("end", []) => Some(Self::End { conference_id }),
            _ => None,
        }
    }

    /// Parses the percent-decoded route produced by a service submission.
    pub fn from_route(route: &[String]) -> Option<Self> {
        let [conference, conference_id, action, rest @ ..] = route else {
            return None;
        };
        if conference != "conference" {
            return None;
        }
        let conference_id = ConferenceId::new(conference_id.parse().ok()?);
        match (action.as_str(), rest) {
            ("participant", [participant]) => Some(Self::Participant {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, operation]) if operation == "mute" => Some(Self::Mute {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, operation]) if operation == "unmute" => {
                Some(Self::Unmute {
                    conference_id,
                    participant_id: ParticipantId::new(participant.parse().ok()?),
                })
            }
            ("participant", [participant, operation]) if operation == "remove" => {
                Some(Self::Remove {
                    conference_id,
                    participant_id: ParticipantId::new(participant.parse().ok()?),
                })
            }
            ("participant", [participant, operation]) if operation == "promote" => {
                Some(Self::Promote {
                    conference_id,
                    participant_id: ParticipantId::new(participant.parse().ok()?),
                })
            }
            ("participant", [participant, operation]) if operation == "demote" => {
                Some(Self::Demote {
                    conference_id,
                    participant_id: ParticipantId::new(participant.parse().ok()?),
                })
            }
            ("end", []) => Some(Self::End { conference_id }),
            _ => None,
        }
    }
}

/// Rendered conference overview in either supported menu family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConferenceListDocument {
    Menu(CiscoIpPhoneMenu),
    IconMenu(CiscoIpPhoneIconMenu),
}

impl ConferenceListDocument {
    /// Renders a conference overview and validates all menu and byte bounds.
    pub fn new(
        conference_id: ConferenceId,
        participants: &[ConferenceListEntry],
        family: ConferenceMenuFamily,
    ) -> Result<Self, PhoneXmlError> {
        if participants.len() > CONFERENCE_LIST_MAX_PARTICIPANTS {
            return Err(PhoneXmlError::LimitExceeded {
                kind: "conference participants",
                actual: participants.len(),
                maximum: CONFERENCE_LIST_MAX_PARTICIPANTS,
            });
        }
        let title = format!("Conference {}", conference_id.get());
        let prompt = if participants.is_empty() {
            "No participants".to_owned()
        } else {
            "Select a participant".to_owned()
        };
        match family {
            ConferenceMenuFamily::Menu => CiscoIpPhoneMenu::new(
                title,
                prompt,
                participants
                    .iter()
                    .map(|participant| CiscoIpPhoneMenuItem {
                        name: Some(conference_participant_label(participant)),
                        url: Some(
                            ConferenceListAction::Participant {
                                conference_id,
                                participant_id: participant.participant_id,
                            }
                            .url(),
                        ),
                    })
                    .chain(std::iter::once(CiscoIpPhoneMenuItem {
                        name: Some("End conference".into()),
                        url: Some(ConferenceListAction::End { conference_id }.url()),
                    }))
                    .collect(),
            )
            .map(Self::Menu),
            ConferenceMenuFamily::IconMenu => CiscoIpPhoneIconMenu::new(
                title,
                prompt,
                participants
                    .iter()
                    .map(|participant| CiscoIpPhoneIconMenuItem {
                        name: Some(conference_participant_label(participant)),
                        url: Some(
                            ConferenceListAction::Participant {
                                conference_id,
                                participant_id: participant.participant_id,
                            }
                            .url(),
                        ),
                        icon_index: Some(u16::from(participant.moderator)),
                    })
                    .chain(std::iter::once(CiscoIpPhoneIconMenuItem {
                        name: Some("End conference".into()),
                        url: Some(ConferenceListAction::End { conference_id }.url()),
                        icon_index: Some(0),
                    }))
                    .collect(),
                conference_icons(),
            )
            .map(Self::IconMenu),
        }
    }

    /// Serializes the rendered menu within [`CONFERENCE_LIST_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        match self {
            Self::Menu(document) => document.to_xml_with_limit(CONFERENCE_LIST_MAX_BYTES),
            Self::IconMenu(document) => document.to_xml_with_limit(CONFERENCE_LIST_MAX_BYTES),
        }
    }

    /// Parses a conference overview using the caller-selected menu family.
    pub fn from_xml(document: &[u8], family: ConferenceMenuFamily) -> Result<Self, PhoneXmlError> {
        match family {
            ConferenceMenuFamily::Menu => {
                CiscoIpPhoneMenu::from_xml_with_limit(document, CONFERENCE_LIST_MAX_BYTES)
                    .map(Self::Menu)
            }
            ConferenceMenuFamily::IconMenu => {
                CiscoIpPhoneIconMenu::from_xml_with_limit(document, CONFERENCE_LIST_MAX_BYTES)
                    .map(Self::IconMenu)
            }
        }
    }

    /// Iterates recognized callback actions in display order.
    pub fn actions(&self) -> impl Iterator<Item = ConferenceListAction> + '_ {
        let urls: Box<dyn Iterator<Item = &str>> = match self {
            Self::Menu(document) => {
                Box::new(document.items.iter().filter_map(|item| item.url.as_deref()))
            }
            Self::IconMenu(document) => {
                Box::new(document.items.iter().filter_map(|item| item.url.as_deref()))
            }
        };
        urls.filter_map(ConferenceListAction::parse)
    }
}

/// Participant-specific conference actions in either supported menu family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConferenceParticipantActionsDocument {
    Menu(CiscoIpPhoneMenu),
    IconMenu(CiscoIpPhoneIconMenu),
}

impl ConferenceParticipantActionsDocument {
    /// Renders the actions currently permitted for one participant.
    ///
    /// `removable` and `demotable` let session policy suppress actions even
    /// when the participant state would otherwise permit them.
    pub fn new(
        conference_id: ConferenceId,
        participant: &ConferenceListEntry,
        removable: bool,
        demotable: bool,
        family: ConferenceMenuFamily,
    ) -> Result<Self, PhoneXmlError> {
        let mut actions = Vec::new();
        if participant.moderator {
            if demotable {
                actions.push((
                    "Demote",
                    ConferenceListAction::Demote {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                ));
            }
        } else {
            let (toggle_name, toggle) = if participant.muted {
                (
                    "Unmute",
                    ConferenceListAction::Unmute {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                )
            } else {
                (
                    "Mute",
                    ConferenceListAction::Mute {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                )
            };
            actions.push((toggle_name, toggle));
            if removable {
                actions.push((
                    "Remove",
                    ConferenceListAction::Remove {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                ));
            }
            actions.push((
                "Promote",
                ConferenceListAction::Promote {
                    conference_id,
                    participant_id: participant.participant_id,
                },
            ));
        }
        let title = format!("Participant {}", participant.participant_id.get());
        match family {
            ConferenceMenuFamily::Menu => CiscoIpPhoneMenu::new(
                title,
                "Choose an action",
                actions
                    .into_iter()
                    .map(|(name, action)| CiscoIpPhoneMenuItem {
                        name: Some(name.into()),
                        url: Some(action.url()),
                    })
                    .collect(),
            )
            .map(Self::Menu),
            ConferenceMenuFamily::IconMenu => CiscoIpPhoneIconMenu::new(
                title,
                "Choose an action",
                actions
                    .into_iter()
                    .map(|(name, action)| CiscoIpPhoneIconMenuItem {
                        name: Some(name.into()),
                        url: Some(action.url()),
                        icon_index: None,
                    })
                    .collect(),
                Vec::new(),
            )
            .map(Self::IconMenu),
        }
    }

    /// Serializes the rendered menu within [`CONFERENCE_LIST_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        match self {
            Self::Menu(document) => document.to_xml_with_limit(CONFERENCE_LIST_MAX_BYTES),
            Self::IconMenu(document) => document.to_xml_with_limit(CONFERENCE_LIST_MAX_BYTES),
        }
    }

    /// Parses participant actions using the caller-selected menu family.
    pub fn from_xml(document: &[u8], family: ConferenceMenuFamily) -> Result<Self, PhoneXmlError> {
        match family {
            ConferenceMenuFamily::Menu => {
                CiscoIpPhoneMenu::from_xml_with_limit(document, CONFERENCE_LIST_MAX_BYTES)
                    .map(Self::Menu)
            }
            ConferenceMenuFamily::IconMenu => {
                CiscoIpPhoneIconMenu::from_xml_with_limit(document, CONFERENCE_LIST_MAX_BYTES)
                    .map(Self::IconMenu)
            }
        }
    }

    /// Iterates recognized callback actions in display order.
    pub fn actions(&self) -> impl Iterator<Item = ConferenceListAction> + '_ {
        let urls: Box<dyn Iterator<Item = &str>> = match self {
            Self::Menu(document) => {
                Box::new(document.items.iter().filter_map(|item| item.url.as_deref()))
            }
            Self::IconMenu(document) => {
                Box::new(document.items.iter().filter_map(|item| item.url.as_deref()))
            }
        };
        urls.filter_map(ConferenceListAction::parse)
    }
}

pub(super) fn conference_participant_label(participant: &ConferenceListEntry) -> String {
    let identity = if !participant.name.trim().is_empty() {
        participant.name.trim()
    } else if !participant.number.trim().is_empty() {
        participant.number.trim()
    } else {
        "Unknown participant"
    };
    let role = if participant.moderator {
        "Moderator"
    } else {
        "Participant"
    };
    let mute = if participant.muted { ", muted" } else { "" };
    format!("{identity} ({role}{mute})")
}

pub(super) fn conference_icons() -> Vec<CiscoIpPhoneIconItem> {
    vec![
        CiscoIpPhoneIconItem {
            index: 0,
            width: 10,
            height: 10,
            depth: 2,
            data: Some("00000000000000000000000000".into()),
        },
        CiscoIpPhoneIconItem {
            index: 1,
            width: 10,
            height: 10,
            depth: 2,
            data: Some("00000155415555554155000000".into()),
        },
    ]
}
