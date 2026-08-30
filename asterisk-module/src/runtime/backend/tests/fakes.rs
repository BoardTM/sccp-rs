//! Fake backend and direct-service capabilities for executor tests.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FakeError(pub(super) &'static str);

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for FakeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum AdvancedOperation {
    ConferenceDestination(ConferenceDestinationOperation),
    Forward(ForwardingOperation),
    Voicemail(VoicemailOperation),
    Transfer(TransferCompletion),
    Bridge(BridgeOperation),
    Barge(BargeOperation),
    Announcement(ConferenceAnnouncementOperation),
    Pickup(PickupOperation),
    Parking(ParkingOperation),
    Management(ManagementEvent),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ServiceRequest {
    Get(String, String),
    Put(String, String, String),
    Delete(String, String),
    HintLookup(String, String),
    HintSubscribe(String, String),
    RecordingStart(PbxCallId, String, String),
    RecordingId,
    RecordingState,
    RecordingMute(RecordingDirection, bool),
    RecordingStop,
}

#[derive(Clone, Default)]
pub(super) struct ServiceHarness {
    pub(super) requests: Arc<Mutex<Vec<ServiceRequest>>>,
    pub(super) failures: Arc<Mutex<HashSet<&'static str>>>,
    pub(super) controller_probe: Option<Arc<Mutex<Controller>>>,
}

impl ServiceHarness {
    pub(super) fn record(
        &self,
        operation: &'static str,
        request: ServiceRequest,
    ) -> Result<(), FakeError> {
        self.requests.lock().unwrap().push(request);
        if let Some(controller) = &self.controller_probe {
            assert!(
                controller.try_lock().is_ok(),
                "direct service operation ran while the controller was locked"
            );
        }
        if self.failures.lock().unwrap().contains(operation) {
            Err(FakeError(operation))
        } else {
            Ok(())
        }
    }

    pub(super) fn fail(&self, operation: &'static str) {
        self.failures.lock().unwrap().insert(operation);
    }
}

#[derive(Clone, Default)]
pub(super) struct FakePersistence {
    pub(super) harness: ServiceHarness,
}

impl PersistentStore for FakePersistence {
    fn get(&self, family: &str, key: &str) -> Result<Option<String>, PersistenceError> {
        self.harness
            .record(
                "persistence:get",
                ServiceRequest::Get(family.into(), key.into()),
            )
            .map_err(|_| PersistenceError::Backend { operation: "get" })?;
        Ok(Some("stored".into()))
    }

    fn put(&self, family: &str, key: &str, value: &str) -> Result<(), PersistenceError> {
        self.harness
            .record(
                "persistence:put",
                ServiceRequest::Put(family.into(), key.into(), value.into()),
            )
            .map_err(|_| PersistenceError::Backend { operation: "put" })
    }

    fn delete(&self, family: &str, key: &str) -> Result<(), PersistenceError> {
        self.harness
            .record(
                "persistence:delete",
                ServiceRequest::Delete(family.into(), key.into()),
            )
            .map_err(|_| PersistenceError::Backend {
                operation: "delete",
            })
    }
}

#[derive(Clone, Default)]
pub(super) struct FakeHints {
    pub(super) harness: ServiceHarness,
}

pub(super) struct FakeHintSubscription;

impl HintProvider for FakeHints {
    type Subscription = FakeHintSubscription;
    type Error = FakeError;

    fn lookup(&self, target: &HintTarget) -> Result<Option<HintSnapshot>, Self::Error> {
        self.harness.record(
            "hints:lookup",
            ServiceRequest::HintLookup(target.context().into(), target.extension().into()),
        )?;
        Ok(Some(HintSnapshot {
            target: target.clone(),
            state: ExtensionState::IDLE,
            reason: HintUpdateReason::Device,
            caller: None,
        }))
    }

    fn subscribe(
        &self,
        target: &HintTarget,
        callback: HintCallback,
    ) -> Result<Self::Subscription, Self::Error> {
        self.harness.record(
            "hints:subscribe",
            ServiceRequest::HintSubscribe(target.context().into(), target.extension().into()),
        )?;
        callback(HintSnapshot {
            target: target.clone(),
            state: ExtensionState::RINGING,
            reason: HintUpdateReason::Device,
            caller: None,
        });
        Ok(FakeHintSubscription)
    }
}

#[derive(Clone, Default)]
pub(super) struct FakeRecordings {
    pub(super) harness: ServiceHarness,
}

pub(super) struct FakeRecordingSession {
    pub(super) harness: ServiceHarness,
    pub(super) state: RecordingState,
}

impl RecordingSessionControl for FakeRecordingSession {
    type Error = FakeError;

    fn id(&self) -> Result<String, Self::Error> {
        self.harness
            .record("recording:id", ServiceRequest::RecordingId)?;
        Ok("recording-1".into())
    }

    fn state(&self) -> Result<RecordingState, Self::Error> {
        self.harness
            .record("recording:state", ServiceRequest::RecordingState)?;
        Ok(self.state)
    }

    fn stop(&mut self) -> Result<(), Self::Error> {
        self.harness
            .record("recording:stop", ServiceRequest::RecordingStop)?;
        self.state = RecordingState::Stopped;
        Ok(())
    }

    fn set_muted(
        &mut self,
        direction: RecordingDirection,
        muted: bool,
    ) -> Result<usize, Self::Error> {
        self.harness.record(
            "recording:mute",
            ServiceRequest::RecordingMute(direction, muted),
        )?;
        self.state = if muted {
            RecordingState::Muted
        } else {
            RecordingState::Active
        };
        Ok(1)
    }
}

impl RecordingProvider for FakeRecordings {
    type Session = FakeRecordingSession;
    type StartError = FakeError;

    fn start_recording(
        &self,
        call_id: PbxCallId,
        filename: &str,
        options: &str,
        callback: RecordingCallback,
    ) -> Result<Self::Session, Self::StartError> {
        self.harness.record(
            "recording:start",
            ServiceRequest::RecordingStart(call_id, filename.into(), options.into()),
        )?;
        callback(RecordingEvent::Started);
        Ok(FakeRecordingSession {
            harness: self.harness.clone(),
            state: RecordingState::Active,
        })
    }
}

#[derive(Default)]
pub(super) struct FakeCapabilities {
    pub(super) persistence: FakePersistence,
    pub(super) hints: FakeHints,
    pub(super) recordings: FakeRecordings,
}

impl FakeCapabilities {
    pub(super) fn with_harness(harness: ServiceHarness) -> Self {
        Self {
            persistence: FakePersistence {
                harness: harness.clone(),
            },
            hints: FakeHints {
                harness: harness.clone(),
            },
            recordings: FakeRecordings { harness },
        }
    }
}

pub(super) struct FakeBackend {
    pub(super) events: Arc<Mutex<Vec<&'static str>>>,
    pub(super) advanced_operations: Arc<Mutex<Vec<AdvancedOperation>>>,
    pub(super) capabilities: FakeCapabilities,
    pub(super) fail: Option<&'static str>,
    pub(super) controller_probe: Option<Arc<Mutex<Controller>>>,
}

impl FakeBackend {
    pub(super) fn record(&self, operation: &'static str) -> Result<(), FakeError> {
        self.events.lock().unwrap().push(operation);
        if let Some(controller) = &self.controller_probe {
            assert!(
                controller.try_lock().is_ok(),
                "backend operation ran while the controller was locked"
            );
        }
        if self.fail == Some(operation) {
            Err(FakeError(operation))
        } else {
            Ok(())
        }
    }
}

impl PbxServiceCapabilities for FakeBackend {
    type Persistence = FakePersistence;
    type Hints = FakeHints;
    type Recordings = FakeRecordings;

    fn persistence(&self) -> &Self::Persistence {
        &self.capabilities.persistence
    }

    fn hints(&self) -> &Self::Hints {
        &self.capabilities.hints
    }

    fn recordings(&self) -> &Self::Recordings {
        &self.capabilities.recordings
    }
}

impl PbxBackendError for FakeBackend {
    type Error = FakeError;
}

impl ChannelBackend for FakeBackend {
    fn create_channel(
        &self,
        _: CallId,
        _: PbxCallId,
        _: &LineBinding,
        _: Codec,
    ) -> Result<(), Self::Error> {
        self.record("backend:create")
    }

    fn create_consultation_channel(
        &self,
        _: PbxCallId,
        _: CallId,
        _: PbxCallId,
        _: &LineBinding,
        _: Codec,
    ) -> Result<(), Self::Error> {
        self.record("backend:create")
    }

    fn start_routing(&self, _: PbxCallId, _: &str, _: &str) -> Result<(), Self::Error> {
        self.record("backend:route")
    }

    fn answer(&self, _: PbxCallId) -> Result<(), Self::Error> {
        self.record("backend:answer")
    }

    fn hangup(&self, _: PbxCallId) -> Result<(), Self::Error> {
        self.record("backend:hangup")
    }

    fn send_digit(&self, _: PbxCallId, _: char) -> Result<(), Self::Error> {
        self.record("backend:digit")
    }

    fn hold(&self, _: PbxCallId) -> Result<(), Self::Error> {
        self.record("backend:hold")
    }

    fn resume(&self, _: PbxCallId) -> Result<(), Self::Error> {
        self.record("backend:resume")
    }
}

impl SupplementaryBackend for FakeBackend {
    fn forward(&self, operation: &ForwardingOperation) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Forward(operation.clone()));
        self.record("backend:forward")
    }

    fn voicemail(&self, operation: &VoicemailOperation) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Voicemail(operation.clone()));
        self.record("backend:voicemail")
    }

    fn start_conference_destination(
        &self,
        operation: &ConferenceDestinationOperation,
    ) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::ConferenceDestination(operation.clone()));
        self.record("backend:conference-destination")
    }
}

