//! SCCP/SPCP message identifiers and direction metadata for the message domain.
//!
//! The numeric values are protocol facts.  The catalog intentionally includes
//! messages which this crate can only preserve opaquely today: knowing the ID
//! is still useful for bounded forwarding and future typed implementations.
//!
//! Start with [`MessageId`] when inspecting an unknown frame. Its
//! [`MessageId::contract`] links the numeric identifier to routing, payload
//! bounds, codec coverage, response selection, and field fidelity. Use
//! [`implemented_message_contracts`] to enumerate the typed subset.

use std::fmt;

use super::wire::{HEADER_SIZE, MAX_FRAME_SIZE};

/// The protocol roles between which a message is normally sent.
///
/// SCCP is not solely a station/client protocol. Conference resources, media
/// resource services, and call-control peers share the same numeric message
/// space. Keeping those routes explicit prevents a decoder or runtime from
/// treating a service-node frame as handset input merely because both travel
/// toward call control.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MessageRoute {
    StationToControl,
    ControlToStation,
    ControlToServiceNode,
    ServiceNodeToControl,
    IntraControl,
}

/// Legacy station-oriented view of the two handset message directions.
///
/// New code should use [`MessageRoute`]. A service-node or intra-control
/// message deliberately has no `MessageDirection`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MessageDirection {
    DeviceToServer,
    ServerToDevice,
}

/// How completely the public message model implements a catalog entry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CodecSupport {
    /// The message has a typed public representation and a checked codec.
    Typed,
    /// Only the identifier, direction, and opaque bytes are preserved.
    OpaqueOnly,
}

/// The rule used to choose and bound a message payload layout.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PayloadLayout {
    /// No semantic payload bytes are carried.
    Empty,
    /// One fixed layout is used for all supported protocol versions.
    Fixed,
    /// The negotiated protocol selects between fixed layouts.
    VersionSelected,
    /// The negotiated protocol and exact body length jointly select a layout.
    VersionAndLengthSelected,
    /// A typed fixed prefix is decoded while a bounded extension is preserved.
    MinimumLengthPreserved,
    /// A bounded length/count field controls a variable tail.
    LengthPrefixed,
    /// A bounded payload is retained exactly while consumers may inspect it.
    BoundedPreserved,
    /// A bounded extension is retained byte-for-byte because its internal
    /// schema is not modeled.
    BoundedOpaque,
    /// NUL-terminated station strings are followed by zero bytes to a
    /// four-byte boundary.
    DynamicWordPadded,
    /// The crate deliberately does not interpret the payload.
    Opaque,
}

/// Whether application code can construct a message without supplying raw
/// wire bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EmissionSupport {
    /// A typed encoder is available.
    Typed,
    /// Bytes can be forwarded explicitly through `KnownOpaque`, but there is
    /// no typed constructor and runtime code must not synthesize the message.
    PreserveOnly,
}

/// Present production/runtime role of a known message.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeUse {
    /// A typed phone-originated input accepted by the session runtime.
    DeviceInput,
    /// A server response required by a currently handled phone request.
    RequiredResponse,
    /// A server output emitted only for the corresponding configured feature
    /// or call state.
    ConditionalServerOutput,
    /// A typed service-node input accepted by its independent runtime.
    ServiceNodeInput,
    /// A service-node output emitted only for an owned reservation transition.
    ConditionalServiceNodeOutput,
    /// The codec is typed for conformance/testing, but ordinary runtime flows
    /// intentionally do not emit it.
    TypedButNotEmitted,
    /// Only catalog metadata and explicit opaque preservation are supported.
    CatalogOnly,
}

/// Whether all semantic wire fields survive typed decoding and re-encoding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FieldFidelity {
    /// Every accepted semantic field is represented; reserved/padding bytes
    /// are validated rather than exposed.
    Lossless,
    /// A server-only producer omits or fills the named fields. Decoding may
    /// project other values, so this is not an exact decode/re-encode guarantee.
    CanonicalServerOutput(&'static str),
    /// Typed decoding is intentionally projected onto the named runtime data.
    SemanticProjection(&'static str),
    /// The uninterpreted bounded body is retained exactly.
    OpaquePreserved,
}

/// SCCP-level response expected for a request or media transaction.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResponseExpectation {
    None,
    Message(MessageId),
    /// The negotiated protocol selects the response identifier.
    VersionSelected {
        /// Response used before `minimum_protocol`.
        before: MessageId,
        /// Response used at and after `minimum_protocol`.
        from: MessageId,
        /// First protocol version that selects `from`.
        minimum_protocol: u8,
    },
    /// Negotiated session inputs select the response identifier.
    SessionSelected {
        /// Response used when `selector` does not select the dynamic form.
        before: MessageId,
        /// Dynamic response selected by `selector`.
        from: MessageId,
        /// Session rule that chooses between the response identifiers.
        selector: SessionResponseSelector,
    },
    /// The response may be any member of this family.
    OneOf(&'static [MessageId]),
}

/// Session rule used to select a dynamic response identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionResponseSelector {
    /// Select the dynamic form when the feature is present or the negotiated
    /// protocol meets the stated minimum.
    DynamicMessagesOrProtocol { minimum_protocol: u8 },
    /// Select the dynamic form only when the feature is present.
    DynamicMessages,
}

/// Verification depth for a wire contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContractVerification {
    Structural,
    StructuralAndValidated,
}

/// Whether an identifier belongs to the base station-control inventory or an
/// independently supported extension family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContractScope {
    Base,
    Supplemental,
}

/// Inclusive payload-size bounds, excluding the 12-byte frame header.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PayloadSizeBounds {
    /// Smallest accepted payload in bytes.
    pub minimum: usize,
    /// Largest accepted payload in bytes.
    pub maximum: usize,
}

/// Machine-readable support record for one known message identifier.
///
/// This is an implementation inventory, not a claim that every cataloged
/// message is safe to send. `OpaqueOnly` entries exist for bounded forwarding
/// and remain non-emittable through the typed API. `response`
/// describes SCCP transaction acknowledgement; TCP acknowledgement is
/// intentionally not treated as application-level acceptance.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MessageContract {
    pub id: MessageId,
    pub scope: ContractScope,
    pub route: MessageRoute,
    pub codec: CodecSupport,
    pub payload_layout: PayloadLayout,
    /// Canonical typed-encoder payload size when there is one stable,
    /// independently useful value. This excludes the 12-byte frame header;
    /// nominally empty decoders may still accept bounded extension bytes.
    pub fixed_payload_bytes: Option<usize>,
    /// Accepted payload-size range when both bounds are known.
    pub payload_size_bounds: Option<PayloadSizeBounds>,
    /// Typed construction versus explicit opaque preservation.
    pub emission: EmissionSupport,
    /// Production/runtime use, distinct from mere encoder availability.
    pub runtime_use: RuntimeUse,
    /// Whether the typed model retains every accepted semantic wire field.
    pub field_fidelity: FieldFidelity,
    /// SCCP response/acknowledgement family, when one exists.
    pub response: ResponseExpectation,
    /// Depth of contract validation performed by the codec.
    pub verification: ContractVerification,
}

