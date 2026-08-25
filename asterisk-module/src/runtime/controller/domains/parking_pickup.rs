use super::super::*;

impl Controller {
    /// Put a newly-created handset call into directed-pickup digit collection.
    /// The configured context and answer policy are retained until Dial or `#`
    /// completes the request.
    pub fn begin_directed_pickup(
        &mut self,
        call_id: CallId,
        permitted: bool,
        enabled: bool,
        context: String,
        answer: bool,
    ) -> Result<(), PickupRejection> {
        if !enabled {
            return Err(PickupRejection::Disabled);
        }
        if !permitted {
            return Err(PickupRejection::Permission);
        }
        let appearance = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(PickupRejection::Unavailable)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(PickupRejection::Unavailable)?;
        if call.state != CallState::Collecting
            || appearance.state != CallState::Collecting
            || call.active_appearance != Some(appearance.id)
            || call.pending_pickup.is_some()
            || !self.devices.contains_key(&appearance.device_id)
        {
            return Err(PickupRejection::Conflict);
        }
        if context.trim().is_empty() {
            return Err(PickupRejection::Unavailable);
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.state = CallState::PickupCollecting;
            call.digits.clear();
            call.digit_deadline = None;
            call.last_digit_at = None;
            call.simulated_enbloc_eligible = false;
            call.pending_pickup = Some(PendingDirectedPickup { context, answer });
        }
        if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance.id) {
            appearance.state = CallState::PickupCollecting;
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(())
    }

