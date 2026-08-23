//! SCCP session events and core call-control flows.

use super::*;
use crate::runtime::controller::VideoFallbackReason;

fn owned_pbx_call(access: &Access, device_id: &DeviceId, call_id: CallId) -> Option<PbxCallId> {
    controller_step(&access.shared.controller, |controller| {
        controller
            .call(call_id)
            .and_then(|call| (&call.device_id == device_id).then_some(call.pbx_id))
    })
}

pub async fn handle_phone_event(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    event: PhoneEvent,
) {
    let PhoneDeviceEvent {
        device_id,
        session_generation,
        event,
    } = match event {
        PhoneEvent::SessionError { peer, error } => {
            ast_log(
                LogLevel::Warning,
                &format!("SCCP session from {peer} ended with an error: {error}"),
            );
            return;
        }
        PhoneEvent::ProtocolWarning {
            peer,
            device_id,
            message_id,
            error,
        } => {
            ast_log(
                LogLevel::Warning,
                &format!(
                    "ignored malformed SCCP message 0x{message_id:04x} from {} ({peer}): {error}",
                    device_id.as_ref().map_or("unregistered", DeviceId::as_str)
                ),
            );
            return;
        }
        PhoneEvent::Device(event) => event,
    };
    if !matches!(&event, PhoneDeviceEventKind::Registered(_))
        && !controller_step(&access.shared.controller, |controller| {
            controller.session_is_current(&device_id, session_generation)
        })
    {
        return;
    }
    let actions = match event {
        PhoneDeviceEventKind::Registered(registration) => {
            let device = registration.id.clone();
            let registered_event =
                registration_event(&device, RegistrationStatus::Registered, Some(&registration));
            let Some((session, affected_conferences, surviving_conferences)) =
                controller_step(&access.shared.controller, |controller| {
                    let mut affected = controller
                        .calls()
                        .filter(|call| call.device_id == device)
                        .filter_map(|call| {
                            controller
                                .conference_session(call.sccp_id)
                                .map(|conference| conference.id)
                        })
                        .collect::<Vec<_>>();
                    affected.sort_unstable();
                    affected.dedup();
                    let session = controller.register_session(session_generation, registration)?;
                    if !session.replaced {
                        affected.clear();
                    }
                    let surviving = affected
                        .iter()
                        .filter_map(|conference_id| {
                            controller.conference_session_by_id(*conference_id).cloned()
                        })
                        .collect::<Vec<_>>();
                    Some((session, affected, surviving))
                })
            else {
                return;
            };
            if session.replaced {
                access
                    .shared
                    .pending_mobility_prompts
                    .lock_unpoisoned()
                    .retain(|(pending_device, _), _| pending_device != &device);
                cancel_forwarding_entry_for_device(access, &device);
                for conference_id in affected_conferences {
                    cancel_conference_announcement(access, conference_id);
                }
            }
            execute_cleanup_effects(access, session.cleanup).await;
            prune_recording_sessions(access, recordings).await;
            for conference in surviving_conferences {
                if access
                    .config()
                    .conference_for_device(&conference.device_id)
                    .is_some_and(|config| config.show_conference_list)
                {
                    show_conference_list(
                        access,
                        conference.device_id,
                        conference.original_handset_call_id,
                    )
                    .await;
                }
            }
            let feature_guard = access.shared.feature_mutations.lock_unpoisoned();
            let config = access.config();
            let defaults = configured_feature_state(&config, &device).unwrap_or_default();
            let previous = controller_step(&access.shared.controller, |controller| {
                controller.feature_state(&device).cloned()
            });
            let (features, restore_error) = registration_state_or_fallback(
                access
                    .shared
                    .feature_store
                    .load_configured_device(&config, &device),
                previous,
                defaults,
            );
            if let Some(error) = restore_error {
                log_feature_store_error(
                    "restore feature state during registration",
                    Some(&device),
                    &error,
                );
            }
            controller_step(&access.shared.controller, |controller| {
                controller.set_feature_state(&device, features.clone());
            });
            let registered = registered_device_ids(&access.shared);
            let registration_result = {
                let mut contexts = access.shared.registration_contexts.lock_unpoisoned();
                contexts.suppressed_devices.remove(&device);
                contexts.reconcile(&config, &registered)
            };
            if let Err(error) = registration_result {
                access
                    .shared
                    .registration_contexts
                    .lock_unpoisoned()
                    .suppressed_devices
                    .insert(device.clone());
                ast_log(
                    LogLevel::Error,
                    &format!(
                        "unable to publish registration-context extensions for a registered device: {error}"
                    ),
                );
                let actions = controller_step(&access.shared.controller, |controller| {
                    controller.disconnected(&device)
                });
                drop(feature_guard);
                if let Err(error) = access
                    .phone
                    .send(PhoneCommand::new(
                        device,
                        PhoneCommandAction::DisconnectDevice {},
                    ))
                    .await
                {
                    ast_log(
                        LogLevel::Error,
                        &format!(
                            "unable to disconnect a device after registration-context publication failed: {error}"
                        ),
                    );
                }
                execute_cleanup_effects(access, actions).await;
                prune_recording_sessions(access, recordings).await;
                Vec::new()
            } else {
                install_blf(access, &device);
                publish_device_lines(access, &device);
                publish_device_features(access, &device, &features);
                drop(feature_guard);
                publish_ami_event(access, &registered_event);
                restore_system_message(access, &device).await;
                restore_mobility_appearances(access, &device).await;
                Vec::new()
            }
        }
        PhoneDeviceEventKind::Disconnected {} => {
            access
                .shared
                .pending_mobility_prompts
                .lock_unpoisoned()
                .retain(|(pending_device, _), _| pending_device != &device_id);
            cancel_forwarding_entry_for_device(access, &device_id);
            let feature_guard = access.shared.feature_mutations.lock_unpoisoned();
            uninstall_device_blf(access, &device_id);
            let (actions, surviving_conferences, affected_conferences) =
                controller_step(&access.shared.controller, |controller| {
                    let mut affected = controller
                        .calls()
                        .filter(|call| call.device_id == device_id)
                        .filter_map(|call| {
                            controller
                                .conference_session(call.sccp_id)
                                .map(|session| session.id)
                        })
                        .collect::<Vec<_>>();
                    affected.sort_unstable();
                    affected.dedup();
                    let actions = controller.disconnected(&device_id);
                    let surviving = affected
                        .iter()
                        .filter_map(|conference_id| {
                            controller.conference_session_by_id(*conference_id).cloned()
                        })
                        .collect::<Vec<_>>();
                    (actions, surviving, affected)
                });
            let registered = registered_device_ids(&access.shared);
            let registration_result = {
                let mut contexts = access.shared.registration_contexts.lock_unpoisoned();
                contexts.suppressed_devices.insert(device_id.clone());
                contexts.reconcile(&access.config(), &registered)
            };
            if let Err(error) = registration_result {
                ast_log(
                    LogLevel::Error,
                    &format!(
                        "unable to remove registration-context extensions for a disconnected device: {error}"
                    ),
                );
            }
            publish_device_lines(access, &device_id);
            drop(feature_guard);
            for conference_id in affected_conferences {
                cancel_conference_announcement(access, conference_id);
            }
            execute_cleanup_effects(access, actions).await;
            for session in surviving_conferences {
                let show_list = access
                    .config()
                    .conference_for_device(&session.device_id)
                    .is_some_and(|conference| conference.show_conference_list);
                if show_list {
                    show_conference_list(
                        access,
                        session.device_id,
                        session.original_handset_call_id,
                    )
                    .await;
                }
            }
            publish_ami_event(
                access,
                &registration_event(&device_id, RegistrationStatus::Disconnected, None),
            );
            Vec::new()
        }
        PhoneDeviceEventKind::Capabilities { capabilities } => {
            controller_step(&access.shared.controller, |controller| {
                controller.update_capabilities(&device_id, session_generation, capabilities)
            });
            Vec::new()
        }
        PhoneDeviceEventKind::OffHook {
            call_id,
            line_instance,
        } => {
            let config = access.config();
            let binding = access.line_binding(&device_id, line_instance.get());
            let hotline = binding
                .as_ref()
                .and_then(|binding| config.hotline_destination_for_binding(binding).cloned());
            drop(config);
            let codec = preferred_codec(
                access,
                &device_id,
                line_instance.get(),
                &PbxAudioFormat::ALL,
            );
            let ringing = controller_step(&access.shared.controller, |controller| {
                controller
                    .call(call_id)
                    .is_some_and(|call| call.state == CallState::Ringing)
            });
            if ringing {
                let transition = controller_step(&access.shared.controller, |controller| {
                    controller.begin_active_call_switch_transaction(&device_id, call_id)
                });
                if let Ok(transition) = transition {
                    execute_answer_call_transition(access, transition).await;
                }
                Vec::new()
            } else if let Some((binding, codec)) = binding.zip(codec) {
                let transition = controller_step(&access.shared.controller, |controller| {
                    if let Some(destination) = hotline {
                        controller.begin_hotline_call_transaction(HotlineCallRequest {
                            handset_call_id: call_id,
                            binding,
                            codec,
                            destination,
                            now: Instant::now(),
                        })
                    } else {
                        controller.begin_additional_phone_call_transaction(
                            call_id,
                            binding,
                            codec,
                            Instant::now(),
                        )
                    }
                });
                if let Ok(transition) = transition {
                    execute_call_transition(access, transition).await;
                }
                Vec::new()
            } else {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id.clone(),
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 4,
                            text: "No compatible audio codec".into(),
                        },
                    ))
                    .await;
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::CloseCall { call_id },
                    ))
                    .await;
                Vec::new()
            }
        }
        PhoneDeviceEventKind::OnHook { call_id, .. } => {
            if cancel_forwarding_entry_for_call(access, &device_id, call_id) {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::CloseCall { call_id },
                    ))
                    .await;
            } else if !handle_transfer_hangup(access, device_id, call_id, true).await {
                handle_handset_hangup(access, call_id, true).await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::Digit { call_id, digit } => {
            if handle_forwarding_digit(access, &device_id, call_id, digit).await {
                Vec::new()
            } else {
                controller_step(&access.shared.controller, |controller| {
                    controller.digit(call_id, digit, Instant::now())
                })
            }
        }
        PhoneDeviceEventKind::EnblocCall {
            call_id, number, ..
        } => {
            if replace_and_commit_forwarding_entry(access, &device_id, call_id, &number).await {
                Vec::new()
            } else {
                controller_step(&access.shared.controller, |controller| {
                    controller.enbloc(call_id, number)
                })
            }
        }
        PhoneDeviceEventKind::FeatureButton { instance } => {
            handle_feature_button(access, device_id, instance.get());
            Vec::new()
        }
        PhoneDeviceEventKind::DoNotDisturbButton { instance } => {
            handle_dnd_button(access, device_id, instance.get());
            Vec::new()
        }
        PhoneDeviceEventKind::MobilityButton { instance } => {
            handle_mobility_button(access, device_id, instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::VoicemailButton {
            call_id,
            line_instance,
        } => {
            let destination = {
                let config = access.config();
                access
                    .line_binding(&device_id, line_instance.get())
                    .and_then(|binding| {
                        config
                            .features_for_line(&binding.line.number)
                            .and_then(|features| features.voicemail.number.as_ref())
                            .map(|destination| destination.as_str().to_owned())
                    })
            };
            if let Some(destination) = destination {
                controller_step(&access.shared.controller, |controller| {
                    controller.enbloc(call_id, destination)
                })
            } else {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 4,
                            text: "Voicemail is not configured for this line".into(),
                        },
                    ))
                    .await;
                Vec::new()
            }
        }
        PhoneDeviceEventKind::ParkingLotButton {
            instance,
            call_id,
            line_instance,
        } => {
            handle_parking_lot_button(
                access,
                device_id,
                instance.get(),
                call_id,
                line_instance.get(),
            )
            .await;
            Vec::new()
        }
        PhoneDeviceEventKind::ParkingMenuSelection { lot, slot } => {
            let _ = begin_parking_retrieval(access, device_id, 0, lot, slot).await;
            Vec::new()
        }
        PhoneDeviceEventKind::PhoneServiceResponse { response } => {
            handle_mobility_response(access, device_id, response).await;
            Vec::new()
        }
        PhoneDeviceEventKind::ConferenceListAction { action } => {
            handle_conference_list_action(access, device_id, action).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Answer,
            ..
        } => {
            let transition = controller_step(&access.shared.controller, |controller| {
                controller.begin_active_call_switch_transaction(&device_id, call_id)
            });
            if let Ok(transition) = transition {
                execute_answer_call_transition(access, transition).await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id,
            line_instance,
            soft_key,
        } if matches!(
            soft_key,
            SoftKey::ImmediateDivert | SoftKey::TransferToVoicemail
        ) =>
        {
            handle_voicemail_soft_key(access, device_id, call_id, line_instance.get(), soft_key)
                .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id,
            line_instance,
            soft_key,
        } if matches!(
            soft_key,
            SoftKey::DoNotDisturb
                | SoftKey::Private
                | SoftKey::ForwardAll
                | SoftKey::ForwardBusy
                | SoftKey::ForwardNoAnswer
        ) =>
        {
            handle_feature_soft_key(access, device_id, call_id, line_instance.get(), soft_key)
                .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::Park,
        } => {
            handle_park_request(access, device_id, call_id, line_instance.get(), None).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: soft_key @ (SoftKey::Pickup | SoftKey::GroupPickup),
        } => {
            handle_pickup_soft_key(
                access,
                device_id,
                call_id,
                line_instance.get(),
                soft_key == SoftKey::Pickup,
            )
            .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::Conference,
        } => {
            handle_conference_soft_key(access, device_id, call_id, line_instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::MeetMe,
        } => {
            handle_conference_destination(access, device_id, call_id, line_instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::ConferenceList,
            ..
        } => {
            show_conference_list(access, device_id, call_id).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::Barge,
        } => {
            handle_barge_soft_key(
                access,
                device_id,
                call_id,
                line_instance.get(),
                BargeMode::Directed,
            )
            .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::Join,
        } => {
            handle_join_soft_key(access, device_id, call_id, line_instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Select,
            ..
        } => {
            let selected = controller_step(&access.shared.controller, |controller| {
                controller.toggle_call_selected(&device_id, call_id)
            });
            if let Some(selected) = selected {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::SetCallSelected { call_id, selected },
                    ))
                    .await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Backspace,
            ..
        } if forwarding_entry_exists(access, &device_id, call_id) => {
            handle_forwarding_backspace(access, &device_id, call_id);
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Dial,
            ..
        } if forwarding_entry_exists(access, &device_id, call_id) => {
            commit_forwarding_entry(access, &device_id, call_id).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::EndCall,
            ..
        } => {
            if cancel_forwarding_entry_for_call(access, &device_id, call_id) {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::CloseCall { call_id },
                    ))
                    .await;
            } else if !handle_transfer_hangup(access, device_id, call_id, false).await {
                handle_handset_hangup(access, call_id, false).await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Hold,
            ..
        } => {
            handle_hold_or_resume(access, call_id, true, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Resume,
            ..
        } => {
            handle_hold_or_resume(access, call_id, false, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id,
            line_instance,
            soft_key: SoftKey::Transfer,
        } => {
            handle_transfer_soft_key(access, device_id, call_id, line_instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(_),
            soft_key: SoftKey::DirectTransfer,
            ..
        } => {
            handle_direct_transfer(access, device_id).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Callback,
            ..
        } => {
            let owner = controller_step(&access.shared.controller, |controller| {
                controller
                    .call(call_id)
                    .map(|call| (call.device_id.clone(), call.sccp_id, call.pbx_id))
            });
            let result = owner
                .as_ref()
                .and_then(|(owner_device, owner_call_id, pbx_id)| {
                    with_channel(access, *pbx_id, |channel| {
                        let channel = unsafe { AsteriskChannel::from_raw(channel.cast()) }
                            .map_err(|_| CallCompletionError::Unavailable)?;
                        AsteriskCallCompletion::new().request_owned(
                            CallCompletionOwnership {
                                requested_device: device_id.as_str(),
                                requested_call_id: call_id.0,
                                owner_device: Some(owner_device.as_str()),
                                owner_call_id: Some(owner_call_id.0),
                            },
                            Some(&channel),
                        )
                    })
                })
                .unwrap_or(Err(CallCompletionError::Unavailable));
            let text = match result {
                Ok(ticket) => {
                    ast_log(
                        LogLevel::Debug,
                        &format!(
                            "accepted call-completion core {} for handset call {}",
                            ticket.core_id, call_id.0
                        ),
                    );
                    "Callback requested"
                }
                Err(error) => error.handset_prompt(),
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
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Monitor,
            ..
        } => {
            if let Err(error) =
                toggle_monitor_recording(access, recordings, &device_id, call_id).await
            {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to change SCCP recording state: {error}"),
                );
                let _ = access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 4,
                            text: "Recording unavailable".into(),
                        },
                    ))
                    .await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::VideoMode,
            ..
        } => controller_step(&access.shared.controller, |controller| {
            controller.video_mode_for_device(&device_id, call_id)
        }),
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key,
            ..
        } => controller_step(&access.shared.controller, |controller| match soft_key {
            SoftKey::Answer => controller.phone_answer(call_id),
            SoftKey::Intercept => controller.steal(call_id),
            SoftKey::Dial => controller.enbloc(call_id, String::new()),
            _ => Vec::new(),
        }),
        PhoneDeviceEventKind::LineButton {
            call_id: Some(call_id),
            ..
        } => {
            let state = controller_step(&access.shared.controller, |controller| {
                controller.call_state(call_id)
            });
            if matches!(state, Some(CallState::Held | CallState::SharedHeld)) {
                let transition = controller_step(&access.shared.controller, |controller| {
                    controller.begin_active_call_switch_transaction(&device_id, call_id)
                });
                if let Ok(transition) = transition {
                    execute_call_transition(access, transition).await;
                }
                Vec::new()
            } else {
                match state {
                    Some(CallState::Ringing | CallState::Connected) => {
                        let transition = controller_step(&access.shared.controller, |controller| {
                            controller.begin_active_call_switch_transaction(&device_id, call_id)
                        });
                        if let Ok(transition) = transition {
                            if state == Some(CallState::Ringing) {
                                execute_answer_call_transition(access, transition).await;
                            } else {
                                execute_call_transition(access, transition).await;
                            }
                        }
                        Vec::new()
                    }
                    Some(CallState::RemoteInUse) => {
                        controller_step(&access.shared.controller, |controller| {
                            controller.steal(call_id)
                        })
                    }
                    _ => Vec::new(),
                }
            }
        }
        PhoneDeviceEventKind::HookFlash {
            call_id: Some(call_id),
            line_instance,
        } => {
            let action = controller_step(&access.shared.controller, |controller| {
                controller.hook_flash_action(&device_id, call_id)
            });
            match action {
                HookFlashAction::AnswerWaiting(waiting_call_id) => {
                    let transition = controller_step(&access.shared.controller, |controller| {
                        controller.begin_active_call_switch_transaction(&device_id, waiting_call_id)
                    });
                    if let Ok(transition) = transition {
                        execute_answer_call_transition(access, transition).await;
                    }
                    Vec::new()
                }
                HookFlashAction::Transfer => {
                    handle_transfer_soft_key(access, device_id, Some(call_id), line_instance.get())
                        .await;
                    Vec::new()
                }
                HookFlashAction::Ignore => Vec::new(),
            }
        }
        PhoneDeviceEventKind::HookFlash { .. } => Vec::new(),
        PhoneDeviceEventKind::ReceiveChannelOpened {
            call_id,
            status: MediaStatus::Ok,
            mut endpoint,
        } => match normalize_phone_media_endpoint(access, &device_id, &mut endpoint) {
            Ok(()) => {
                let codec_id = endpoint.codec.wire_value();
                let packet_ms = endpoint.packet_ms;
                let (actions, accepted) =
                    controller_step(&access.shared.controller, |controller| {
                        let actions =
                            controller.media_opened_for_device(&device_id, call_id, endpoint);
                        let accepted = controller.call(call_id).is_some_and(|call| {
                            call.device_id == device_id
                                && call.audio == MediaStreamState::Open(endpoint)
                        });
                        (actions, accepted)
                    });
                execute_effects(access, actions).await;
                if accepted {
                    publish_ami_event(
                        access,
                        &media_event(
                            &device_id,
                            call_id,
                            AmiMediaKind::Audio,
                            AmiMediaDirection::Receive,
                            AmiMediaState::Open,
                            MediaStatus::Ok,
                            Some(codec_id),
                            Some(packet_ms),
                        ),
                    );
                }
                Vec::new()
            }
            Err(error) => {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "phone reported an unusable media endpoint for call {call_id:?}: {error}"
                    ),
                );
                handle_handset_hangup(access, call_id, false).await;
                Vec::new()
            }
        },
        PhoneDeviceEventKind::ReceiveChannelOpened {
            call_id, status, ..
        } => {
            ast_log(
                LogLevel::Warning,
                &format!("phone failed to open media for call {call_id:?}: {status:?}"),
            );
            handle_handset_hangup(access, call_id, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::MultimediaReceiveChannelOpened {
            call_id,
            codec,
            mut endpoint,
            passthrough_party_id: _,
        } => {
            let pbx_id = owned_pbx_call(access, &device_id, call_id);
            let normalized = normalize_phone_video_endpoint(access, &device_id, &mut endpoint);
            let accepted = normalized.is_ok()
                && controller_step(&access.shared.controller, |controller| {
                    controller.video_receive_opened_for_device(
                        &device_id,
                        session_generation,
                        call_id,
                        codec,
                        endpoint,
                    )
                });
            let configured = accepted
                && pbx_id.is_some_and(|pbx_id| {
                    set_remote_video_endpoint(access, pbx_id, endpoint).is_ok()
                });
            if configured {
                if let Some(pbx_id) = pbx_id {
                    let queued = with_channel(access, pbx_id, |channel| unsafe {
                        native_channel::queue_control(
                            NonNull::new_unchecked(channel),
                            native_channel::ChannelControl::VideoUpdate,
                        )
                    });
                    if !matches!(queued, Some(Ok(()))) {
                        ast_log(
                            LogLevel::Warning,
                            &format!("unable to request a fresh video frame for call {call_id:?}"),
                        );
                    }
                }
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Video,
                        AmiMediaDirection::Receive,
                        AmiMediaState::Open,
                        MediaStatus::Ok,
                        Some(codec.wire_value()),
                        None,
                    ),
                );
            } else {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to configure video receive endpoint for call {call_id:?}"),
                );
            }
            if configured {
                controller_step(&access.shared.controller, |controller| {
                    controller.begin_video_transmit_for_device(
                        &device_id,
                        session_generation,
                        call_id,
                    )
                })
            } else {
                controller_step(&access.shared.controller, |controller| {
                    controller
                        .video_fallback_for_device(
                            &device_id,
                            session_generation,
                            call_id,
                            VideoFallbackReason::ReceiveFailed,
                        )
                        .into_effects()
                })
            }
        }
        PhoneDeviceEventKind::MultimediaReceiveChannelFailed {
            call_id,
            codec,
            status,
            endpoint: _,
            passthrough_party_id: _,
        } => {
            let fallback = controller_step(&access.shared.controller, |controller| {
                controller.video_fallback_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    VideoFallbackReason::ReceiveFailed,
                )
            });
            let accepted = fallback.is_applied();
            let actions = fallback.into_effects();
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Video,
                        AmiMediaDirection::Receive,
                        AmiMediaState::Failed,
                        status,
                        Some(codec.wire_value()),
                        None,
                    ),
                );
            }
            actions
        }
        PhoneDeviceEventKind::MultimediaReceiveChannelTimedOut {
            call_id,
            codec,
            passthrough_party_id: _,
        } => {
            let fallback = controller_step(&access.shared.controller, |controller| {
                controller.video_fallback_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    VideoFallbackReason::ReceiveFailed,
                )
            });
            let accepted = fallback.is_applied();
            let actions = fallback.into_effects();
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "phone did not acknowledge video receive setup for call {call_id:?} with codec {codec:?}"
                    ),
                );
            }
            actions
        }
        PhoneDeviceEventKind::MultimediaTransmitStarted {
            call_id,
            codec,
            endpoint,
            passthrough_party_id,
        } => {
            let accepted = controller_step(&access.shared.controller, |controller| {
                controller.video_transmit_opened_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    codec,
                    endpoint,
                    passthrough_party_id,
                )
            });
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Video,
                        AmiMediaDirection::Transmit,
                        AmiMediaState::Open,
                        MediaStatus::Ok,
                        Some(codec.wire_value()),
                        None,
                    ),
                );
            } else {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "ignored unexpected video transmit acknowledgement for call {call_id:?}"
                    ),
                );
            }
            Vec::new()
        }
        PhoneDeviceEventKind::MultimediaTransmitFailed {
            call_id,
            codec,
            status,
            endpoint: _,
            passthrough_party_id: _,
        } => {
            let fallback = controller_step(&access.shared.controller, |controller| {
                controller.video_fallback_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    VideoFallbackReason::TransmitFailed,
                )
            });
            let accepted = fallback.is_applied();
            let actions = fallback.into_effects();
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Video,
                        AmiMediaDirection::Transmit,
                        AmiMediaState::Failed,
                        status,
                        Some(codec.wire_value()),
                        None,
                    ),
                );
            }
            actions
        }
        PhoneDeviceEventKind::MultimediaTransmitTimedOut {
            call_id,
            codec,
            passthrough_party_id: _,
        } => {
            let fallback = controller_step(&access.shared.controller, |controller| {
                controller.video_fallback_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    VideoFallbackReason::TransmitFailed,
                )
            });
            let accepted = fallback.is_applied();
            let actions = fallback.into_effects();
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "phone did not acknowledge video transmit setup for call {call_id:?} with codec {codec:?}"
                    ),
                );
            }
            actions
        }
        PhoneDeviceEventKind::TransmitChannelImplied { call_id, endpoint } => {
            let codec_id = endpoint.codec.wire_value();
            let packet_ms = endpoint.packet_ms;
            let accepted = controller_step(&access.shared.controller, |controller| {
                controller.media_transmission_started_for_device(&device_id, call_id, endpoint);
                controller.call(call_id).is_some_and(|call| {
                    call.device_id == device_id
                        && call.audio_transmit == MediaStreamState::Open(endpoint)
                })
            });
            if accepted {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Audio,
                        AmiMediaDirection::Transmit,
                        AmiMediaState::Open,
                        MediaStatus::Ok,
                        Some(codec_id),
                        Some(packet_ms),
                    ),
                );
            }
            Vec::new()
        }
        PhoneDeviceEventKind::TransmitChannelStarted {
            call_id,
            status: MediaStatus::Ok,
            mut endpoint,
        } => match normalize_phone_media_endpoint(access, &device_id, &mut endpoint) {
            Ok(()) => {
                let codec_id = endpoint.codec.wire_value();
                let packet_ms = endpoint.packet_ms;
                let (actions, accepted) =
                    controller_step(&access.shared.controller, |controller| {
                        let actions = controller
                            .media_transmission_started_for_device(&device_id, call_id, endpoint);
                        let accepted = controller.call(call_id).is_some_and(|call| {
                            call.device_id == device_id
                                && call.audio_transmit == MediaStreamState::Open(endpoint)
                        });
                        (actions, accepted)
                    });
                execute_effects(access, actions).await;
                if accepted {
                    publish_ami_event(
                        access,
                        &media_event(
                            &device_id,
                            call_id,
                            AmiMediaKind::Audio,
                            AmiMediaDirection::Transmit,
                            AmiMediaState::Open,
                            MediaStatus::Ok,
                            Some(codec_id),
                            Some(packet_ms),
                        ),
                    );
                }
                Vec::new()
            }
            Err(error) => {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "phone reported an unusable transmit endpoint for call {call_id:?}: {error}"
                    ),
                );
                handle_handset_hangup(access, call_id, false).await;
                Vec::new()
            }
        },
        PhoneDeviceEventKind::TransmitChannelStarted {
            call_id, status, ..
        } => {
            ast_log(
                LogLevel::Warning,
                &format!("phone failed to start media for call {call_id:?}: {status:?}"),
            );
            handle_handset_hangup(access, call_id, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::HandsetAcknowledgementTimedOut {
            call_id,
            acknowledgement,
            ..
        } => {
            // A coupled 79x1 transaction is settled atomically by the
            // protocol session's TransmitChannelImplied event. Any timeout
            // reaching this layer therefore belongs to a still-unsettled
            // required acknowledgement and is call-fatal.
            let operation = match acknowledgement {
                HandsetAcknowledgement::OpenReceiveChannel => "open receive channel",
                HandsetAcknowledgement::StartMediaTransmission => "start media transmission",
            };
            ast_log(
                LogLevel::Warning,
                &format!("phone did not acknowledge {operation} for call {call_id:?}"),
            );
            handle_handset_hangup(access, call_id, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::MediaTransmissionFailed {
            call_id,
            status,
            endpoint,
        } => {
            let disposition =
                recover_failed_media_transmission(access, &device_id, call_id, endpoint);
            if disposition != MediaFailureDisposition::Ignored {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Audio,
                        AmiMediaDirection::Transmit,
                        AmiMediaState::Failed,
                        status,
                        Some(endpoint.codec.wire_value()),
                        Some(endpoint.packet_ms),
                    ),
                );
            }
            match disposition {
                MediaFailureDisposition::Retrying => ast_log(
                    LogLevel::Notice,
                    &format!(
                        "retrying failed direct media through the local RTP anchor for call {call_id:?}"
                    ),
                ),
                MediaFailureDisposition::Hangup => {
                    ast_log(
                        LogLevel::Warning,
                        &format!(
                            "media transmission failed without an available recovery path for call {call_id:?}: {status:?}"
                        ),
                    );
                    handle_handset_hangup(access, call_id, false).await;
                }
                MediaFailureDisposition::Ignored => {}
            }
            Vec::new()
        }
        PhoneDeviceEventKind::MulticastReceptionStarted {
            conference_id: _,
            call_id: _,
            route: _,
        }
        | PhoneDeviceEventKind::MulticastReceptionFailed {
            conference_id: _,
            call_id: _,
            status: _,
        }
        | PhoneDeviceEventKind::MulticastReceptionTimedOut {
            conference_id: _,
            call_id: _,
        }
        | PhoneDeviceEventKind::MulticastTransmissionStarted {
            conference_id: _,
            call_id: _,
            route: _,
        }
        | PhoneDeviceEventKind::MulticastTransmissionFailed {
            conference_id: _,
            call_id: _,
            status: _,
            address: _,
            port: _,
        } => Vec::new(),
        PhoneDeviceEventKind::ConnectionStatisticsCollected { .. } => Vec::new(),
        PhoneDeviceEventKind::Alarm { severity, text, .. } => {
            ast_log(
                LogLevel::Warning,
                &format!(
                    "SCCP alarm from {device_id} ({severity:?}, {} bytes)",
                    text.len()
                ),
            );
            publish_ami_event(access, &alarm_event(&device_id, severity));
            Vec::new()
        }
        PhoneDeviceEventKind::XmlAlarm { telemetry } => {
            if let Some(summary) = telemetry.summary() {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "typed phone alarm from {device_id} ({:?}, reason {:?})",
                        summary.kind, summary.reason_for_out_of_service
                    ),
                );
                publish_ami_event(access, &xml_alarm_event(&device_id, summary));
            } else if let PhoneAlarmTelemetry::Opaque(alarm) = telemetry {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "opaque phone alarm from {device_id} ({} bytes)",
                        alarm.as_bytes().len()
                    ),
                );
            }
            Vec::new()
        }
        PhoneDeviceEventKind::LocationInformation { telemetry } => {
            if let Some(summary) = telemetry.summary() {
                ast_log(
                    LogLevel::Debug,
                    &format!(
                        "typed phone location information from {device_id} ({:?}, off-premises {})",
                        summary.kind, summary.off_premises
                    ),
                );
            } else if let PhoneLocationTelemetry::Opaque(location) = telemetry {
                ast_log(
                    LogLevel::Debug,
                    &format!(
                        "opaque phone location information from {device_id} ({} bytes)",
                        location.as_bytes().len()
                    ),
                );
            }
            Vec::new()
        }
        PhoneDeviceEventKind::HeadsetStatusChanged { .. }
        | PhoneDeviceEventKind::MediaPathChanged { .. } => Vec::new(),
        PhoneDeviceEventKind::UnhandledMessage { message } => {
            ast_log(
                LogLevel::Debug,
                &format!("unhandled SCCP message from {device_id}: {message:?}"),
            );
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey { .. } | PhoneDeviceEventKind::LineButton { .. } => {
            Vec::new()
        }
    };
    execute_effects(access, actions).await;
}