macro_rules! message_catalog {
    ($(($variant:ident, $value:expr, $route:ident)),+ $(,)?) => {
        /// A Skinny message identifier.
        ///
        /// Unknown values are retained to keep decoding forward-compatible.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum MessageId {
            $($variant,)+
            Unknown(u32),
        }

        impl MessageId {
            pub const ALL_KNOWN: &'static [Self] = &[$(Self::$variant,)+];

            pub const fn wire_value(self) -> u32 {
                match self {
                    $(Self::$variant => $value,)+
                    Self::Unknown(value) => value,
                }
            }

            /// Returns the protocol route for a known identifier.
            ///
            /// Unknown identifiers return `None` because direction cannot be
            /// inferred from their numeric value alone.
            pub const fn route(self) -> Option<MessageRoute> {
                match self {
                    $(Self::$variant => Some(MessageRoute::$route),)+
                    Self::Unknown(_) => None,
                }
            }

            /// Return the legacy two-ended station direction, if applicable.
            pub const fn direction(self) -> Option<MessageDirection> {
                match self.route() {
                    Some(MessageRoute::StationToControl) => {
                        Some(MessageDirection::DeviceToServer)
                    }
                    Some(MessageRoute::ControlToStation) => {
                        Some(MessageDirection::ServerToDevice)
                    }
                    Some(MessageRoute::ControlToServiceNode)
                    | Some(MessageRoute::ServiceNodeToControl)
                    | Some(MessageRoute::IntraControl)
                    | None => None,
                }
            }

            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($variant),)+
                    Self::Unknown(_) => "Unknown",
                }
            }

            pub const fn is_known(self) -> bool {
                !matches!(self, Self::Unknown(_))
            }

            /// Return the codec and wire contract for this identifier.
            pub const fn contract(self) -> Option<MessageContract> {
                let route = match self.route() {
                    Some(route) => route,
                    None => return None,
                };
                let codec = codec_support(self);
                Some(MessageContract {
                    id: self,
                    scope: contract_scope(self),
                    route,
                    codec,
                    payload_layout: payload_layout(self, codec),
                    fixed_payload_bytes: fixed_payload_bytes(self),
                    payload_size_bounds: payload_size_bounds(self),
                    emission: match codec {
                        CodecSupport::Typed => EmissionSupport::Typed,
                        CodecSupport::OpaqueOnly => EmissionSupport::PreserveOnly,
                    },
                    runtime_use: runtime_use(self, route, codec),
                    field_fidelity: field_fidelity(self),
                    response: primary_response(self),
                    verification: verification(self),
                })
            }
        }

        impl From<u32> for MessageId {
            fn from(value: u32) -> Self {
                match value {
                    $($value => Self::$variant,)+
                    value => Self::Unknown(value),
                }
            }
        }
    };
}

const fn contract_scope(id: MessageId) -> ContractScope {
    use MessageId::*;
    match id {
        IpPort
        | MediaPortList
        | SetHookFlashDetect
        | StartMediaReception
        | StopMediaReception
        | EnunciatorCommand
        | ExtensionDeviceCapabilities
        | SpcpRegisterTokenRequest
        | SpcpRegisterTokenAck
        | SpcpRegisterTokenReject => ContractScope::Supplemental,
        _ => ContractScope::Base,
    }
}

/// Exhaustive by design: adding an identifier must make the compiler require
/// an explicit fidelity decision. In particular, there is no catch-all that
/// silently upgrades a new typed codec to `Lossless`.
const fn field_fidelity(id: MessageId) -> FieldFidelity {
    use MessageId::*;
    match id {
        MediaPortList
        | SetHookFlashDetect
        | StartMediaReception
        | StopMediaReception
        | EnunciatorCommand
        | SpcpRegisterTokenRequest
        | SpcpRegisterTokenAck
        | SpcpRegisterTokenReject
        | Unknown(_) => FieldFidelity::OpaquePreserved,

        KeepAlive
        | ConfigStatusRequest
        | TimeDateRequest
        | VersionRequest
        | ServerRequest
        | SoftKeySetRequest
        | SoftKeyTemplateRequest => FieldFidelity::SemanticProjection(
            "nominally empty request; bounded extension bytes are accepted but not modeled",
        ),
        ButtonTemplateRequest => FieldFidelity::SemanticProjection(
            "the optional total-button-count request word is accepted but not modeled",
        ),
        Register => FieldFidelity::Lossless,
        CapabilitiesResponse => FieldFidelity::SemanticProjection(
            "advertised capability count; inactive fixed-array entries are not modeled",
        ),
        RegisterTokenRequest => FieldFidelity::SemanticProjection(
            "simultaneously populated IPv4 and IPv6 station addresses collapse to one address",
        ),
        Unregister => FieldFidelity::SemanticProjection(
            "an empty reason-zero body is accepted and normalized to the typed reason",
        ),
        MediaTransmissionFailure => FieldFidelity::SemanticProjection(
            "the public status is synthesized because the failure wire layouts carry no status",
        ),
        HeadsetStatus => FieldFidelity::SemanticProjection(
            "non-canonical raw states are projected onto a boolean",
        ),
        RegisterAvailableLines => FieldFidelity::SemanticProjection(
            "an absent or short legacy body is projected onto zero available lines",
        ),
        PortResponse => FieldFidelity::SemanticProjection(
            "pre-v20 bodies omit media type, which is synthesized on decode",
        ),
        Alarm => FieldFidelity::Lossless,

        RegisterAck | ConfigStatus | ConfigStatusDynamic => FieldFidelity::Lossless,
        LineStatus | LineStatusDynamic => FieldFidelity::CanonicalServerOutput(
            "display label/fully-qualified display name and display-options word",
        ),
        ServerResponse => {
            FieldFidelity::SemanticProjection("empty server-list slot positions are not retained")
        }
        DefineTimeDate => FieldFidelity::Lossless,
        CallState => FieldFidelity::CanonicalServerOutput("visibility, precedence and domain"),
        CallInfo | CallInfoDynamic => FieldFidelity::CanonicalServerOutput(
            "mailboxes, call instance/security and version-selected party metadata",
        ),
        ForwardStatus => FieldFidelity::CanonicalServerOutput(
            "aggregate active flag and inactive forwarding-number slots",
        ),
        StopTone => FieldFidelity::CanonicalServerOutput("post-v11 tone word"),
        ButtonTemplate | SoftKeyTemplateResponse | SoftKeySetResponse => {
            FieldFidelity::CanonicalServerOutput(
                "template offsets/count metadata and unused fixed-array entries",
            )
        }
        ConnectionStatisticsRequest => {
            FieldFidelity::CanonicalServerOutput("post-v18 directory-number alignment bytes")
        }
        ClearDisplay => FieldFidelity::CanonicalServerOutput("display-control word"),
        KeepAliveAck | CapabilitiesRequest | ClearNotify | DeactivateCallPlane
        | RegisterTokenAck | CallCountResponse => FieldFidelity::CanonicalServerOutput(
            "nominally empty response; accepted extension bytes are not modeled",
        ),
        UnregisterAck => FieldFidelity::CanonicalServerOutput("acknowledgement body word"),
        StartAnnouncement => FieldFidelity::CanonicalServerOutput(
            "unused announcement and conference-party array entries",
        ),

        IpPort
        | KeypadButton
        | EnblocCall
        | Stimulus
        | OffHook
        | OnHook
        | HookFlash
        | ForwardStatusRequest
        | SpeedDialStatusRequest
        | LineStatusRequest
        | MulticastMediaReceptionAck
        | OpenReceiveChannelAck
        | ConnectionStatisticsResponse
        | OffHookWithCallingParty
        | SoftKeyEvent
        | MediaResourceNotification
        | DeviceToUserData
        | DeviceToUserDataResponse
        | UpdateCapabilities
        | ClearConference
        | ServiceUrlStatusRequest
        | FeatureStatusRequest
        | CreateConferenceResponse
        | DeleteConferenceResponse
        | ModifyConferenceResponse
        | AddParticipantResponse
        | AuditConferenceResponse
        | AuditParticipantResponse
        | DeviceToUserDataV1
        | DeviceToUserDataResponseV1
        | UpdateCapabilitiesV2
        | UpdateCapabilitiesV3
        | QosReservationNotify
        | QosErrorNotify
        | SubscriptionStatusRequest
        | MediaPathEvent
        | StartTone
        | SetRinger
        | SetLamp
        | SetSpeakerMode
        | SetMicrophoneMode
        | StartMediaTransmission
        | CloseReceiveChannel
        | StopMediaTransmission
        | SpeedDialStatus
        | Version
        | DisplayText
        | RegisterReject
        | Reset
        | StartMulticastMediaReception
        | StartMulticastMediaTransmission
        | StopMulticastMediaReception
        | StopMulticastMediaTransmission
        | OpenReceiveChannel
        | SelectSoftKeys
        | DisplayPromptStatus
        | ClearPromptStatus
        | DisplayNotify
        | ActivateCallPlane
        | BackspaceResponse
        | RegisterTokenReject
        | DialedNumber
        | UserToDeviceData
        | FeatureStatus
        | DisplayPriorityNotify
        | ClearPriorityNotify
        | StopAnnouncement
        | AnnouncementFinish
        | SubscribeDtmfPayloadRequest
        | SubscribeDtmfPayloadResponse
        | SubscribeDtmfPayloadError
        | UnsubscribeDtmfPayloadRequest
        | UnsubscribeDtmfPayloadResponse
        | UnsubscribeDtmfPayloadError
        | ServiceUrlStatus
        | CallSelectStatus
        | CreateConferenceRequest
        | DeleteConferenceRequest
        | ModifyConferenceRequest
        | AddParticipantRequest
        | DropParticipantRequest
        | AuditConferenceRequest
        | AuditParticipantRequest
        | ChangeParticipantRequest
        | UserToDeviceDataV1
        | DisplayDynamicNotify
        | DisplayDynamicPriorityNotify
        | DisplayDynamicPromptStatus
        | FeatureStatusDynamic
        | ServiceUrlStatusDynamic
        | SpeedDialStatusDynamic
        | PortRequest
        | PortClose
        | QosListen
        | QosPath
        | QosTeardown
        | UpdateDscp
        | QosModify
        | SubscriptionStatus
        | Notification
        | StartMediaTransmissionAck
        | CallHistoryDisposition
        | LocationInfo
        | XmlAlarm
        | CallCountRequest
        | RecordingStatus
        | MediaPathCapability => FieldFidelity::Lossless,
        StartSessionTransmission
        | StopSessionTransmission
        | OpenMultimediaChannel
        | StartMultimediaTransmission
        | MiscellaneousCommand => FieldFidelity::Lossless,
        StartMediaFailureDetection => FieldFidelity::Lossless,
        MwiNotification
        | MwiResponse
        | OpenMultimediaReceiveChannelAck
        | StartMultimediaTransmissionAck
        | ExtensionDeviceCapabilities
        | NotifyDtmfTone
        | SendDtmfTone
        | StopMultimediaTransmission
        | FlowControlCommand
        | CloseMultimediaReceiveChannel
        | VideoDisplayCommand
        | FlowControlNotify => FieldFidelity::Lossless,
    }
}

