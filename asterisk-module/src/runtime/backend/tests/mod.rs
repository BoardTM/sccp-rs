use std::collections::HashSet;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::call::forwarding::{ForwardingContext, ForwardingDestination, ForwardingRouteReason};
use crate::call::transfer::{TransferCompletionKind, TransferId, TransferLeg};
use crate::call::voicemail::{VoicemailAction, VoicemailTarget, VoicemailTransactionId};
use crate::config::HintTarget;
use crate::config::LineConfig;
use crate::media::recording::{
    RecordingCallback, RecordingDirection, RecordingEvent, RecordingProvider,
    RecordingSessionControl, RecordingState, RecordingTarget,
};
use crate::presence::blf::{HintCallback, HintSnapshot};
use crate::presence::hints::HintProvider;
use crate::presence::hints::{ExtensionState, HintUpdateReason};
use crate::runtime::controller::Controller;
use crate::state::persistence::PersistenceError;
use crate::state::persistence::PersistentStore;
use sccp_protocol::{DeviceRegistration, DeviceType, ProtocolVersion, StationTransport};

mod fakes;
use fakes::*;

fn binding() -> LineBinding {
    LineBinding {
        device_id: DeviceId::new("SEP001122334455").unwrap(),
        line_instance: 1,
        appearance: sccp_protocol::LineAppearance::new(
            1,
            sccp_protocol::LineDefinition {
                number: "1001".into(),
                display_name: "Desk".into(),
            },
        ),
        line: LineConfig {
            number: "1001".into(),
            label: "Desk".into(),
            context: "from-sccp".into(),
            caller_name: "Desk".into(),
            caller_number: "1001".into(),
            mailbox: None,
            language: "en".into(),
            account_code: None,
            channel_variables: Vec::new(),
        },
    }
}

fn registration() -> DeviceRegistration {
    DeviceRegistration {
        id: binding().device_id,
        peer: "192.0.2.10:2000".parse().unwrap(),
        transport: StationTransport::Clear,
        reported_address: Some("192.0.2.10".parse().unwrap()),
        reported_ipv6_address: None,
        device_type: DeviceType::Unknown(0),
        protocol: ProtocolVersion::V22,
        firmware: "test".into(),
    }
}

fn connected_outbound_controller() -> Controller {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(1), binding(), Codec::Pcmu, Instant::now());
    controller.enbloc(CallId(1), "2100".into());
    controller.pbx_answer(PbxCallId(1));
    controller
}

fn active_conference_controller() -> Controller {
    let mut controller = connected_outbound_controller();
    controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    controller.enbloc(CallId(2), "2200".into());
    controller.pbx_answer(PbxCallId(2));
    controller.confirm_conference(CallId(2)).unwrap();
    assert!(controller.conference_merged(CallId(2)));
    controller
}

fn fake_backend(events: &Arc<Mutex<Vec<&'static str>>>, fail: Option<&'static str>) -> FakeBackend {
    FakeBackend {
        events: Arc::clone(events),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail,
        controller_probe: None,
    }
}

fn conference_progress(effects: &[DriverEffect], completed: usize) -> ConferenceStartProgress {
    effects[..completed].iter().fold(
        ConferenceStartProgress::default(),
        |mut progress, effect| {
            progress |= effect.into();
            progress
        },
    )
}

fn handset_operation(effect: &HandsetEffect) -> &'static str {
    match effect {
        HandsetEffect::BeginCall { .. } => "handset:begin-call",
        HandsetEffect::SetCallState {
            state: HandsetCallState::Hold,
            ..
        } => "handset:hold",
        HandsetEffect::ShowConferenceList { .. } => "handset:conference-list",
        HandsetEffect::ShowConferenceParticipantActions { .. } => {
            "handset:conference-participant-actions"
        }
        _ => "handset:other",
    }
}

fn info_effect() -> HandsetEffect {
    HandsetEffect::SetCallInfo {
        device_id: binding().device_id,
        call_id: CallId(7),
        info: CallInfo {
            direction: sccp_protocol::CallDirection::Outbound,
            calling_name: "Desk".into(),
            calling_number: "1001".into(),
            called_name: String::new(),
            called_number: String::new(),
            ..CallInfo::default()
        },
    }
}

fn backend_with_services(harness: ServiceHarness) -> FakeBackend {
    FakeBackend {
        events: Arc::new(Mutex::new(Vec::new())),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::with_harness(harness),
        fail: None,
        controller_probe: None,
    }
}

#[tokio::test]
async fn fake_backend_and_handset_effects_execute_in_order() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: None,
        controller_probe: None,
    };
    let handset_events = Arc::clone(&events);
    execute_effects(
        &backend,
        vec![
            PbxEffect::CreateChannel {
                handset_call_id: CallId(7),
                call_id: PbxCallId(1),
                binding: Box::new(binding()),
                codec: Codec::Pcmu,
            }
            .into(),
            info_effect().into(),
            PbxEffect::Answer {
                call_id: PbxCallId(1),
            }
            .into(),
        ],
        move |_| {
            handset_events.lock().unwrap().push("handset");
            async { Ok::<_, FakeError>(()) }
        },
    )
    .await
    .unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        ["backend:create", "handset", "backend:answer"]
    );
}