pub fn configured_mobility_button(config: &ModuleConfig, slot: &MobilitySlot) -> bool {
    config.devices.get(&slot.device_id).is_some_and(|device| {
        device.buttons.iter().any(|button| {
            matches!(
                button,
                ButtonDefinition::Feature(feature)
                    if feature.instance == slot.button_instance
                        && feature.feature == ButtonType::Mobility
            )
        })
    })
}

pub(super) fn reserve_mobility_prompt(
    access: &Access,
    slot: MobilitySlot,
) -> Option<TransactionId> {
    let mut prompts = access.shared.pending_mobility_prompts.lock_unpoisoned();
    prompts.retain(|_, pending_slot| pending_slot != &slot);
    for _ in 0..=prompts.len() {
        let raw = access
            .shared
            .next_mobility_prompt_id
            .fetch_add(1, Ordering::Relaxed) as u32;
        if raw == 0 {
            continue;
        }
        let transaction_id = TransactionId::new(raw);
        let key = (slot.device_id.clone(), transaction_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = prompts.entry(key) {
            entry.insert(slot);
            return Some(transaction_id);
        }
    }
    None
}

pub(super) async fn mobility_status(access: &Access, device_id: DeviceId, text: &'static str) {
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::SetStatusMessage {
                message: HandsetStatusMessage::Display {
                    text: text.into(),
                    timeout_seconds: 4,
                    priority: None,
                },
                beep: false,
            },
        ))
        .await;
}