const fn runtime_use(id: MessageId, route: MessageRoute, support: CodecSupport) -> RuntimeUse {
    use MessageId::*;
    if matches!(support, CodecSupport::OpaqueOnly) {
        return RuntimeUse::CatalogOnly;
    }
    if matches!(route, MessageRoute::StationToControl) {
        return RuntimeUse::DeviceInput;
    }
    if matches!(route, MessageRoute::ServiceNodeToControl) {
        return match id {
            QosReservationNotify | QosErrorNotify => RuntimeUse::ServiceNodeInput,
            _ => RuntimeUse::TypedButNotEmitted,
        };
    }
    if matches!(route, MessageRoute::ControlToServiceNode) {
        return match id {
            QosListen | QosPath | QosTeardown | UpdateDscp | QosModify => {
                RuntimeUse::ConditionalServiceNodeOutput
            }
            _ => RuntimeUse::TypedButNotEmitted,
        };
    }
    if !matches!(route, MessageRoute::ControlToStation) {
        return RuntimeUse::TypedButNotEmitted;
    }
    match id {
        RegisterAck
        | RegisterReject
        | KeepAliveAck
        | UnregisterAck
        | CapabilitiesRequest
        | ConfigStatus
        | LineStatus
        | LineStatusDynamic
        | ButtonTemplate
        | Version
        | ServerResponse
        | DefineTimeDate
        | SoftKeyTemplateResponse
        | SoftKeySetResponse
        | RegisterTokenAck
        | RegisterTokenReject
        | FeatureStatus
        | FeatureStatusDynamic
        | ServiceUrlStatus
        | ServiceUrlStatusDynamic
        | CallCountResponse => RuntimeUse::RequiredResponse,

        StartMulticastMediaReception
        | StartMulticastMediaTransmission
        | StopMulticastMediaReception
        | StopMulticastMediaTransmission
        | StartSessionTransmission
        | StopSessionTransmission
        | ClearConference
        | DisplayNotify
        | DisplayDynamicNotify
        | ClearNotify
        | DeactivateCallPlane
        | UserToDeviceData
        | SubscribeDtmfPayloadRequest
        | SubscribeDtmfPayloadError
        | UnsubscribeDtmfPayloadRequest
        | UnsubscribeDtmfPayloadError
        | CreateConferenceRequest
        | DeleteConferenceRequest
        | ModifyConferenceRequest
        | AddParticipantRequest
        | DropParticipantRequest
        | AuditConferenceRequest
        | AuditParticipantRequest
        | ChangeParticipantRequest => RuntimeUse::TypedButNotEmitted,

        _ => RuntimeUse::ConditionalServerOutput,
    }
}

const fn codec_support(id: MessageId) -> CodecSupport {
    use MessageId::*;
    match id {
        MediaPortList
        | SetHookFlashDetect
        | StartMediaReception
        | StopMediaReception
        | EnunciatorCommand
        | SpcpRegisterTokenRequest
        | SpcpRegisterTokenAck
        | SpcpRegisterTokenReject => CodecSupport::OpaqueOnly,
        KeepAlive
        | Register
        | IpPort
        | KeypadButton
        | EnblocCall
        | Stimulus
        | OffHook
        | OnHook
        | HookFlash
        | ForwardStatusRequest
        | SpeedDialStatusRequest
        | LineStatusRequest
        | ConfigStatusRequest
        | TimeDateRequest
        | ButtonTemplateRequest
        | VersionRequest
        | CapabilitiesResponse
        | ServerRequest
        | Alarm
        | MulticastMediaReceptionAck
        | OpenReceiveChannelAck
        | ConnectionStatisticsResponse
        | OffHookWithCallingParty
        | SoftKeySetRequest
        | SoftKeyEvent
        | Unregister
        | SoftKeyTemplateRequest
        | RegisterTokenRequest
        | MediaTransmissionFailure
        | HeadsetStatus
        | MediaResourceNotification
        | RegisterAvailableLines
        | DeviceToUserData
        | DeviceToUserDataResponse
        | UpdateCapabilities
        | ClearConference
        | ServiceUrlStatusRequest
        | FeatureStatusRequest
        | CreateConferenceResponse
        | DeleteConferenceResponse
        | ModifyConferenceResponse
        | AddParticipantResponse
        | AuditConferenceResponse
        | AuditParticipantResponse
        | DeviceToUserDataV1
        | DeviceToUserDataResponseV1
        | UpdateCapabilitiesV2
        | UpdateCapabilitiesV3
        | PortResponse
        | QosReservationNotify
        | QosErrorNotify
        | SubscriptionStatusRequest
        | MediaPathEvent
        | StartMediaFailureDetection
        | RegisterAck
        | StartTone
        | StopTone
        | SetRinger
        | SetLamp
        | SetSpeakerMode
        | SetMicrophoneMode
        | StartMediaTransmission
        | StopMediaTransmission
        | CallInfo
        | ForwardStatus
        | SpeedDialStatus
        | LineStatus
        | ConfigStatus
        | DefineTimeDate
        | ButtonTemplate
        | Version
        | DisplayText
        | ClearDisplay
        | CapabilitiesRequest
        | RegisterReject
        | ServerResponse
        | Reset
        | KeepAliveAck
        | StartMulticastMediaReception
        | StartMulticastMediaTransmission
        | StopMulticastMediaReception
        | StopMulticastMediaTransmission
        | OpenReceiveChannel
        | CloseReceiveChannel
        | ConnectionStatisticsRequest
        | SoftKeyTemplateResponse
        | SoftKeySetResponse
        | SelectSoftKeys
        | CallState
        | DisplayPromptStatus
        | ClearPromptStatus
        | DisplayNotify
        | ClearNotify
        | ActivateCallPlane
        | DeactivateCallPlane
        | UnregisterAck
        | BackspaceResponse
        | RegisterTokenAck
        | RegisterTokenReject
        | DialedNumber
        | UserToDeviceData
        | FeatureStatus
        | DisplayPriorityNotify
        | ClearPriorityNotify
        | StartAnnouncement
        | StopAnnouncement
        | AnnouncementFinish
        | SubscribeDtmfPayloadRequest
        | SubscribeDtmfPayloadResponse
        | SubscribeDtmfPayloadError
        | UnsubscribeDtmfPayloadRequest
        | UnsubscribeDtmfPayloadResponse
        | UnsubscribeDtmfPayloadError
        | ServiceUrlStatus
        | CallSelectStatus
        | CreateConferenceRequest
        | DeleteConferenceRequest
        | ModifyConferenceRequest
        | AddParticipantRequest
        | DropParticipantRequest
        | AuditConferenceRequest
        | AuditParticipantRequest
        | ChangeParticipantRequest
        | UserToDeviceDataV1
        | DisplayDynamicNotify
        | DisplayDynamicPriorityNotify
        | DisplayDynamicPromptStatus
        | FeatureStatusDynamic
        | LineStatusDynamic
        | ServiceUrlStatusDynamic
        | SpeedDialStatusDynamic
        | CallInfoDynamic
        | PortRequest
        | PortClose
        | QosListen
        | QosPath
        | QosTeardown
        | UpdateDscp
        | QosModify
        | SubscriptionStatus
        | Notification
        | StartMediaTransmissionAck
        | CallHistoryDisposition
        | LocationInfo
        | XmlAlarm
        | CallCountRequest
        | CallCountResponse
        | RecordingStatus
        | MediaPathCapability => CodecSupport::Typed,
        StartSessionTransmission
        | StopSessionTransmission
        | OpenMultimediaChannel
        | StartMultimediaTransmission
        | MiscellaneousCommand => CodecSupport::Typed,
        MwiNotification
        | MwiResponse
        | OpenMultimediaReceiveChannelAck
        | StartMultimediaTransmissionAck
        | ExtensionDeviceCapabilities
        | NotifyDtmfTone
        | SendDtmfTone
        | StopMultimediaTransmission
        | FlowControlCommand
        | CloseMultimediaReceiveChannel
        | VideoDisplayCommand
        | FlowControlNotify => CodecSupport::Typed,
        ConfigStatusDynamic => CodecSupport::Typed,
        Unknown(_) => CodecSupport::OpaqueOnly,
    }
}

