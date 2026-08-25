//! Conference, barge, shared-line, and PBX effect orchestration.

use super::*;

pub(super) fn conference_mutation_is_active(
    access: &Access,
    mutation: ConferenceMutationToken,
) -> bool {
    controller_step(&access.shared.controller, |controller| {
        controller.conference_mutation_is_active(mutation)
    })
}

pub(super) async fn handle_barge_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
    mode: BargeMode,
) {
    let binding = access.line_binding(&device_id, line_instance);
    let Some(binding) = binding else {
        return;
    };
    let Some(codec) = preferred_codec(access, &device_id, line_instance, &PbxAudioFormat::ALL)
    else {
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::DisplayPrompt {
                    call_id,
                    timeout_seconds: 4,
                    text: "Barge codec unavailable".into(),
                },
            ))
            .await;
        return;
    };
    let result = controller_step(&access.shared.controller, |controller| {
        controller.barge(call_id, binding, codec, mode)
    });
    match result {
        Ok(effects) => execute_effects(access, effects).await,
        Err(rejection) => {
            let text = match rejection {
                BargeRejection::Private => "Private call",
                BargeRejection::Capability => "Barge codec unavailable",
                BargeRejection::Conflict => "Another shared action won",
                BargeRejection::AlreadyBarged => "Another barge is active",
                BargeRejection::Unavailable | BargeRejection::NotRemote => "Barge unavailable",
            };
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::DisplayPrompt {
                        call_id,
                        timeout_seconds: 4,
                        text: text.into(),
                    },
                ))
                .await;
        }
    }
}

pub(super) async fn handle_join_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
) {
    let state = controller_step(&access.shared.controller, |controller| {
        controller.call_state(call_id)
    });
    if state == Some(CallState::RemoteInUse) {
        handle_barge_soft_key(
            access,
            device_id,
            call_id,
            line_instance,
            BargeMode::Conference,
        )
        .await;
        return;
    }

    let conference_policy = access.config().conference_for_device(&device_id).cloned();
    let permitted = conference_policy
        .as_ref()
        .is_some_and(|conference| conference.allowed);
    let media_policy = conference_policy.map(|conference| ConferenceMediaPolicy {
        music_on_hold_class: conference.music_on_hold_class,
        mute_on_entry: conference.mute_on_entry,
        play_general_announcements: conference.play_general_announcements,
        play_participant_announcements: conference.play_participant_announcements,
    });
    let result = controller_step(&access.shared.controller, |controller| {
        controller
            .join_calls_with_media(
                &device_id,
                call_id,
                permitted,
                media_policy.unwrap_or_default(),
            )
            .and_then(|effects| {
                controller
                    .claim_conference_mutation(call_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceRejection::Conflict)
            })
    });
    match result {
        Ok((mutation, effects)) => {
            let session = controller_step(&access.shared.controller, |controller| {
                controller.conference_session(call_id).cloned()
            });
            if let Some(session) = session {
                execute_selected_conference_merge(access, session, mutation, effects).await;
            }
        }
        Err(rejection) => {
            let text = match rejection {
                ConferenceRejection::Disabled => "Conference disabled",
                ConferenceRejection::NotConnected => "Select two connected calls",
                ConferenceRejection::Conflict => "Conference selection unavailable",
                ConferenceRejection::Unavailable => "Conference unavailable",
            };
            display_conference_prompt(access, device_id, call_id, text).await;
        }
    }
}

pub async fn show_conference_list(access: &Access, device_id: DeviceId, call_id: CallId) {
    let session = controller_step(&access.shared.controller, |controller| {
        controller.conference_session(call_id).cloned()
    });
    let Some(session) = session.filter(|session| {
        session.phase == ConferencePhase::Active && session.device_id == device_id
    }) else {
        display_conference_prompt(access, device_id, call_id, "No active conference").await;
        return;
    };
    if let Err(error) = execute_handset_effect(access, session.list_effect(call_id)).await {
        ast_log(
            LogLevel::Warning,
            &format!("unable to show conference list: {error}"),
        );
    }
}

pub(super) async fn show_conference_list_if_configured(
    access: &Access,
    session: &crate::runtime::controller::ConferenceSession,
) {
    let enabled = access
        .config()
        .conference_for_device(&session.device_id)
        .is_some_and(|conference| conference.show_conference_list);
    if enabled {
        show_conference_list(
            access,
            session.device_id.clone(),
            session.original_handset_call_id,
        )
        .await;
    }
}