#[tokio::test]
async fn rejected_audio_admission_emits_no_open_and_cleanup_allows_a_new_call() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(1), binding(), Codec::Pcmu, Instant::now());
    controller.enbloc(CallId(1), "2100".into());
    let effects = controller.pbx_answer(PbxCallId(1));
    assert!(matches!(
        effects.as_slice(),
        [DriverEffect::Handset(HandsetEffect::BeginMedia {
            call_id: CallId(1),
            ..
        })]
    ));

    let admission = crate::media::encryption::AudioEncryptionAdmission::new(
        crate::media::encryption::MediaEncryptionPolicy::new(
            crate::media::encryption::MediaEncryptionRequirement::Required,
            [crate::media::encryption::MediaEncryptionProfile::AES_128_HMAC_SHA1_80],
        )
        .unwrap(),
        crate::media::encryption::StationEncryptionCapabilities::NotReported,
        LocalEncryptionCapabilities::default(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = fake_backend(&events, None);
    let handset_events = Arc::clone(&events);
    let result = execute_effects(&backend, effects, move |_| {
        let decision = admission.decide();
        let handset_events = Arc::clone(&handset_events);
        async move {
            decision.map_err(|_| FakeError("media-admission"))?;
            handset_events.lock().unwrap().push("handset:media-open");
            Ok(())
        }
    })
    .await;

    assert!(matches!(
        result,
        Err(EffectExecutionError::Handset {
            error: FakeError("media-admission"),
            ..
        })
    ));
    assert!(events.lock().unwrap().is_empty());

    let cleanup = controller
        .pbx_hangup_with_effects(PbxCallId(1))
        .expect("admitted call remains available for failure cleanup");
    assert!(controller.call(CallId(1)).is_none());
    assert!(controller.pbx_call(PbxCallId(1)).is_none());

    let cleanup_events = Arc::clone(&events);
    execute_cleanup_effects(&backend, cleanup.effects, move |_| {
        cleanup_events.lock().unwrap().push("handset:cleanup");
        async { Ok::<_, FakeError>(()) }
    })
    .await;
    assert_eq!(*events.lock().unwrap(), ["handset:cleanup"]);

    let retry = controller.begin_phone_call(CallId(2), binding(), Codec::Pcmu, Instant::now());
    assert!(!retry.is_empty());
    assert!(controller.call(CallId(2)).is_some());
}

#[tokio::test]
async fn conference_consultation_executes_confirmed_begin_call_before_channel_creation() {
    let mut controller = connected_outbound_controller();
    let effects = controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = fake_backend(&events, None);
    let handset_events = Arc::clone(&events);

    execute_effects(&backend, effects, move |effect| {
        handset_events
            .lock()
            .unwrap()
            .push(handset_operation(&effect));
        async { Ok::<_, FakeError>(()) }
    })
    .await
    .unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "backend:hold",
            "handset:hold",
            "handset:begin-call",
            "backend:create",
        ]
    );
}

#[tokio::test]
async fn conference_consultation_failure_before_begin_call_executes_exact_abort() {
    let mut controller = connected_outbound_controller();
    let effects = controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = fake_backend(&events, None);
    let handset_events = Arc::clone(&events);
    let error = execute_effects(&backend, effects.clone(), move |effect| {
        let operation = handset_operation(&effect);
        handset_events.lock().unwrap().push(operation);
        async move {
            if operation == "handset:hold" {
                Err(FakeError("handset:hold"))
            } else {
                Ok(())
            }
        }
    })
    .await
    .unwrap_err();
    let EffectExecutionError::Handset { index, .. } = error else {
        panic!("expected handset execution error");
    };
    assert_eq!(*events.lock().unwrap(), ["backend:hold", "handset:hold"]);

    let progress = conference_progress(&effects, index);
    let cleanup = controller.abort_conference(
        CallId(2),
        false,
        progress.channel_created(),
        progress.active_leg_held(),
        progress.active_handset_held(),
    );
    let cleanup_events = Arc::new(Mutex::new(Vec::new()));
    let cleanup_backend = fake_backend(&cleanup_events, None);
    let handset_events = Arc::clone(&cleanup_events);
    let errors = execute_cleanup_effects(&cleanup_backend, cleanup, move |effect| {
        handset_events
            .lock()
            .unwrap()
            .push(handset_operation(&effect));
        async { Ok::<_, FakeError>(()) }
    })
    .await;

    assert!(errors.is_empty());
    assert_eq!(
        *cleanup_events.lock().unwrap(),
        ["backend:resume", "handset:other"]
    );
    assert!(controller.call(CallId(2)).is_none());
}

#[tokio::test]
async fn conference_consultation_failure_after_begin_call_executes_exact_abort() {
    let mut controller = connected_outbound_controller();
    let effects = controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = fake_backend(&events, Some("backend:create"));
    let handset_events = Arc::clone(&events);
    let error = execute_effects(&backend, effects.clone(), move |effect| {
        handset_events
            .lock()
            .unwrap()
            .push(handset_operation(&effect));
        async { Ok::<_, FakeError>(()) }
    })
    .await
    .unwrap_err();
    let EffectExecutionError::Backend { index, .. } = error else {
        panic!("expected backend execution error");
    };
    assert_eq!(
        *events.lock().unwrap(),
        [
            "backend:hold",
            "handset:hold",
            "handset:begin-call",
            "backend:create",
        ]
    );

    let progress = conference_progress(&effects, index);
    let cleanup = controller.abort_conference(
        CallId(2),
        false,
        progress.channel_created(),
        progress.active_leg_held(),
        progress.active_handset_held(),
    );
    let cleanup_events = Arc::new(Mutex::new(Vec::new()));
    let cleanup_backend = fake_backend(&cleanup_events, None);
    let handset_events = Arc::clone(&cleanup_events);
    let errors = execute_cleanup_effects(&cleanup_backend, cleanup, move |effect| {
        handset_events
            .lock()
            .unwrap()
            .push(handset_operation(&effect));
        async { Ok::<_, FakeError>(()) }
    })
    .await;

    assert!(errors.is_empty());
    assert_eq!(
        *cleanup_events.lock().unwrap(),
        ["backend:resume", "handset:other", "handset:other"]
    );
    assert!(controller.call(CallId(2)).is_none());
}