const fn payload_layout(id: MessageId, support: CodecSupport) -> PayloadLayout {
    use MessageId::*;
    if matches!(support, CodecSupport::OpaqueOnly) {
        return PayloadLayout::Opaque;
    }
    match id {
        KeepAlive
        | ConfigStatusRequest
        | TimeDateRequest
        | ButtonTemplateRequest
        | VersionRequest
        | ServerRequest
        | SoftKeySetRequest
        | SoftKeyTemplateRequest
        | KeepAliveAck
        | CapabilitiesRequest
        | ClearDisplay
        | ClearNotify
        | DeactivateCallPlane
        | RegisterTokenAck
        | CallCountResponse => PayloadLayout::Empty,

        UpdateCapabilities | KeypadButton | EnblocCall => PayloadLayout::VersionAndLengthSelected,

        Register | UpdateCapabilitiesV3 | AddParticipantResponse => {
            PayloadLayout::MinimumLengthPreserved
        }

        CapabilitiesResponse => PayloadLayout::LengthPrefixed,

        UpdateCapabilitiesV2 => PayloadLayout::Fixed,

        XmlAlarm => PayloadLayout::BoundedPreserved,

        OpenReceiveChannelAck
        | ConnectionStatisticsResponse
        | MediaTransmissionFailure
        | PortResponse
        | ServerResponse
        | ForwardStatus
        | DialedNumber
        | OpenReceiveChannel
        | ConnectionStatisticsRequest
        | StartMediaTransmission
        | StartMulticastMediaReception
        | StartMulticastMediaTransmission
        | OpenMultimediaReceiveChannelAck
        | StartMultimediaTransmissionAck
        | StartSessionTransmission
        | StopSessionTransmission
        | OpenMultimediaChannel
        | StartMultimediaTransmission
        | PortRequest
        | PortClose => PayloadLayout::VersionSelected,

        StartMediaTransmissionAck => PayloadLayout::VersionAndLengthSelected,

        DeviceToUserData
        | DeviceToUserDataResponse
        | DeviceToUserDataV1
        | DeviceToUserDataResponseV1
        | CreateConferenceResponse
        | ModifyConferenceResponse
        | AuditConferenceResponse
        | UserToDeviceData
        | UserToDeviceDataV1
        | CreateConferenceRequest
        | ModifyConferenceRequest => PayloadLayout::LengthPrefixed,

        AuditParticipantResponse => PayloadLayout::BoundedOpaque,

        DisplayDynamicNotify
        | DisplayDynamicPriorityNotify
        | DisplayDynamicPromptStatus
        | ConfigStatusDynamic
        | LineStatusDynamic
        | ServiceUrlStatusDynamic
        | CallInfoDynamic => PayloadLayout::DynamicWordPadded,

        _ => PayloadLayout::Fixed,
    }
}

const fn fixed_payload_bytes(id: MessageId) -> Option<usize> {
    use MessageId::*;
    match id {
        KeepAlive
        | ConfigStatusRequest
        | TimeDateRequest
        | ButtonTemplateRequest
        | VersionRequest
        | ServerRequest
        | SoftKeySetRequest
        | SoftKeyTemplateRequest
        | KeepAliveAck
        | CapabilitiesRequest
        | ClearDisplay
        | ClearNotify
        | DeactivateCallPlane
        | RegisterTokenAck
        | CallCountResponse => Some(0),
        RegisterAck => Some(20),
        UpdateCapabilitiesV2 => Some(2_000),
        MwiNotification => Some(88),
        MwiResponse => Some(32),
        LineStatus => Some(112),
        DefineTimeDate => Some(36),
        AddParticipantResponse => Some(272),
        AuditConferenceRequest => Some(0),
        SubscribeDtmfPayloadRequest | UnsubscribeDtmfPayloadRequest => Some(16),
        SubscribeDtmfPayloadResponse
        | SubscribeDtmfPayloadError
        | UnsubscribeDtmfPayloadResponse
        | UnsubscribeDtmfPayloadError => Some(12),
        ButtonTemplate => Some(96),
        LocationInfo => Some(2_404),
        SoftKeyTemplateResponse => Some(652),
        SoftKeySetResponse => Some(780),
        MulticastMediaReceptionAck => Some(12),
        NotifyDtmfTone | SendDtmfTone | VideoDisplayCommand => Some(12),
        StopMediaTransmission
        | CloseReceiveChannel
        | StopMultimediaTransmission
        | FlowControlCommand
        | CloseMultimediaReceiveChannel
        | FlowControlNotify => Some(16),
        MiscellaneousCommand => Some(52),
        ExtensionDeviceCapabilities => Some(164),
        StartMediaFailureDetection => Some(28),
        QosReservationNotify | QosTeardown | UpdateDscp => Some(24),
        QosErrorNotify => Some(44),
        QosListen => Some(172),
        QosPath => Some(168),
        QosModify => Some(152),
        _ => None,
    }
}

const fn payload_size_bounds(id: MessageId) -> Option<PayloadSizeBounds> {
    use MessageId::*;
    const MAX_PAYLOAD_BYTES: usize = MAX_FRAME_SIZE - HEADER_SIZE;
    match id {
        KeepAlive
        | ConfigStatusRequest
        | TimeDateRequest
        | ButtonTemplateRequest
        | VersionRequest
        | ServerRequest
        | SoftKeySetRequest
        | SoftKeyTemplateRequest
        | KeepAliveAck
        | CapabilitiesRequest
        | ClearDisplay
        | ClearNotify
        | DeactivateCallPlane
        | RegisterTokenAck
        | CallCountResponse => Some(PayloadSizeBounds {
            minimum: 0,
            maximum: MAX_PAYLOAD_BYTES,
        }),
        Register => Some(PayloadSizeBounds {
            minimum: 124,
            maximum: 172,
        }),
        XmlAlarm => Some(PayloadSizeBounds {
            minimum: 0,
            maximum: 2_048,
        }),
        AddParticipantResponse => Some(PayloadSizeBounds {
            minimum: 12,
            maximum: 272,
        }),
        CapabilitiesResponse => Some(PayloadSizeBounds {
            minimum: 4,
            maximum: 292,
        }),
        UpdateCapabilitiesV3 => Some(PayloadSizeBounds {
            minimum: 20,
            maximum: 2_380,
        }),
        _ => match fixed_payload_bytes(id) {
            Some(size) => Some(PayloadSizeBounds {
                minimum: size,
                maximum: size,
            }),
            None => None,
        },
    }
}