pub(super) async fn handle_conference_list_action(
    access: &Access,
    device_id: DeviceId,
    action: ConferenceListAction,
) {
    let conference_id = match action {
        ConferenceListAction::Participant { conference_id, .. }
        | ConferenceListAction::Mute { conference_id, .. }
        | ConferenceListAction::Unmute { conference_id, .. }
        | ConferenceListAction::Remove { conference_id, .. }
        | ConferenceListAction::Promote { conference_id, .. }
        | ConferenceListAction::Demote { conference_id, .. }
        | ConferenceListAction::End { conference_id } => conference_id,
    };
    let session = controller_step(&access.shared.controller, |controller| {
        controller.conference_session_by_id(conference_id).cloned()
    });
    let Some(session) = session.filter(|session| {
        session.phase == ConferencePhase::Active && session.device_id == device_id
    }) else {
        return;
    };
    match action {
        ConferenceListAction::Participant { participant_id, .. } => {
            let Some(participant) = session.participants.get(participant_id) else {
                return;
            };
            let Some(effect) = session.participant_actions_effect(participant_id) else {
                let label = if participant.display_name.is_empty() {
                    participant.number.clone()
                } else {
                    participant.display_name.clone()
                };
                display_conference_prompt(
                    access,
                    device_id,
                    session.original_handset_call_id,
                    &label,
                )
                .await;
                return;
            };
            if let Err(error) = execute_handset_effect(access, effect).await {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to show conference participant actions: {error}"),
                );
            }
        }
        ConferenceListAction::Mute { participant_id, .. } => {
            let _ = set_conference_participant_muted(access, session, participant_id, true).await;
        }
        ConferenceListAction::Unmute { participant_id, .. } => {
            let _ = set_conference_participant_muted(access, session, participant_id, false).await;
        }
        ConferenceListAction::Remove { participant_id, .. } => {
            let _ = remove_conference_participant(access, session, participant_id).await;
        }
        ConferenceListAction::Promote { participant_id, .. } => {
            let _ =
                set_conference_participant_moderator(access, session, participant_id, true).await;
        }
        ConferenceListAction::Demote { participant_id, .. } => {
            let _ =
                set_conference_participant_moderator(access, session, participant_id, false).await;
        }
        ConferenceListAction::End { .. } => {
            let effects = controller_step(&access.shared.controller, |controller| {
                controller.end_conference_by_moderator(&device_id, session.id)
            });
            match effects {
                Ok(effects) => {
                    cancel_conference_announcement(access, session.id);
                    execute_cleanup_effects(access, effects).await;
                }
                Err(rejection) => {
                    let text = match rejection {
                        ConferenceEndRejection::Unavailable => "Conference unavailable",
                        ConferenceEndRejection::NotModerator => "Moderator access required",
                        ConferenceEndRejection::Conflict => "Conference action pending",
                    };
                    display_conference_prompt(
                        access,
                        session.device_id,
                        session.original_handset_call_id,
                        text,
                    )
                    .await;
                }
            }
        }
    }
}

pub async fn remove_conference_participant(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    participant_id: sccp_protocol::ParticipantId,
) -> Result<(), ServiceProviderError> {
    let effects = controller_step(&access.shared.controller, |controller| {
        controller
            .begin_conference_participant_removal(&session.device_id, session.id, participant_id)
            .and_then(|effects| {
                controller
                    .claim_conference_mutation_by_id(session.id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceParticipantRejection::Conflict)
            })
    });
    let (mutation, effects) = match effects {
        Ok(operation) => operation,
        Err(rejection) => {
            let provider_error = conference_participant_service_error(rejection);
            let text = match rejection {
                ConferenceParticipantRejection::Unavailable => "Conference unavailable",
                ConferenceParticipantRejection::NotModerator => "Moderator access required",
                ConferenceParticipantRejection::InvalidParticipant => "Participant unavailable",
                ConferenceParticipantRejection::Moderator => "Moderator cannot be removed",
                ConferenceParticipantRejection::LastModerator => {
                    "At least one moderator is required"
                }
                ConferenceParticipantRejection::Conflict => "Participant cannot be removed",
            };
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                text,
            )
            .await;
            return Err(provider_error);
        }
    };

    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            return Err(ServiceProviderError::ConferenceConflict);
        }
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            let aborted = controller_step(&access.shared.controller, |controller| {
                if !controller.conference_mutation_is_active(mutation) {
                    return false;
                }
                let aborted =
                    controller.abort_conference_participant_removal(session.id, participant_id);
                controller.complete_conference_mutation(mutation);
                aborted
            });
            if !aborted {
                let removed = controller_step(&access.shared.controller, |controller| {
                    controller
                        .conference_session_by_id(session.id)
                        .is_some_and(|conference| {
                            conference.participants.get(participant_id).is_none()
                        })
                });
                if removed {
                    return Ok(());
                }
            }
            ast_log(
                LogLevel::Warning,
                &format!("conference participant removal failed: {error}"),
            );
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to remove participant",
            )
            .await;
            return Err(ServiceProviderError::Delivery);
        }
        if !conference_mutation_is_active(access, mutation) {
            return Err(ServiceProviderError::ConferenceConflict);
        }
    }

    let cleanup = controller_step(&access.shared.controller, |controller| {
        if !controller.conference_mutation_is_active(mutation) {
            return None;
        }
        let cleanup = controller.conference_participant_removed(session.id, participant_id);
        controller.complete_conference_mutation(mutation);
        cleanup
    });
    let committed_here = cleanup.is_some();
    if let Some(cleanup) = cleanup {
        execute_effects(access, cleanup).await;
    }
    let removed = controller_step(&access.shared.controller, |controller| {
        controller
            .conference_session_by_id(session.id)
            .is_some_and(|conference| conference.participants.get(participant_id).is_none())
    });
    if removed {
        if !committed_here {
            return Ok(());
        }
        let announcement = controller_step(&access.shared.controller, |controller| {
            controller.conference_announcement_effects(
                session.id,
                ConferenceAnnouncement::ParticipantRemoved(participant_id),
            )
        });
        execute_effects(access, announcement).await;
        show_conference_list_if_configured(access, &session).await;
        Ok(())
    } else {
        Err(ServiceProviderError::ConferenceConflict)
    }
}