#[tokio::test]
async fn conference_invite_failures_before_and_after_begin_call_preserve_the_conference() {
    for (handset_failure, backend_failure, expected_events) in [
        (
            Some("handset:hold"),
            None,
            vec!["backend:hold", "handset:hold"],
        ),
        (
            None,
            Some("backend:create"),
            vec![
                "backend:hold",
                "handset:hold",
                "handset:begin-call",
                "backend:create",
            ],
        ),
    ] {
        let mut controller = active_conference_controller();
        let conference_id = controller.conference_session(CallId(1)).unwrap().id;
        let effects = controller
            .begin_conference_invite(CallId(1), CallId(3), binding(), Codec::Pcmu, Instant::now())
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = fake_backend(&events, backend_failure);
        let handset_events = Arc::clone(&events);
        let error = execute_effects(&backend, effects.clone(), move |effect| {
            let operation = handset_operation(&effect);
            handset_events.lock().unwrap().push(operation);
            async move {
                if handset_failure == Some(operation) {
                    Err(FakeError(operation))
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap_err();
        let index = match error {
            EffectExecutionError::Backend { index, .. }
            | EffectExecutionError::Handset { index, .. } => index,
        };
        assert_eq!(*events.lock().unwrap(), expected_events);

        let progress = conference_progress(&effects, index);
        let cleanup = controller.abort_conference_invite(
            CallId(3),
            progress.channel_created(),
            progress.active_leg_held(),
            progress.active_handset_held(),
        );
        let cleanup_backend = fake_backend(&Arc::new(Mutex::new(Vec::new())), None);
        let errors = execute_cleanup_effects(&cleanup_backend, cleanup, |_| async {
            Ok::<_, FakeError>(())
        })
        .await;

        assert!(errors.is_empty());
        assert!(controller.call(CallId(3)).is_none());
        let session = controller.conference_session(CallId(1)).unwrap();
        assert_eq!(session.id, conference_id);
        assert!(session.pending_invite.is_none());
        assert_eq!(session.participants.iter().len(), 2);
    }
}

#[tokio::test]
async fn conference_invite_and_ui_use_the_confirmed_effect_boundary() {
    let mut controller = active_conference_controller();
    let invite_effects = controller
        .begin_conference_invite(CallId(1), CallId(3), binding(), Codec::Pcmu, Instant::now())
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = fake_backend(&events, None);
    let handset_events = Arc::clone(&events);
    execute_effects(&backend, invite_effects, move |effect| {
        handset_events
            .lock()
            .unwrap()
            .push(handset_operation(&effect));
        async { Ok::<_, FakeError>(()) }
    })
    .await
    .unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        [
            "backend:hold",
            "handset:hold",
            "handset:begin-call",
            "backend:create",
        ]
    );

    controller.abort_conference_invite(CallId(3), true, true, true);
    let session = controller.conference_session(CallId(1)).unwrap();
    let participant_id = session
        .participants
        .iter()
        .find(|participant| !participant.moderator)
        .unwrap()
        .id;
    let ui_effects = vec![
        session.list_effect(CallId(1)).into(),
        session
            .participant_actions_effect(participant_id)
            .unwrap()
            .into(),
        PbxEffect::Answer {
            call_id: PbxCallId(1),
        }
        .into(),
    ];
    let ui_events = Arc::new(Mutex::new(Vec::new()));
    let backend = fake_backend(&ui_events, None);
    let handset_events = Arc::clone(&ui_events);
    let error = execute_effects(&backend, ui_effects, move |effect| {
        let operation = handset_operation(&effect);
        handset_events.lock().unwrap().push(operation);
        async move {
            if operation == "handset:conference-participant-actions" {
                Err(FakeError("handset:conference-participant-actions"))
            } else {
                Ok(())
            }
        }
    })
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        EffectExecutionError::Handset { index: 1, .. }
    ));
    assert_eq!(
        *ui_events.lock().unwrap(),
        [
            "handset:conference-list",
            "handset:conference-participant-actions",
        ]
    );
}

#[tokio::test]
async fn typed_transfer_reaches_backend_once_in_source_target_order() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let operations = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::clone(&operations),
        capabilities: FakeCapabilities::default(),
        fail: None,
        controller_probe: None,
    };
    let operation = TransferCompletion {
        transaction_id: TransferId(7),
        device_id: DeviceId::new("SEP001122334455").unwrap(),
        source: TransferLeg {
            handset_call_id: CallId(10),
            pbx_call_id: PbxCallId(100),
        },
        consultation: TransferLeg {
            handset_call_id: CallId(20),
            pbx_call_id: PbxCallId(200),
        },
        kind: TransferCompletionKind::Attended,
    };
    execute_effects(
        &backend,
        vec![
            PbxEffect::Transfer {
                operation: operation.clone(),
            }
            .into(),
        ],
        |_| async { Ok::<_, FakeError>(()) },
    )
    .await
    .unwrap();

    assert_eq!(*events.lock().unwrap(), ["backend:bridge-transfer"]);
    assert_eq!(
        *operations.lock().unwrap(),
        [AdvancedOperation::Transfer(operation)]
    );
}

#[tokio::test]
async fn typed_transfer_failure_retains_transaction_identity_and_stops_handset_work() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: Some("backend:bridge-transfer"),
        controller_probe: None,
    };
    let operation = TransferCompletion {
        transaction_id: TransferId(9),
        device_id: DeviceId::new("SEP001122334455").unwrap(),
        source: TransferLeg {
            handset_call_id: CallId(10),
            pbx_call_id: PbxCallId(100),
        },
        consultation: TransferLeg {
            handset_call_id: CallId(20),
            pbx_call_id: PbxCallId(200),
        },
        kind: TransferCompletionKind::Blind,
    };
    let error = execute_effects(
        &backend,
        vec![
            PbxEffect::Transfer {
                operation: operation.clone(),
            }
            .into(),
            info_effect().into(),
        ],
        |_| async { Err(FakeError("unexpected handset work")) },
    )
    .await
    .unwrap_err();

    assert_eq!(*events.lock().unwrap(), ["backend:bridge-transfer"]);
    assert!(matches!(
        error,
        EffectExecutionError::Backend {
            index: 0,
            effect,
            error: FakeError("backend:bridge-transfer"),
        } if *effect == (PbxEffect::Transfer { operation })
    ));
}