pub(super) async fn handle_mobility_button(access: &Access, device_id: DeviceId, instance: u32) {
    let _mobility_guard = access.shared.mobility_mutations.lock().await;
    let Ok(slot) = MobilitySlot::new(device_id.clone(), instance) else {
        return;
    };
    if !configured_mobility_button(&access.config(), &slot) {
        return;
    }
    let logout = access
        .shared
        .mobility
        .lock_unpoisoned()
        .appearance_for_slot(&slot)
        .is_some();
    if logout {
        let prepared = access
            .shared
            .mobility
            .lock_unpoisoned()
            .prepare_logout(&slot);
        if let Ok(prepared) = prepared {
            if mobility_appearance_has_calls(access, prepared.previous()) {
                let _ = access.shared.mobility.lock_unpoisoned().abort(&prepared);
                mobility_status(access, device_id, "Mobility line is in use").await;
            } else if apply_mobility_transaction(access, &prepared).await {
                mobility_status(access, device_id, "Mobility logout complete").await;
            } else {
                mobility_status(access, device_id, "Mobility logout failed").await;
            }
        }
        return;
    }

    let Some(transaction_id) = reserve_mobility_prompt(access, slot.clone()) else {
        mobility_status(access, device_id, "Mobility unavailable").await;
        return;
    };
    let document = match mobility_login_document(slot.button_instance) {
        Ok(document) => document,
        Err(_) => {
            access
                .shared
                .pending_mobility_prompts
                .lock_unpoisoned()
                .remove(&(device_id.clone(), transaction_id));
            mobility_status(access, device_id, "Mobility unavailable").await;
            return;
        }
    };
    if access
        .phone
        .send_confirmed(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::ShowInputService {
                line_instance: LineInstance::new(0),
                call_reference: CallReference::new(0),
                application_id: ApplicationId::new(MOBILITY_APPLICATION_ID),
                transaction_id,
                priority: PhoneServicePriority::NORMAL,
                document,
            },
        ))
        .await
        .is_err()
    {
        access
            .shared
            .pending_mobility_prompts
            .lock_unpoisoned()
            .remove(&(device_id, transaction_id));
    }
}