pub async fn set_conference_participant_muted(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    participant_id: sccp_protocol::ParticipantId,
    muted: bool,
) -> Result<(), ServiceProviderError> {
    let effects = controller_step(&access.shared.controller, |controller| {
        controller
            .begin_conference_participant_mute(
                &session.device_id,
                session.id,
                participant_id,
                muted,
            )
            .and_then(|effects| {
                controller
                    .claim_conference_mutation_by_id(session.id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceParticipantRejection::Conflict)
            })
    });
    let (mutation, effects) = match effects {
        Ok(operation) => operation,
        Err(rejection) => {
            let provider_error = conference_participant_service_error(rejection);
            let text = match rejection {
                ConferenceParticipantRejection::Unavailable => "Conference unavailable",
                ConferenceParticipantRejection::NotModerator => "Moderator access required",
                ConferenceParticipantRejection::InvalidParticipant => "Participant unavailable",
                ConferenceParticipantRejection::Moderator => "Moderator cannot be muted",
                ConferenceParticipantRejection::LastModerator => {
                    "At least one moderator is required"
                }
                ConferenceParticipantRejection::Conflict => "Participant state changed",
            };
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                text,
            )
            .await;
            return Err(provider_error);
        }
    };

    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            return Err(ServiceProviderError::ConferenceConflict);
        }
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            controller_step(&access.shared.controller, |controller| {
                if controller.conference_mutation_is_active(mutation) {
                    controller.abort_conference_participant_mute(session.id, participant_id, muted);
                    controller.complete_conference_mutation(mutation);
                }
            });
            ast_log(
                LogLevel::Warning,
                &format!("conference participant mute failed: {error}"),
            );
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to update participant",
            )
            .await;
            return Err(ServiceProviderError::Delivery);
        }
        if !conference_mutation_is_active(access, mutation) {
            return Err(ServiceProviderError::ConferenceConflict);
        }
    }

    let committed = controller_step(&access.shared.controller, |controller| {
        if !controller.conference_mutation_is_active(mutation) {
            return false;
        }
        let committed = controller.conference_participant_muted(session.id, participant_id, muted);
        controller.complete_conference_mutation(mutation);
        committed
    });
    if committed {
        let announcement = controller_step(&access.shared.controller, |controller| {
            controller.conference_announcement_effects(
                session.id,
                if muted {
                    ConferenceAnnouncement::ParticipantMuted(participant_id)
                } else {
                    ConferenceAnnouncement::ParticipantUnmuted(participant_id)
                },
            )
        });
        execute_effects(access, announcement).await;
        show_conference_list_if_configured(access, &session).await;
        Ok(())
    } else {
        Err(ServiceProviderError::ConferenceConflict)
    }
}

pub async fn set_conference_participant_moderator(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    participant_id: sccp_protocol::ParticipantId,
    moderator: bool,
) -> Result<(), ServiceProviderError> {
    let effects = controller_step(&access.shared.controller, |controller| {
        controller
            .begin_conference_participant_role_change(
                &session.device_id,
                session.id,
                participant_id,
                moderator,
            )
            .and_then(|effects| {
                controller
                    .claim_conference_mutation_by_id(session.id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceParticipantRejection::Conflict)
            })
    });
    let (mutation, effects) = match effects {
        Ok(operation) => operation,
        Err(rejection) => {
            let provider_error = conference_participant_service_error(rejection);
            let text = match rejection {
                ConferenceParticipantRejection::Unavailable => "Conference unavailable",
                ConferenceParticipantRejection::NotModerator => "Moderator access required",
                ConferenceParticipantRejection::InvalidParticipant => "Participant unavailable",
                ConferenceParticipantRejection::Moderator
                | ConferenceParticipantRejection::Conflict => "Participant state changed",
                ConferenceParticipantRejection::LastModerator => {
                    "At least one moderator is required"
                }
            };
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                text,
            )
            .await;
            return Err(provider_error);
        }
    };

    let backend = AsteriskBackend::new(access);
    let mut compensation = Vec::new();
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            compensation.reverse();
            execute_cleanup_effects(access, compensation).await;
            return Err(ServiceProviderError::ConferenceConflict);
        }
        let compensate = match &effect {
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    BridgeOperation::SetParticipantMusicOnHold {
                        bridge_id,
                        participant_id,
                        call_id,
                        class,
                        enabled,
                    },
            }) => Some(
                PbxEffect::Bridge {
                    operation: BridgeOperation::SetParticipantMusicOnHold {
                        bridge_id: *bridge_id,
                        participant_id: *participant_id,
                        call_id: *call_id,
                        class: class.clone(),
                        enabled: !enabled,
                    },
                }
                .into(),
            ),
            _ => None,
        };
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            controller_step(&access.shared.controller, |controller| {
                if controller.conference_mutation_is_active(mutation) {
                    controller.abort_conference_participant_role_change(
                        session.id,
                        participant_id,
                        moderator,
                    );
                    controller.complete_conference_mutation(mutation);
                }
            });
            compensation.reverse();
            execute_cleanup_effects(access, compensation).await;
            ast_log(
                LogLevel::Warning,
                &format!("conference participant role change failed: {error}"),
            );
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to update participant",
            )
            .await;
            return Err(ServiceProviderError::Delivery);
        }
        if let Some(compensate) = compensate {
            compensation.push(compensate);
        }
        if !conference_mutation_is_active(access, mutation) {
            compensation.reverse();
            execute_cleanup_effects(access, compensation).await;
            return Err(ServiceProviderError::ConferenceConflict);
        }
    }

    let committed = controller_step(&access.shared.controller, |controller| {
        if !controller.conference_mutation_is_active(mutation) {
            return false;
        }
        let committed =
            controller.conference_participant_role_changed(session.id, participant_id, moderator);
        controller.complete_conference_mutation(mutation);
        committed
    });
    if committed {
        show_conference_list_if_configured(access, &session).await;
        Ok(())
    } else {
        compensation.reverse();
        execute_cleanup_effects(access, compensation).await;
        Err(ServiceProviderError::ConferenceConflict)
    }
}