#[tokio::test]
async fn backend_error_stops_later_effects_and_reports_position() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: Some("backend:answer"),
        controller_probe: None,
    };
    let handset_events = Arc::clone(&events);
    let error = execute_effects(
        &backend,
        vec![
            info_effect().into(),
            PbxEffect::Answer {
                call_id: PbxCallId(1),
            }
            .into(),
            info_effect().into(),
        ],
        move |_| {
            handset_events.lock().unwrap().push("handset");
            async { Ok::<_, FakeError>(()) }
        },
    )
    .await
    .unwrap_err();

    let EffectExecutionError::Backend {
        index,
        effect,
        error,
    } = error
    else {
        panic!("expected backend execution error");
    };
    assert_eq!(index, 1);
    assert!(matches!(*effect, PbxEffect::Answer { .. }));
    assert_eq!(error, FakeError("backend:answer"));
    assert_eq!(*events.lock().unwrap(), ["handset", "backend:answer"]);
}

#[tokio::test]
async fn terminal_cleanup_attempts_every_backend_and_handset_effect_after_failures() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: Some("backend:bridge-destroy"),
        controller_probe: None,
    };
    let handset_events = Arc::clone(&events);
    let errors = execute_cleanup_effects(
        &backend,
        vec![
            PbxEffect::Bridge {
                operation: BridgeOperation::Destroy {
                    bridge_id: PbxBridgeId(9),
                },
            }
            .into(),
            PbxEffect::Hangup {
                call_id: PbxCallId(1),
            }
            .into(),
            info_effect().into(),
            PbxEffect::Answer {
                call_id: PbxCallId(2),
            }
            .into(),
        ],
        move |_| {
            handset_events.lock().unwrap().push("handset");
            async { Err::<(), _>(FakeError("handset")) }
        },
    )
    .await;

    assert_eq!(
        *events.lock().unwrap(),
        [
            "backend:bridge-destroy",
            "backend:hangup",
            "handset",
            "backend:answer",
        ]
    );
    assert_eq!(errors.len(), 2);
    assert!(matches!(
        &errors[0],
        EffectExecutionError::Backend { index: 0, .. }
    ));
    assert!(matches!(
        &errors[1],
        EffectExecutionError::Handset { index: 2, .. }
    ));
}

#[tokio::test]
async fn handset_error_is_propagated_with_the_effect_position() {
    let backend = FakeBackend {
        events: Arc::new(Mutex::new(Vec::new())),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: None,
        controller_probe: None,
    };
    let error = execute_effects(&backend, vec![info_effect().into()], |_| async {
        Err::<(), _>(FakeError("handset"))
    })
    .await
    .unwrap_err();

    let EffectExecutionError::Handset {
        index,
        effect,
        error,
    } = error
    else {
        panic!("expected handset execution error");
    };
    assert_eq!(index, 0);
    assert!(matches!(*effect, HandsetEffect::SetCallInfo { .. }));
    assert_eq!(error, FakeError("handset"));
}

#[tokio::test]
async fn backend_media_result_is_delivered_before_the_next_effect() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: None,
        controller_probe: None,
    };
    let handset_events = Arc::clone(&events);
    let endpoint = MediaEndpoint {
        address: "192.0.2.20".parse().unwrap(),
        rtp_port: 20_000,
        rtcp_port: 20_001,
        codec: Codec::Pcmu,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 101,
    };
    execute_effects(
        &backend,
        vec![
            PbxEffect::ConfigureMedia {
                call_id: PbxCallId(1),
                device_id: binding().device_id,
                handset_call_id: CallId(7),
                codec: Codec::Pcmu,
                remote: endpoint,
            }
            .into(),
            PbxEffect::Answer {
                call_id: PbxCallId(1),
            }
            .into(),
        ],
        move |effect| {
            assert!(matches!(effect, HandsetEffect::StartMedia { .. }));
            handset_events.lock().unwrap().push("handset:media");
            async { Ok::<_, FakeError>(()) }
        },
    )
    .await
    .unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        ["backend:media", "handset:media", "backend:answer"]
    );
}

#[tokio::test]
async fn coupled_media_configuration_never_sends_a_second_transmit_request() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: None,
        controller_probe: None,
    };
    let endpoint = MediaEndpoint {
        address: "192.0.2.20".parse().unwrap(),
        rtp_port: 20_000,
        rtcp_port: 20_001,
        codec: Codec::Pcmu,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 101,
    };
    let handset_events = Arc::clone(&events);

    execute_effects(
        &backend,
        vec![
            PbxEffect::ConfigureMediaOnly {
                call_id: PbxCallId(1),
                device_id: binding().device_id,
                codec: Codec::Pcmu,
                remote: endpoint,
            }
            .into(),
        ],
        move |_| {
            handset_events.lock().unwrap().push("handset:unexpected");
            async { Ok::<(), FakeError>(()) }
        },
    )
    .await
    .unwrap();

    assert_eq!(*events.lock().unwrap(), ["backend:media"]);
}