impl MediaBackend for FakeBackend {
    fn audio_encryption_capabilities(&self) -> LocalEncryptionCapabilities {
        LocalEncryptionCapabilities::default()
    }

    fn configure_media(
        &self,
        _: PbxCallId,
        _: &DeviceId,
        remote: MediaEndpoint,
        _: Codec,
    ) -> Result<MediaEndpoint, Self::Error> {
        self.record("backend:media")?;
        Ok(remote)
    }
}

impl BridgeBackend for FakeBackend {
    fn transfer(&self, operation: &TransferCompletion) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Transfer(operation.clone()));
        self.record("backend:bridge-transfer")
    }

    fn bridge(&self, operation: &BridgeOperation) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Bridge(operation.clone()));
        let operation = match operation {
            BridgeOperation::Create { .. } => "backend:bridge-create",
            BridgeOperation::Destroy { .. } => "backend:bridge-destroy",
            BridgeOperation::AddParticipant { .. } => "backend:bridge-add",
            BridgeOperation::RemoveParticipant { .. } => "backend:bridge-remove",
            BridgeOperation::MergeConsultation { .. } => "backend:bridge-merge-consultation",
            BridgeOperation::MergeCalls { .. } => "backend:bridge-merge-calls",
            BridgeOperation::MergeParticipant { .. } => "backend:bridge-merge-participant",
            BridgeOperation::SetParticipantMuted { muted: true, .. } => {
                "backend:bridge-mute-participant"
            }
            BridgeOperation::SetParticipantMuted { muted: false, .. } => {
                "backend:bridge-unmute-participant"
            }
            BridgeOperation::RemoveConferenceParticipant { .. } => {
                "backend:bridge-remove-conference-participant"
            }
            BridgeOperation::SetParticipantMusicOnHold { enabled: true, .. } => {
                "backend:bridge-start-music"
            }
            BridgeOperation::SetParticipantMusicOnHold { enabled: false, .. } => {
                "backend:bridge-stop-music"
            }
        };
        self.record(operation)
    }

    fn barge(&self, operation: &BargeOperation) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Barge(operation.clone()));
        self.record(match operation {
            BargeOperation::Join { .. } => "backend:barge-join",
            BargeOperation::Leave { .. } => "backend:barge-leave",
        })
    }

    fn announce(&self, operation: &ConferenceAnnouncementOperation) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Announcement(operation.clone()));
        self.record("backend:conference-announcement")
    }
}

impl CallServiceBackend for FakeBackend {
    fn pickup(&self, operation: &PickupOperation) -> Result<PickupOutcome, Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Pickup(operation.clone()));
        let operation = match operation {
            PickupOperation::Group { .. } => "backend:pickup-group",
            PickupOperation::Directed { .. } => "backend:pickup-directed",
        };
        self.record(operation)?;
        Ok(PickupOutcome {
            calling_name: "Caller".into(),
            calling_number: "2100".into(),
            connected_name: "Target".into(),
            connected_number: "2200".into(),
            redirecting_name: "Reception".into(),
            redirecting_number: "2000".into(),
        })
    }

    fn parking(&self, operation: &ParkingOperation) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Parking(operation.clone()));
        let operation = match operation {
            ParkingOperation::Park { .. } => "backend:park",
            ParkingOperation::Retrieve { .. } => "backend:parking-retrieve",
        };
        self.record(operation)
    }
}

impl ManagementBackend for FakeBackend {
    fn publish_management_event(&self, event: &ManagementEvent) -> Result<(), Self::Error> {
        self.advanced_operations
            .lock()
            .unwrap()
            .push(AdvancedOperation::Management(event.clone()));
        self.record("backend:management")
    }
}