pub(super) async fn start_conference_invite(
    access: &Access,
    device_id: DeviceId,
    moderator_call_id: CallId,
    line_instance: u32,
) {
    let current = controller_step(&access.shared.controller, |controller| {
        controller.call(moderator_call_id)
    });
    let Some(current) = current.filter(|call| call.device_id == device_id) else {
        return;
    };
    let selected_line = if line_instance == 0 {
        current.line_instance
    } else {
        line_instance
    };
    let config = access.config();
    let permitted = config
        .conference_for_device(&device_id)
        .is_some_and(|conference| conference.allowed);
    let binding = access.line_binding(&device_id, selected_line);
    drop(config);
    let Some(binding) = binding.filter(|_| permitted) else {
        display_conference_prompt(access, device_id, moderator_call_id, "Conference disabled")
            .await;
        return;
    };
    let Some(codec) = preferred_codec(
        access,
        &device_id,
        binding.line_instance,
        &PbxAudioFormat::ALL,
    ) else {
        display_conference_prompt(
            access,
            device_id,
            moderator_call_id,
            "Conference codec unavailable",
        )
        .await;
        return;
    };
    let invite_call_id = access.phone.reserve_call_id();
    let result = controller_step(&access.shared.controller, |controller| {
        controller
            .begin_conference_invite(
                moderator_call_id,
                invite_call_id,
                binding,
                codec,
                Instant::now(),
            )
            .and_then(|effects| {
                controller
                    .claim_conference_mutation(invite_call_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceRejection::Conflict)
            })
    });
    match result {
        Ok((mutation, effects)) => {
            execute_conference_invite_start(access, invite_call_id, mutation, effects).await;
        }
        Err(rejection) => {
            let text = match rejection {
                ConferenceRejection::Disabled => "Moderator access required",
                ConferenceRejection::NotConnected => "Connect the conference first",
                ConferenceRejection::Conflict => "Conference invite unavailable",
                ConferenceRejection::Unavailable => "Conference unavailable",
            };
            display_conference_prompt(access, device_id, moderator_call_id, text).await;
        }
    }
}

pub(super) async fn handle_conference_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
) {
    let session = controller_step(&access.shared.controller, |controller| {
        controller.conference_session(call_id).cloned()
    });
    if let Some(session) = session {
        if session.phase == ConferencePhase::Active {
            if session
                .pending_invite
                .as_ref()
                .is_some_and(|invite| invite.participant.handset_call_id == call_id)
            {
                let effects = controller_step(&access.shared.controller, |controller| {
                    controller
                        .confirm_conference_invite(call_id)
                        .and_then(|effects| {
                            controller
                                .claim_conference_mutation(call_id)
                                .map(|mutation| (mutation, effects))
                                .ok_or(ConferenceRejection::Conflict)
                        })
                });
                match effects {
                    Ok((mutation, effects)) => {
                        execute_conference_invite_merge(access, session, mutation, effects).await
                    }
                    Err(ConferenceRejection::NotConnected) => {
                        display_conference_prompt(
                            access,
                            device_id,
                            call_id,
                            "Invite is not connected",
                        )
                        .await;
                    }
                    Err(_) => {
                        display_conference_prompt(
                            access,
                            device_id,
                            call_id,
                            "Unable to add participant",
                        )
                        .await;
                    }
                }
                return;
            }
            if session.pending_invite.is_none()
                && session.participants.iter().any(|participant| {
                    participant.moderator && participant.handset_call_id == call_id
                })
            {
                start_conference_invite(access, device_id, call_id, line_instance).await;
                return;
            }
            display_conference_prompt(access, device_id, call_id, "Conference invite pending")
                .await;
            return;
        }
        if session.phase != ConferencePhase::Consultation
            || session.consultation_handset_call_id != call_id
        {
            display_conference_prompt(access, device_id, call_id, "Conference already active")
                .await;
            return;
        }
        let effects = controller_step(&access.shared.controller, |controller| {
            controller.confirm_conference(call_id).and_then(|effects| {
                controller
                    .claim_conference_mutation(call_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceRejection::Conflict)
            })
        });
        match effects {
            Ok((mutation, effects)) => {
                execute_conference_merge(access, session, mutation, effects).await
            }
            Err(ConferenceRejection::NotConnected) => {
                display_conference_prompt(
                    access,
                    device_id,
                    call_id,
                    "Consultation is not connected",
                )
                .await;
            }
            Err(_) => {
                display_conference_prompt(access, device_id, call_id, "Conference unavailable")
                    .await;
            }
        }
        return;
    }

    let current = controller_step(&access.shared.controller, |controller| {
        controller.call(call_id)
    });
    let Some(current) = current.filter(|call| call.device_id == device_id) else {
        return;
    };
    let selected_line = if line_instance == 0 {
        current.line_instance
    } else {
        line_instance
    };
    let config = access.config();
    let conference_policy = config.conference_for_device(&device_id).cloned();
    let permitted = conference_policy
        .as_ref()
        .is_some_and(|conference| conference.allowed);
    let media_policy = conference_policy.map(|conference| ConferenceMediaPolicy {
        music_on_hold_class: conference.music_on_hold_class,
        mute_on_entry: conference.mute_on_entry,
        play_general_announcements: conference.play_general_announcements,
        play_participant_announcements: conference.play_participant_announcements,
    });
    let binding = access.line_binding(&device_id, selected_line);
    drop(config);
    let Some(binding) = binding else {
        return;
    };
    let Some(codec) = preferred_codec(
        access,
        &device_id,
        binding.line_instance,
        &PbxAudioFormat::ALL,
    ) else {
        display_conference_prompt(access, device_id, call_id, "Conference codec unavailable").await;
        return;
    };
    let consultation_call_id = access.phone.reserve_call_id();
    let result = controller_step(&access.shared.controller, |controller| {
        controller
            .begin_conference_with_media(
                ConferenceConsultationRequest {
                    original_call_id: call_id,
                    consultation_call_id,
                    binding,
                    codec,
                    now: Instant::now(),
                    permitted,
                },
                media_policy.unwrap_or_default(),
            )
            .and_then(|effects| {
                controller
                    .claim_conference_mutation(consultation_call_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceRejection::Conflict)
            })
    });
    match result {
        Ok((mutation, effects)) => {
            execute_conference_start(access, consultation_call_id, mutation, effects).await;
        }
        Err(rejection) => {
            let text = match rejection {
                ConferenceRejection::Disabled => "Conference disabled",
                ConferenceRejection::NotConnected => "Connect the call first",
                ConferenceRejection::Conflict => "Conference already pending",
                ConferenceRejection::Unavailable => "Conference unavailable",
            };
            display_conference_prompt(access, device_id, call_id, text).await;
        }
    }
}