const CAPABILITY_RESPONSES: &[MessageId] = &[
    MessageId::CapabilitiesResponse,
    MessageId::UpdateCapabilities,
    MessageId::UpdateCapabilitiesV2,
    MessageId::UpdateCapabilitiesV3,
];

const REGISTER_TOKEN_RESPONSES: &[MessageId] =
    &[MessageId::RegisterTokenAck, MessageId::RegisterTokenReject];

const fn primary_response(id: MessageId) -> ResponseExpectation {
    use MessageId::*;
    match id {
        Register => ResponseExpectation::Message(RegisterAck),
        KeepAlive => ResponseExpectation::Message(KeepAliveAck),
        Unregister => ResponseExpectation::Message(UnregisterAck),
        ConfigStatusRequest => ResponseExpectation::SessionSelected {
            before: ConfigStatus,
            from: ConfigStatusDynamic,
            selector: SessionResponseSelector::DynamicMessagesOrProtocol {
                minimum_protocol: 9,
            },
        },
        TimeDateRequest => ResponseExpectation::Message(DefineTimeDate),
        ButtonTemplateRequest => ResponseExpectation::Message(ButtonTemplate),
        VersionRequest => ResponseExpectation::Message(Version),
        CapabilitiesRequest => ResponseExpectation::OneOf(CAPABILITY_RESPONSES),
        ServerRequest => ResponseExpectation::Message(ServerResponse),
        OpenReceiveChannel => ResponseExpectation::Message(OpenReceiveChannelAck),
        ConnectionStatisticsRequest => ResponseExpectation::Message(ConnectionStatisticsResponse),
        SoftKeySetRequest => ResponseExpectation::Message(SoftKeySetResponse),
        SoftKeyTemplateRequest => ResponseExpectation::Message(SoftKeyTemplateResponse),
        RegisterTokenRequest => ResponseExpectation::OneOf(REGISTER_TOKEN_RESPONSES),
        LineStatusRequest => ResponseExpectation::SessionSelected {
            before: LineStatus,
            from: LineStatusDynamic,
            selector: SessionResponseSelector::DynamicMessagesOrProtocol {
                minimum_protocol: 9,
            },
        },
        SpeedDialStatusRequest => ResponseExpectation::Message(SpeedDialStatus),
        ServiceUrlStatusRequest => ResponseExpectation::SessionSelected {
            before: ServiceUrlStatus,
            from: ServiceUrlStatusDynamic,
            selector: SessionResponseSelector::DynamicMessagesOrProtocol {
                minimum_protocol: 9,
            },
        },
        FeatureStatusRequest => ResponseExpectation::SessionSelected {
            before: FeatureStatus,
            from: FeatureStatusDynamic,
            selector: SessionResponseSelector::DynamicMessages,
        },
        CreateConferenceRequest => ResponseExpectation::Message(CreateConferenceResponse),
        DeleteConferenceRequest => ResponseExpectation::Message(DeleteConferenceResponse),
        ModifyConferenceRequest => ResponseExpectation::Message(ModifyConferenceResponse),
        AddParticipantRequest => ResponseExpectation::Message(AddParticipantResponse),
        AuditConferenceRequest => ResponseExpectation::Message(AuditConferenceResponse),
        AuditParticipantRequest => ResponseExpectation::Message(AuditParticipantResponse),
        PortRequest => ResponseExpectation::Message(PortResponse),
        SubscribeDtmfPayloadRequest => ResponseExpectation::Message(SubscribeDtmfPayloadResponse),
        UnsubscribeDtmfPayloadRequest => {
            ResponseExpectation::Message(UnsubscribeDtmfPayloadResponse)
        }
        StartMediaTransmission => ResponseExpectation::Message(StartMediaTransmissionAck),
        OpenMultimediaChannel => ResponseExpectation::Message(OpenMultimediaReceiveChannelAck),
        StartMultimediaTransmission => ResponseExpectation::Message(StartMultimediaTransmissionAck),
        CallCountRequest => ResponseExpectation::Message(CallCountResponse),
        _ => ResponseExpectation::None,
    }
}

const fn verification(id: MessageId) -> ContractVerification {
    use MessageId::*;
    match id {
        KeepAlive
        | Register
        | OffHook
        | OnHook
        | SoftKeyEvent
        | UpdateCapabilities
        | MediaPathEvent
        | OpenReceiveChannelAck
        | RegisterAck
        | SetRinger
        | SetLamp
        | StartMediaTransmission
        | ButtonTemplate
        | DefineTimeDate
        | SoftKeyTemplateResponse
        | SoftKeySetResponse
        | SelectSoftKeys
        | CallState
        | ActivateCallPlane
        | ClearPromptStatus
        | DisplayDynamicPromptStatus
        | LineStatusDynamic
        | CallInfoDynamic
        | OpenReceiveChannel
        | CloseReceiveChannel
        | StartMediaTransmissionAck => ContractVerification::StructuralAndValidated,
        _ => ContractVerification::Structural,
    }
}