pub(super) async fn handle_mobility_response(
    access: &Access,
    device_id: DeviceId,
    response: PhoneServiceEvent,
) {
    let _mobility_guard = access.shared.mobility_mutations.lock().await;
    if response.routing.application_id != ApplicationId::new(MOBILITY_APPLICATION_ID) {
        return;
    }
    let slot = access
        .shared
        .pending_mobility_prompts
        .lock_unpoisoned()
        .remove(&(device_id.clone(), response.routing.transaction_id));
    let Some(slot) = slot else {
        return;
    };
    if response.routing.line_instance != LineInstance::new(0)
        || response.routing.call_reference != CallReference::new(0)
    {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    }
    let PhoneServicePayload::Submission(submission) = response.payload else {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    };
    let Ok(request) = parse_mobility_login_submission(slot.button_instance, &submission) else {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    };
    let config = access.config();
    if !configured_mobility_button(&config, &slot)
        || config
            .appearances_for_device(&device_id)
            .any(|binding| binding.line.number == request.line_number())
    {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    }
    let Ok(line) = authenticate_line(&config, request.line_number(), request.credential()) else {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    };
    let configured_instances = config
        .appearances_for_device(&device_id)
        .map(|binding| binding.line_instance)
        .collect::<Vec<_>>();
    drop(config);
    let prepared =
        access
            .shared
            .mobility
            .lock_unpoisoned()
            .prepare_login(slot, line, configured_instances);
    match prepared {
        Ok(MobilityPreparation::Unchanged(_)) => {
            mobility_status(access, device_id, "Mobility already active").await;
        }
        Ok(MobilityPreparation::Transaction(prepared)) => {
            if mobility_appearance_has_calls(access, prepared.previous()) {
                let _ = access.shared.mobility.lock_unpoisoned().abort(&prepared);
                mobility_status(access, device_id, "Mobility line is in use").await;
            } else if apply_mobility_transaction(access, &prepared).await {
                mobility_status(access, device_id, "Mobility login complete").await;
            } else {
                mobility_status(access, device_id, "Mobility login failed").await;
            }
        }
        Err(_) => mobility_status(access, device_id, "Mobility login rejected").await,
    }
}