pub(super) async fn handle_conference_destination(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
) {
    let config = access.config();
    let policy = access
        .line_binding(&device_id, line_instance)
        .as_ref()
        .and_then(|binding| config.conference_dialing_for_binding(binding));
    let target_matches = controller_step(&access.shared.controller, |controller| {
        controller
            .call(call_id)
            .is_some_and(|call| call.device_id == device_id && call.line_instance == line_instance)
    });
    if !target_matches {
        return;
    }
    let Some(policy) = policy.filter(|policy| policy.enabled && policy.destination.is_some())
    else {
        display_conference_prompt(
            access,
            device_id.clone(),
            call_id,
            "Conference dialing unavailable",
        )
        .await;
        let effects = controller_step(&access.shared.controller, |controller| {
            controller.hangup(call_id)
        });
        execute_cleanup_effects(access, effects).await;
        return;
    };
    let Some(destination) = policy.destination else {
        debug_assert!(false, "enabled conference policy lost its destination");
        return;
    };
    let result = controller_step(&access.shared.controller, |controller| {
        controller.begin_conference_destination(ConferenceDestinationRequest {
            device_id: device_id.clone(),
            handset_call_id: call_id,
            destination,
            application_options: policy.application_options,
        })
    });
    match result {
        Ok(effects) => {
            execute_conference_destination_start(access, device_id, call_id, effects).await
        }
        Err(_) => {
            display_conference_prompt(access, device_id, call_id, "Conference dialing unavailable")
                .await;
            let effects = controller_step(&access.shared.controller, |controller| {
                controller.hangup(call_id)
            });
            execute_cleanup_effects(access, effects).await;
        }
    }
}

pub(super) async fn execute_conference_destination_start(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    effects: Vec<DriverEffect>,
) {
    let mutation = effects.iter().find_map(|effect| match effect {
        DriverEffect::Backend(PbxEffect::StartConferenceDestination { operation }) => {
            Some(operation.mutation)
        }
        _ => None,
    });
    let Some(mutation) = mutation else {
        return;
    };
    let held_calls = effects
        .iter()
        .filter_map(|effect| match effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id }) => Some(*call_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let backend = AsteriskBackend::new(access);
    let mut completed_holds = Vec::new();
    for (index, effect) in effects.into_iter().enumerate() {
        if !controller_step(&access.shared.controller, |controller| {
            controller.conference_mutation_is_active(mutation)
        }) {
            return;
        }
        let held_call = match &effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id }) => Some(*call_id),
            _ => None,
        };
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference destination launch failed: {error}"),
            );
            display_conference_prompt(access, device_id, call_id, "Conference dialing failed")
                .await;
            let cleanup = controller_step(&access.shared.controller, |controller| {
                controller.conference_destination_failed(
                    mutation,
                    call_id,
                    &held_calls,
                    &completed_holds,
                )
            });
            execute_cleanup_effects(access, cleanup).await;
            return;
        }
        if let Some(held_call) = held_call {
            completed_holds.push(held_call);
        }
        if !controller_step(&access.shared.controller, |controller| {
            controller.conference_mutation_is_active(mutation)
        }) {
            return;
        }
    }
}

pub(super) async fn execute_conference_start(
    access: &Access,
    consultation_call_id: CallId,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    let mut progress = ConferenceStartProgress::default();
    let consultation_pbx = controller_step(&access.shared.controller, |controller| {
        controller
            .conference_session(consultation_call_id)
            .map(|session| session.consultation_call_id)
    });
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            if progress.channel_created()
                && let Some(pbx_id) = consultation_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
        let completed = ConferenceStartProgress::from(&effect);
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference consultation setup failed: {error}"),
            );
            let cleanup = controller_step(&access.shared.controller, |controller| {
                if !controller.conference_mutation_is_active(mutation) {
                    return Vec::new();
                }
                let cleanup = controller.abort_conference(
                    consultation_call_id,
                    false,
                    progress.channel_created(),
                    progress.active_leg_held(),
                    progress.active_handset_held(),
                );
                controller.complete_conference_mutation(mutation);
                cleanup
            });
            execute_cleanup_effects(access, cleanup).await;
            if progress.channel_created()
                && let Some(pbx_id) = consultation_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
        progress |= completed;
        if !conference_mutation_is_active(access, mutation) {
            if progress.channel_created()
                && let Some(pbx_id) = consultation_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
    }
    controller_step(&access.shared.controller, |controller| {
        controller.complete_conference_mutation(mutation)
    });
}