message_catalog! {
    (KeepAlive, 0x0000, StationToControl),
    (Register, 0x0001, StationToControl),
    (IpPort, 0x0002, StationToControl),
    (KeypadButton, 0x0003, StationToControl),
    (EnblocCall, 0x0004, StationToControl),
    (Stimulus, 0x0005, StationToControl),
    (OffHook, 0x0006, StationToControl),
    (OnHook, 0x0007, StationToControl),
    (HookFlash, 0x0008, StationToControl),
    (ForwardStatusRequest, 0x0009, StationToControl),
    (SpeedDialStatusRequest, 0x000a, StationToControl),
    (LineStatusRequest, 0x000b, StationToControl),
    (ConfigStatusRequest, 0x000c, StationToControl),
    (TimeDateRequest, 0x000d, StationToControl),
    (ButtonTemplateRequest, 0x000e, StationToControl),
    (VersionRequest, 0x000f, StationToControl),
    (CapabilitiesResponse, 0x0010, StationToControl),
    (MediaPortList, 0x0011, StationToControl),
    (ServerRequest, 0x0012, StationToControl),
    (Alarm, 0x0020, StationToControl),
    (MulticastMediaReceptionAck, 0x0021, StationToControl),
    (OpenReceiveChannelAck, 0x0022, StationToControl),
    (ConnectionStatisticsResponse, 0x0023, StationToControl),
    (OffHookWithCallingParty, 0x0024, StationToControl),
    (SoftKeySetRequest, 0x0025, StationToControl),
    (SoftKeyEvent, 0x0026, StationToControl),
    (Unregister, 0x0027, StationToControl),
    (SoftKeyTemplateRequest, 0x0028, StationToControl),
    (RegisterTokenRequest, 0x0029, StationToControl),
    (MediaTransmissionFailure, 0x002a, StationToControl),
    (HeadsetStatus, 0x002b, StationToControl),
    (MediaResourceNotification, 0x002c, ServiceNodeToControl),
    (RegisterAvailableLines, 0x002d, StationToControl),
    (DeviceToUserData, 0x002e, StationToControl),
    (DeviceToUserDataResponse, 0x002f, StationToControl),
    (UpdateCapabilities, 0x0030, StationToControl),
    (OpenMultimediaReceiveChannelAck, 0x0031, StationToControl),
    (ClearConference, 0x0032, ServiceNodeToControl),
    (ServiceUrlStatusRequest, 0x0033, StationToControl),
    (FeatureStatusRequest, 0x0034, StationToControl),
    (CreateConferenceResponse, 0x0035, ServiceNodeToControl),
    (DeleteConferenceResponse, 0x0036, ServiceNodeToControl),
    (ModifyConferenceResponse, 0x0037, ServiceNodeToControl),
    (AddParticipantResponse, 0x0038, ServiceNodeToControl),
    (AuditConferenceResponse, 0x0039, ServiceNodeToControl),
    (AuditParticipantResponse, 0x0040, ServiceNodeToControl),
    (DeviceToUserDataV1, 0x0041, StationToControl),
    (DeviceToUserDataResponseV1, 0x0042, StationToControl),
    (UpdateCapabilitiesV2, 0x0043, StationToControl),
    (UpdateCapabilitiesV3, 0x0044, StationToControl),
    (PortResponse, 0x0045, ServiceNodeToControl),
    (QosReservationNotify, 0x0046, ServiceNodeToControl),
    (QosErrorNotify, 0x0047, ServiceNodeToControl),
    (SubscriptionStatusRequest, 0x0048, StationToControl),
    (MediaPathEvent, 0x0049, StationToControl),
    (MediaPathCapability, 0x004a, StationToControl),
    (MwiNotification, 0x004c, ServiceNodeToControl),

    (RegisterAck, 0x0081, ControlToStation),
    (StartTone, 0x0082, ControlToStation),
    (StopTone, 0x0083, ControlToStation),
    (SetRinger, 0x0085, ControlToStation),
    (SetLamp, 0x0086, ControlToStation),
    (SetHookFlashDetect, 0x0087, ControlToStation),
    (SetSpeakerMode, 0x0088, ControlToStation),
    (SetMicrophoneMode, 0x0089, ControlToStation),
    (StartMediaTransmission, 0x008a, ControlToStation),
    (StopMediaTransmission, 0x008b, ControlToStation),
    (StartMediaReception, 0x008c, ControlToStation),
    (StopMediaReception, 0x008d, ControlToStation),
    (CallInfo, 0x008f, ControlToStation),
    (ForwardStatus, 0x0090, ControlToStation),
    (SpeedDialStatus, 0x0091, ControlToStation),
    (LineStatus, 0x0092, ControlToStation),
    (ConfigStatus, 0x0093, ControlToStation),
    (DefineTimeDate, 0x0094, ControlToStation),
    (StartSessionTransmission, 0x0095, ControlToServiceNode),
    (StopSessionTransmission, 0x0096, ControlToServiceNode),
    (ButtonTemplate, 0x0097, ControlToStation),
    (Version, 0x0098, ControlToStation),
    (DisplayText, 0x0099, ControlToStation),
    (ClearDisplay, 0x009a, ControlToStation),
    (CapabilitiesRequest, 0x009b, ControlToStation),
    (EnunciatorCommand, 0x009c, ControlToStation),
    (RegisterReject, 0x009d, ControlToStation),
    (ServerResponse, 0x009e, ControlToStation),
    (Reset, 0x009f, ControlToStation),
    (KeepAliveAck, 0x0100, ControlToStation),
    (StartMulticastMediaReception, 0x0101, ControlToStation),
    (StartMulticastMediaTransmission, 0x0102, ControlToStation),
    (StopMulticastMediaReception, 0x0103, ControlToStation),
    (StopMulticastMediaTransmission, 0x0104, ControlToStation),
    (OpenReceiveChannel, 0x0105, ControlToStation),
    (CloseReceiveChannel, 0x0106, ControlToStation),
    (ConnectionStatisticsRequest, 0x0107, ControlToStation),
    (SoftKeyTemplateResponse, 0x0108, ControlToStation),
    (SoftKeySetResponse, 0x0109, ControlToStation),
    (SelectSoftKeys, 0x0110, ControlToStation),
    (CallState, 0x0111, ControlToStation),
    (DisplayPromptStatus, 0x0112, ControlToStation),
    (ClearPromptStatus, 0x0113, ControlToStation),
    (DisplayNotify, 0x0114, ControlToStation),
    (ClearNotify, 0x0115, ControlToStation),
    (ActivateCallPlane, 0x0116, ControlToStation),
    (DeactivateCallPlane, 0x0117, ControlToStation),
    (UnregisterAck, 0x0118, ControlToStation),
    (BackspaceResponse, 0x0119, ControlToStation),
    (RegisterTokenAck, 0x011a, ControlToStation),
    (RegisterTokenReject, 0x011b, ControlToStation),
    (StartMediaFailureDetection, 0x011c, ControlToStation),
    (DialedNumber, 0x011d, ControlToStation),
    (UserToDeviceData, 0x011e, ControlToStation),
    (FeatureStatus, 0x011f, ControlToStation),
    (DisplayPriorityNotify, 0x0120, ControlToStation),
    (ClearPriorityNotify, 0x0121, ControlToStation),
    (StartAnnouncement, 0x0122, IntraControl),
    (StopAnnouncement, 0x0123, IntraControl),
    (AnnouncementFinish, 0x0124, IntraControl),
    (NotifyDtmfTone, 0x0127, ControlToStation),
    (SendDtmfTone, 0x0128, ControlToStation),
    (SubscribeDtmfPayloadRequest, 0x0129, ControlToStation),
    (SubscribeDtmfPayloadResponse, 0x012a, StationToControl),
    (SubscribeDtmfPayloadError, 0x012b, ControlToStation),
    (UnsubscribeDtmfPayloadRequest, 0x012c, ControlToStation),
    (UnsubscribeDtmfPayloadResponse, 0x012d, StationToControl),
    (UnsubscribeDtmfPayloadError, 0x012e, ControlToStation),
    (ServiceUrlStatus, 0x012f, ControlToStation),
    (CallSelectStatus, 0x0130, ControlToStation),
    (OpenMultimediaChannel, 0x0131, ControlToStation),
    (StartMultimediaTransmission, 0x0132, ControlToStation),
    (StopMultimediaTransmission, 0x0133, ControlToStation),
    (MiscellaneousCommand, 0x0134, ControlToStation),
    (FlowControlCommand, 0x0135, ControlToStation),
    (CloseMultimediaReceiveChannel, 0x0136, ControlToStation),
    (CreateConferenceRequest, 0x0137, ControlToServiceNode),
    (DeleteConferenceRequest, 0x0138, ControlToServiceNode),
    (ModifyConferenceRequest, 0x0139, ControlToServiceNode),
    (AddParticipantRequest, 0x013a, ControlToServiceNode),
    (DropParticipantRequest, 0x013b, ControlToServiceNode),
    (AuditConferenceRequest, 0x013c, ControlToServiceNode),
    (AuditParticipantRequest, 0x013d, ControlToServiceNode),
    (ChangeParticipantRequest, 0x013e, ControlToServiceNode),
    (UserToDeviceDataV1, 0x013f, ControlToStation),
    (VideoDisplayCommand, 0x0140, ControlToStation),
    (FlowControlNotify, 0x0141, ControlToStation),
    (ConfigStatusDynamic, 0x0142, ControlToStation),
    (DisplayDynamicNotify, 0x0143, ControlToStation),
    (DisplayDynamicPriorityNotify, 0x0144, ControlToStation),
    (DisplayDynamicPromptStatus, 0x0145, ControlToStation),
    (FeatureStatusDynamic, 0x0146, ControlToStation),
    (LineStatusDynamic, 0x0147, ControlToStation),
    (ServiceUrlStatusDynamic, 0x0148, ControlToStation),
    (SpeedDialStatusDynamic, 0x0149, ControlToStation),
    (CallInfoDynamic, 0x014a, ControlToStation),
    (PortRequest, 0x014b, ControlToStation),
    (PortClose, 0x014c, ControlToStation),
    (QosListen, 0x014d, ControlToServiceNode),
    (QosPath, 0x014e, ControlToServiceNode),
    (QosTeardown, 0x014f, ControlToServiceNode),
    (UpdateDscp, 0x0150, ControlToServiceNode),
    (QosModify, 0x0151, ControlToServiceNode),

    (SubscriptionStatus, 0x0152, ControlToStation),
    (Notification, 0x0153, ControlToStation),
    (StartMediaTransmissionAck, 0x0154, StationToControl),
    (StartMultimediaTransmissionAck, 0x0155, StationToControl),
    (CallHistoryDisposition, 0x0156, ControlToStation),
    (LocationInfo, 0x0157, StationToControl),
    (MwiResponse, 0x0158, ControlToServiceNode),
    (ExtensionDeviceCapabilities, 0x0159, StationToControl),
    (XmlAlarm, 0x015a, StationToControl),
    (CallCountRequest, 0x015e, StationToControl),
    (CallCountResponse, 0x015f, ControlToStation),
    (RecordingStatus, 0x0160, ControlToStation),

    (SpcpRegisterTokenRequest, 0x8000, StationToControl),
    (SpcpRegisterTokenAck, 0x8100, ControlToStation),
    (SpcpRegisterTokenReject, 0x8101, ControlToStation),
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(value) => write!(f, "Unknown(0x{value:04x})"),
            known => f.write_str(known.name()),
        }
    }
}