pub(super) fn mobility_appearance_has_calls(
    access: &Access,
    appearance: Option<&crate::call::mobility::RoamingAppearance>,
) -> bool {
    appearance.is_some_and(|appearance| {
        controller_step(&access.shared.controller, |controller| {
            controller.calls().any(|call| {
                call.device_id == appearance.slot.device_id
                    && call.line_instance == appearance.binding.line_instance
            })
        })
    })
}

pub fn mobility_device_registered(access: &Access, device_id: &DeviceId) -> bool {
    controller_step(&access.shared.controller, |controller| {
        controller.is_registered(device_id)
    })
}

pub(super) struct RuntimeMobilityWriter<'a> {
    access: &'a Access,
}

impl MobilityAppearanceWriter for RuntimeMobilityWriter<'_> {
    type Error = ();

    fn write<'a>(
        &'a mut self,
        appearance: &'a crate::call::mobility::RoamingAppearance,
        install: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send + 'a>>
    {
        Box::pin(async move {
            if !mobility_device_registered(self.access, &appearance.slot.device_id) {
                return if install { Err(()) } else { Ok(()) };
            }
            self.access
                .phone
                .send_confirmed(PhoneCommand::new(
                    appearance.slot.device_id.clone(),
                    PhoneCommandAction::SetMobilityAppearance {
                        mobility_instance: LineInstance::new(appearance.slot.button_instance),
                        appearance: install.then(|| appearance.binding.appearance.clone()),
                    },
                ))
                .await
                .map_err(|_| ())
        })
    }
}