pub(super) async fn execute_conference_invite_start(
    access: &Access,
    invite_call_id: CallId,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    let mut progress = ConferenceStartProgress::default();
    let invite_pbx = controller_step(&access.shared.controller, |controller| {
        controller
            .conference_session(invite_call_id)
            .and_then(|session| session.pending_invite.as_ref())
            .map(|invite| invite.participant.pbx_call_id)
    });
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            if progress.channel_created()
                && let Some(pbx_id) = invite_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
        let completed = ConferenceStartProgress::from(&effect);
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference invite setup failed: {error}"),
            );
            let cleanup = controller_step(&access.shared.controller, |controller| {
                if !controller.conference_mutation_is_active(mutation) {
                    return Vec::new();
                }
                let cleanup = controller.abort_conference_invite(
                    invite_call_id,
                    progress.channel_created(),
                    progress.active_leg_held(),
                    progress.active_handset_held(),
                );
                controller.complete_conference_mutation(mutation);
                cleanup
            });
            execute_cleanup_effects(access, cleanup).await;
            if progress.channel_created()
                && let Some(pbx_id) = invite_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
        progress |= completed;
        if !conference_mutation_is_active(access, mutation) {
            if progress.channel_created()
                && let Some(pbx_id) = invite_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
    }
    controller_step(&access.shared.controller, |controller| {
        controller.complete_conference_mutation(mutation)
    });
}

const CONFERENCE_BRIDGE_READY_RETRY_INTERVAL: Duration = Duration::from_millis(20);
const CONFERENCE_BRIDGE_READY_TIMEOUT: Duration = Duration::from_secs(1);

fn is_transient_conference_bridge_readiness_error(
    error: &EffectExecutionError<AsteriskBackendError, String>,
) -> bool {
    matches!(
        error,
        EffectExecutionError::Backend {
            effect,
            error: AsteriskBackendError::CallFeature(CallFeatureError::NotFound {
                operation: "merge conference consultation",
            }),
            ..
        } if matches!(
            effect.as_ref(),
            PbxEffect::Bridge {
                operation: BridgeOperation::MergeConsultation { .. },
            }
        )
    )
}

/// Asterisk documents that a two-party bridge can temporarily have no bridge
/// (or fewer than two members) while its members finish joining. SCCP answer
/// and soft-key events are asynchronous to that transition, so wait for the
/// exact consultation-bridge lookup to become ready without retrying topology
/// conflicts or native merge failures.
async fn execute_conference_merge_effect(
    access: &Access,
    backend: &AsteriskBackend<'_>,
    index: usize,
    effect: DriverEffect,
    mutation: ConferenceMutationToken,
) -> Result<bool, EffectExecutionError<AsteriskBackendError, String>> {
    let started = Instant::now();
    let mut retries = 0_u32;
    loop {
        match execute_one_effect(access, backend, index, effect.clone()).await {
            Ok(()) => {
                if retries != 0 {
                    ast_log(
                        LogLevel::Debug,
                        &format!(
                            "conference bridge became ready after {retries} retr{}",
                            if retries == 1 { "y" } else { "ies" }
                        ),
                    );
                }
                return Ok(true);
            }
            Err(error)
                if is_transient_conference_bridge_readiness_error(&error)
                    && started.elapsed() < CONFERENCE_BRIDGE_READY_TIMEOUT =>
            {
                if !conference_mutation_is_active(access, mutation) {
                    return Ok(false);
                }
                retries += 1;
                tokio::time::sleep(CONFERENCE_BRIDGE_READY_RETRY_INTERVAL).await;
                if !conference_mutation_is_active(access, mutation) {
                    return Ok(false);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) async fn execute_conference_merge(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    let mut bridge_created = false;
    let mut original_resumed = false;
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            remove_channel(access, session.consultation_call_id);
            return;
        }
        let completed_create = matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: BridgeOperation::Create { .. }
            })
        );
        let completed_resume = matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. }));
        let completed =
            execute_conference_merge_effect(access, &backend, index, effect, mutation).await;
        let completed = match completed {
            Ok(completed) => completed,
            Err(error) => {
                ast_log(
                    LogLevel::Warning,
                    &format!("conference merge failed: {error}"),
                );
                let cleanup = controller_step(&access.shared.controller, |controller| {
                    if !controller.conference_mutation_is_active(mutation) {
                        return Vec::new();
                    }
                    let cleanup = controller.abort_conference(
                        session.consultation_handset_call_id,
                        bridge_created,
                        true,
                        !original_resumed,
                        true,
                    );
                    controller.complete_conference_mutation(mutation);
                    cleanup
                });
                execute_cleanup_effects(access, cleanup).await;
                remove_channel(access, session.consultation_call_id);
                display_conference_prompt(
                    access,
                    session.device_id,
                    session.original_handset_call_id,
                    "Unable to create conference",
                )
                .await;
                return;
            }
        };
        if !completed {
            remove_channel(access, session.consultation_call_id);
            return;
        }
        bridge_created |= completed_create;
        original_resumed |= completed_resume;
        if !conference_mutation_is_active(access, mutation) {
            remove_channel(access, session.consultation_call_id);
            return;
        }
    }
    let (committed, announcement) = controller_step(&access.shared.controller, |controller| {
        if !controller.conference_mutation_is_active(mutation) {
            return (false, None);
        }
        let committed = controller.conference_merged(session.consultation_handset_call_id);
        let announcement = committed.then(|| {
            controller
                .conference_announcement_effects(session.id, ConferenceAnnouncement::Connected)
        });
        controller.complete_conference_mutation(mutation);
        (committed, announcement)
    });
    if !committed {
        return;
    }
    if let Some(effects) = announcement {
        execute_effects(access, effects).await;
    }
    display_conference_prompt(
        access,
        session.device_id.clone(),
        session.consultation_handset_call_id,
        "Conference connected",
    )
    .await;
    show_conference_list_if_configured(access, &session).await;
}