/// Iterates the complete typed implementation inventory in wire-ID order.
///
/// Opaque-only contracts are intentionally excluded. To inspect every known
/// identifier, iterate [`MessageId::ALL_KNOWN`] and call
/// [`MessageId::contract`] instead.
pub fn implemented_message_contracts() -> impl Iterator<Item = MessageContract> {
    MessageId::ALL_KNOWN
        .iter()
        .filter_map(|id| id.contract())
        .filter(|contract| contract.codec == CodecSupport::Typed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn known_catalog_values_are_unique_and_round_trip() {
        let mut values = HashSet::new();
        for id in MessageId::ALL_KNOWN {
            assert!(values.insert(id.wire_value()), "duplicate {id}");
            assert_eq!(MessageId::from(id.wire_value()), *id);
            assert!(id.route().is_some());
            assert!(id.is_known());
        }
        assert!(MessageId::ALL_KNOWN.len() > 140);
    }

    #[test]
    fn supplemental_contract_scope_is_explicit_and_closed() {
        let supplemental = MessageId::ALL_KNOWN
            .iter()
            .copied()
            .filter(|id| id.contract().unwrap().scope == ContractScope::Supplemental)
            .collect::<Vec<_>>();

        assert_eq!(
            supplemental,
            [
                MessageId::IpPort,
                MessageId::MediaPortList,
                MessageId::SetHookFlashDetect,
                MessageId::StartMediaReception,
                MessageId::StopMediaReception,
                MessageId::EnunciatorCommand,
                MessageId::ExtensionDeviceCapabilities,
                MessageId::SpcpRegisterTokenRequest,
                MessageId::SpcpRegisterTokenAck,
                MessageId::SpcpRegisterTokenReject,
            ]
        );
    }

    #[test]
    fn unknown_identifiers_remain_lossless() {
        let id = MessageId::from(0xdead_beef);
        assert_eq!(id, MessageId::Unknown(0xdead_beef));
        assert_eq!(id.wire_value(), 0xdead_beef);
        assert_eq!(id.direction(), None);
    }

    #[test]
    fn dtmf_subscription_responses_have_the_device_to_server_direction() {
        assert_eq!(
            MessageId::SubscribeDtmfPayloadRequest.direction(),
            Some(MessageDirection::ServerToDevice)
        );
        assert_eq!(
            MessageId::SubscribeDtmfPayloadResponse.direction(),
            Some(MessageDirection::DeviceToServer)
        );
        assert_eq!(
            MessageId::UnsubscribeDtmfPayloadRequest.direction(),
            Some(MessageDirection::ServerToDevice)
        );
        assert_eq!(
            MessageId::UnsubscribeDtmfPayloadResponse.direction(),
            Some(MessageDirection::DeviceToServer)
        );
    }

    #[test]
    fn every_known_id_has_an_explicit_support_and_runtime_contract() {
        for id in MessageId::ALL_KNOWN {
            let contract = id.contract().expect("known ID has a contract");
            assert_eq!(contract.id, *id);
            assert_eq!(contract.route, id.route().unwrap());
            match contract.codec {
                CodecSupport::Typed => {
                    assert_eq!(contract.emission, EmissionSupport::Typed);
                    assert_ne!(contract.runtime_use, RuntimeUse::CatalogOnly);
                }
                CodecSupport::OpaqueOnly => {
                    assert_eq!(contract.emission, EmissionSupport::PreserveOnly);
                    assert_eq!(contract.runtime_use, RuntimeUse::CatalogOnly);
                    assert_eq!(contract.payload_layout, PayloadLayout::Opaque);
                }
            }
            match contract.field_fidelity {
                FieldFidelity::CanonicalServerOutput(detail) => {
                    assert!(matches!(
                        contract.route,
                        MessageRoute::ControlToStation
                            | MessageRoute::ControlToServiceNode
                            | MessageRoute::IntraControl
                    ));
                    assert!(!detail.is_empty());
                }
                FieldFidelity::SemanticProjection(detail) => {
                    assert_eq!(contract.codec, CodecSupport::Typed);
                    assert!(!detail.is_empty());
                }
                FieldFidelity::OpaquePreserved => {
                    assert_eq!(contract.codec, CodecSupport::OpaqueOnly);
                }
                FieldFidelity::Lossless => {}
            }
        }
        assert!(implemented_message_contracts().count() > 100);

        for id in [
            MessageId::OpenReceiveChannel,
            MessageId::StartMediaTransmission,
            MessageId::StartMediaTransmissionAck,
            MessageId::KeypadButton,
            MessageId::EnblocCall,
            MessageId::Register,
            MessageId::Alarm,
            MessageId::DefineTimeDate,
        ] {
            assert_eq!(
                id.contract().unwrap().field_fidelity,
                FieldFidelity::Lossless
            );
        }
    }

    #[test]
    fn semantic_field_fidelity_overclaims_are_explicitly_excluded() {
        for (id, omitted) in [
            (MessageId::LineStatus, "display label"),
            (MessageId::CallInfo, "mailboxes"),
            (MessageId::CallState, "visibility"),
        ] {
            let FieldFidelity::CanonicalServerOutput(detail) =
                id.contract().unwrap().field_fidelity
            else {
                panic!("{id} must not claim lossless field fidelity");
            };
            assert!(detail.contains(omitted), "{id}: {detail}");
        }

        for id in [MessageId::ConfigStatus, MessageId::ConfigStatusDynamic] {
            assert_eq!(
                id.contract().unwrap().field_fidelity,
                FieldFidelity::Lossless
            );
        }

        for id in [
            MessageId::CapabilitiesResponse,
            MessageId::MediaTransmissionFailure,
            MessageId::PortResponse,
        ] {
            assert!(matches!(
                id.contract().unwrap().field_fidelity,
                FieldFidelity::SemanticProjection(_)
            ));
        }
    }

    #[test]
    fn variable_layout_messages_never_claim_one_fixed_payload_size() {
        for contract in MessageId::ALL_KNOWN.iter().filter_map(|id| id.contract()) {
            if matches!(
                contract.payload_layout,
                PayloadLayout::VersionSelected
                    | PayloadLayout::VersionAndLengthSelected
                    | PayloadLayout::BoundedPreserved
            ) {
                assert_eq!(
                    contract.fixed_payload_bytes, None,
                    "{} has a variable payload layout",
                    contract.id
                );
            }
        }
    }

    #[test]
    fn bounded_and_counted_payload_contracts_report_their_wire_limits() {
        for id in [
            MessageId::KeepAlive,
            MessageId::ConfigStatusRequest,
            MessageId::ButtonTemplateRequest,
            MessageId::KeepAliveAck,
        ] {
            assert_eq!(
                id.contract().unwrap().payload_size_bounds,
                Some(PayloadSizeBounds {
                    minimum: 0,
                    maximum: MAX_FRAME_SIZE - HEADER_SIZE,
                }),
                "{id}"
            );
        }

        let capabilities = MessageId::CapabilitiesResponse.contract().unwrap();
        assert_eq!(capabilities.payload_layout, PayloadLayout::LengthPrefixed);
        assert_eq!(
            capabilities.payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 4,
                maximum: 292,
            })
        );

        let version_two = MessageId::UpdateCapabilitiesV2.contract().unwrap();
        assert_eq!(version_two.payload_layout, PayloadLayout::Fixed);
        assert_eq!(version_two.fixed_payload_bytes, Some(2_000));

        let version_three = MessageId::UpdateCapabilitiesV3.contract().unwrap();
        assert_eq!(
            version_three.payload_layout,
            PayloadLayout::MinimumLengthPreserved
        );
        assert_eq!(
            version_three.payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 20,
                maximum: 2_380,
            })
        );
    }

    #[test]
    fn service_message_payload_bounds_are_explicit() {
        assert_eq!(
            MessageId::XmlAlarm.contract().unwrap().payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 0,
                maximum: 2_048,
            })
        );
        assert_eq!(
            MessageId::AddParticipantResponse
                .contract()
                .unwrap()
                .payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 12,
                maximum: 272,
            })
        );

        for (id, size) in [
            (MessageId::AuditConferenceRequest, 0),
            (MessageId::SubscribeDtmfPayloadRequest, 16),
            (MessageId::SubscribeDtmfPayloadResponse, 12),
            (MessageId::SubscribeDtmfPayloadError, 12),
            (MessageId::UnsubscribeDtmfPayloadRequest, 16),
            (MessageId::UnsubscribeDtmfPayloadResponse, 12),
            (MessageId::UnsubscribeDtmfPayloadError, 12),
        ] {
            assert_eq!(
                id.contract().unwrap().payload_size_bounds,
                Some(PayloadSizeBounds {
                    minimum: size,
                    maximum: size,
                }),
                "{id}"
            );
        }
    }

    #[test]
    fn media_contracts_record_fixed_and_version_selected_sizes() {
        for id in [
            MessageId::OpenMultimediaReceiveChannelAck,
            MessageId::StartMultimediaTransmissionAck,
            MessageId::StartSessionTransmission,
            MessageId::StopSessionTransmission,
            MessageId::OpenMultimediaChannel,
            MessageId::StartMultimediaTransmission,
            MessageId::PortRequest,
            MessageId::PortClose,
        ] {
            let contract = id.contract().unwrap();
            assert_eq!(contract.payload_layout, PayloadLayout::VersionSelected);
            assert_eq!(contract.fixed_payload_bytes, None);
        }

        for (id, size) in [
            (MessageId::MulticastMediaReceptionAck, 12),
            (MessageId::CloseReceiveChannel, 16),
            (MessageId::StopMediaTransmission, 16),
            (MessageId::MiscellaneousCommand, 52),
            (MessageId::QosReservationNotify, 24),
            (MessageId::QosErrorNotify, 44),
            (MessageId::QosListen, 172),
            (MessageId::QosPath, 168),
            (MessageId::QosTeardown, 24),
            (MessageId::UpdateDscp, 24),
            (MessageId::QosModify, 152),
        ] {
            assert_eq!(id.contract().unwrap().fixed_payload_bytes, Some(size));
        }

        assert_eq!(
            MessageId::StartMediaTransmissionAck
                .contract()
                .unwrap()
                .payload_layout,
            PayloadLayout::VersionAndLengthSelected
        );
        assert_eq!(
            MessageId::LocationInfo
                .contract()
                .unwrap()
                .payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 2_404,
                maximum: 2_404,
            })
        );
        for id in [
            MessageId::CloseReceiveChannel,
            MessageId::StopMediaTransmission,
        ] {
            assert_eq!(
                id.contract().unwrap().field_fidelity,
                FieldFidelity::Lossless
            );
        }
    }

    #[test]
    fn session_transmission_contracts_use_the_service_node_codec() {
        for id in [
            MessageId::StartSessionTransmission,
            MessageId::StopSessionTransmission,
        ] {
            let contract = id.contract().unwrap();
            assert_eq!(contract.route, MessageRoute::ControlToServiceNode);
            assert_eq!(contract.codec, CodecSupport::Typed);
            assert_eq!(contract.emission, EmissionSupport::Typed);
            assert_eq!(contract.runtime_use, RuntimeUse::TypedButNotEmitted);
            assert_eq!(contract.field_fidelity, FieldFidelity::Lossless);
            assert_eq!(contract.payload_layout, PayloadLayout::VersionSelected);
        }
    }

    #[test]
    fn supplemental_token_messages_remain_preserve_only() {
        for id in [
            MessageId::SpcpRegisterTokenRequest,
            MessageId::SpcpRegisterTokenAck,
            MessageId::SpcpRegisterTokenReject,
        ] {
            let contract = id.contract().unwrap();
            assert_eq!(contract.codec, CodecSupport::OpaqueOnly);
            assert_eq!(contract.emission, EmissionSupport::PreserveOnly);
            assert_eq!(contract.runtime_use, RuntimeUse::CatalogOnly);
            assert_eq!(contract.payload_layout, PayloadLayout::Opaque);
        }
    }

    #[test]
    fn runtime_emission_is_distinct_from_typed_encodability() {
        let dtmf = MessageId::SubscribeDtmfPayloadRequest.contract().unwrap();
        assert_eq!(dtmf.codec, CodecSupport::Typed);
        assert_eq!(dtmf.emission, EmissionSupport::Typed);
        assert_eq!(dtmf.runtime_use, RuntimeUse::TypedButNotEmitted);

        let open = MessageId::OpenReceiveChannel.contract().unwrap();
        assert_eq!(open.runtime_use, RuntimeUse::ConditionalServerOutput);
        assert_eq!(
            open.response,
            ResponseExpectation::Message(MessageId::OpenReceiveChannelAck)
        );

        for id in [
            MessageId::MiscellaneousCommand,
            MessageId::FlowControlCommand,
            MessageId::FlowControlNotify,
        ] {
            assert_eq!(
                id.contract().unwrap().runtime_use,
                RuntimeUse::ConditionalServerOutput
            );
        }

        for (id, response, runtime_use) in [
            (
                MessageId::OpenMultimediaChannel,
                MessageId::OpenMultimediaReceiveChannelAck,
                RuntimeUse::ConditionalServerOutput,
            ),
            (
                MessageId::StartMultimediaTransmission,
                MessageId::StartMultimediaTransmissionAck,
                RuntimeUse::ConditionalServerOutput,
            ),
        ] {
            let contract = id.contract().unwrap();
            assert_eq!(contract.route, MessageRoute::ControlToStation);
            assert_eq!(contract.codec, CodecSupport::Typed);
            assert_eq!(contract.emission, EmissionSupport::Typed);
            assert_eq!(contract.runtime_use, runtime_use);
            assert_eq!(contract.field_fidelity, FieldFidelity::Lossless);
            assert_eq!(contract.payload_layout, PayloadLayout::VersionSelected);
            assert_eq!(contract.response, ResponseExpectation::Message(response));
        }
    }

    #[test]
    fn dynamic_response_contracts_include_every_session_selector() {
        for (request, before, from) in [
            (
                MessageId::ConfigStatusRequest,
                MessageId::ConfigStatus,
                MessageId::ConfigStatusDynamic,
            ),
            (
                MessageId::LineStatusRequest,
                MessageId::LineStatus,
                MessageId::LineStatusDynamic,
            ),
            (
                MessageId::ServiceUrlStatusRequest,
                MessageId::ServiceUrlStatus,
                MessageId::ServiceUrlStatusDynamic,
            ),
        ] {
            assert_eq!(
                request.contract().unwrap().response,
                ResponseExpectation::SessionSelected {
                    before,
                    from,
                    selector: SessionResponseSelector::DynamicMessagesOrProtocol {
                        minimum_protocol: 9,
                    },
                }
            );
        }

        assert_eq!(
            MessageId::FeatureStatusRequest.contract().unwrap().response,
            ResponseExpectation::SessionSelected {
                before: MessageId::FeatureStatus,
                from: MessageId::FeatureStatusDynamic,
                selector: SessionResponseSelector::DynamicMessages,
            }
        );
        assert_eq!(
            MessageId::SpeedDialStatusRequest
                .contract()
                .unwrap()
                .response,
            ResponseExpectation::Message(MessageId::SpeedDialStatus)
        );
    }
}