#[tokio::test]
async fn pickup_result_is_delivered_with_parties_before_the_next_effect() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let handset_effects = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: None,
        controller_probe: None,
    };
    let received = Arc::clone(&handset_effects);
    let handset_events = Arc::clone(&events);
    execute_effects(
        &backend,
        vec![
            PbxEffect::Pickup {
                operation: PickupOperation::Directed {
                    call_id: PbxCallId(2),
                    device_id: binding().device_id,
                    handset_call_id: CallId(20),
                    codec: Codec::Pcma,
                    extension: "2100".into(),
                    context: "from-phones".into(),
                    answer: false,
                },
            }
            .into(),
            PbxEffect::Answer {
                call_id: PbxCallId(2),
            }
            .into(),
        ],
        move |effect| {
            handset_events.lock().unwrap().push("handset:pickup");
            received.lock().unwrap().push(effect);
            async { Ok::<_, FakeError>(()) }
        },
    )
    .await
    .unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "backend:pickup-directed",
            "handset:pickup",
            "backend:answer"
        ]
    );
    assert_eq!(
        *handset_effects.lock().unwrap(),
        [HandsetEffect::PickupCompleted {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            call_id: CallId(20),
            codec: Codec::Pcma,
            answer: false,
            parties: PickupOutcome {
                calling_name: "Caller".into(),
                calling_number: "2100".into(),
                connected_name: "Target".into(),
                connected_number: "2200".into(),
                redirecting_name: "Reception".into(),
                redirecting_number: "2000".into(),
            },
        }]
    );
}

#[tokio::test]
async fn advanced_operations_dispatch_typed_payloads_in_order() {
    let bridge = PbxBridgeId(9);
    let operations = vec![
        AdvancedOperation::Forward(ForwardingOperation {
            call_id: PbxCallId(7),
            context: ForwardingContext::new("from-sccp").unwrap(),
            destination: ForwardingDestination::new("private-2000").unwrap(),
            reason: ForwardingRouteReason::Busy,
        }),
        AdvancedOperation::Voicemail(VoicemailOperation {
            transaction_id: VoicemailTransactionId(3),
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            handset_call_id: CallId(8),
            pbx_call_id: PbxCallId(8),
            action: VoicemailAction::ImmediateDivert,
            target: VoicemailTarget::new("from-sccp", "private-voicemail").unwrap(),
        }),
        AdvancedOperation::ConferenceDestination(ConferenceDestinationOperation {
            call_id: PbxCallId(8),
            destination: "700".into(),
            application_options: "Mac".into(),
            handset_call_id: CallId(8),
            held_calls: Vec::new(),
            mutation: ConferenceMutationToken::for_test(PbxCallId(8)),
        }),
        AdvancedOperation::Bridge(BridgeOperation::Create { bridge_id: bridge }),
        AdvancedOperation::Bridge(BridgeOperation::AddParticipant {
            bridge_id: bridge,
            call_id: PbxCallId(1),
        }),
        AdvancedOperation::Bridge(BridgeOperation::MergeConsultation {
            bridge_id: bridge,
            original_call_id: PbxCallId(1),
            consultation_call_id: PbxCallId(2),
        }),
        AdvancedOperation::Bridge(BridgeOperation::MergeCalls {
            bridge_id: bridge,
            call_ids: vec![PbxCallId(1), PbxCallId(2), PbxCallId(3)],
        }),
        AdvancedOperation::Bridge(BridgeOperation::MergeParticipant {
            bridge_id: bridge,
            call_id: PbxCallId(4),
        }),
        AdvancedOperation::Bridge(BridgeOperation::SetParticipantMuted {
            bridge_id: bridge,
            participant_id: ParticipantId::new(7),
            call_id: PbxCallId(4),
            muted: true,
        }),
        AdvancedOperation::Bridge(BridgeOperation::SetParticipantMuted {
            bridge_id: bridge,
            participant_id: ParticipantId::new(7),
            call_id: PbxCallId(4),
            muted: false,
        }),
        AdvancedOperation::Bridge(BridgeOperation::RemoveConferenceParticipant {
            bridge_id: bridge,
            participant_id: ParticipantId::new(7),
            call_id: PbxCallId(4),
        }),
        AdvancedOperation::Bridge(BridgeOperation::SetParticipantMusicOnHold {
            bridge_id: bridge,
            participant_id: ParticipantId::new(7),
            call_id: PbxCallId(4),
            class: "office".into(),
            enabled: true,
        }),
        AdvancedOperation::Bridge(BridgeOperation::SetParticipantMusicOnHold {
            bridge_id: bridge,
            participant_id: ParticipantId::new(7),
            call_id: PbxCallId(4),
            class: "office".into(),
            enabled: false,
        }),
        AdvancedOperation::Barge(BargeOperation::Join {
            bridge_id: PbxBridgeId(10),
            target_call_id: PbxCallId(1),
            barger_call_id: PbxCallId(6),
        }),
        AdvancedOperation::Pickup(PickupOperation::Group {
            call_id: PbxCallId(2),
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            handset_call_id: CallId(20),
            codec: Codec::Pcmu,
            answer: true,
        }),
        AdvancedOperation::Pickup(PickupOperation::Directed {
            call_id: PbxCallId(3),
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            handset_call_id: CallId(30),
            codec: Codec::Pcma,
            extension: "2100".into(),
            context: "from-phones".into(),
            answer: false,
        }),
        AdvancedOperation::Parking(ParkingOperation::Park {
            call_id: PbxCallId(4),
            lot: Some("executive".into()),
        }),
        AdvancedOperation::Parking(ParkingOperation::Retrieve {
            call_id: PbxCallId(5),
            lot: None,
            slot: "701".into(),
        }),
        AdvancedOperation::Management(ManagementEvent {
            kind: ManagementEventKind::Call,
            fields: vec![ManagementField::new("CallId", 5_u64)],
        }),
        AdvancedOperation::Bridge(BridgeOperation::RemoveParticipant {
            bridge_id: bridge,
            call_id: PbxCallId(1),
        }),
        AdvancedOperation::Bridge(BridgeOperation::Destroy { bridge_id: bridge }),
        AdvancedOperation::Barge(BargeOperation::Leave {
            bridge_id: PbxBridgeId(10),
            barger_call_id: PbxCallId(6),
            last_participant: true,
        }),
        AdvancedOperation::Announcement(ConferenceAnnouncementOperation {
            conference_id: ConferenceId::new(11),
            targets: vec![ConferenceAnnouncementTarget {
                participant_id: ParticipantId::new(1),
                call_id: PbxCallId(1),
            }],
            announcement: ConferenceAnnouncement::Connected,
        }),
    ];
    let effects = operations
        .iter()
        .cloned()
        .map(|operation| match operation {
            AdvancedOperation::ConferenceDestination(operation) => {
                PbxEffect::StartConferenceDestination { operation }.into()
            }
            AdvancedOperation::Forward(operation) => PbxEffect::Forward { operation }.into(),
            AdvancedOperation::Voicemail(operation) => PbxEffect::Voicemail { operation }.into(),
            AdvancedOperation::Transfer(operation) => PbxEffect::Transfer { operation }.into(),
            AdvancedOperation::Bridge(operation) => PbxEffect::Bridge { operation }.into(),
            AdvancedOperation::Barge(operation) => PbxEffect::Barge { operation }.into(),
            AdvancedOperation::Pickup(operation) => PbxEffect::Pickup { operation }.into(),
            AdvancedOperation::Parking(operation) => PbxEffect::Parking { operation }.into(),
            AdvancedOperation::Announcement(operation) => {
                PbxEffect::ConferenceAnnouncement { operation }.into()
            }
            AdvancedOperation::Management(event) => {
                PbxEffect::PublishManagementEvent { event }.into()
            }
        })
        .collect();
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let backend = FakeBackend {
        events: Arc::clone(&events),
        advanced_operations: Arc::clone(&recorded),
        capabilities: FakeCapabilities::default(),
        fail: None,
        controller_probe: None,
    };

    execute_effects(&backend, effects, |_| async { Ok::<_, FakeError>(()) })
        .await
        .unwrap();

    assert_eq!(*recorded.lock().unwrap(), operations);
    assert_eq!(
        *events.lock().unwrap(),
        [
            "backend:forward",
            "backend:voicemail",
            "backend:conference-destination",
            "backend:bridge-create",
            "backend:bridge-add",
            "backend:bridge-merge-consultation",
            "backend:bridge-merge-calls",
            "backend:bridge-merge-participant",
            "backend:bridge-mute-participant",
            "backend:bridge-unmute-participant",
            "backend:bridge-remove-conference-participant",
            "backend:bridge-start-music",
            "backend:bridge-stop-music",
            "backend:barge-join",
            "backend:pickup-group",
            "backend:pickup-directed",
            "backend:park",
            "backend:parking-retrieve",
            "backend:management",
            "backend:bridge-remove",
            "backend:bridge-destroy",
            "backend:barge-leave",
            "backend:conference-announcement",
        ]
    );
}