pub(super) async fn apply_mobility_transaction(
    access: &Access,
    transaction: &PreparedMobilityTransaction,
) -> bool {
    let mut writer = RuntimeMobilityWriter { access };
    if execute_mobility_io(&mut writer, transaction).await.is_err() {
        let _ = access.shared.mobility.lock_unpoisoned().abort(transaction);
        return false;
    }
    let committed = access
        .shared
        .mobility
        .lock_unpoisoned()
        .commit(transaction)
        .is_ok();
    if !committed {
        let _ = rollback_mobility_io(&mut writer, transaction).await;
        let _ = access.shared.mobility.lock_unpoisoned().abort(transaction);
    }
    committed
}

pub(super) async fn restore_mobility_appearances(access: &Access, device_id: &DeviceId) {
    let appearances = access
        .shared
        .mobility
        .lock_unpoisoned()
        .appearances_for_device(device_id)
        .cloned()
        .collect::<Vec<_>>();
    for appearance in appearances {
        let mut writer = RuntimeMobilityWriter { access };
        if writer.write(&appearance, true).await.is_err() {
            ast_log(
                LogLevel::Warning,
                "unable to restore a roaming mobility appearance after registration",
            );
        }
    }
}

pub(super) async fn handle_transfer_soft_key(
    access: &Access,
    device_id: DeviceId,
    reported_call_id: Option<CallId>,
    line_instance: u32,
) {
    let existing = controller_step(&access.shared.controller, |controller| {
        controller
            .transfer_transaction_for_device(&device_id)
            .cloned()
    });
    if let Some(existing) = existing {
        let feedback_call_id = reported_call_id
            .filter(|call_id| call_id.0 != 0)
            .or_else(|| existing.consultation.map(|leg| leg.handset_call_id))
            .unwrap_or(existing.source.handset_call_id);
        let plan = controller_step(&access.shared.controller, |controller| {
            controller.complete_device_transfer(
                &device_id,
                reported_call_id,
                TransferTrigger::TransferKey,
            )
        });
        match plan {
            Ok(plan) => execute_transfer_completion(access, plan).await,
            Err(rejection) => {
                show_transfer_rejection(access, device_id, feedback_call_id, rejection).await
            }
        }
        return;
    }

    let Some(call_id) = reported_call_id.filter(|call_id| call_id.0 != 0) else {
        ast_log(
            LogLevel::Warning,
            &format!("transfer request for device {device_id} did not identify a source call"),
        );
        return;
    };

    let config = access.config();
    let binding = access.line_binding(&device_id, line_instance);
    let complete_on_hangup = config.general.transfer_on_hangup;
    drop(config);
    let codec = preferred_codec(access, &device_id, line_instance, &PbxAudioFormat::ALL);
    let Some((binding, codec)) = binding.zip(codec) else {
        return;
    };
    let consultation_call_id = access.phone.reserve_call_id();
    let result = controller_step(&access.shared.controller, |controller| {
        let effects = controller.begin_transfer(TransferConsultationRequest {
            source_call_id: call_id,
            consultation_call_id,
            binding,
            codec,
            complete_on_hangup,
            now: Instant::now(),
        });
        let transaction = controller
            .transfer_transaction(consultation_call_id)
            .cloned();
        (effects, transaction)
    });
    let (effects, transaction) = result;
    let effects = match effects {
        Ok(effects) => effects,
        Err(rejection) => {
            show_transfer_rejection(access, device_id, call_id, rejection).await;
            return;
        }
    };
    let Some(transaction) = transaction else {
        show_transfer_rejection(access, device_id, call_id, TransferRejection::Conflict).await;
        return;
    };
    execute_transfer_start(access, transaction, effects).await;
}

async fn show_transfer_rejection(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    rejection: TransferRejection,
) {
    ast_log(
        LogLevel::Warning,
        &format!(
            "transfer request rejected for device {device_id} call {}: {rejection:?}",
            call_id.0
        ),
    );
    let text = if rejection == TransferRejection::CompletionInProgress {
        "Transfer in progress"
    } else {
        "Can Not Complete Transfer"
    };
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::StartTone {
                call_id,
                tone: Tone::BeepBonk,
            },
        ))
        .await;
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

pub(super) async fn execute_transfer_start(
    access: &Access,
    transaction: crate::call::transfer::TransferTransaction,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if !transfer_generation_is_active(access, &transaction) {
            return;
        }
        let milestone = if matches!(
            &effect,
            DriverEffect::Backend(PbxEffect::Hold { call_id })
                if *call_id == transaction.source.pbx_call_id
        ) {
            Some(TransferSetupMilestone::SourceBackendHeld)
        } else if matches!(
            &effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id,
                state: PhoneCallState::Hold,
                ..
            }) if *call_id == transaction.source.handset_call_id
        ) {
            Some(TransferSetupMilestone::SourceHandsetHeld)
        } else if matches!(
            &effect,
            DriverEffect::Backend(PbxEffect::CreateConsultationChannel { call_id, .. })
                if transaction.consultation.is_some_and(|leg| leg.pbx_call_id == *call_id)
        ) {
            Some(TransferSetupMilestone::ConsultationChannelCreated)
        } else if matches!(
            &effect,
            DriverEffect::Handset(HandsetEffect::BeginTransfer {
                consultation_call_id,
                ..
            }) if transaction.consultation.is_some_and(|leg| {
                leg.handset_call_id == *consultation_call_id
            })
        ) {
            Some(TransferSetupMilestone::ConsultationHandsetStarted)
        } else {
            None
        };
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("transfer consultation setup failed: {error}"),
            );
            if let Some(milestone) = milestone {
                compensate_unrecorded_transfer_setup(access, &backend, &transaction, milestone)
                    .await;
            }
            abort_transfer_execution(access, &transaction).await;
            return;
        }
        if let Some(milestone) = milestone
            && !record_transfer_setup_milestone(access, &transaction, milestone)
        {
            compensate_unrecorded_transfer_setup(access, &backend, &transaction, milestone).await;
            return;
        }
        if milestone.is_none() && !transfer_generation_is_active(access, &transaction) {
            close_stale_transfer_consultation(access, &transaction).await;
            return;
        }
    }
}

pub(super) fn transfer_generation_is_active(
    access: &Access,
    transaction: &crate::call::transfer::TransferTransaction,
) -> bool {
    controller_step(&access.shared.controller, |controller| {
        controller.transfer_generation_is_active(&transaction.device_id, transaction.id)
    })
}

pub(super) fn record_transfer_setup_milestone(
    access: &Access,
    transaction: &crate::call::transfer::TransferTransaction,
    milestone: TransferSetupMilestone,
) -> bool {
    controller_step(&access.shared.controller, |controller| {
        controller
            .transfer_setup_completed(&transaction.device_id, transaction.id, milestone)
            .is_ok()
    })
}