pub(super) async fn execute_selected_conference_merge(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    let mut bridge_created = false;
    let mut resumed_call_ids = Vec::new();
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            return;
        }
        let completed_create = matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: BridgeOperation::Create { .. }
            })
        );
        let resumed = match &effect {
            DriverEffect::Backend(PbxEffect::Resume { call_id }) => Some(*call_id),
            _ => None,
        };
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("selected-call conference merge failed: {error}"),
            );
            let cleanup = controller_step(&access.shared.controller, |controller| {
                if !controller.conference_mutation_is_active(mutation) {
                    return Vec::new();
                }
                let cleanup = controller.abort_join_conference(
                    session.original_handset_call_id,
                    bridge_created,
                    &resumed_call_ids,
                );
                controller.complete_conference_mutation(mutation);
                cleanup
            });
            execute_cleanup_effects(access, cleanup).await;
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to join calls",
            )
            .await;
            return;
        }
        bridge_created |= completed_create;
        if let Some(call_id) = resumed {
            resumed_call_ids.push(call_id);
        }
        if !conference_mutation_is_active(access, mutation) {
            return;
        }
    }
    let (committed, announcement) = controller_step(&access.shared.controller, |controller| {
        if !controller.conference_mutation_is_active(mutation) {
            return (false, None);
        }
        let committed = controller.conference_merged(session.original_handset_call_id);
        let announcement = committed.then(|| {
            controller
                .conference_announcement_effects(session.id, ConferenceAnnouncement::Connected)
        });
        controller.complete_conference_mutation(mutation);
        (committed, announcement)
    });
    if !committed {
        return;
    }
    if let Some(effects) = announcement {
        execute_effects(access, effects).await;
    }
    display_conference_prompt(
        access,
        session.device_id.clone(),
        session.original_handset_call_id,
        "Conference connected",
    )
    .await;
    show_conference_list_if_configured(access, &session).await;
}

pub(super) async fn execute_conference_invite_merge(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let Some(invite) = session.pending_invite.as_ref() else {
        return;
    };
    let invite_call_id = invite.participant.handset_call_id;
    let invite_pbx_id = invite.participant.pbx_call_id;
    let backend = AsteriskBackend::new(access);
    let mut moderator_resumed = false;
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            remove_channel(access, invite_pbx_id);
            return;
        }
        let completed_resume = matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. }));
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference participant merge failed: {error}"),
            );
            let cleanup = controller_step(&access.shared.controller, |controller| {
                if !controller.conference_mutation_is_active(mutation) {
                    return Vec::new();
                }
                let cleanup = controller.abort_conference_invite(
                    invite_call_id,
                    true,
                    !moderator_resumed,
                    true,
                );
                controller.complete_conference_mutation(mutation);
                cleanup
            });
            execute_cleanup_effects(access, cleanup).await;
            remove_channel(access, invite_pbx_id);
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to add participant",
            )
            .await;
            return;
        }
        moderator_resumed |= completed_resume;
        if !conference_mutation_is_active(access, mutation) {
            remove_channel(access, invite_pbx_id);
            return;
        }
    }
    let (committed, announcement) = controller_step(&access.shared.controller, |controller| {
        if !controller.conference_mutation_is_active(mutation) {
            return (false, None);
        }
        let committed = controller.conference_invite_merged(invite_call_id);
        let announcement = committed.then(|| {
            controller.conference_announcement_effects(
                session.id,
                ConferenceAnnouncement::ParticipantJoined(invite.participant.id),
            )
        });
        controller.complete_conference_mutation(mutation);
        (committed, announcement)
    });
    if !committed {
        return;
    }
    if let Some(effects) = announcement {
        execute_effects(access, effects).await;
    }
    display_conference_prompt(
        access,
        session.device_id.clone(),
        invite_call_id,
        "Participant added",
    )
    .await;
    show_conference_list_if_configured(access, &session).await;
}

pub(super) async fn display_conference_prompt(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    text: &str,
) {
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 4,
                text: text.into(),
            },
        ))
        .await;
}

pub fn publish_device_features(access: &Access, device_id: &DeviceId, state: &DeviceFeatureState) {
    let config = access.config();
    let line_instances = forwarding_ui_line_instances(
        None,
        config
            .appearances_for_device(device_id)
            .map(|binding| (binding.line.number.as_str(), binding.line_instance)),
    )
    .unwrap_or_default();
    for line_instance in line_instances {
        access.spawn_phone(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetForwardStatus {
                line_instance: LineInstance::new(line_instance),
                forward_all: state
                    .forwarding
                    .all
                    .clone()
                    .map(ForwardingDestination::into_string),
                forward_busy: state
                    .forwarding
                    .busy
                    .clone()
                    .map(ForwardingDestination::into_string),
                forward_no_answer: state
                    .forwarding
                    .no_answer
                    .clone()
                    .map(ForwardingDestination::into_string),
            },
        ));
    }
    let Some(device) = config.devices.get(device_id) else {
        return;
    };
    access.spawn_phone(PhoneCommand::new(
        device_id.clone(),
        PhoneCommandAction::SetStatusMessage {
            message: handset_status_message(state.dnd),
            beep: false,
        },
    ));
    for (instance, button_mode) in config.dnd_buttons_for_device(device_id) {
        access.spawn_phone(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetDoNotDisturbStatus {
                instance: LineInstance::new(instance),
                mode: phone_dnd_mode(state.dnd),
                button_mode: phone_dnd_button_mode(button_mode),
            },
        ));
    }
    for button in &device.buttons {
        let ButtonDefinition::Feature(feature) = button else {
            continue;
        };
        if feature.feature == ButtonType::DoNotDisturb {
            continue;
        }
        let enabled = match feature.feature {
            ButtonType::ForwardAll => state.forwarding.all.is_some(),
            ButtonType::ForwardBusy => state.forwarding.busy.is_some(),
            ButtonType::ForwardNoAnswer => state.forwarding.no_answer.is_some(),
            ButtonType::ParkingLot => config
                .parking_lot_for_button(device_id, feature.instance)
                .is_some_and(|button| {
                    access
                        .shared
                        .parking_registry
                        .lock_unpoisoned()
                        .lot_has_calls(&button.lot)
                }),
            _ => state
                .buttons
                .get(&feature.instance)
                .copied()
                .unwrap_or(false),
        };
        access.spawn_phone(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetFeatureStatus {
                instance: LineInstance::new(feature.instance),
                enabled,
            },
        ));
    }
}