#[tokio::test]
async fn every_advanced_effect_propagates_errors_and_stops_the_queue() {
    let cases = [
        (
            "backend:forward",
            PbxEffect::Forward {
                operation: ForwardingOperation {
                    call_id: PbxCallId(7),
                    context: ForwardingContext::new("from-sccp").unwrap(),
                    destination: ForwardingDestination::new("private-2000").unwrap(),
                    reason: ForwardingRouteReason::NoAnswer,
                },
            },
        ),
        (
            "backend:voicemail",
            PbxEffect::Voicemail {
                operation: VoicemailOperation {
                    transaction_id: VoicemailTransactionId(4),
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    handset_call_id: CallId(8),
                    pbx_call_id: PbxCallId(8),
                    action: VoicemailAction::TransferSelected,
                    target: VoicemailTarget::new("from-sccp", "private-voicemail").unwrap(),
                },
            },
        ),
        (
            "backend:conference-destination",
            PbxEffect::StartConferenceDestination {
                operation: ConferenceDestinationOperation {
                    call_id: PbxCallId(8),
                    destination: "700".into(),
                    application_options: "Mac".into(),
                    handset_call_id: CallId(8),
                    held_calls: Vec::new(),
                    mutation: ConferenceMutationToken::for_test(PbxCallId(8)),
                },
            },
        ),
        (
            "backend:bridge-create",
            PbxEffect::Bridge {
                operation: BridgeOperation::Create {
                    bridge_id: PbxBridgeId(9),
                },
            },
        ),
        (
            "backend:bridge-add",
            PbxEffect::Bridge {
                operation: BridgeOperation::AddParticipant {
                    bridge_id: PbxBridgeId(9),
                    call_id: PbxCallId(1),
                },
            },
        ),
        (
            "backend:bridge-merge-consultation",
            PbxEffect::Bridge {
                operation: BridgeOperation::MergeConsultation {
                    bridge_id: PbxBridgeId(9),
                    original_call_id: PbxCallId(1),
                    consultation_call_id: PbxCallId(2),
                },
            },
        ),
        (
            "backend:bridge-merge-calls",
            PbxEffect::Bridge {
                operation: BridgeOperation::MergeCalls {
                    bridge_id: PbxBridgeId(9),
                    call_ids: vec![PbxCallId(1), PbxCallId(2)],
                },
            },
        ),
        (
            "backend:bridge-merge-participant",
            PbxEffect::Bridge {
                operation: BridgeOperation::MergeParticipant {
                    bridge_id: PbxBridgeId(9),
                    call_id: PbxCallId(3),
                },
            },
        ),
        (
            "backend:bridge-mute-participant",
            PbxEffect::Bridge {
                operation: BridgeOperation::SetParticipantMuted {
                    bridge_id: PbxBridgeId(9),
                    participant_id: ParticipantId::new(7),
                    call_id: PbxCallId(3),
                    muted: true,
                },
            },
        ),
        (
            "backend:bridge-unmute-participant",
            PbxEffect::Bridge {
                operation: BridgeOperation::SetParticipantMuted {
                    bridge_id: PbxBridgeId(9),
                    participant_id: ParticipantId::new(7),
                    call_id: PbxCallId(3),
                    muted: false,
                },
            },
        ),
        (
            "backend:bridge-remove-conference-participant",
            PbxEffect::Bridge {
                operation: BridgeOperation::RemoveConferenceParticipant {
                    bridge_id: PbxBridgeId(9),
                    participant_id: ParticipantId::new(7),
                    call_id: PbxCallId(3),
                },
            },
        ),
        (
            "backend:bridge-start-music",
            PbxEffect::Bridge {
                operation: BridgeOperation::SetParticipantMusicOnHold {
                    bridge_id: PbxBridgeId(9),
                    participant_id: ParticipantId::new(7),
                    call_id: PbxCallId(3),
                    class: "office".into(),
                    enabled: true,
                },
            },
        ),
        (
            "backend:bridge-stop-music",
            PbxEffect::Bridge {
                operation: BridgeOperation::SetParticipantMusicOnHold {
                    bridge_id: PbxBridgeId(9),
                    participant_id: ParticipantId::new(7),
                    call_id: PbxCallId(3),
                    class: "office".into(),
                    enabled: false,
                },
            },
        ),
        (
            "backend:barge-join",
            PbxEffect::Barge {
                operation: BargeOperation::Join {
                    bridge_id: PbxBridgeId(10),
                    target_call_id: PbxCallId(1),
                    barger_call_id: PbxCallId(6),
                },
            },
        ),
        (
            "backend:barge-leave",
            PbxEffect::Barge {
                operation: BargeOperation::Leave {
                    bridge_id: PbxBridgeId(10),
                    barger_call_id: PbxCallId(6),
                    last_participant: true,
                },
            },
        ),
        (
            "backend:bridge-remove",
            PbxEffect::Bridge {
                operation: BridgeOperation::RemoveParticipant {
                    bridge_id: PbxBridgeId(9),
                    call_id: PbxCallId(1),
                },
            },
        ),
        (
            "backend:bridge-destroy",
            PbxEffect::Bridge {
                operation: BridgeOperation::Destroy {
                    bridge_id: PbxBridgeId(9),
                },
            },
        ),
        (
            "backend:pickup-group",
            PbxEffect::Pickup {
                operation: PickupOperation::Group {
                    call_id: PbxCallId(2),
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    handset_call_id: CallId(20),
                    codec: Codec::Pcmu,
                    answer: false,
                },
            },
        ),
        (
            "backend:pickup-directed",
            PbxEffect::Pickup {
                operation: PickupOperation::Directed {
                    call_id: PbxCallId(2),
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    handset_call_id: CallId(20),
                    codec: Codec::Pcmu,
                    extension: "2100".into(),
                    context: "from-phones".into(),
                    answer: true,
                },
            },
        ),
        (
            "backend:park",
            PbxEffect::Parking {
                operation: ParkingOperation::Park {
                    call_id: PbxCallId(3),
                    lot: None,
                },
            },
        ),
        (
            "backend:parking-retrieve",
            PbxEffect::Parking {
                operation: ParkingOperation::Retrieve {
                    call_id: PbxCallId(3),
                    lot: Some("executive".into()),
                    slot: "701".into(),
                },
            },
        ),
        (
            "backend:management",
            PbxEffect::PublishManagementEvent {
                event: ManagementEvent {
                    kind: ManagementEventKind::Alarm,
                    fields: vec![ManagementField::new("Text", "warning")],
                },
            },
        ),
        (
            "backend:conference-announcement",
            PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: ConferenceId::new(11),
                    targets: vec![ConferenceAnnouncementTarget {
                        participant_id: ParticipantId::new(1),
                        call_id: PbxCallId(3),
                    }],
                    announcement: ConferenceAnnouncement::Connected,
                },
            },
        ),
    ];

    for (failure, effect) in cases {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: Some(failure),
            controller_probe: None,
        };
        let error = execute_effects(
            &backend,
            vec![effect.clone().into(), info_effect().into()],
            |_| async { Ok::<_, FakeError>(()) },
        )
        .await
        .unwrap_err();
        let EffectExecutionError::Backend {
            index,
            effect: failed_effect,
            error,
        } = error
        else {
            panic!("expected backend execution error");
        };
        assert_eq!(index, 0);
        assert_eq!(*failed_effect, effect);
        assert_eq!(error, FakeError(failure));
        assert_eq!(*events.lock().unwrap(), [failure]);
    }
}