pub(super) async fn compensate_unrecorded_transfer_setup(
    access: &Access,
    backend: &AsteriskBackend<'_>,
    transaction: &crate::call::transfer::TransferTransaction,
    milestone: TransferSetupMilestone,
) {
    match milestone {
        TransferSetupMilestone::SourceBackendHeld => {
            let _ = backend.resume(transaction.source.pbx_call_id);
        }
        TransferSetupMilestone::SourceHandsetHeld => {
            let _ = execute_handset_effect(
                access,
                HandsetEffect::SetCallState {
                    device_id: transaction.device_id.clone(),
                    call_id: transaction.source.handset_call_id,
                    state: PhoneCallState::Connected,
                    stop_media: false,
                },
            )
            .await;
        }
        TransferSetupMilestone::ConsultationChannelCreated => {
            if let Some(consultation) = transaction.consultation {
                let _ = backend.hangup(consultation.pbx_call_id);
                remove_channel(access, consultation.pbx_call_id);
            }
        }
        TransferSetupMilestone::ConsultationHandsetStarted => {
            if let Some(consultation) = transaction.consultation {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        transaction.device_id.clone(),
                        PhoneCommandAction::CloseCall {
                            call_id: consultation.handset_call_id,
                        },
                    ))
                    .await;
            }
        }
    }
}

pub(super) async fn close_stale_transfer_consultation(
    access: &Access,
    transaction: &crate::call::transfer::TransferTransaction,
) {
    if let Some(consultation) = transaction.consultation {
        let _ = execute_handset_effect(
            access,
            HandsetEffect::SetCallState {
                device_id: transaction.device_id.clone(),
                call_id: consultation.handset_call_id,
                state: PhoneCallState::OnHook,
                stop_media: true,
            },
        )
        .await;
    }
}

pub(super) async fn abort_transfer_execution(
    access: &Access,
    transaction: &crate::call::transfer::TransferTransaction,
) {
    let outcome = controller_step(&access.shared.controller, |controller| {
        controller.abort_transfer(
            &transaction.device_id,
            transaction.id,
            TransferCancellationReason::ConsultationFailure,
        )
    });
    if let Ok(outcome) = outcome {
        let consultation_created = outcome
            .transaction
            .execution_progress
            .completed(crate::call::transfer::TransferSetupMilestone::ConsultationChannelCreated);
        execute_cleanup_effects(access, outcome.effects).await;
        if consultation_created && let Some(consultation) = transaction.consultation {
            remove_channel(access, consultation.pbx_call_id);
        }
    }
}

pub(super) async fn handle_direct_transfer(access: &Access, device_id: DeviceId) {
    let (plan, active_call) = controller_step(&access.shared.controller, |controller| {
        (
            controller.direct_transfer(&device_id),
            controller
                .registered_device(&device_id)
                .and_then(|device| device.active_call()),
        )
    });
    match plan {
        Ok(plan) => execute_transfer_completion(access, plan).await,
        Err(rejection) => {
            if let Some(call_id) = active_call {
                show_transfer_rejection(access, device_id, call_id, rejection).await;
            }
        }
    }
}

pub(super) async fn execute_transfer_completion(access: &Access, plan: TransferCompletionPlan) {
    let completion = plan.completion;
    if !controller_step(&access.shared.controller, |controller| {
        controller.transfer_generation_is_active(&completion.device_id, completion.transaction_id)
    }) {
        return;
    }
    debug_assert!(matches!(
        plan.effects.as_slice(),
        [DriverEffect::Backend(PbxEffect::Transfer { operation })] if operation == &completion
    ));
    let channels = retain_two_channels(
        access,
        completion.source.pbx_call_id,
        completion.consultation.pbx_call_id,
    );
    let _ = access
        .phone
        .send(PhoneCommand::new(
            completion.device_id.clone(),
            PhoneCommandAction::DisplayPrompt {
                call_id: completion.consultation.handset_call_id,
                timeout_seconds: 0,
                text: "Completing transfer".into(),
            },
        ))
        .await;
    ast_log(
        LogLevel::Notice,
        &format!(
            "starting {:?} transfer {} for device {} between PBX calls {} and {}",
            completion.kind,
            completion.transaction_id.0,
            completion.device_id,
            completion.source.pbx_call_id.0,
            completion.consultation.pbx_call_id.0,
        ),
    );

    let task_access = access.clone();
    access.handle.spawn(async move {
        let started = Instant::now();
        let result = if let Some((source, consultation)) = channels {
            let mut native = tokio::task::spawn_blocking(move || unsafe {
                native_channel::attended_transfer(
                    source.resource().as_non_null(),
                    consultation.resource().as_non_null(),
                )
            });
            tokio::select! {
                result = &mut native => result.unwrap_or(native_channel::AttendedTransferResult::Failed),
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    ast_log(
                        LogLevel::Warning,
                        &format!(
                            "transfer {} for device {} is still pending in Asterisk after 5 seconds",
                            completion.transaction_id.0,
                            completion.device_id,
                        ),
                    );
                    native.await.unwrap_or(native_channel::AttendedTransferResult::Failed)
                }
            }
        } else {
            native_channel::AttendedTransferResult::Invalid
        };
        finish_transfer_completion(&task_access, completion, result, started.elapsed()).await;
    });
}

async fn finish_transfer_completion(
    access: &Access,
    completion: TransferCompletion,
    result: native_channel::AttendedTransferResult,
    elapsed: Duration,
) {
    let active = controller_step(&access.shared.controller, |controller| {
        controller.transfer_generation_is_active(&completion.device_id, completion.transaction_id)
    });
    if !active {
        return;
    }
    ast_log(
        if result == native_channel::AttendedTransferResult::Success {
            LogLevel::Notice
        } else {
            LogLevel::Warning
        },
        &format!(
            "transfer {} for device {} completed as {result:?} after {} ms",
            completion.transaction_id.0,
            completion.device_id,
            elapsed.as_millis(),
        ),
    );

    if result == native_channel::AttendedTransferResult::Success {
        let outcome = controller_step(&access.shared.controller, |controller| {
            controller.transfer_succeeded(&completion.device_id, completion.transaction_id)
        });
        if let Some(outcome) = outcome {
            execute_cleanup_effects(access, outcome.effects).await;
        }
        remove_channel(access, completion.source.pbx_call_id);
        remove_channel(access, completion.consultation.pbx_call_id);
        return;
    }

    let outcome = controller_step(&access.shared.controller, |controller| {
        controller.abort_transfer(
            &completion.device_id,
            completion.transaction_id,
            TransferCancellationReason::BackendFailure,
        )
    });
    if let Ok(outcome) = outcome {
        let deferred = outcome.transaction.deferred_action;
        execute_cleanup_effects(access, outcome.effects).await;
        if completion.kind != TransferCompletionKind::Direct {
            remove_channel(access, completion.consultation.pbx_call_id);
        }
        show_transfer_rejection(
            access,
            completion.device_id.clone(),
            completion.source.handset_call_id,
            TransferRejection::Conflict,
        )
        .await;
        if deferred == Some(DeferredTransferAction::OnHook) {
            handle_handset_hangup(access, completion.source.handset_call_id, true).await;
        }
    }
}

pub(super) async fn cancel_transfer(
    access: &Access,
    transaction: crate::call::transfer::TransferTransaction,
    reason: TransferCancellationReason,
) -> bool {
    let outcome = controller_step(&access.shared.controller, |controller| {
        controller.abort_transfer(&transaction.device_id, transaction.id, reason)
    });
    let Ok(outcome) = outcome else {
        return false;
    };
    execute_cleanup_effects(access, outcome.effects).await;
    if transaction.mode == TransferMode::Consultation
        && let Some(consultation) = transaction.consultation
    {
        remove_channel(access, consultation.pbx_call_id);
    }
    true
}