    /// Attempt the oldest ringing call permitted by this channel's configured
    /// numeric or named pickup groups.
    pub fn group_pickup(
        &mut self,
        call_id: CallId,
        permitted: bool,
        answer: bool,
    ) -> Result<Vec<DriverEffect>, PickupRejection> {
        if !permitted {
            return Err(PickupRejection::Permission);
        }
        let appearance = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(PickupRejection::Unavailable)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(PickupRejection::Unavailable)?;
        if call.state != CallState::Collecting
            || appearance.state != CallState::Collecting
            || call.active_appearance != Some(appearance.id)
            || call.pending_pickup.is_some()
            || !self.devices.contains_key(&appearance.device_id)
        {
            return Err(PickupRejection::Conflict);
        }
        let pbx_id = appearance.pbx_id;
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.state = if answer {
                CallState::Connected
            } else {
                CallState::Ringing
            };
            call.active_appearance = answer.then_some(appearance.id);
            call.digit_deadline = None;
        }
        if let Some(stored) = self.call_registry.appearances.get_mut(&appearance.id) {
            stored.state = if answer {
                CallState::Connected
            } else {
                CallState::Ringing
            };
            stored.audio = if answer {
                MediaStreamState::Opening
            } else {
                MediaStreamState::Closed
            };
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::Pickup {
                operation: PickupOperation::Group {
                    call_id: pbx_id,
                    device_id: appearance.device_id,
                    handset_call_id: appearance.sccp_id,
                    codec: appearance.codec,
                    answer,
                },
            }
            .into(),
        ])
    }

    /// Start parking the active PBX call owned by this handset appearance.
    /// The final assigned slot arrives asynchronously from the backend.
    pub fn park(
        &mut self,
        call_id: CallId,
        enabled: bool,
        lot: Option<String>,
    ) -> Result<Vec<DriverEffect>, ParkingRejection> {
        if !enabled {
            return Err(ParkingRejection::Disabled);
        }
        let appearance = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(ParkingRejection::Unavailable)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(ParkingRejection::Unavailable)?;
        if self.redirect_claims.contains(&appearance.pbx_id)
            || call.state != CallState::Connected
            || appearance.state != CallState::Connected
            || call.active_appearance != Some(appearance.id)
        {
            return Err(ParkingRejection::Conflict);
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.state = CallState::Parking;
        }
        if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance.id) {
            appearance.state = CallState::Parking;
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::Parking {
                operation: ParkingOperation::Park {
                    call_id: appearance.pbx_id,
                    lot,
                },
            }
            .into(),
        ])
    }

    /// Roll back a synchronous or timed-out parking attempt without changing
    /// call ownership or allocating another PBX identity.
    pub fn parking_failed(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        if self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .is_none_or(|call| call.state != CallState::Parking)
        {
            return Vec::new();
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.state = CallState::Connected;
        }
        if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance.id) {
            appearance.state = CallState::Connected;
        }
        debug_assert!(self.invariant_error().is_none());
        vec![
            HandsetEffect::SetCallState {
                device_id: appearance.device_id,
                call_id,
                state: HandsetCallState::Connected,
                stop_media: false,
            }
            .into(),
        ]
    }

    /// Publish the assigned slot before closing the owner's now-parked SCCP
    /// channel. The backend retains the parked peer independently.
    pub fn parking_confirmed(&mut self, call_id: CallId, slot: u32) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        if self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .is_none_or(|call| call.state != CallState::Parking)
        {
            return Vec::new();
        }
        let mut effects = vec![
            HandsetEffect::SetCallInfo {
                device_id: appearance.device_id.clone(),
                call_id,
                info: CallInfo {
                    direction: CallDirection::Outbound,
                    calling_name: String::new(),
                    calling_number: String::new(),
                    called_name: "Parked".into(),
                    called_number: slot.to_string(),
                    ..CallInfo::default()
                },
            }
            .into(),
            HandsetEffect::SetCallState {
                device_id: appearance.device_id,
                call_id,
                state: HandsetCallState::Park,
                stop_media: true,
            }
            .into(),
        ];
        effects.extend(self.hangup(call_id));
        effects
    }

    /// Create the retriever's PBX channel and invoke the backend parking
    /// application. The registry claim is owned by the adapter so competing
    /// handsets cannot create a second retriever.
    pub fn begin_parking_retrieval(
        &mut self,
        call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        lot: Option<String>,
        slot: u32,
        info: CallInfo,
    ) -> Result<Vec<DriverEffect>, ParkingRejection> {
        if self.call_registry.by_sccp.contains_key(&call_id)
            || !self.devices.contains_key(&binding.device_id)
            || slot == 0
        {
            return Err(ParkingRejection::Conflict);
        }
        let pbx_id = self.allocate_pbx_id();
        let appearance_id = self.allocate_appearance_id();
        let device_id = binding.device_id.clone();
        let line_instance = binding.line_instance;
        let pbx_call = PbxCall {
            id: pbx_id,
            line: binding.line.number.clone(),
            context: binding.line.context.clone(),
            direction: CallDirection::Outbound,
            state: CallState::Retrieving,
            outbound_phase: None,
            outbound_identity_stage: OutboundIdentityStage::Awaiting,
            digits: String::new(),
            privacy: true,
            metadata: CallMetadata::default(),
            pending_pickup: None,
            appearance_ids: Vec::new(),
            active_appearance: Some(appearance_id),
            digit_deadline: None,
            last_digit_at: None,
            simulated_enbloc_eligible: false,
            overlap_enabled: false,
        };
        let appearance = CallAppearance {
            id: appearance_id,
            sccp_id: call_id,
            pbx_id,
            device_id: device_id.clone(),
            line_instance,
            state: CallState::Retrieving,
            ring_mode: binding.appearance.ring_mode,
            privacy: true,
            info: info.clone(),
            codec,
            audio: MediaStreamState::Closed,
            audio_transmit: MediaStreamState::Closed,
            video: VideoMediaState::default(),
            auto_answer_mode: None,
        };
        if !self.insert_pbx_call(pbx_call, appearance) {
            return Err(ParkingRejection::Conflict);
        }
        self.select_line(&device_id, line_instance);
        self.set_call_selected(&device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::CreateChannel {
                handset_call_id: call_id,
                call_id: pbx_id,
                binding: Box::new(binding),
                codec,
            }
            .into(),
            HandsetEffect::SetCallInfo {
                device_id,
                call_id,
                info,
            }
            .into(),
            PbxEffect::Parking {
                operation: ParkingOperation::Retrieve {
                    call_id: pbx_id,
                    lot,
                    slot: slot.to_string(),
                },
            }
            .into(),
        ])
    }

    /// Finish retrieval after the backend reports that this claimant won.
    pub fn parking_retrieved(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        if self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .is_none_or(|call| call.state != CallState::Retrieving)
        {
            return Vec::new();
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.state = CallState::Connected;
        }
        if let Some(stored) = self.call_registry.appearances.get_mut(&appearance.id) {
            stored.state = CallState::Connected;
            stored.audio = MediaStreamState::Opening;
        }
        debug_assert!(self.invariant_error().is_none());
        vec![
            HandsetEffect::SetCallState {
                device_id: appearance.device_id.clone(),
                call_id,
                state: HandsetCallState::Connected,
                stop_media: false,
            }
            .into(),
            HandsetEffect::BeginMedia {
                device_id: appearance.device_id,
                call_id,
                codec: appearance.codec,
            }
            .into(),
        ]
    }

    pub fn parking_retrieval_failed(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        if self
            .appearance_for_call(call_id)
            .is_some_and(|appearance| appearance.state == CallState::Retrieving)
        {
            self.hangup(call_id)
        } else {
            Vec::new()
        }
    }
}