#[test]
fn direct_capabilities_preserve_typed_requests_callbacks_and_sessions() {
    let harness = ServiceHarness::default();
    let backend = backend_with_services(harness.clone());
    let hint_target = HintTarget::parse("1001@internal").unwrap();

    assert_eq!(
        backend.persistence().get("driver", "device/dnd").unwrap(),
        Some("stored".into())
    );
    backend
        .persistence()
        .put("driver", "device/dnd", "silent")
        .unwrap();
    backend
        .persistence()
        .delete("driver", "device/dnd")
        .unwrap();

    assert_eq!(
        backend.hints().lookup(&hint_target).unwrap(),
        Some(HintSnapshot {
            target: hint_target.clone(),
            state: ExtensionState::IDLE,
            reason: HintUpdateReason::Device,
            caller: None,
        })
    );
    let hint_updates = Arc::new(Mutex::new(Vec::new()));
    let callback_updates = Arc::clone(&hint_updates);
    let _subscription = backend
        .hints()
        .subscribe(
            &hint_target,
            Arc::new(move |update| callback_updates.lock().unwrap().push(update)),
        )
        .unwrap();
    assert!(matches!(
        hint_updates.lock().unwrap().as_slice(),
        [HintSnapshot {
            state: ExtensionState::RINGING,
            ..
        }]
    ));

    let recording_events = Arc::new(Mutex::new(Vec::new()));
    let callback_events = Arc::clone(&recording_events);
    let mut recording = backend
        .recordings()
        .start_recording(
            PbxCallId(7),
            RecordingTarget::ExplicitlyNamed("call.wav".into()),
            "b",
            Arc::new(move |event| callback_events.lock().unwrap().push(event)),
        )
        .unwrap();
    assert_eq!(recording.id().unwrap(), "recording-1");
    assert_eq!(recording.state().unwrap(), RecordingState::Active);
    assert_eq!(
        recording.set_muted(RecordingDirection::Both, true).unwrap(),
        1
    );
    assert_eq!(recording.state().unwrap(), RecordingState::Muted);
    recording.stop().unwrap();
    assert_eq!(recording.state().unwrap(), RecordingState::Stopped);
    assert_eq!(*recording_events.lock().unwrap(), [RecordingEvent::Started]);

    assert_eq!(
        *harness.requests.lock().unwrap(),
        [
            ServiceRequest::Get("driver".into(), "device/dnd".into()),
            ServiceRequest::Put("driver".into(), "device/dnd".into(), "silent".into(),),
            ServiceRequest::Delete("driver".into(), "device/dnd".into()),
            ServiceRequest::HintLookup("internal".into(), "1001".into()),
            ServiceRequest::HintSubscribe("internal".into(), "1001".into()),
            ServiceRequest::RecordingStart(
                PbxCallId(7),
                RecordingTarget::ExplicitlyNamed("call.wav".into()),
                "b".into()
            ),
            ServiceRequest::RecordingId,
            ServiceRequest::RecordingState,
            ServiceRequest::RecordingMute(RecordingDirection::Both, true),
            ServiceRequest::RecordingState,
            ServiceRequest::RecordingStop,
            ServiceRequest::RecordingState,
        ]
    );
}