pub(super) const fn phone_dnd_mode(mode: DndMode) -> PhoneDndMode {
    match mode {
        DndMode::Off => PhoneDndMode::Off,
        DndMode::Silent => PhoneDndMode::Silent,
        DndMode::Reject => PhoneDndMode::Reject,
    }
}

pub(super) const fn phone_dnd_button_mode(mode: DndButtonMode) -> PhoneDndButtonMode {
    match mode {
        DndButtonMode::Cycle => PhoneDndButtonMode::Cycle,
        DndButtonMode::Silent => PhoneDndButtonMode::Silent,
        DndButtonMode::Reject => PhoneDndButtonMode::Reject,
    }
}

pub fn publish_ami_event(access: &Access, event: &ManagementEvent) {
    if let Err(error) = access.shared.ami_events.publish(event) {
        ast_log(
            LogLevel::Warning,
            &format!("unable to publish a management event: {error}"),
        );
    }
}

pub fn publish_feature_changes(
    access: &Access,
    device_id: &DeviceId,
    previous: &DeviceFeatureState,
    current: &DeviceFeatureState,
) {
    for change in feature_changes(previous, current) {
        publish_ami_event(access, &feature_event(device_id, change));
    }
}

pub async fn expire_forwarding_entries(access: &Access, now: Instant) {
    let expired = access
        .shared
        .forwarding_entries
        .lock_unpoisoned()
        .claim_expired(now);
    for outcome in expired {
        match outcome {
            ForwardingExpiryOutcome::Cancel(entry) => {
                if send_confirmed_forwarding(
                    access,
                    PhoneCommand::new(
                        entry.device_id,
                        PhoneCommandAction::CloseCall {
                            call_id: entry.call_id,
                        },
                    ),
                )
                .await
                    == ForwardingWriteOutcome::Failed
                {
                    ast_log(
                        LogLevel::Warning,
                        "unable to close expired forwarding collection on the handset",
                    );
                }
            }
            ForwardingExpiryOutcome::Commit(commit) => {
                finish_forwarding_commit(access, commit).await;
            }
        }
    }
}

pub async fn expire_no_answer_routes(access: &Access, now: Instant) {
    let expired = access
        .shared
        .no_answer_timers
        .lock_unpoisoned()
        .claim_expired(now);
    for timer in expired {
        let (line, claimed) = controller_step(&access.shared.controller, |controller| {
            (
                controller
                    .pbx_call(timer.call_id)
                    .map(|call| call.line.clone()),
                controller.claim_ringing_forward(timer.call_id),
            )
        });
        if !claimed {
            let _ = access
                .shared
                .no_answer_timers
                .lock_unpoisoned()
                .cancel(timer.call_id, timer.id);
            continue;
        }
        let operation = ForwardingOperation {
            call_id: timer.call_id,
            context: timer.context,
            destination: timer.destination,
            reason: ForwardingRouteReason::NoAnswer,
        };
        if let Err(error) = AsteriskBackend::new(access).forward(&operation) {
            controller_step(&access.shared.controller, |controller| {
                controller.rollback_ringing_forward(timer.call_id)
            });
            let _ = access
                .shared
                .no_answer_timers
                .lock_unpoisoned()
                .cancel(timer.call_id, timer.id);
            ast_log(
                LogLevel::Warning,
                &format!(
                    "unable to apply no-answer routing for PBX call {}: {error}",
                    timer.call_id.0
                ),
            );
            continue;
        }
        if access
            .shared
            .no_answer_timers
            .lock_unpoisoned()
            .commit(timer.call_id, timer.id)
            .is_err()
        {
            controller_step(&access.shared.controller, |controller| {
                controller.rollback_ringing_forward(timer.call_id)
            });
            continue;
        }
        let effects = controller_step(&access.shared.controller, |controller| {
            controller.complete_ringing_forward(timer.call_id)
        });
        if let Some(line) = line {
            publish_line(access, &line);
        }
        execute_effects(access, effects).await;
    }
}

pub fn cancel_no_answer_timer(access: &Access, pbx_id: PbxCallId) -> bool {
    let mut timers = access.shared.no_answer_timers.lock_unpoisoned();
    let Some(timer_id) = timers.get(pbx_id).map(|timer| timer.id) else {
        return false;
    };
    timers.cancel_pending(pbx_id, timer_id).is_ok()
}

pub fn clear_no_answer_route(access: &Access, pbx_id: PbxCallId) {
    access
        .shared
        .no_answer_plans
        .lock_unpoisoned()
        .remove(&pbx_id);
    let mut timers = access.shared.no_answer_timers.lock_unpoisoned();
    if let Some(timer_id) = timers.get(pbx_id).map(|timer| timer.id) {
        let _ = timers.cancel(pbx_id, timer_id);
    }
}
