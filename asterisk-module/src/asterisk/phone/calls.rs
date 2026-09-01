//! SCCP session events and core call-control flows.

use super::transfer::cancel_transfer;
use super::{
    Access, AsteriskBackend, BridgeOperation, CallId, ConferenceId, ConferenceMutationToken,
    ConferenceParticipantRejection, DeviceId, DriverEffect, HandsetEffect, LogLevel, ParticipantId,
    PbxCallId, PbxEffect, PhoneDeviceEventKind, PhoneEvent, RuntimeRecordings,
    TransferCancellationReason, ast_log, cancel_conference_announcement,
    conference_mutation_is_active, controller_step, display_conference_prompt,
    execute_cleanup_effects, execute_effects, execute_one_effect, handset_effects,
    show_conference_list,
};
mod call_control;
mod media_events;
mod session;
mod telemetry;

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
    let device_event = match event {
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
    if !matches!(&device_event.event, PhoneDeviceEventKind::Registered(_))
        && !controller_step(&access.shared.controller, |controller| {
            controller.session_is_current(&device_event.device_id, device_event.session_generation)
        })
    {
        return;
    }

    let actions = match phone_event_family(&device_event.event) {
        PhoneEventFamily::Session => {
            session::handle_session_event(access, recordings, device_event).await
        }
        PhoneEventFamily::CallControl => {
            call_control::handle_call_control_event(access, recordings, device_event).await
        }
        PhoneEventFamily::Media => media_events::handle_media_event(access, device_event).await,
        PhoneEventFamily::Telemetry => {
            telemetry::handle_telemetry_event(access, device_event).await
        }
    };
    execute_effects(access, actions).await;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhoneEventFamily {
    Session,
    CallControl,
    Media,
    Telemetry,
}

fn phone_event_family(event: &PhoneDeviceEventKind) -> PhoneEventFamily {
    match event {
        PhoneDeviceEventKind::Registered(_)
        | PhoneDeviceEventKind::Disconnected {}
        | PhoneDeviceEventKind::Capabilities { .. } => PhoneEventFamily::Session,

        PhoneDeviceEventKind::OffHook { .. }
        | PhoneDeviceEventKind::OnHook { .. }
        | PhoneDeviceEventKind::Digit { .. }
        | PhoneDeviceEventKind::EnblocCall { .. }
        | PhoneDeviceEventKind::SpeedDial { .. }
        | PhoneDeviceEventKind::SoftKey { .. }
        | PhoneDeviceEventKind::LineButton { .. }
        | PhoneDeviceEventKind::HookFlash { .. }
        | PhoneDeviceEventKind::FeatureButton { .. }
        | PhoneDeviceEventKind::DoNotDisturbButton { .. }
        | PhoneDeviceEventKind::RecordingButton { .. }
        | PhoneDeviceEventKind::MobilityButton { .. }
        | PhoneDeviceEventKind::VoicemailButton { .. }
        | PhoneDeviceEventKind::ParkingLotButton { .. }
        | PhoneDeviceEventKind::ParkingMenuSelection { .. }
        | PhoneDeviceEventKind::PhoneServiceResponse { .. }
        | PhoneDeviceEventKind::ConferenceListAction { .. } => PhoneEventFamily::CallControl,

        PhoneDeviceEventKind::ReceiveChannelOpened { .. }
        | PhoneDeviceEventKind::MultimediaReceiveChannelOpened { .. }
        | PhoneDeviceEventKind::MultimediaReceiveChannelFailed { .. }
        | PhoneDeviceEventKind::MultimediaReceiveChannelTimedOut { .. }
        | PhoneDeviceEventKind::MultimediaTransmitStarted { .. }
        | PhoneDeviceEventKind::MultimediaTransmitFailed { .. }
        | PhoneDeviceEventKind::MultimediaTransmitTimedOut { .. }
        | PhoneDeviceEventKind::TransmitChannelOpen { .. }
        | PhoneDeviceEventKind::HandsetAcknowledgementTimedOut { .. }
        | PhoneDeviceEventKind::MediaTransmissionFailed { .. }
        | PhoneDeviceEventKind::MulticastReceptionStarted { .. }
        | PhoneDeviceEventKind::MulticastReceptionFailed { .. }
        | PhoneDeviceEventKind::MulticastReceptionTimedOut { .. }
        | PhoneDeviceEventKind::MulticastTransmissionStarted { .. }
        | PhoneDeviceEventKind::MulticastTransmissionFailed { .. }
        | PhoneDeviceEventKind::ConnectionStatisticsCollected { .. } => PhoneEventFamily::Media,

        PhoneDeviceEventKind::Alarm { .. }
        | PhoneDeviceEventKind::XmlAlarm { .. }
        | PhoneDeviceEventKind::LocationInformation { .. }
        | PhoneDeviceEventKind::HeadsetStatusChanged { .. }
        | PhoneDeviceEventKind::MediaPathChanged { .. }
        | PhoneDeviceEventKind::UnhandledMessage { .. } => PhoneEventFamily::Telemetry,
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