pub(super) async fn handle_transfer_hangup(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    physical: bool,
) -> bool {
    let transaction = controller_step(&access.shared.controller, |controller| {
        controller.transfer_transaction(call_id).cloned()
    });
    let Some(transaction) = transaction else {
        return false;
    };
    if transaction.phase == TransferPhase::Completing {
        let action = if physical {
            DeferredTransferAction::OnHook
        } else {
            DeferredTransferAction::EndCall
        };
        let deferred = controller_step(&access.shared.controller, |controller| {
            controller.defer_transfer_action(&transaction.device_id, transaction.id, action)
        });
        if deferred.is_ok() {
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::DisplayPrompt {
                        call_id,
                        timeout_seconds: 4,
                        text: "Transfer in progress".into(),
                    },
                ))
                .await;
        }
        return true;
    }
    if physical
        && transaction
            .consultation
            .is_some_and(|leg| leg.handset_call_id == call_id)
    {
        let plan = controller_step(&access.shared.controller, |controller| {
            controller.complete_transfer(&device_id, call_id, TransferTrigger::ConsultationHangup)
        });
        if let Ok(plan) = plan {
            execute_transfer_completion(access, plan).await;
            return true;
        }
    }
    let source_hung_up = transaction.source.handset_call_id == call_id;
    let reason = if physical && source_hung_up {
        TransferCancellationReason::SourceHangup
    } else if physical {
        TransferCancellationReason::ConsultationHangup
    } else {
        TransferCancellationReason::EndCall
    };
    let cancelled = cancel_transfer(access, transaction, reason).await;
    if physical && source_hung_up {
        !cancelled
    } else {
        true
    }
}

pub(super) async fn handle_handset_hangup(
    access: &Access,
    call_id: CallId,
    physical_on_hook: bool,
) {
    let (conference_id, effects, surviving_conference) =
        controller_step(&access.shared.controller, |controller| {
            let conference_id = controller
                .conference_session(call_id)
                .map(|session| session.id);
            let effects = if physical_on_hook {
                controller.hangup(call_id)
            } else {
                controller.terminate(call_id)
            };
            let surviving = conference_id
                .and_then(|conference_id| controller.conference_session_by_id(conference_id))
                .cloned();
            (conference_id, effects, surviving)
        });
    if let Some(session) = surviving_conference {
        execute_cleanup_effects(access, effects).await;
        let show_list = access
            .config()
            .conference_for_device(&session.device_id)
            .is_some_and(|conference| conference.show_conference_list);
        if show_list {
            show_conference_list(access, session.device_id, session.original_handset_call_id).await;
        }
    } else if let Some(conference_id) = conference_id {
        execute_cleanup_effects(access, effects).await;
        cancel_conference_announcement(access, conference_id);
    } else {
        execute_effects(access, effects).await;
    }
}

pub async fn handle_hold_or_resume(
    access: &Access,
    call_id: CallId,
    held: bool,
    pbx_originated: bool,
) {
    if !held && !pbx_originated {
        let transaction = controller_step(&access.shared.controller, |controller| {
            controller.transfer_transaction(call_id).cloned()
        });
        if let Some(transaction) = transaction
            && transaction.source.handset_call_id == call_id
        {
            let _ = cancel_transfer(
                access,
                transaction,
                TransferCancellationReason::SourceResume,
            )
            .await;
            return;
        }
    }
    enum HoldPlan {
        Missing,
        Regular(Vec<DriverEffect>),
        Conference {
            device_id: DeviceId,
            result: Result<
                (
                    ConferenceId,
                    ParticipantId,
                    ConferenceMutationToken,
                    Vec<DriverEffect>,
                ),
                ConferenceParticipantRejection,
            >,
        },
    }

    let plan = controller_step(&access.shared.controller, |controller| {
        let Some(device_id) = controller.call_device_id(call_id).cloned() else {
            return HoldPlan::Missing;
        };
        if controller.conference_session(call_id).is_none() {
            return HoldPlan::Regular(if held {
                controller.hold(call_id)
            } else {
                controller.resume(call_id)
            });
        }
        let result = (|| {
            let effects = controller.begin_conference_moderator_leg_transition(call_id, held)?;
            let session = controller
                .conference_session(call_id)
                .ok_or(ConferenceParticipantRejection::Unavailable)?;
            let participant = session
                .participants
                .iter()
                .find(|participant| participant.handset_call_id == call_id)
                .ok_or(ConferenceParticipantRejection::InvalidParticipant)?;
            let conference_id = session.id;
            let participant_id = participant.id;
            let mutation = controller
                .claim_conference_mutation_by_id(conference_id)
                .ok_or(ConferenceParticipantRejection::Conflict)?;
            Ok((conference_id, participant_id, mutation, effects))
        })();
        HoldPlan::Conference { device_id, result }
    });
    let (device_id, conference_id, participant_id, mutation, effects) = match plan {
        HoldPlan::Missing => return,
        HoldPlan::Regular(effects) => {
            let effects = if pbx_originated {
                handset_effects(effects)
            } else {
                effects
            };
            execute_effects(access, effects).await;
            return;
        }
        HoldPlan::Conference {
            device_id,
            result: Ok((conference_id, participant_id, mutation, effects)),
        } => (device_id, conference_id, participant_id, mutation, effects),
        HoldPlan::Conference {
            device_id,
            result: Err(rejection),
        } => {
            let text = match rejection {
                ConferenceParticipantRejection::NotModerator => "Moderator action required",
                ConferenceParticipantRejection::Conflict => "Conference action in progress",
                ConferenceParticipantRejection::Unavailable
                | ConferenceParticipantRejection::InvalidParticipant
                | ConferenceParticipantRejection::Moderator
                | ConferenceParticipantRejection::LastModerator => "Conference unavailable",
            };
            display_conference_prompt(access, device_id, call_id, text).await;
            return;
        }
    };

    let backend = AsteriskBackend::new(access);
    let mut completed_music = Vec::new();
    let mut handset_attempted = false;
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            return;
        }
        let music_participant = match &effect {
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: BridgeOperation::SetParticipantMusicOnHold { participant_id, .. },
            }) => Some(*participant_id),
            _ => None,
        };
        if matches!(
            &effect,
            DriverEffect::Handset(
                HandsetEffect::SetCallState { .. }
                    | HandsetEffect::BeginMedia { .. }
                    | HandsetEffect::BeginAnswerMedia { .. }
            )
        ) {
            handset_attempted = true;
        }
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference moderator leg transition failed: {error}"),
            );
            let rollback = controller_step(&access.shared.controller, |controller| {
                if !controller.conference_mutation_is_active(mutation) {
                    return Vec::new();
                }
                let rollback = controller.abort_conference_moderator_leg_transition(
                    conference_id,
                    participant_id,
                    held,
                    &completed_music,
                    handset_attempted,
                );
                controller.complete_conference_mutation(mutation);
                rollback
            });
            execute_cleanup_effects(access, rollback).await;
            display_conference_prompt(
                access,
                device_id.clone(),
                call_id,
                if held {
                    "Unable to hold conference"
                } else {
                    "Unable to resume conference"
                },
            )
            .await;
            return;
        }
        if let Some(participant_id) = music_participant {
            completed_music.push(participant_id);
        }
        if !conference_mutation_is_active(access, mutation) {
            return;
        }
    }

    let (committed, rollback) = controller_step(&access.shared.controller, |controller| {
        if !controller.conference_mutation_is_active(mutation) {
            return (false, Vec::new());
        }
        let committed =
            controller.conference_moderator_leg_transitioned(conference_id, participant_id, held);
        let rollback = if committed {
            Vec::new()
        } else {
            controller.abort_conference_moderator_leg_transition(
                conference_id,
                participant_id,
                held,
                &completed_music,
                handset_attempted,
            )
        };
        controller.complete_conference_mutation(mutation);
        (committed, rollback)
    });
    if !committed {
        execute_cleanup_effects(access, rollback).await;
        return;
    }
    if !held {
        let session = controller_step(&access.shared.controller, |controller| {
            controller.conference_session_by_id(conference_id).cloned()
        });
        if let Some(session) = session
            && access
                .config()
                .conference_for_device(&session.device_id)
                .is_some_and(|conference| conference.show_conference_list)
        {
            show_conference_list(access, session.device_id, session.original_handset_call_id).await;
        }
    }
}
