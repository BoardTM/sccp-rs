//! SCCP/SIP call-state coordination.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use sccp_protocol::{
    CallDirection, CallId, CallInfo, CallState, Codec, Command as SccpCommand,
    CommandAction as SccpCommandAction, DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
    DEFAULT_AUDIO_PACKET_MS, DeviceEvent, DeviceEventKind, DeviceId, DeviceRegistration, Digit,
    DtmfMode, Event as SccpEvent, LineInstance, MediaEndpoint, MediaStatus, MediaTrafficClass,
    ServerHandle, SessionGeneration, SoftKey, StationMediaCapabilities,
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::{AppConfig, LineConfig};
use crate::media::{
    AllocatedRelay, DirectMediaRoute, MediaMode, MediaPolicy, PortAllocator, RelayAddresses,
    RelayError, RtpRelay,
};
use crate::sip::{
    AccountConfig, AccountId, DialogState, RemoteMedia, SipCallId, SipEvent, SipHandle,
    SipMediaState,
};

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error("unable to initialize the RTP relay: {0}")]
    Relay(#[from] RelayError),
    #[error("SCCP event stream stopped")]
    SccpStopped,
    #[error("SIP event stream stopped")]
    SipStopped,
}

#[derive(Clone, Debug)]
struct AccountBinding {
    device: DeviceId,
    line_instance: u32,
    line: LineConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BridgeCallState {
    Collecting,
    Calling,
    Ringing,
    Connected,
    Held,
    TransferCollecting,
}

enum RelayPath {
    Reserved(AllocatedRelay),
    Running(RtpRelay),
}

enum MediaRouteState {
    Unallocated,
    Relay(RelayPath),
    SwitchingToDirect(RelayPath),
    Direct,
    SwitchingToRelay(AllocatedRelay),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaRouteKind {
    Unallocated,
    Relay,
    Direct,
    Switching,
}

impl MediaRouteState {
    fn kind(&self) -> MediaRouteKind {
        match self {
            Self::Unallocated => MediaRouteKind::Unallocated,
            Self::Relay(_) => MediaRouteKind::Relay,
            Self::Direct => MediaRouteKind::Direct,
            Self::SwitchingToDirect(_) | Self::SwitchingToRelay(_) => MediaRouteKind::Switching,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaRouteAction {
    None,
    SetInitialDirect,
    ReofferDirect,
    ReofferRelay,
}

#[derive(Clone, Copy)]
enum PhoneMediaState {
    Closed,
    Opening,
    Ready(MediaEndpoint),
    Started {
        source: MediaEndpoint,
        target: MediaEndpoint,
    },
}

impl PhoneMediaState {
    fn source(self) -> Option<MediaEndpoint> {
        match self {
            Self::Ready(source) | Self::Started { source, .. } => Some(source),
            Self::Closed | Self::Opening => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InboundProgress {
    NotApplicable,
    WaitingForPhone,
    Ringing,
    AnswerRequested,
    Answered,
}

struct PendingIncomingCall {
    account: AccountId,
    relay: AllocatedRelay,
}

struct RegisteredStation {
    generation: SessionGeneration,
    registration: DeviceRegistration,
    capabilities: StationMediaCapabilities,
}

fn is_current_session(current: Option<SessionGeneration>, incoming: SessionGeneration) -> bool {
    current == Some(incoming)
}

struct BridgeCall {
    device: DeviceId,
    line_instance: u32,
    account: AccountId,
    sip_call: Option<SipCallId>,
    direction: CallDirection,
    state: BridgeCallState,
    digits: String,
    digit_deadline: Option<Instant>,
    phone_media: PhoneMediaState,
    remote_media: Option<RemoteMedia>,
    media_route: MediaRouteState,
    codec: Codec,
    inbound_progress: InboundProgress,
}

pub struct Coordinator {
    config: Arc<AppConfig>,
    sccp: ServerHandle,
    sccp_events: mpsc::Receiver<SccpEvent>,
    sip: SipHandle,
    sip_events: mpsc::UnboundedReceiver<SipEvent>,
    allocator: PortAllocator,
    media_policy: MediaPolicy,
    registrations: HashMap<DeviceId, RegisteredStation>,
    accounts_by_line: HashMap<(DeviceId, u32), AccountId>,
    bindings_by_account: HashMap<AccountId, AccountBinding>,
    calls: HashMap<CallId, BridgeCall>,
    sccp_by_sip: HashMap<SipCallId, CallId>,
    pending_incoming: HashMap<SipCallId, PendingIncomingCall>,
    last_dialed: HashMap<(DeviceId, u32), String>,
}

impl Coordinator {
    pub fn new(
        config: Arc<AppConfig>,
        sccp: ServerHandle,
        sccp_events: mpsc::Receiver<SccpEvent>,
        sip: SipHandle,
        sip_events: mpsc::UnboundedReceiver<SipEvent>,
    ) -> Result<Self, CoordinatorError> {
        let allocator = PortAllocator::new(
            config.media.bind_address,
            config.media.advertised_address,
            config.media.port_range.clone(),
        )?;
        let media_policy = MediaPolicy::new(
            config
                .media
                .direct_routes
                .iter()
                .map(|route| DirectMediaRoute {
                    phones: route.phones.clone(),
                    sip: route.sip.clone(),
                })
                .collect(),
        );
        Ok(Self {
            config,
            sccp,
            sccp_events,
            sip,
            sip_events,
            allocator,
            media_policy,
            registrations: HashMap::new(),
            accounts_by_line: HashMap::new(),
            bindings_by_account: HashMap::new(),
            calls: HashMap::new(),
            sccp_by_sip: HashMap::new(),
            pending_incoming: HashMap::new(),
            last_dialed: HashMap::new(),
        })
    }

    pub async fn run(mut self) -> Result<(), CoordinatorError> {
        let mut deadlines = tokio::time::interval(Duration::from_millis(100));
        deadlines.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = self.sccp_events.recv() => {
                    self.handle_sccp(event.ok_or(CoordinatorError::SccpStopped)?).await;
                }
                event = self.sip_events.recv() => {
                    self.handle_sip(event.ok_or(CoordinatorError::SipStopped)?).await;
                }
                _ = deadlines.tick() => self.handle_digit_deadlines().await,
            }
        }
    }

    async fn handle_sccp(&mut self, event: SccpEvent) {
        let DeviceEvent {
            device_id,
            session_generation,
            event,
        } = match event {
            SccpEvent::SessionError { peer, error } => {
                warn!(%peer, %error, "SCCP session ended with an error");
                return;
            }
            SccpEvent::ProtocolWarning {
                peer,
                device_id,
                message_id,
                error,
            } => {
                warn!(%peer, ?device_id, message_id = format_args!("0x{message_id:04x}"), %error, "ignored malformed SCCP application message");
                return;
            }
            SccpEvent::Device(event) => event,
        };
        if !matches!(event, DeviceEventKind::Registered(_))
            && !is_current_session(
                self.registrations
                    .get(&device_id)
                    .map(|station| station.generation),
                session_generation,
            )
        {
            return;
        }
        match event {
            DeviceEventKind::Registered(registration) => {
                self.register_phone(session_generation, registration).await
            }
            DeviceEventKind::Disconnected { .. } => self.unregister_phone(&device_id).await,
            DeviceEventKind::Capabilities { capabilities } => {
                if let Some(station) = self.registrations.get_mut(&device_id) {
                    station.capabilities = capabilities;
                }
            }
            DeviceEventKind::OffHook {
                call_id,
                line_instance,
            } => {
                self.phone_off_hook(device_id, call_id, line_instance.get())
                    .await;
            }
            DeviceEventKind::OnHook { call_id, .. } => self.end_from_phone(call_id).await,
            DeviceEventKind::Digit { call_id, digit, .. } => self.phone_digit(call_id, digit).await,
            DeviceEventKind::EnblocCall {
                call_id,
                line_instance,
                number,
            } => {
                if !self.calls.contains_key(&call_id) {
                    self.begin_outbound(device_id, call_id, line_instance.get())
                        .await;
                }
                if let Some(call) = self.calls.get_mut(&call_id) {
                    call.digits = number;
                    call.digit_deadline = None;
                }
                self.dial(call_id).await;
            }
            DeviceEventKind::SoftKey {
                call_id, soft_key, ..
            } => {
                if let Some(call_id) = call_id {
                    self.soft_key(call_id, soft_key).await;
                }
            }
            DeviceEventKind::FeatureButton { .. }
            | DeviceEventKind::DoNotDisturbButton { .. }
            | DeviceEventKind::MobilityButton { .. }
            | DeviceEventKind::VoicemailButton { .. } => {}
            DeviceEventKind::ParkingLotButton { .. }
            | DeviceEventKind::ParkingMenuSelection { .. } => {}
            DeviceEventKind::PhoneServiceResponse { .. }
            | DeviceEventKind::ConferenceListAction { .. } => {}
            DeviceEventKind::LineButton { .. } | DeviceEventKind::HookFlash { .. } => {}
            DeviceEventKind::ReceiveChannelOpened {
                call_id,
                status,
                endpoint,
                ..
            } => {
                if status == MediaStatus::Ok {
                    self.phone_media_ready(call_id, endpoint).await;
                } else {
                    self.fail_call(call_id, "Phone rejected media channel")
                        .await;
                }
            }
            DeviceEventKind::TransmitChannelImplied { .. }
            | DeviceEventKind::TransmitChannelStarted { .. }
            | DeviceEventKind::MultimediaReceiveChannelOpened { .. }
            | DeviceEventKind::MultimediaReceiveChannelFailed { .. }
            | DeviceEventKind::MultimediaReceiveChannelTimedOut { .. }
            | DeviceEventKind::MultimediaTransmitStarted { .. }
            | DeviceEventKind::MultimediaTransmitFailed { .. }
            | DeviceEventKind::MultimediaTransmitTimedOut { .. }
            | DeviceEventKind::HandsetAcknowledgementTimedOut { .. }
            | DeviceEventKind::MulticastReceptionStarted { .. }
            | DeviceEventKind::MulticastReceptionFailed { .. }
            | DeviceEventKind::MulticastReceptionTimedOut { .. }
            | DeviceEventKind::MulticastTransmissionStarted { .. }
            | DeviceEventKind::MulticastTransmissionFailed { .. }
            | DeviceEventKind::HeadsetStatusChanged { .. }
            | DeviceEventKind::MediaPathChanged { .. } => {}
            DeviceEventKind::MediaTransmissionFailed { call_id, .. } => {
                self.fail_call(call_id, "Phone reported media transmission failure")
                    .await;
            }
            DeviceEventKind::ConnectionStatisticsCollected { .. } => {}
            DeviceEventKind::Alarm { severity, text, .. } => {
                warn!(%device_id, ?severity, %text, "SCCP handset alarm");
            }
            DeviceEventKind::XmlAlarm { telemetry } => {
                warn!(
                    %device_id,
                    summary = ?telemetry.summary(),
                    opaque = telemetry.is_opaque(),
                    "SCCP handset XML alarm"
                );
            }
            DeviceEventKind::LocationInformation { telemetry } => {
                debug!(
                    %device_id,
                    summary = ?telemetry.summary(),
                    opaque = telemetry.is_opaque(),
                    "SCCP handset location information"
                );
            }
            DeviceEventKind::UnhandledMessage { message } => {
                debug!(%device_id, ?message, "unhandled SCCP message");
            }
        }
    }

    async fn handle_sip(&mut self, event: SipEvent) {
        match event {
            SipEvent::Registration {
                account,
                registered,
                status,
                reason,
            } => {
                if let Some(binding) = self.bindings_by_account.get(&account) {
                    info!(device = %binding.device, line = binding.line_instance, %registered, %status, %reason, "SIP registration changed");
                }
            }
            SipEvent::IncomingCall {
                account,
                call,
                remote_uri,
                remote_media,
            } => {
                self.incoming_call(account, call, remote_uri, remote_media)
                    .await;
            }
            SipEvent::CallState {
                call,
                state,
                status,
                reason,
                ..
            } => {
                self.sip_call_state(call, state, status, reason).await;
            }
            SipEvent::MediaState {
                call,
                state,
                remote,
            } => {
                self.sip_media_state(call, state, remote).await;
            }
            SipEvent::MediaReofferCompleted {
                call,
                accepted,
                status,
                reason,
            } => {
                self.media_reoffer_completed(call, accepted, status, reason)
                    .await;
            }
            SipEvent::MessageWaiting { account, waiting } => {
                if let Some(binding) = self.bindings_by_account.get(&account) {
                    self.send_sccp(SccpCommand::new(
                        binding.device.clone(),
                        SccpCommandAction::SetMwi {
                            line_instance: LineInstance::new(binding.line_instance),
                            enabled: waiting,
                        },
                    ))
                    .await;
                }
            }
            SipEvent::MediaAdvertisementRequired {
                account,
                call,
                reply,
            } => {
                let address = self.media_advertisement(account, call);
                let _ = reply.send(address);
            }
        }
    }

    async fn register_phone(
        &mut self,
        generation: SessionGeneration,
        registration: DeviceRegistration,
    ) {
        let device = registration.id.clone();
        match self.registrations.get(&device) {
            Some(current) if current.generation >= generation => return,
            Some(_) => self.unregister_phone(&device).await,
            None => {}
        }
        self.registrations.insert(
            device.clone(),
            RegisteredStation {
                generation,
                registration,
                capabilities: StationMediaCapabilities::default(),
            },
        );
        let Some(phone) = self.config.phone(&device).cloned() else {
            return;
        };
        for (index, line) in phone.lines.into_iter().enumerate() {
            let line_instance = index as u32 + 1;
            let account_config = sip_account_config(&line);
            match self.sip.add_account(account_config).await {
                Ok(account) => {
                    self.accounts_by_line
                        .insert((device.clone(), line_instance), account);
                    self.bindings_by_account.insert(
                        account,
                        AccountBinding {
                            device: device.clone(),
                            line_instance,
                            line,
                        },
                    );
                }
                Err(error) => warn!(%device, %line_instance, %error, "unable to add SIP account"),
            }
        }
    }

    async fn unregister_phone(&mut self, device: &DeviceId) {
        self.registrations.remove(device);
        let calls: Vec<_> = self
            .calls
            .iter()
            .filter_map(|(id, call)| (&call.device == device).then_some(*id))
            .collect();
        for call_id in calls {
            self.end_call(call_id, false).await;
        }
        let accounts: Vec<_> = self
            .accounts_by_line
            .iter()
            .filter_map(|((id, _), account)| (id == device).then_some(*account))
            .collect();
        self.accounts_by_line.retain(|(id, _), _| id != device);
        for account in accounts {
            self.bindings_by_account.remove(&account);
            if let Err(error) = self.sip.remove_account(account).await {
                debug!(%error, "SIP account was already removed");
            }
        }
    }

    async fn begin_outbound(&mut self, device: DeviceId, call_id: CallId, line_instance: u32) {
        let Some(account) = self
            .accounts_by_line
            .get(&(device.clone(), line_instance))
            .copied()
        else {
            self.prompt(&device, call_id, "SIP line is not registered")
                .await;
            return;
        };
        if self.calls.contains_key(&call_id) {
            return;
        }
        let codec = self.preferred_codec(&device);
        self.calls.insert(
            call_id,
            BridgeCall {
                device: device.clone(),
                line_instance,
                account,
                sip_call: None,
                direction: CallDirection::Outbound,
                state: BridgeCallState::Collecting,
                digits: String::new(),
                digit_deadline: None,
                phone_media: PhoneMediaState::Closed,
                remote_media: None,
                media_route: MediaRouteState::Unallocated,
                codec,
                inbound_progress: InboundProgress::NotApplicable,
            },
        );
        self.send_sccp(SccpCommand::new(
            device,
            SccpCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 0,
                text: "Enter number".into(),
            },
        ))
        .await;
    }

    async fn phone_off_hook(&mut self, device: DeviceId, call_id: CallId, line_instance: u32) {
        if self.calls.get(&call_id).is_some_and(|call| {
            call.direction == CallDirection::Inbound && call.state == BridgeCallState::Ringing
        }) {
            if let Some(call) = self.calls.get_mut(&call_id) {
                call.inbound_progress = InboundProgress::AnswerRequested;
            }
            self.answer_incoming(call_id).await;
        } else {
            self.begin_outbound(device, call_id, line_instance).await;
        }
    }

    async fn phone_digit(&mut self, call_id: CallId, digit: Digit) {
        let character = digit.as_char();
        let Some(call) = self.calls.get_mut(&call_id) else {
            return;
        };
        match call.state {
            BridgeCallState::Collecting | BridgeCallState::TransferCollecting => {
                if character == '#' {
                    call.digit_deadline = None;
                } else {
                    call.digits.push(character);
                    call.digit_deadline = Some(
                        Instant::now()
                            + Duration::from_millis(self.config.sip.interdigit_timeout_ms),
                    );
                    return;
                }
            }
            BridgeCallState::Connected => {
                if let Some(sip_call) = call.sip_call
                    && let Err(error) = self.sip.send_dtmf(sip_call, character.to_string()).await
                {
                    warn!(%error, "unable to send DTMF");
                }
                return;
            }
            _ => return,
        }
        if self
            .calls
            .get(&call_id)
            .is_some_and(|call| call.state == BridgeCallState::TransferCollecting)
        {
            self.complete_blind_transfer(call_id).await;
        } else {
            self.dial(call_id).await;
        }
    }

    async fn handle_digit_deadlines(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .calls
            .iter()
            .filter_map(|(id, call)| {
                call.digit_deadline
                    .is_some_and(|deadline| deadline <= now)
                    .then_some(*id)
            })
            .collect();
        for call_id in expired {
            if let Some(call) = self.calls.get_mut(&call_id) {
                call.digit_deadline = None;
            }
            if self
                .calls
                .get(&call_id)
                .is_some_and(|call| call.state == BridgeCallState::TransferCollecting)
            {
                self.complete_blind_transfer(call_id).await;
            } else {
                self.dial(call_id).await;
            }
        }
    }

    async fn dial(&mut self, call_id: CallId) {
        let Some(call) = self.calls.get_mut(&call_id) else {
            return;
        };
        if call.state != BridgeCallState::Collecting || call.digits.is_empty() {
            return;
        }
        call.state = BridgeCallState::Calling;
        call.digit_deadline = None;
        let device = call.device.clone();
        let codec = call.codec;
        self.last_dialed
            .insert((device.clone(), call.line_instance), call.digits.clone());
        let source = match self.allocator.allocate() {
            Ok(relay) => {
                let source = relay_target(relay.addresses, codec);
                call.media_route = MediaRouteState::Relay(RelayPath::Reserved(relay));
                source
            }
            Err(error) => {
                warn!(%error, "unable to reserve RTP relay");
                self.fail_call(call_id, "No media ports available").await;
                return;
            }
        };
        call.phone_media = PhoneMediaState::Opening;
        self.send_sccp(SccpCommand::new(
            device,
            SccpCommandAction::OpenReceiveChannel {
                call_id,
                source: Some(source),
                codec,
                packet_ms: DEFAULT_AUDIO_PACKET_MS,
                max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
                dtmf_mode: DtmfMode::Auto,
                audio_processing: sccp_protocol::AudioProcessingPolicy::default(),
            },
        ))
        .await;
    }

    async fn make_outbound_after_phone_media(&mut self, call_id: CallId) {
        let Some(call) = self.calls.get(&call_id) else {
            return;
        };
        if call.direction != CallDirection::Outbound
            || call.sip_call.is_some()
            || call.digits.is_empty()
        {
            return;
        }
        let MediaRouteState::Relay(RelayPath::Reserved(relay)) = &call.media_route else {
            return;
        };
        let Some(binding) = self.bindings_by_account.get(&call.account) else {
            return;
        };
        let destination = destination_uri(&call.digits, &binding.line.registrar);
        let advertised = socket_v4(relay.addresses.sip_facing_rtp);
        let account = call.account;
        let device = call.device.clone();
        let line_instance = call.line_instance;
        let digits = call.digits.clone();
        match self.sip.make_call(account, destination, advertised).await {
            Ok(sip_call) => {
                if let Some(call) = self.calls.get_mut(&call_id) {
                    call.sip_call = Some(sip_call);
                }
                self.sccp_by_sip.insert(sip_call, call_id);
                self.send_sccp(SccpCommand::new(
                    device.clone(),
                    SccpCommandAction::SetCallInfo {
                        call_id,
                        info: CallInfo {
                            direction: CallDirection::Outbound,
                            calling_name: binding.line.display_name.clone(),
                            calling_number: binding.line.number.clone(),
                            called_name: digits.clone(),
                            called_number: digits,
                            ..CallInfo::default()
                        },
                    },
                ))
                .await;
                self.send_sccp(SccpCommand::new(
                    device,
                    SccpCommandAction::SetCallState {
                        call_id,
                        state: CallState::RingOut,
                    },
                ))
                .await;
                debug!(%line_instance, ?sip_call, "outbound SIP call created");
            }
            Err(error) => {
                warn!(%error, "unable to create outbound SIP call");
                self.fail_call(call_id, "SIP call failed").await;
            }
        }
    }

    async fn incoming_call(
        &mut self,
        account: AccountId,
        sip_call: SipCallId,
        remote_uri: String,
        remote_media: Option<RemoteMedia>,
    ) {
        let pending = self.pending_incoming.remove(&sip_call);
        let Some(binding) = self.bindings_by_account.get(&account).cloned() else {
            let _ = self.sip.reject(sip_call, 404).await;
            return;
        };
        let codec = remote_media.map_or_else(
            || self.preferred_codec(&binding.device),
            |remote| remote.codec,
        );
        if !self.device_supports_codec(&binding.device, codec) {
            warn!(
                ?sip_call,
                ?codec,
                "incoming SIP call offered no handset-compatible codec"
            );
            let _ = self.sip.reject(sip_call, 488).await;
            return;
        }
        let relay = match pending {
            Some(pending) if pending.account == account => pending.relay,
            Some(_) => {
                warn!(
                    ?sip_call,
                    "discarding media reserved for a different SIP account"
                );
                let _ = self.sip.reject(sip_call, 500).await;
                return;
            }
            None => match self.allocator.allocate() {
                Ok(relay) => relay,
                Err(error) => {
                    warn!(%error, "unable to reserve media for incoming call");
                    let _ = self.sip.reject(sip_call, 503).await;
                    return;
                }
            },
        };
        let advertised = socket_v4(relay.addresses.sip_facing_rtp);
        if let Err(error) = self.sip.set_media_advertisement(sip_call, advertised).await {
            warn!(%error, "unable to configure incoming SDP");
            let _ = self.sip.reject(sip_call, 500).await;
            return;
        }
        let (caller_name, caller_number) = parse_party(&remote_uri);
        let info = CallInfo {
            direction: CallDirection::Inbound,
            calling_name: caller_name,
            calling_number: caller_number,
            called_name: binding.line.display_name.clone(),
            called_number: binding.line.number.clone(),
            ..CallInfo::default()
        };
        let call_id = match self
            .sccp
            .offer_incoming_call(
                binding.device.clone(),
                LineInstance::new(binding.line_instance),
                info,
            )
            .await
        {
            Ok(call_id) => call_id,
            Err(error) => {
                warn!(%error, "unable to offer SIP call to SCCP phone");
                let _ = self.sip.reject(sip_call, 480).await;
                return;
            }
        };
        let waiting = self.calls.values().any(|call| {
            call.device == binding.device
                && matches!(
                    call.state,
                    BridgeCallState::Connected | BridgeCallState::Held
                )
        });
        let source = relay_target(relay.addresses, codec);
        self.calls.insert(
            call_id,
            BridgeCall {
                device: binding.device.clone(),
                line_instance: binding.line_instance,
                account,
                sip_call: Some(sip_call),
                direction: CallDirection::Inbound,
                state: BridgeCallState::Ringing,
                digits: String::new(),
                digit_deadline: None,
                phone_media: PhoneMediaState::Opening,
                remote_media,
                media_route: MediaRouteState::Relay(RelayPath::Reserved(relay)),
                codec,
                inbound_progress: InboundProgress::WaitingForPhone,
            },
        );
        self.sccp_by_sip.insert(sip_call, call_id);
        self.send_sccp(SccpCommand::new(
            binding.device.clone(),
            SccpCommandAction::OpenReceiveChannel {
                call_id,
                source: Some(source),
                codec,
                packet_ms: DEFAULT_AUDIO_PACKET_MS,
                max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
                dtmf_mode: DtmfMode::Auto,
                audio_processing: sccp_protocol::AudioProcessingPolicy::default(),
            },
        ))
        .await;
        if waiting {
            self.send_sccp(SccpCommand::new(
                binding.device.clone(),
                SccpCommandAction::SetCallState {
                    call_id,
                    state: CallState::CallWaiting,
                },
            ))
            .await;
            self.send_sccp(SccpCommand::new(
                binding.device.clone(),
                SccpCommandAction::StopRinging { call_id },
            ))
            .await;
        }
    }

    fn media_advertisement(
        &mut self,
        account: AccountId,
        sip_call: SipCallId,
    ) -> Option<SocketAddrV4> {
        if let Some(call_id) = self.sccp_by_sip.get(&sip_call).copied()
            && let Some(call) = self.calls.get(&call_id)
        {
            if matches!(
                call.media_route,
                MediaRouteState::Direct | MediaRouteState::SwitchingToRelay(_)
            ) {
                return call
                    .phone_media
                    .source()
                    .and_then(|media| match media.address {
                        IpAddr::V4(address) => Some(SocketAddrV4::new(address, media.rtp_port)),
                        IpAddr::V6(_) => None,
                    });
            }
            if let Some(address) = relay_advertisement(&call.media_route) {
                return Some(address);
            }
        }
        if let Some(pending) = self.pending_incoming.get(&sip_call) {
            return (pending.account == account)
                .then(|| socket_v4(pending.relay.addresses.sip_facing_rtp));
        }
        if !self.bindings_by_account.contains_key(&account) {
            return None;
        }
        match self.allocator.allocate() {
            Ok(relay) => {
                let address = socket_v4(relay.addresses.sip_facing_rtp);
                self.pending_incoming
                    .insert(sip_call, PendingIncomingCall { account, relay });
                Some(address)
            }
            Err(error) => {
                warn!(%error, "unable to reserve media while creating SDP");
                None
            }
        }
    }

    async fn phone_media_ready(&mut self, call_id: CallId, mut endpoint: MediaEndpoint) {
        let Some(call) = self.calls.get(&call_id) else {
            return;
        };
        if endpoint.address.is_unspecified() {
            let Some(address) = self.phone_address(&call.device) else {
                warn!(device = %call.device, "phone supplied no usable media address");
                self.fail_call(call_id, "Phone has no usable media address")
                    .await;
                return;
            };
            endpoint.address = IpAddr::V4(address);
        }
        let direction = call.direction;
        if let Some(call) = self.calls.get_mut(&call_id) {
            call.phone_media = PhoneMediaState::Ready(endpoint);
        }
        if direction == CallDirection::Outbound {
            self.make_outbound_after_phone_media(call_id).await;
        } else {
            self.reconcile_media_route(call_id).await;
            let (sip_call, answer_requested) =
                self.calls.get_mut(&call_id).map_or((None, false), |call| {
                    let should_ring = matches!(
                        call.inbound_progress,
                        InboundProgress::WaitingForPhone | InboundProgress::AnswerRequested
                    );
                    let answer_requested =
                        call.inbound_progress == InboundProgress::AnswerRequested;
                    if call.inbound_progress == InboundProgress::WaitingForPhone {
                        call.inbound_progress = InboundProgress::Ringing;
                    }
                    (
                        should_ring.then_some(call.sip_call).flatten(),
                        answer_requested,
                    )
                });
            if let Some(sip_call) = sip_call
                && let Err(error) = self.sip.ringing(sip_call).await
            {
                warn!(%error, "unable to send SIP 180 Ringing");
            }
            if answer_requested {
                self.answer_incoming(call_id).await;
            }
        }
        self.activate_media(call_id).await;
    }

    async fn sip_call_state(
        &mut self,
        sip_call: SipCallId,
        state: DialogState,
        status: u16,
        reason: String,
    ) {
        let Some(call_id) = self.sccp_by_sip.get(&sip_call).copied() else {
            if state == DialogState::Disconnected {
                self.pending_incoming.remove(&sip_call);
            }
            return;
        };
        let Some(call) = self.calls.get_mut(&call_id) else {
            return;
        };
        let device = call.device.clone();
        match state {
            DialogState::Early | DialogState::Connecting => {
                self.send_sccp(SccpCommand::new(
                    device,
                    SccpCommandAction::SetCallState {
                        call_id,
                        state: CallState::Proceed,
                    },
                ))
                .await;
            }
            DialogState::Confirmed => {
                call.state = BridgeCallState::Connected;
                self.send_sccp(SccpCommand::new(
                    device.clone(),
                    SccpCommandAction::StopRinging { call_id },
                ))
                .await;
                self.send_sccp(SccpCommand::new(
                    device,
                    SccpCommandAction::SetCallState {
                        call_id,
                        state: CallState::Connected,
                    },
                ))
                .await;
                self.reconcile_media_route(call_id).await;
                self.activate_media(call_id).await;
            }
            DialogState::Disconnected => {
                info!(?sip_call, %status, %reason, "SIP call disconnected");
                self.end_call(call_id, true).await;
            }
            _ => {}
        }
    }

    async fn sip_media_state(
        &mut self,
        sip_call: SipCallId,
        state: SipMediaState,
        remote: Option<RemoteMedia>,
    ) {
        let Some(call_id) = self.sccp_by_sip.get(&sip_call).copied() else {
            return;
        };
        if let Some(remote) = remote {
            let supported = self
                .calls
                .get(&call_id)
                .is_some_and(|call| self.device_supports_codec(&call.device, remote.codec));
            if !supported {
                warn!(?sip_call, codec = ?remote.codec, "SIP selected a codec unsupported by the handset");
                self.fail_call(call_id, "No compatible audio codec").await;
                return;
            }
            let (reopen, endpoint_changed) =
                self.calls.get(&call_id).map_or((false, false), |call| {
                    (
                        call.codec != remote.codec,
                        call.remote_media
                            .is_some_and(|previous| previous.endpoint != remote.endpoint),
                    )
                });
            if let Some(call) = self.calls.get_mut(&call_id) {
                call.remote_media = Some(remote);
                if reopen {
                    call.codec = remote.codec;
                    call.phone_media = PhoneMediaState::Opening;
                }
            }
            if reopen {
                if let Some(call) = self.calls.get(&call_id) {
                    let Some(source) = open_receive_source(call, remote.codec) else {
                        warn!(?call_id, "unable to determine receive-media source");
                        self.fail_call(call_id, "No receive-media source").await;
                        return;
                    };
                    self.send_sccp(SccpCommand::new(
                        call.device.clone(),
                        SccpCommandAction::CloseReceiveChannel { call_id },
                    ))
                    .await;
                    self.send_sccp(SccpCommand::new(
                        call.device.clone(),
                        SccpCommandAction::OpenReceiveChannel {
                            call_id,
                            source: Some(source),
                            codec: remote.codec,
                            packet_ms: DEFAULT_AUDIO_PACKET_MS,
                            max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
                            dtmf_mode: DtmfMode::Auto,
                            audio_processing: sccp_protocol::AudioProcessingPolicy::default(),
                        },
                    ))
                    .await;
                }
                return;
            }
            self.reconcile_media_route(call_id).await;
            if endpoint_changed {
                self.refresh_running_media(call_id).await;
            }
        }
        match state {
            SipMediaState::Active | SipMediaState::RemoteHold => self.activate_media(call_id).await,
            SipMediaState::Error => self.fail_call(call_id, "SIP media failed").await,
            SipMediaState::None | SipMediaState::LocalHold => {}
        }
    }

    async fn activate_media(&mut self, call_id: CallId) {
        let command = 'activation: {
            let Some(call) = self.calls.get_mut(&call_id) else {
                return;
            };
            if matches!(call.phone_media, PhoneMediaState::Started { .. }) {
                return;
            }
            let (Some(phone), Some(remote)) = (call.phone_media.source(), call.remote_media) else {
                return;
            };
            let route = std::mem::replace(&mut call.media_route, MediaRouteState::Unallocated);
            let (route, target) = match route {
                MediaRouteState::Direct => (MediaRouteState::Direct, direct_target(remote)),
                MediaRouteState::Relay(RelayPath::Reserved(relay)) => {
                    let addresses = relay.addresses;
                    info!(?call_id, phone = %phone.address, peer = %remote.endpoint, "starting encoded RTP relay");
                    let running = match relay.start(
                        SocketAddr::new(phone.address, phone.rtp_port),
                        SocketAddr::V4(remote.endpoint),
                    ) {
                        Ok(running) => running,
                        Err(error) => {
                            break 'activation Err(error);
                        }
                    };
                    (
                        MediaRouteState::Relay(RelayPath::Running(running)),
                        relay_target(addresses, remote.codec),
                    )
                }
                MediaRouteState::Relay(RelayPath::Running(running)) => {
                    let IpAddr::V4(phone_address) = phone.address else {
                        call.media_route = MediaRouteState::Relay(RelayPath::Running(running));
                        return;
                    };
                    running.update_endpoints(
                        SocketAddrV4::new(phone_address, phone.rtp_port),
                        remote.endpoint,
                    );
                    let target = relay_target(running.addresses, remote.codec);
                    (MediaRouteState::Relay(RelayPath::Running(running)), target)
                }
                pending @ (MediaRouteState::Unallocated
                | MediaRouteState::SwitchingToDirect(_)
                | MediaRouteState::SwitchingToRelay(_)) => {
                    call.media_route = pending;
                    return;
                }
            };
            call.media_route = route;
            call.phone_media = PhoneMediaState::Started {
                source: phone,
                target,
            };
            Ok(SccpCommand::new(
                call.device.clone(),
                SccpCommandAction::StartMedia {
                    call_id,
                    endpoint: target,
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: sccp_protocol::AudioProcessingPolicy::default(),
                    traffic_class: MediaTrafficClass::from_wire(184),
                },
            ))
        };
        match command {
            Ok(command) => self.send_sccp(command).await,
            Err(error) => {
                warn!(%error, "unable to start RTP relay");
                self.fail_call(call_id, "Unable to start audio relay").await;
            }
        }
    }

    async fn refresh_running_media(&mut self, call_id: CallId) {
        let restart_direct = {
            let Some(call) = self.calls.get(&call_id) else {
                return;
            };
            let (Some(phone), Some(remote)) = (call.phone_media.source(), call.remote_media) else {
                return;
            };
            match (&call.media_route, call.phone_media) {
                (MediaRouteState::Direct, PhoneMediaState::Started { target, .. }) => {
                    target != direct_target(remote)
                }
                (MediaRouteState::Relay(RelayPath::Running(relay)), _) => {
                    let IpAddr::V4(phone_address) = phone.address else {
                        return;
                    };
                    relay.update_endpoints(
                        SocketAddrV4::new(phone_address, phone.rtp_port),
                        remote.endpoint,
                    );
                    false
                }
                _ => false,
            }
        };
        if restart_direct {
            self.restart_phone_media(call_id).await;
        }
    }

    async fn restart_phone_media(&mut self, call_id: CallId) {
        let device = self.calls.get_mut(&call_id).and_then(|call| {
            if let PhoneMediaState::Started { source, .. } = call.phone_media {
                call.phone_media = PhoneMediaState::Ready(source);
                Some(call.device.clone())
            } else {
                None
            }
        });
        if let Some(device) = device {
            self.send_sccp(SccpCommand::new(
                device,
                SccpCommandAction::StopMedia { call_id },
            ))
            .await;
        }
        self.activate_media(call_id).await;
    }

    async fn reconcile_media_route(&mut self, call_id: CallId) {
        let Some(call) = self.calls.get(&call_id) else {
            return;
        };
        let (Some(phone), Some(remote), Some(sip_call)) =
            (call.phone_media.source(), call.remote_media, call.sip_call)
        else {
            return;
        };
        let IpAddr::V4(phone_address) = phone.address else {
            return;
        };
        let desired = self.media_policy.select(
            phone_address,
            *remote.endpoint.ip(),
            remote.codec == call.codec,
        );
        let action = select_media_route_action(
            call.direction,
            call.inbound_progress,
            call.state == BridgeCallState::Connected,
            desired,
            call.media_route.kind(),
        );
        let advertised_phone = SocketAddrV4::new(phone_address, phone.rtp_port);

        if action == MediaRouteAction::SetInitialDirect {
            if let Err(error) = self
                .sip
                .set_media_advertisement(sip_call, advertised_phone)
                .await
            {
                warn!(%error, "unable to configure direct media; using relay");
            } else if let Some(call) = self.calls.get_mut(&call_id) {
                call.media_route = MediaRouteState::Direct;
                info!(?sip_call, phone = %phone_address, peer = %remote.endpoint, "selected direct RTP for incoming call");
            }
            return;
        }

        match action {
            MediaRouteAction::ReofferDirect => {
                let should_reoffer = self.calls.get_mut(&call_id).is_some_and(|call| {
                    match std::mem::replace(&mut call.media_route, MediaRouteState::Unallocated) {
                        MediaRouteState::Relay(path) => {
                            call.media_route = MediaRouteState::SwitchingToDirect(path);
                            true
                        }
                        route => {
                            call.media_route = route;
                            false
                        }
                    }
                });
                if should_reoffer
                    && let Err(error) = self.sip.reoffer_media(sip_call, advertised_phone).await
                {
                    warn!(%error, "unable to start direct-media re-offer; keeping relay");
                    if let Some(call) = self.calls.get_mut(&call_id)
                        && let MediaRouteState::SwitchingToDirect(path) =
                            std::mem::replace(&mut call.media_route, MediaRouteState::Unallocated)
                    {
                        call.media_route = MediaRouteState::Relay(path);
                    }
                }
            }
            MediaRouteAction::ReofferRelay => {
                let relay = match self.allocator.allocate() {
                    Ok(relay) => relay,
                    Err(error) => {
                        warn!(%error, "unable to reserve relay for media route change");
                        self.fail_call(call_id, "Unable to reserve a safe media route")
                            .await;
                        return;
                    }
                };
                let advertised = socket_v4(relay.addresses.sip_facing_rtp);
                if let Some(call) = self.calls.get_mut(&call_id) {
                    call.media_route = MediaRouteState::SwitchingToRelay(relay);
                }
                if let Err(error) = self.sip.reoffer_media(sip_call, advertised).await {
                    warn!(%error, "unable to start required relay-media re-offer");
                    self.fail_call(call_id, "Unable to establish a safe media route")
                        .await;
                }
            }
            MediaRouteAction::None | MediaRouteAction::SetInitialDirect => {}
        }
    }

    async fn media_reoffer_completed(
        &mut self,
        sip_call: SipCallId,
        accepted: bool,
        status: u16,
        reason: String,
    ) {
        let Some(call_id) = self.sccp_by_sip.get(&sip_call).copied() else {
            return;
        };
        enum Completion {
            Changed,
            Unchanged(Option<SocketAddrV4>),
            Failed,
            Ignore,
        }
        let completion = {
            let Some(call) = self.calls.get_mut(&call_id) else {
                return;
            };
            match std::mem::replace(&mut call.media_route, MediaRouteState::Unallocated) {
                MediaRouteState::SwitchingToDirect(_) if accepted => {
                    call.media_route = MediaRouteState::Direct;
                    Completion::Changed
                }
                MediaRouteState::SwitchingToDirect(path) => {
                    let advertised = relay_path_advertisement(&path);
                    call.media_route = MediaRouteState::Relay(path);
                    Completion::Unchanged(Some(advertised))
                }
                MediaRouteState::SwitchingToRelay(relay) if accepted => {
                    call.media_route = MediaRouteState::Relay(RelayPath::Reserved(relay));
                    Completion::Changed
                }
                MediaRouteState::SwitchingToRelay(_) => {
                    call.media_route = MediaRouteState::Direct;
                    Completion::Failed
                }
                route => {
                    call.media_route = route;
                    Completion::Ignore
                }
            }
        };
        match completion {
            Completion::Changed => {
                info!(?sip_call, %status, %reason, "SIP media route change accepted");
                self.restart_phone_media(call_id).await;
            }
            Completion::Unchanged(advertised) => {
                warn!(?sip_call, %status, %reason, "SIP media route change rejected");
                if let Some(advertised) = advertised
                    && let Err(error) = self.sip.set_media_advertisement(sip_call, advertised).await
                {
                    warn!(%error, "unable to restore local media advertisement");
                }
                self.activate_media(call_id).await;
            }
            Completion::Failed => {
                warn!(?sip_call, %status, %reason, "required relay-media route change rejected");
                self.fail_call(call_id, "SIP peer rejected the safe media route")
                    .await;
            }
            Completion::Ignore => {}
        }
    }

    async fn soft_key(&mut self, call_id: CallId, soft_key: SoftKey) {
        match soft_key {
            SoftKey::Dial => {
                if self
                    .calls
                    .get(&call_id)
                    .is_some_and(|call| call.state == BridgeCallState::TransferCollecting)
                {
                    self.complete_blind_transfer(call_id).await;
                } else {
                    self.dial(call_id).await;
                }
            }
            SoftKey::Backspace => {
                if let Some(call) = self.calls.get_mut(&call_id) {
                    call.digits.pop();
                }
            }
            SoftKey::EndCall => self.end_from_phone(call_id).await,
            SoftKey::Answer => {
                if let Some(call) = self.calls.get_mut(&call_id) {
                    call.inbound_progress = InboundProgress::AnswerRequested;
                }
                self.answer_incoming(call_id).await;
            }
            SoftKey::Hold => self.hold(call_id).await,
            SoftKey::Resume => self.resume(call_id).await,
            SoftKey::Transfer => self.transfer(call_id).await,
            SoftKey::Conference => self.conference(call_id).await,
            SoftKey::Redial => {
                let number = self.calls.get(&call_id).and_then(|call| {
                    self.last_dialed
                        .get(&(call.device.clone(), call.line_instance))
                        .cloned()
                });
                if let (Some(number), Some(call)) = (number, self.calls.get_mut(&call_id)) {
                    call.digits = number;
                    self.dial(call_id).await;
                }
            }
            _ => {}
        }
    }

    async fn answer_incoming(&mut self, call_id: CallId) {
        let Some(call) = self.calls.get(&call_id) else {
            return;
        };
        if call.direction != CallDirection::Inbound
            || call.phone_media.source().is_none()
            || call.inbound_progress != InboundProgress::AnswerRequested
        {
            return;
        }
        let Some(sip_call) = call.sip_call else {
            return;
        };
        if let Some(call) = self.calls.get_mut(&call_id) {
            call.inbound_progress = InboundProgress::Answered;
        }
        if let Err(error) = self.sip.answer(sip_call).await {
            warn!(%error, "unable to answer SIP call");
            self.fail_call(call_id, "Unable to answer").await;
            return;
        }
        if let Some(call) = self.calls.get(&call_id) {
            self.send_sccp(SccpCommand::new(
                call.device.clone(),
                SccpCommandAction::StopRinging { call_id },
            ))
            .await;
        }
        self.activate_media(call_id).await;
    }

    async fn hold(&mut self, call_id: CallId) {
        let Some(call) = self.calls.get(&call_id) else {
            return;
        };
        let (Some(sip_call), device) = (call.sip_call, call.device.clone()) else {
            return;
        };
        if let Err(error) = self.sip.hold(sip_call).await {
            warn!(%error, "unable to hold SIP call");
            return;
        }
        if let Some(call) = self.calls.get_mut(&call_id) {
            call.state = BridgeCallState::Held;
            if let PhoneMediaState::Started { source, .. } = call.phone_media {
                call.phone_media = PhoneMediaState::Ready(source);
            }
        }
        self.send_sccp(SccpCommand::new(
            device.clone(),
            SccpCommandAction::StopMedia { call_id },
        ))
        .await;
        self.send_sccp(SccpCommand::new(
            device,
            SccpCommandAction::SetCallState {
                call_id,
                state: CallState::Hold,
            },
        ))
        .await;
    }

    async fn resume(&mut self, call_id: CallId) {
        let Some(call) = self.calls.get(&call_id) else {
            return;
        };
        let (Some(sip_call), device) = (call.sip_call, call.device.clone()) else {
            return;
        };
        if let Err(error) = self.sip.resume(sip_call).await {
            warn!(%error, "unable to resume SIP call");
            return;
        }
        if let Some(call) = self.calls.get_mut(&call_id) {
            call.state = BridgeCallState::Connected;
        }
        self.send_sccp(SccpCommand::new(
            device.clone(),
            SccpCommandAction::SetCallState {
                call_id,
                state: CallState::Connected,
            },
        ))
        .await;
        self.activate_media(call_id).await;
    }

    async fn transfer(&mut self, call_id: CallId) {
        let Some(call) = self.calls.get(&call_id) else {
            return;
        };
        let device = call.device.clone();
        let other = self.calls.iter().find_map(|(id, candidate)| {
            (*id != call_id
                && candidate.device == device
                && candidate.state == BridgeCallState::Connected)
                .then_some(candidate.sip_call)
                .flatten()
        });
        if let (Some(active), Some(other)) = (call.sip_call, other) {
            if let Err(error) = self.sip.attended_transfer(active, other).await {
                warn!(%error, "attended transfer failed");
            }
            return;
        }
        self.hold(call_id).await;
        if let Some(call) = self.calls.get_mut(&call_id) {
            call.state = BridgeCallState::TransferCollecting;
            call.digits.clear();
            call.digit_deadline = None;
        }
        self.prompt(&device, call_id, "Transfer to").await;
        self.send_sccp(SccpCommand::new(
            device,
            SccpCommandAction::SetCallState {
                call_id,
                state: CallState::Transfer,
            },
        ))
        .await;
    }

    async fn complete_blind_transfer(&mut self, call_id: CallId) {
        let Some(call) = self.calls.get(&call_id) else {
            return;
        };
        if call.digits.is_empty() {
            return;
        }
        let Some(binding) = self.bindings_by_account.get(&call.account) else {
            return;
        };
        let destination = destination_uri(&call.digits, &binding.line.registrar);
        let sip_call = call.sip_call;
        if let Some(sip_call) = sip_call {
            match self.sip.blind_transfer(sip_call, destination).await {
                Ok(()) => {
                    self.prompt(&call.device.clone(), call_id, "Transfer sent")
                        .await
                }
                Err(error) => {
                    warn!(%error, "blind transfer failed");
                    self.prompt(&call.device.clone(), call_id, "Transfer failed")
                        .await;
                }
            }
        }
    }

    async fn conference(&mut self, call_id: CallId) {
        let Some(code) = self.config.sip.conference_feature_code.clone() else {
            return;
        };
        let Some(sip_call) = self.calls.get(&call_id).and_then(|call| call.sip_call) else {
            return;
        };
        if let Err(error) = self.sip.send_dtmf(sip_call, code).await {
            warn!(%error, "unable to invoke conference feature code");
        }
    }

    async fn end_from_phone(&mut self, call_id: CallId) {
        self.end_call(call_id, false).await;
    }

    async fn end_call(&mut self, call_id: CallId, from_sip: bool) {
        let Some(call) = self.calls.remove(&call_id) else {
            return;
        };
        if let Some(sip_call) = call.sip_call {
            self.sccp_by_sip.remove(&sip_call);
            if !from_sip {
                let _ = self.sip.hangup(sip_call).await;
            }
        }
        self.send_sccp(SccpCommand::new(
            call.device,
            SccpCommandAction::CloseCall { call_id },
        ))
        .await;
    }

    async fn fail_call(&mut self, call_id: CallId, message: &str) {
        if let Some(call) = self.calls.get(&call_id) {
            self.prompt(&call.device.clone(), call_id, message).await;
            self.send_sccp(SccpCommand::new(
                call.device.clone(),
                SccpCommandAction::SetCallState {
                    call_id,
                    state: CallState::Congestion,
                },
            ))
            .await;
        }
        self.end_call(call_id, false).await;
    }

    async fn prompt(&self, device: &DeviceId, call_id: CallId, text: &str) {
        self.send_sccp(SccpCommand::new(
            device.clone(),
            SccpCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 5,
                text: text.into(),
            },
        ))
        .await;
    }

    async fn send_sccp(&self, command: SccpCommand) {
        if let Err(error) = self.sccp.send(command).await {
            debug!(%error, "unable to deliver SCCP command");
        }
    }

    fn preferred_codec(&self, device: &DeviceId) -> Codec {
        let Some(capabilities) = self
            .registrations
            .get(device)
            .map(|station| station.capabilities.audio())
        else {
            return Codec::Pcmu;
        };
        if capabilities
            .iter()
            .any(|capability| capability.codec == Codec::Pcmu)
        {
            Codec::Pcmu
        } else if capabilities
            .iter()
            .any(|capability| capability.codec == Codec::Pcma)
        {
            Codec::Pcma
        } else {
            Codec::Pcmu
        }
    }

    fn device_supports_codec(&self, device: &DeviceId, codec: Codec) -> bool {
        self.registrations
            .get(device)
            .map(|station| station.capabilities.audio())
            .map_or(codec == Codec::Pcmu, |capabilities| {
                capabilities
                    .iter()
                    .any(|capability| capability.codec == codec)
            })
    }

    fn phone_address(&self, device: &DeviceId) -> Option<Ipv4Addr> {
        let registration = &self.registrations.get(device)?.registration;
        registration
            .reported_address_for_peer()
            .and_then(|address| match address {
                IpAddr::V4(address) if !address.is_unspecified() => Some(address),
                _ => None,
            })
            .or_else(|| match registration.peer.ip() {
                IpAddr::V4(address) => Some(address),
                IpAddr::V6(address) => address.to_ipv4_mapped(),
            })
    }
}

fn sip_account_config(line: &LineConfig) -> AccountConfig {
    let authority = sip_authority(&line.registrar);
    AccountConfig {
        identity_uri: format!("sip:{}@{authority}", line.username),
        registrar_uri: line.registrar.clone(),
        username: line.username.clone(),
        auth_username: line
            .auth_username
            .clone()
            .unwrap_or_else(|| line.username.clone()),
        password: line.password.clone(),
        outbound_proxy: line.outbound_proxy.clone(),
    }
}

fn select_media_route_action(
    direction: CallDirection,
    inbound_progress: InboundProgress,
    connected: bool,
    desired: MediaMode,
    current: MediaRouteKind,
) -> MediaRouteAction {
    if direction == CallDirection::Inbound
        && matches!(
            inbound_progress,
            InboundProgress::WaitingForPhone | InboundProgress::AnswerRequested
        )
        && desired == MediaMode::Direct
        && current == MediaRouteKind::Relay
    {
        return MediaRouteAction::SetInitialDirect;
    }
    if !connected {
        return MediaRouteAction::None;
    }
    match (current, desired) {
        (MediaRouteKind::Relay, MediaMode::Direct) => MediaRouteAction::ReofferDirect,
        (MediaRouteKind::Direct, MediaMode::Relay) => MediaRouteAction::ReofferRelay,
        _ => MediaRouteAction::None,
    }
}

fn destination_uri(number: &str, registrar: &str) -> String {
    if number.starts_with("sip:") || number.starts_with("sips:") {
        number.to_string()
    } else {
        format!("sip:{number}@{}", sip_authority(registrar))
    }
}

fn sip_authority(registrar: &str) -> &str {
    registrar
        .strip_prefix("sip:")
        .or_else(|| registrar.strip_prefix("sips:"))
        .unwrap_or(registrar)
        .split(';')
        .next()
        .unwrap_or(registrar)
}

fn parse_party(uri: &str) -> (String, String) {
    let name = uri.find('<').map_or_else(String::new, |index| {
        uri[..index].trim().trim_matches('"').to_string()
    });
    let address = uri
        .split('<')
        .nth(1)
        .and_then(|part| part.split('>').next())
        .unwrap_or(uri);
    let number = address
        .trim()
        .strip_prefix("sip:")
        .or_else(|| address.trim().strip_prefix("sips:"))
        .unwrap_or(address)
        .split('@')
        .next()
        .unwrap_or(address)
        .to_string();
    (
        if name.is_empty() {
            number.clone()
        } else {
            name
        },
        number,
    )
}

fn socket_v4(address: SocketAddr) -> SocketAddrV4 {
    match address {
        SocketAddr::V4(address) => address,
        SocketAddr::V6(_) => unreachable!("media allocator is IPv4-only"),
    }
}

fn relay_path_advertisement(path: &RelayPath) -> SocketAddrV4 {
    match path {
        RelayPath::Reserved(relay) => socket_v4(relay.addresses.sip_facing_rtp),
        RelayPath::Running(relay) => socket_v4(relay.addresses.sip_facing_rtp),
    }
}

fn relay_advertisement(route: &MediaRouteState) -> Option<SocketAddrV4> {
    match route {
        MediaRouteState::Relay(path) | MediaRouteState::SwitchingToDirect(path) => {
            Some(relay_path_advertisement(path))
        }
        MediaRouteState::SwitchingToRelay(relay) => Some(socket_v4(relay.addresses.sip_facing_rtp)),
        MediaRouteState::Unallocated | MediaRouteState::Direct => None,
    }
}

fn open_receive_source(call: &BridgeCall, codec: Codec) -> Option<MediaEndpoint> {
    match &call.media_route {
        MediaRouteState::Relay(path) | MediaRouteState::SwitchingToDirect(path) => {
            let addresses = match path {
                RelayPath::Reserved(relay) => relay.addresses,
                RelayPath::Running(relay) => relay.addresses,
            };
            Some(relay_target(addresses, codec))
        }
        MediaRouteState::SwitchingToRelay(relay) => Some(relay_target(relay.addresses, codec)),
        MediaRouteState::Direct => call.remote_media.map(direct_target),
        MediaRouteState::Unallocated => None,
    }
}

fn direct_target(remote: RemoteMedia) -> MediaEndpoint {
    MediaEndpoint {
        address: IpAddr::V4(*remote.endpoint.ip()),
        rtp_port: remote.endpoint.port(),
        rtcp_port: remote.endpoint.port().saturating_add(1),
        codec: remote.codec,
        packet_ms: DEFAULT_AUDIO_PACKET_MS,
        max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
        telephone_event_payload: 101,
    }
}

fn relay_target(addresses: RelayAddresses, codec: Codec) -> MediaEndpoint {
    MediaEndpoint {
        address: IpAddr::V4(ipv4(addresses.phone_facing_rtp)),
        rtp_port: addresses.phone_facing_rtp.port(),
        rtcp_port: addresses.phone_facing_rtcp.port(),
        codec,
        packet_ms: DEFAULT_AUDIO_PACKET_MS,
        max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
        telephone_event_payload: 101,
    }
}

fn ipv4(address: SocketAddr) -> Ipv4Addr {
    *socket_v4(address).ip()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_events_require_the_current_registration_generation() {
        let first = SessionGeneration::new(1).unwrap();
        let replacement = SessionGeneration::new(2).unwrap();

        assert!(is_current_session(Some(first), first));
        assert!(!is_current_session(Some(replacement), first));
        assert!(!is_current_session(Some(first), replacement));
        assert!(!is_current_session(None, first));
    }

    #[test]
    fn builds_destination_from_registrar() {
        assert_eq!(
            destination_uri("1234", "sip:pbx.example:5060;transport=udp"),
            "sip:1234@pbx.example:5060"
        );
        assert_eq!(
            destination_uri("sip:service@example.test", "sip:ignored"),
            "sip:service@example.test"
        );
    }

    #[test]
    fn parses_named_and_bare_parties() {
        assert_eq!(
            parse_party("\"Alice\" <sip:1002@example.test>"),
            ("Alice".into(), "1002".into())
        );
        assert_eq!(
            parse_party("sip:1003@example.test"),
            ("1003".into(), "1003".into())
        );
    }

    #[test]
    fn inbound_direct_route_is_set_before_the_answer() {
        assert_eq!(
            select_media_route_action(
                CallDirection::Inbound,
                InboundProgress::WaitingForPhone,
                false,
                MediaMode::Direct,
                MediaRouteKind::Relay,
            ),
            MediaRouteAction::SetInitialDirect
        );
    }

    #[test]
    fn outbound_direct_route_waits_for_a_confirmed_dialog() {
        assert_eq!(
            select_media_route_action(
                CallDirection::Outbound,
                InboundProgress::NotApplicable,
                false,
                MediaMode::Direct,
                MediaRouteKind::Relay,
            ),
            MediaRouteAction::None
        );
        assert_eq!(
            select_media_route_action(
                CallDirection::Outbound,
                InboundProgress::NotApplicable,
                true,
                MediaMode::Direct,
                MediaRouteKind::Relay,
            ),
            MediaRouteAction::ReofferDirect
        );
    }

    #[test]
    fn established_direct_route_moves_to_relay_when_topology_changes() {
        assert_eq!(
            select_media_route_action(
                CallDirection::Inbound,
                InboundProgress::Answered,
                true,
                MediaMode::Relay,
                MediaRouteKind::Direct,
            ),
            MediaRouteAction::ReofferRelay
        );
        assert_eq!(
            select_media_route_action(
                CallDirection::Inbound,
                InboundProgress::Answered,
                true,
                MediaMode::Relay,
                MediaRouteKind::Switching,
            ),
            MediaRouteAction::None,
            "overlapping media negotiations are suppressed"
        );
    }
}