#[test]
fn every_direct_capability_propagates_its_backend_error() {
    let harness = ServiceHarness::default();
    let backend = backend_with_services(harness.clone());
    let hint_target = HintTarget::parse("1001@internal").unwrap();

    harness.fail("persistence:get");
    assert!(matches!(
        backend.persistence().get("driver", "key"),
        Err(PersistenceError::Backend { operation: "get" })
    ));
    harness.fail("persistence:put");
    assert!(matches!(
        backend.persistence().put("driver", "key", "value"),
        Err(PersistenceError::Backend { operation: "put" })
    ));
    harness.fail("persistence:delete");
    assert!(matches!(
        backend.persistence().delete("driver", "key"),
        Err(PersistenceError::Backend {
            operation: "delete"
        })
    ));

    harness.fail("hints:lookup");
    assert_eq!(
        backend.hints().lookup(&hint_target).unwrap_err(),
        FakeError("hints:lookup")
    );
    harness.fail("hints:subscribe");
    assert_eq!(
        backend
            .hints()
            .subscribe(&hint_target, Arc::new(|_| {}))
            .err(),
        Some(FakeError("hints:subscribe"))
    );

    harness.fail("recording:start");
    assert_eq!(
        backend
            .recordings()
            .start_recording(
                PbxCallId(7),
                RecordingTarget::ExplicitlyNamed("call.wav".into()),
                "",
                Arc::new(|_| {}),
            )
            .err(),
        Some(FakeError("recording:start"))
    );

    let session_harness = ServiceHarness::default();
    let session_backend = backend_with_services(session_harness.clone());
    let mut recording = session_backend
        .recordings()
        .start_recording(
            PbxCallId(8),
            RecordingTarget::ExplicitlyNamed("call.wav".into()),
            "",
            Arc::new(|_| {}),
        )
        .unwrap();
    session_harness.fail("recording:id");
    assert_eq!(recording.id().unwrap_err(), FakeError("recording:id"));
    session_harness.fail("recording:state");
    assert_eq!(recording.state().unwrap_err(), FakeError("recording:state"));
    session_harness.fail("recording:mute");
    assert_eq!(
        recording
            .set_muted(RecordingDirection::Read, true)
            .unwrap_err(),
        FakeError("recording:mute")
    );
    session_harness.fail("recording:stop");
    assert_eq!(recording.stop().unwrap_err(), FakeError("recording:stop"));
}

#[test]
fn direct_capabilities_run_after_controller_locks_are_released() {
    let controller = Arc::new(Mutex::new(Controller::new(Duration::from_secs(1))));
    let harness = ServiceHarness {
        controller_probe: Some(Arc::clone(&controller)),
        ..ServiceHarness::default()
    };
    let backend = backend_with_services(harness);
    let hint_target = HintTarget::parse("1001@internal").unwrap();
    let effects = {
        controller.lock().unwrap().begin_phone_call(
            CallId(7),
            binding(),
            Codec::Pcmu,
            Instant::now(),
        )
    };
    assert!(!effects.is_empty());

    backend.persistence().get("driver", "key").unwrap();
    backend.hints().lookup(&hint_target).unwrap();
    let hint_controller = Arc::clone(&controller);
    backend
        .hints()
        .subscribe(
            &hint_target,
            Arc::new(move |_| {
                assert!(
                    hint_controller.try_lock().is_ok(),
                    "hint callback entered while the controller was locked"
                );
            }),
        )
        .unwrap();
    let recording_controller = controller;
    backend
        .recordings()
        .start_recording(
            PbxCallId(1),
            RecordingTarget::Automatic,
            "",
            Arc::new(move |_| {
                assert!(
                    recording_controller.try_lock().is_ok(),
                    "recording callback entered while the controller was locked"
                );
            }),
        )
        .unwrap();
}

#[tokio::test]
async fn controller_lock_is_released_before_backend_execution() {
    let controller = Arc::new(Mutex::new(Controller::new(Duration::from_secs(1))));
    let effects = {
        controller.lock().unwrap().begin_phone_call(
            CallId(7),
            binding(),
            Codec::Pcmu,
            Instant::now(),
        )
    };
    let backend = FakeBackend {
        events: Arc::new(Mutex::new(Vec::new())),
        advanced_operations: Arc::new(Mutex::new(Vec::new())),
        capabilities: FakeCapabilities::default(),
        fail: None,
        controller_probe: Some(controller),
    };
    execute_effects(&backend, effects, |_| async { Ok::<_, FakeError>(()) })
        .await
        .unwrap();
}
