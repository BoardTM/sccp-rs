//! Typed numeric values used by SCCP messages.
//!
//! Most Skinny numeric fields are extensible firmware contracts.  Data-bearing
//! `Unknown` variants keep them type-safe without making newer phones fail to
//! decode. Convert from the wire with `From<u32>`, inspect known values through
//! `ALL_KNOWN`, and use `wire_value` (or `Into<u32>`) when encoding.

use std::fmt;

use bitflags::bitflags;

use super::wire::CodecError;

macro_rules! wire_enum {
    ($(#[$meta:meta])* pub enum $name:ident { $($variant:ident = $value:expr),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum $name {
            $($variant,)+
            Unknown(u32),
        }

        impl $name {
            pub const ALL_KNOWN: &'static [Self] = &[$(Self::$variant,)+];

            pub const fn wire_value(self) -> u32 {
                match self {
                    $(Self::$variant => $value,)+
                    Self::Unknown(value) => value,
                }
            }

            pub const fn is_known(self) -> bool {
                !matches!(self, Self::Unknown(_))
            }
        }

        impl From<u32> for $name {
            fn from(value: u32) -> Self {
                match value {
                    $($value => Self::$variant,)+
                    value => Self::Unknown(value),
                }
            }
        }

        impl From<$name> for u32 {
            fn from(value: $name) -> Self {
                value.wire_value()
            }
        }
    };
}

/// A negotiated SCCP protocol version in the supported 3..=22 range.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProtocolVersion(u8);

impl ProtocolVersion {
    pub const MIN: Self = Self(3);
    pub const MAX: Self = Self(22);
    pub const V3: Self = Self(3);
    pub const V5: Self = Self(5);
    pub const V7: Self = Self(7);
    pub const V8: Self = Self(8);
    pub const V9: Self = Self(9);
    pub const V10: Self = Self(10);
    pub const V11: Self = Self(11);
    pub const V12: Self = Self(12);
    pub const V13: Self = Self(13);
    pub const V14: Self = Self(14);
    pub const V15: Self = Self(15);
    pub const V16: Self = Self(16);
    pub const V17: Self = Self(17);
    pub const V18: Self = Self(18);
    pub const V19: Self = Self(19);
    pub const V20: Self = Self(20);
    pub const V21: Self = Self(21);
    pub const V22: Self = Self(22);

    /// Validates and constructs an exact supported protocol version.
    pub fn new(value: u32) -> Result<Self, CodecError> {
        let value = u8::try_from(value).map_err(|_| CodecError::UnsupportedProtocol(value))?;
        if !(Self::MIN.0..=Self::MAX.0).contains(&value) {
            return Err(CodecError::UnsupportedProtocol(u32::from(value)));
        }
        Ok(Self(value))
    }

    /// Negotiate the highest version supported by both peers.
    pub fn negotiate(advertised: u32) -> Result<Self, CodecError> {
        if advertised < u32::from(Self::MIN.0) {
            return Err(CodecError::UnsupportedProtocol(advertised));
        }
        Self::new(advertised.min(u32::from(Self::MAX.0)))
    }

    pub const fn wire(self) -> u32 {
        self.0 as u32
    }

    /// Returns the group of version-dependent wire layouts selected by this version.
    pub const fn layout(self) -> LayoutProfile {
        match self.0 {
            3..=4 => LayoutProfile::V3,
            5..=7 => LayoutProfile::V5,
            8..=10 => LayoutProfile::V8,
            11..=14 => LayoutProfile::V11,
            15 => LayoutProfile::V15,
            16 => LayoutProfile::V16,
            17 => LayoutProfile::V17,
            18 => LayoutProfile::V18,
            19..=21 => LayoutProfile::V19,
            _ => LayoutProfile::V22,
        }
    }

    /// General station UI messages use dynamic text layouts from version 9.
    pub const fn uses_dynamic_general_ui(self) -> bool {
        self.0 >= Self::V9.0
    }

    /// Returns the number of strings in this version's dynamic call-info body.
    pub const fn dynamic_call_info_layout(self) -> DynamicCallInfoLayout {
        match self.0 {
            ..=15 => DynamicCallInfoLayout::Fields12,
            16..=18 => DynamicCallInfoLayout::Fields13,
            _ => DynamicCallInfoLayout::Fields15,
        }
    }

    /// Reports whether dynamic speed-dial status is selected by protocol version.
    pub const fn uses_dynamic_speed_dial_status(self) -> bool {
        self.0 >= Self::V9.0
    }
}

/// Version-selected shape of a dynamic call-information payload.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DynamicCallInfoLayout {
    Fields12,
    Fields13,
    Fields15,
}

impl DynamicCallInfoLayout {
    pub const fn string_count(self) -> usize {
        match self {
            Self::Fields12 => 12,
            Self::Fields13 => 13,
            Self::Fields15 => 15,
        }
    }
}

impl fmt::Debug for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "V{}", self.0)
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

impl TryFrom<u32> for ProtocolVersion {
    type Error = CodecError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProtocolVersion> for u32 {
    fn from(value: ProtocolVersion) -> Self {
        value.wire()
    }
}

/// Wire-layout transitions used between SCCP versions 3 and 22.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LayoutProfile {
    /// Layouts used by versions 3 and 4.
    V3,
    /// Layouts used by versions 5 through 7.
    V5,
    /// Layouts used by versions 8 through 10.
    V8,
    /// Layouts used by versions 11 through 14.
    V11,
    /// Layouts specific to version 15.
    V15,
    /// Layouts specific to version 16.
    V16,
    /// Layouts specific to version 17.
    V17,
    /// Layouts specific to version 18.
    V18,
    /// Layouts used by versions 19 through 21.
    V19,
    /// Layouts used by version 22.
    V22,
}

wire_enum! {
    /// Station device model identifier.
    pub enum DeviceType {
        Undefined = 0,
        Phone30SpPlus = 1,
        Phone12SpPlus = 2,
        Phone12Sp = 3,
        Phone12 = 4,
        Phone30Vip = 5,
        Cisco7910 = 6,
        Cisco7960 = 7,
        Cisco7940 = 8,
        Cisco7935 = 9,
        Vgc = 10,
        Ata186 = 12,
        Ata188 = 13,
        Virtual30SpPlus = 20,
        PhoneApplication = 21,
        AnalogAccess = 30,
        DigitalAccessPri = 40,
        DigitalAccessT1 = 41,
        DigitalAccessTitan2 = 42,
        AnalogAccessElvis = 43,
        DigitalAccessLennon = 47,
        ConferenceBridge = 50,
        ConferenceBridgeYoko = 51,
        ConferenceBridgeDixieland = 52,
        ConferenceBridgeSummit = 53,
        H225 = 60,
        H323Phone = 61,
        H323Trunk = 62,
        MusicOnHold = 70,
        Pilot = 71,
        TapiPort = 72,
        TapiRoutePoint = 73,
        VoiceInbox = 80,
        VoiceInboxAdmin = 81,
        LineAnnunciator = 82,
        SoftwareMtpDixieland = 83,
        CiscoMediaServer = 84,
        ConferenceBridgeFlint = 85,
        RouteList = 90,
        LoadSimulator = 100,
        MediaTerminationPoint = 110,
        MediaTerminationPointYoko = 111,
        MediaTerminationPointDixieland = 112,
        MediaTerminationPointSummit = 113,
        Cisco7941 = 115,
        Cisco7971 = 119,
        MgcpStation = 120,
        MgcpTrunk = 121,
        RasProxy = 122,
        CiscoAddon7914 = 124,
        Trunk = 125,
        Annunciator = 126,
        MonitorBridge = 127,
        Recorder = 128,
        MonitorBridgeYoko = 129,
        SipTrunk = 131,
        CiscoAddon7915_12 = 227,
        CiscoAddon7915_24 = 228,
        CiscoAddon7916_12 = 229,
        CiscoAddon7916_24 = 230,
        NokiaESeries = 275,
        Cisco7985 = 302,
        Cisco7911 = 307,
        Cisco7961Ge = 308,
        Cisco7941Ge = 309,
        Cisco7931 = 348,
        Cisco7921 = 365,
        Cisco7906 = 369,
        NokiaIcc = 376,
        Cisco7962 = 404,
        Cisco7937 = 431,
        Cisco7942 = 434,
        Cisco7945 = 435,
        Cisco7965 = 436,
        Cisco7975 = 437,
        Cisco7925 = 484,
        Cisco6921 = 495,
        Cisco6941 = 496,
        Cisco6961 = 497,
        Cisco6901 = 547,
        Cisco6911 = 548,
        Cisco6945 = 564,
        Cisco7926 = 577,
        Cisco8945 = 585,
        Cisco8941 = 586,
        CiscoIpCommunicator = 30016,
        Cisco7905 = 20000,
        Cisco7920 = 30002,
        Cisco7970 = 30006,
        Cisco7912 = 30007,
        Cisco7902 = 30008,
        Cisco7961 = 30018,
        Cisco7936 = 30019,
        AnalogGateway = 30027,
        BriGateway = 30028,
        Spa521s = 80000,
        Spa524sg = 80001,
        Spa502g = 80003,
        Spa504g = 80004,
        Spa525g = 80005,
        Spa508g = 80006,
        Spa509g = 80007,
        Spa525g2 = 80009,
        Spa303g = 80011,
        Spa512g = 80012,
        Spa514g = 80013,
        AddonSpa500s = 99991,
        AddonSpa500ds = 99992,
        AddonSpa932ds = 99993,
        NotDefined = 99999
    }
}

/// Broad media class for a codec capability.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CodecKind {
    Audio,
    Video,
    Text,
    Data,
    TelephoneEvent,
    Unknown,
}

wire_enum! {
    /// Skinny payload capability / codec identifier.
    pub enum Codec {
        None = 0x0000,
        NonStandard = 0x0001,
        Pcma = 0x0002,
        G711Alaw56k = 0x0003,
        Pcmu = 0x0004,
        G711Ulaw56k = 0x0005,
        G72264k = 0x0006,
        G72256k = 0x0007,
        G72248k = 0x0008,
        G7231 = 0x0009,
        G728 = 0x000a,
        G729 = 0x000b,
        G729A = 0x000c,
        Is11172 = 0x000d,
        Is13818 = 0x000e,
        G729B = 0x000f,
        G729Ab = 0x0010,
        GsmFullRate = 0x0012,
        GsmHalfRate = 0x0013,
        GsmEnhancedFullRate = 0x0014,
        Wideband256k = 0x0019,
        Data64k = 0x0020,
        Data56k = 0x0021,
        G7221_32k = 0x0028,
        G7221_24k = 0x0029,
        Aac = 0x002a,
        Mp4aLatm128 = 0x002b,
        Mp4aLatm64 = 0x002c,
        Mp4aLatm56 = 0x002d,
        Mp4aLatm48 = 0x002e,
        Mp4aLatm32 = 0x002f,
        Mp4aLatm24 = 0x0030,
        Mp4aLatm = 0x0031,
        Gsm = 0x0050,
        ActiveVoice = 0x0051,
        G726_32k = 0x0052,
        G726_24k = 0x0053,
        G726_16k = 0x0054,
        G729AnnexB = 0x0055,
        Ilbc = 0x0056,
        Isac = 0x0059,
        Opus = 0x005a,
        Amr = 0x0061,
        AmrWb = 0x0062,
        H261 = 0x0064,
        H263 = 0x0065,
        H263Plus = 0x0066,
        H264 = 0x0067,
        H264Svc = 0x0068,
        T120 = 0x0069,
        H224 = 0x006a,
        T38Fax = 0x006b,
        Tote = 0x006c,
        H265 = 0x006d,
        H264Uc = 0x006e,
        Xv150ModemRelay711u = 0x006f,
        NseVbd711u = 0x0070,
        Xv150ModemRelay729a = 0x0071,
        NseVbd729a = 0x0072,
        H264Fec = 0x0073,
        ClearChannel = 0x0078,
        UniversalTranscoder = 0x00de,
        DtmfOutOfBandRfc2833 = 0x0101,
        DtmfPassthrough = 0x0102,
        DtmfDynamic = 0x0103,
        DtmfOutOfBand = 0x0104,
        DtmfInBandRfc2833 = 0x0105,
        CfbTones = 0x0106,
        DtmfNoAudio = 0x012b,
        V150ModemRelay = 0x012c,
        V150Sprt = 0x012d,
        V150Sse = 0x012e
    }
}

impl Codec {
    /// Backward-compatible name for the SCCP numeric value.
    pub const fn skinny(self) -> u32 {
        self.wire_value()
    }

    /// Classifies this capability into its broad media family.
    pub const fn kind(self) -> CodecKind {
        match self {
            Self::H261
            | Self::H263
            | Self::H263Plus
            | Self::H264
            | Self::H264Svc
            | Self::H265
            | Self::H264Uc
            | Self::H264Fec => CodecKind::Video,
            Self::T120 | Self::H224 => CodecKind::Text,
            Self::Data64k
            | Self::Data56k
            | Self::T38Fax
            | Self::Tote
            | Self::Xv150ModemRelay711u
            | Self::NseVbd711u
            | Self::Xv150ModemRelay729a
            | Self::NseVbd729a
            | Self::ClearChannel
            | Self::UniversalTranscoder
            | Self::V150ModemRelay
            | Self::V150Sprt
            | Self::V150Sse => CodecKind::Data,
            Self::DtmfOutOfBandRfc2833
            | Self::DtmfPassthrough
            | Self::DtmfDynamic
            | Self::DtmfOutOfBand
            | Self::DtmfInBandRfc2833
            | Self::DtmfNoAudio
            | Self::CfbTones => CodecKind::TelephoneEvent,
            Self::None | Self::NonStandard | Self::Unknown(_) => CodecKind::Unknown,
            _ => CodecKind::Audio,
        }
    }

    /// Returns the nominal clock rate in hertz for supported audio codecs.
    ///
    /// Non-audio, unrecognized, and codecs without a defined mapping return
    /// `None`.
    pub const fn sample_rate(self) -> Option<u32> {
        match self {
            Self::G72264k
            | Self::G72256k
            | Self::G72248k
            | Self::G7221_32k
            | Self::G7221_24k
            | Self::Wideband256k
            | Self::AmrWb => Some(16_000),
            Self::Opus | Self::Isac => Some(48_000),
            codec if matches!(codec.kind(), CodecKind::Audio) => Some(8_000),
            _ => None,
        }
    }

    /// Returns the codec's default RTP payload type when statically assigned.
    ///
    /// Dynamically assigned codecs return `None` and require negotiated
    /// payload metadata.
    pub const fn rtp_payload_type(self) -> Option<u8> {
        match self {
            Self::Pcmu | Self::G711Ulaw56k => Some(0),
            Self::Gsm => Some(3),
            Self::G7231 => Some(4),
            Self::Pcma | Self::G711Alaw56k => Some(8),
            Self::G72264k | Self::G72256k | Self::G72248k => Some(9),
            Self::G729 | Self::G729A | Self::G729B | Self::G729Ab | Self::G729AnnexB => Some(18),
            Self::Wideband256k => Some(25),
            Self::Ilbc => Some(97),
            Self::G7221_32k => Some(102),
            Self::Opus => Some(107),
            Self::G726_32k => Some(112),
            _ => None,
        }
    }
}

wire_enum! {
    /// Station call-state indication shown by the call plane.
    pub enum CallState {
        OffHook = 1,
        OnHook = 2,
        RingOut = 3,
        RingIn = 4,
        Connected = 5,
        Busy = 6,
        Congestion = 7,
        Hold = 8,
        CallWaiting = 9,
        Transfer = 10,
        Park = 11,
        Proceed = 12,
        RemoteMultiline = 13,
        InvalidNumber = 14,
        HoldYellow = 15,
        IntercomOneWay = 16,
        HoldRed = 17
    }
}

wire_enum! {
    /// Direction and origin classification attached to call information.
    pub enum CallType {
        Inbound = 1,
        Outbound = 2,
        Forward = 3
    }
}

wire_enum! {
    /// Operational severity attached to a station alarm report.
    pub enum AlarmSeverity {
        Critical = 0,
        Warning = 1,
        Informational = 2,
        ProtocolUnknown = 4,
        Major = 7,
        Minor = 8,
        Marginal = 10,
        TraceInfo = 20
    }
}

wire_enum! {
    /// Result status returned by media-channel operations.
    pub enum MediaStatus {
        Ok = 0,
        UnspecifiedError = 1,
        OutOfChannels = 2,
        CodecTooComplex = 3,
        InvalidPartyId = 4,
        InvalidCallReference = 5,
        InvalidCodec = 6,
        InvalidPacketSize = 7,
        OutOfSockets = 8,
        EncoderOrDecoderFailed = 9,
        InvalidDynamicPayload = 10,
        RequestedAddressTypeUnavailable = 11,
        DeviceOnHook = 12
    }
}

wire_enum! {
    /// Physical or logical button stimulus reported by a station.
    pub enum Stimulus {
        Unused = 0x00,
        LastNumberRedial = 0x01,
        SpeedDial = 0x02,
        Hold = 0x03,
        Transfer = 0x04,
        ForwardAll = 0x05,
        ForwardBusy = 0x06,
        ForwardNoAnswer = 0x07,
        Display = 0x08,
        Line = 0x09,
        T120Chat = 0x0a,
        T120Whiteboard = 0x0b,
        T120ApplicationSharing = 0x0c,
        T120FileTransfer = 0x0d,
        Video = 0x0e,
        Voicemail = 0x0f,
        AnswerRelease = 0x10,
        AutoAnswer = 0x11,
        Select = 0x12,
        Privacy = 0x13,
        ServiceUrl = 0x14,
        BlfSpeedDial = 0x15,
        DirectedPark = 0x16,
        Intercom = 0x17,
        MaliciousCall = 0x1b,
        GenericAppB1 = 0x21,
        GenericAppB2 = 0x22,
        GenericAppB3 = 0x23,
        GenericAppB4 = 0x24,
        GenericAppB5 = 0x25,
        MultiblinkFeature = 0x26,
        MeetMeConference = 0x7b,
        Conference = 0x7d,
        CallPark = 0x7e,
        CallPickup = 0x7f,
        GroupCallPickup = 0x80,
        Mobility = 0x81,
        DoNotDisturb = 0x82,
        ConferenceList = 0x83,
        RemoveLastParticipant = 0x84,
        QualityReportTool = 0x85,
        Callback = 0x86,
        OtherPickup = 0x87,
        VideoMode = 0x88,
        NewCall = 0x89,
        EndCall = 0x8a,
        HuntGroupLogin = 0x8b,
        Queuing = 0x8f,
        ParkingLot = 0xc0,
        Messages = 0xc2,
        Directory = 0xc3,
        Application = 0xc5,
        Headset = 0xc6,
        Keypad = 0xf0,
        AcousticEchoCancellation = 0xfd,
        Undefined = 0xff
    }
}

wire_enum! {
    /// Soft-key events are one-based positions in the advertised template.
    pub enum SoftKey {
        Redial = 1,
        NewCall = 2,
        Hold = 3,
        Transfer = 4,
        ForwardAll = 5,
        ForwardBusy = 6,
        ForwardNoAnswer = 7,
        Backspace = 8,
        EndCall = 9,
        Resume = 10,
        Answer = 11,
        Info = 12,
        Conference = 13,
        Park = 14,
        Join = 15,
        MeetMe = 16,
        Pickup = 17,
        GroupPickup = 18,
        Monitor = 19,
        Callback = 20,
        Barge = 21,
        DoNotDisturb = 22,
        ConferenceList = 23,
        Select = 24,
        Private = 25,
        TransferToVoicemail = 26,
        DirectTransfer = 27,
        ImmediateDivert = 28,
        VideoMode = 29,
        Intercept = 30,
        Empty = 31,
        Dial = 32
    }
}

wire_enum! {
    /// Call-state context used to select an advertised soft-key set.
    pub enum KeyMode {
        OnHook = 0,
        Connected = 1,
        OnHold = 2,
        RingIn = 3,
        OffHook = 4,
        ConnectedTransfer = 5,
        DigitsFollowing = 6,
        ConnectedConference = 7,
        RingOut = 8,
        OffHookFeature = 9,
        InUseHint = 10,
        OnHookStealable = 11,
        HoldConference = 12,
        Empty = 13
    }
}

wire_enum! {
    /// Audible ringer pattern selected for the station.
    pub enum RingerMode {
        Off = 1,
        Inside = 2,
        Outside = 3,
        Feature = 4,
        Silent = 5,
        Urgent = 6,
        Bellcore1 = 7,
        Bellcore2 = 8,
        Bellcore3 = 9,
        Bellcore4 = 10,
        Bellcore5 = 11
    }
}

wire_enum! {
    /// Whether a ringer command applies once or continuously.
    pub enum RingDuration {
        Normal = 1,
        Single = 2
    }
}

wire_enum! {
    /// Visual state selected for a station lamp.
    pub enum LampMode {
        Off = 1,
        On = 2,
        Wink = 3,
        Flash = 4,
        Blink = 5,
        Hold = 6,
        Ring = 7,
        Custom1 = 8,
        Custom2 = 9
    }
}

wire_enum! {
    /// Tone identifier used by tone and announcement commands.
    pub enum Tone {
        Silence = 0x00,
        Dtmf1 = 0x01,
        Dtmf2 = 0x02,
        Dtmf3 = 0x03,
        Dtmf4 = 0x04,
        Dtmf5 = 0x05,
        Dtmf6 = 0x06,
        Dtmf7 = 0x07,
        Dtmf8 = 0x08,
        Dtmf9 = 0x09,
        Dtmf0 = 0x0a,
        DtmfStar = 0x0e,
        DtmfPound = 0x0f,
        DtmfA = 0x10,
        DtmfB = 0x11,
        DtmfC = 0x12,
        DtmfD = 0x13,
        InsideDial = 0x21,
        OutsideDial = 0x22,
        LineBusy = 0x23,
        Alerting = 0x24,
        Reorder = 0x25,
        RecorderWarning = 0x26,
        RecorderDetected = 0x27,
        Reverting = 0x28,
        ReceiverOffHook = 0x29,
        PartialDial = 0x2a,
        NoSuchNumber = 0x2b,
        BusyVerification = 0x2c,
        CallWaiting = 0x2d,
        Confirmation = 0x2e,
        CampOn = 0x2f,
        RecallDial = 0x30,
        ZipZip = 0x31,
        Zip = 0x32,
        BeepBonk = 0x33,
        Music = 0x34,
        Hold = 0x35,
        Test = 0x36,
        MonitorWarning = 0x37,
        AddCallWaiting = 0x40,
        PriorityCallWaiting = 0x41,
        BargeIn = 0x43,
        DistinctAlert = 0x44,
        PriorityAlert = 0x45,
        ReminderRing = 0x46,
        PrecedenceRingback = 0x47,
        Preemption = 0x48,
        NoTone = 0x7f,
        MeetMeGreeting = 0x80,
        MeetMeNumberInvalid = 0x81,
        MeetMeNumberFailed = 0x82,
        MeetMeEnterPin = 0x83,
        MeetMeInvalidPin = 0x84,
        MeetMeFailedPin = 0x85,
        MeetMeCfbFailed = 0x86,
        MeetMeEnterAccessCode = 0x87,
        MeetMeAccessCodeInvalid = 0x88,
        MeetMeAccessCodeFailed = 0x89
    }
}

wire_enum! {
    /// Media direction in which a station should play a tone.
    pub enum ToneDirection {
        User = 0,
        Network = 1,
        Both = 2
    }
}

wire_enum! {
    /// Station audio-path component named by a media-path event.
    pub enum MediaPathId {
        None = 0,
        Headset = 1,
        Handset = 2,
        Speaker = 3
    }
}

wire_enum! {
    /// Availability transition reported for a station media path.
    pub enum MediaPathEvent {
        None = 0,
        On = 1,
        Off = 2
    }
}

wire_enum! {
    /// Capability state reported for a station media path.
    pub enum MediaPathCapability {
        None = 0,
        Enable = 1,
        Disable = 2,
        Monitor = 3
    }
}

wire_enum! {
    /// Media class used when allocating or closing ports.
    pub enum MediaType {
        Invalid = 0,
        Audio = 1,
        MainVideo = 2,
        Fecc = 3,
        PresentationVideo = 4,
        Bfcp = 5,
        IxChannel = 6,
        T38 = 7
    }
}

wire_enum! {
    /// Transport family requested for a media endpoint.
    pub enum MediaTransport {
        Rtp = 1,
        Udp = 2,
        Tcp = 3
    }
}

wire_enum! {
    /// RSVP reservation direction carried by SCCP QoS service messages.
    pub enum QosDirection {
        Send = 1,
        Receive = 2,
        SendReceive = 3
    }
}

wire_enum! {
    /// RSVP reservation style used by QoS path setup.
    pub enum QosReservationStyle {
        FixedFilter = 1,
        SharedExplicit = 2,
        WildcardFilter = 3
    }
}

wire_enum! {
    /// QoS service failure reported independently of RSVP protocol errors.
    pub enum QosErrorCode {
        ReservationTimeout = 0,
        PathFailed = 1,
        ReservationFailed = 2,
        ListenFailed = 3,
        ResourceUnavailable = 4,
        ListenTimeout = 5,
        ReservationRetriesFailed = 6,
        PathRetriesFailed = 7,
        ReservationPreempted = 8,
        PathPreempted = 9,
        ReservationModifyFailed = 10,
        PathModifyFailed = 11,
        ReservationTornDown = 12
    }
}

wire_enum! {
    /// RSVP protocol error returned by a failed QoS reservation.
    pub enum RsvpErrorCode {
        Confirm = 0,
        Admission = 1,
        Administrative = 2,
        NoPathInformation = 3,
        NoSenderInformation = 4,
        ConflictingStyle = 5,
        UnknownStyle = 6,
        ConflictingDestinationPorts = 7,
        ConflictingSourcePorts = 8,
        ServicePreempted = 12,
        UnknownObjectClass = 13,
        UnknownClassType = 14,
        Api = 20,
        Traffic = 21,
        TrafficSystem = 22,
        System = 23,
        RoutingProblem = 24
    }
}

wire_enum! {
    /// Requested acknowledgement behavior at the end of an announcement.
    pub enum EndOfAnnouncementAck {
        NotRequired = 0,
        Required = 1
    }
}

wire_enum! {
    /// Ordering policy for playing an announcement sequence.
    pub enum AnnouncementPlayMode {
        XmlConfigured = 0,
        OneShot = 1,
        Continuous = 2
    }
}

wire_enum! {
    /// Completion status returned when announcement playback finishes.
    pub enum AnnouncementPlayStatus {
        Ok = 0,
        Error = 1
    }
}

wire_enum! {
    /// Result returned for a message-waiting notification.
    pub enum MessageWaitingResult {
        Ok = 0,
        GeneralError = 1,
        RequestRejected = 2,
        VoicemailCountOutOfBounds = 3,
        FaxCountOutOfBounds = 4,
        InvalidPriorityVoicemailCount = 5,
        InvalidPriorityFaxCount = 6
    }
}

wire_enum! {
    /// Whether a connection-statistics response clears the station counters.
    pub enum StatisticsProcessing {
        Clear = 0,
        DoNotClear = 1
    }
}

wire_enum! {
    /// Network-address family selected by a versioned media layout.
    pub enum IpAddressType {
        Ipv4 = 0,
        Ipv6 = 1,
        Ipv4AndIpv6 = 2,
        Invalid = 3
    }
}

wire_enum! {
    /// Restart scope requested from a station.
    pub enum ResetType {
        Reset = 1,
        Restart = 2,
        ApplyConfiguration = 3
    }
}

wire_enum! {
    /// Policy used to transport connected-call DTMF digits.
    pub enum DtmfMode {
        Auto = 0,
        Rfc2833 = 1,
        Skinny = 2
    }
}

wire_enum! {
    /// Call-forwarding condition represented by a forwarding entry.
    pub enum CallForwardKind {
        None = 0,
        All = 1,
        Busy = 2,
        NoAnswer = 3
    }
}

wire_enum! {
    /// Precedence assigned to a call or media request.
    pub enum CallPriority {
        Highest = 0,
        High = 1,
        Medium = 2,
        Low = 3,
        Normal = 4
    }
}

wire_enum! {
    /// Ordered status-line notification slots. Larger values take precedence.
    pub enum NotificationPriority {
        Idle = 0,
        Voicemail = 1,
        Monitor = 2,
        Privacy = 3,
        DoNotDisturb = 4,
        CallForward = 5,
        Timed = 6
    }
}

wire_enum! {
    /// Visibility policy for call-information presentation.
    pub enum CallInfoVisibility {
        Default = 0,
        Collapsed = 1,
        Hidden = 2
    }
}

wire_enum! {
    /// Security indication presented for a call.
    pub enum CallSecurityState {
        UnknownState = 0,
        NotAuthenticated = 1,
        Authenticated = 2
    }
}

wire_enum! {
    /// Busy-lamp-field availability reported by a subscription notification.
    pub enum BusyLampFieldState {
        UnknownState = 0,
        Idle = 1,
        InUse = 2,
        DoNotDisturb = 3,
        Alerting = 4
    }
}

wire_enum! {
    /// Result of a phone-book/BLF subscription request.
    pub enum SubscriptionCause {
        Ok = 0,
        RouteFailure = 1,
        AuthenticationFailure = 2,
        Timeout = 3,
        TrunkTerminated = 4,
        TrunkForbidden = 5,
        Throttled = 6
    }
}

wire_enum! {
    /// Picture-size profile used by a video capability.
    pub enum VideoFormat {
        Undefined = 0,
        Sqcif = 1,
        Qcif = 2,
        Cif = 3,
        Cif4 = 4,
        Cif16 = 5,
        Custom = 6,
        ProtocolUnknown = 232
    }
}

wire_enum! {
    /// Codec-specific operation carried by a miscellaneous multimedia command.
    pub enum MiscCommandType {
        VideoFreezePicture = 0,
        VideoFastUpdatePicture = 1,
        VideoFastUpdateGob = 2,
        VideoFastUpdateMacroblock = 3,
        LostPicture = 4,
        LostPartialPicture = 5,
        RecoveryReferencePicture = 6,
        TemporalSpatialTradeoff = 7
    }
}

wire_enum! {
    /// Station echo-cancellation policy for an audio channel.
    pub enum EchoCancellation {
        Off = 0,
        On = 1
    }
}

wire_enum! {
    /// Station-side voice-activity detection/silence suppression policy.
    pub enum SilenceSuppression {
        Off = 0,
        On = 1
    }
}

wire_enum! {
    /// Bit-rate selector occupying the codec qualifier word for G.723.
    pub enum G723BitRate {
        Rate5_3 = 1,
        Rate6_3 = 2
    }
}

wire_enum! {
    /// Station result attached to an unregister acknowledgement.
    pub enum UnregisterStatus {
        Ok = 0,
        Error = 1,
        ActiveCall = 2
    }
}

wire_enum! {
    /// Button definitions use the stimulus values plus provisioning-only
    /// placeholder values in the 0xf1..=0xf5 range.
    pub enum ButtonType {
        Unused = 0x00,
        LastNumberRedial = 0x01,
        SpeedDial = 0x02,
        Hold = 0x03,
        Transfer = 0x04,
        ForwardAll = 0x05,
        ForwardBusy = 0x06,
        ForwardNoAnswer = 0x07,
        Display = 0x08,
        Line = 0x09,
        T120Chat = 0x0a,
        T120Whiteboard = 0x0b,
        T120ApplicationSharing = 0x0c,
        T120FileTransfer = 0x0d,
        Video = 0x0e,
        Voicemail = 0x0f,
        AnswerRelease = 0x10,
        AutoAnswer = 0x11,
        Select = 0x12,
        Feature = 0x13,
        ServiceUrl = 0x14,
        BlfSpeedDial = 0x15,
        DirectedPark = 0x16,
        Intercom = 0x17,
        MaliciousCall = 0x1b,
        GenericAppB1 = 0x21,
        GenericAppB2 = 0x22,
        GenericAppB3 = 0x23,
        GenericAppB4 = 0x24,
        GenericAppB5 = 0x25,
        MultiblinkFeature = 0x26,
        MeetMeConference = 0x7b,
        Conference = 0x7d,
        CallPark = 0x7e,
        CallPickup = 0x7f,
        GroupCallPickup = 0x80,
        Mobility = 0x81,
        DoNotDisturb = 0x82,
        ConferenceList = 0x83,
        RemoveLastParticipant = 0x84,
        QualityReportTool = 0x85,
        Callback = 0x86,
        OtherPickup = 0x87,
        VideoMode = 0x88,
        NewCall = 0x89,
        EndCall = 0x8a,
        HuntGroupLogin = 0x8b,
        Queuing = 0x8f,
        ParkingLot = 0xc0,
        Messages = 0xc2,
        Directory = 0xc3,
        Application = 0xc5,
        Headset = 0xc6,
        Keypad = 0xf0,
        PlaceholderMulti = 0xf1,
        PlaceholderLine = 0xf2,
        PlaceholderSpeedDial = 0xf3,
        PlaceholderHint = 0xf4,
        PlaceholderAbbreviatedDial = 0xf5,
        AcousticEchoCancellation = 0xfd,
        Undefined = 0xff
    }
}

wire_enum! {
    /// SRTP encryption algorithm selected for a media channel.
    pub enum EncryptionMethod {
        None = 0,
        Aes128HmacSha1_32 = 1,
        Aes128HmacSha1_80 = 2,
        F8_128HmacSha1_32 = 3,
        F8_128HmacSha1_80 = 4,
        AeadAes128Gcm = 5,
        AeadAes256Gcm = 6
    }
}

wire_enum! {
    /// SRTP algorithm support advertised by a media capability.
    pub enum EncryptionCapability {
        NotCapable = 0,
        Capable = 1
    }
}

wire_enum! {
    /// History bucket assigned to a completed call.
    pub enum CallHistoryDisposition {
        Ignore = 0,
        Placed = 1,
        Received = 2,
        Missed = 3,
        ProtocolUnknown = 0xffff_fffe
    }
}

wire_enum! {
    /// Station speaker state selected by call control.
    pub enum SpeakerMode {
        On = 1,
        Off = 2
    }
}

wire_enum! {
    /// Station microphone state selected by call control.
    pub enum MicrophoneMode {
        On = 1,
        Off = 2
    }
}

wire_enum! {
    /// Media resource allocated for a station-managed conference.
    pub enum ConferenceResourceType {
        Conference = 0,
        InteractiveVoiceResponse = 1
    }
}

wire_enum! {
    /// Outcome returned by conference creation.
    pub enum CreateConferenceResult {
        Ok = 0,
        ResourceNotAvailable = 1,
        ConferenceAlreadyExists = 2,
        SystemError = 3
    }
}

wire_enum! {
    /// Outcome returned by conference deletion.
    pub enum DeleteConferenceResult {
        Ok = 0,
        ConferenceDoesNotExist = 1,
        SystemError = 2
    }
}

wire_enum! {
    /// Outcome returned by conference modification.
    pub enum ModifyConferenceResult {
        Ok = 0,
        ResourceNotAvailable = 1,
        ConferenceDoesNotExist = 2,
        InvalidParameter = 3,
        MoreActiveCallsThanReserved = 4,
        InvalidResourceType = 5,
        SystemError = 6
    }
}

wire_enum! {
    /// Outcome returned when attaching a conference participant.
    pub enum AddParticipantResult {
        Ok = 0,
        ResourceNotAvailable = 1,
        ConferenceDoesNotExist = 2,
        DuplicateCallReference = 3,
        SystemError = 4
    }
}

wire_enum! {
    /// Outcome returned by a conference-participant audit.
    pub enum AuditParticipantResult {
        Ok = 0,
        ConferenceDoesNotExist = 1
    }
}

bitflags! {
    /// Identity fields a station must suppress for a conference participant.
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
    pub struct PartyInformationRestrictions: u32 {
        const CALLING_NAME = 1 << 0;
        const CALLING_NUMBER = 1 << 1;
        const CALLED_NAME = 1 << 2;
        const CALLED_NUMBER = 1 << 3;
        const ORIGINAL_CALLED_NAME = 1 << 4;
        const ORIGINAL_CALLED_NUMBER = 1 << 5;
        const LAST_REDIRECT_NAME = 1 << 6;
        const LAST_REDIRECT_NUMBER = 1 << 7;
    }
}

/// Negotiated inputs that select station-facing message layouts.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct StationSessionContext {
    /// Negotiated frame and payload version.
    pub protocol: ProtocolVersion,
    /// Station feature bits that can select layouts independently of version.
    pub features: PhoneFeatures,
}

impl StationSessionContext {
    /// Creates the layout-selection context for a registered station session.
    pub const fn new(protocol: ProtocolVersion, features: PhoneFeatures) -> Self {
        Self { protocol, features }
    }

    /// Reports whether general UI responses use their dynamic string layouts.
    pub const fn uses_dynamic_general_ui(self) -> bool {
        self.protocol.uses_dynamic_general_ui()
            || self.features.contains(PhoneFeatures::DYNAMIC_MESSAGES)
    }

    /// Reports whether feature status uses its dynamic response identifier.
    pub const fn uses_dynamic_feature_status(self) -> bool {
        self.features.contains(PhoneFeatures::DYNAMIC_MESSAGES)
    }

    /// Reports whether speed-dial status uses the dynamic identifier and
    /// variable-string payload selected by the registered station session.
    pub const fn uses_dynamic_speed_dial_status(self) -> bool {
        self.protocol.uses_dynamic_speed_dial_status()
            || self.features.contains(PhoneFeatures::DYNAMIC_MESSAGES)
    }

    /// Returns the call-info string layout selected by the negotiated version.
    pub const fn dynamic_call_info_layout(self) -> DynamicCallInfoLayout {
        self.protocol.dynamic_call_info_layout()
    }

    /// Returns the dynamic service-URL string count selected by the session.
    pub const fn dynamic_service_url_string_count(self) -> usize {
        if self.protocol.wire() >= ProtocolVersion::V19.wire() {
            3
        } else {
            2
        }
    }
}

impl From<ProtocolVersion> for StationSessionContext {
    fn from(protocol: ProtocolVersion) -> Self {
        Self::new(protocol, PhoneFeatures::empty())
    }
}

/// Dynamic RTP payload type used for telephone-event DTMF when a station is
/// configured to send digits through the media stream.
pub const RFC2833_TELEPHONE_EVENT_PAYLOAD: u8 = 101;

impl DtmfMode {
    /// Resolves the automatic policy against the feature bits advertised by
    /// the registered station. Explicit policies are always preserved.
    pub const fn resolve(self, features: PhoneFeatures) -> Self {
        match self {
            Self::Auto if features.contains(PhoneFeatures::RFC2833) => Self::Rfc2833,
            Self::Auto => Self::Skinny,
            explicit => explicit,
        }
    }

    /// Returns the wire payload type for the resolved policy. A zero payload
    /// tells the station to report connected-call digits as signaling events.
    pub const fn telephone_event_payload(self, features: PhoneFeatures) -> u8 {
        match self.resolve(features) {
            Self::Rfc2833 => RFC2833_TELEPHONE_EVENT_PAYLOAD,
            Self::Skinny | Self::Unknown(_) => 0,
            Self::Auto => unreachable!(),
        }
    }
}

bitflags! {
    /// Feature flags advertised in the three-byte station protocol field.
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
    pub struct PhoneFeatures: u32 {
        // The low byte contains the protocol version. Feature bits occupy the
        // following three bytes.
        const PORT_REQUEST = 1 << 17;
        const UTF8 = 1 << 20;
        const DYNAMIC_MESSAGES = 1 << 24;
        const RFC2833 = 1 << 26;
        const INTERNAL_CM_MEDIA = 1 << 28;
        const MULTIPLE_ACTIVE_CALLS = 1 << 30;
        const ABBREVIATED_DIAL = 1 << 31;
    }
}

bitflags! {
    /// Permitted media directions in a capability entry.
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
    pub struct ReceiveTransmit: u32 {
        const RECEIVE = 1;
        const TRANSMIT = 2;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// One keypad digit, including the extended A-D symbols.
pub enum Digit {
    /// A numeric digit; valid decoded values are zero through nine.
    Number(u8),
    Star,
    Pound,
    A,
    B,
    C,
    D,
    /// An unrecognized keypad word retained from the wire.
    Unknown(u32),
}

impl Digit {
    /// Converts a keypad wire word to a typed digit without discarding unknowns.
    pub const fn from_keypad(value: u32) -> Self {
        match value {
            0..=9 => Self::Number(value as u8),
            10 => Self::Star,
            11 => Self::Pound,
            12 => Self::A,
            13 => Self::B,
            14 => Self::C,
            15 => Self::D,
            value => Self::Unknown(value),
        }
    }

    /// Returns the numeric keypad word used on the wire.
    pub const fn keypad_value(self) -> u32 {
        match self {
            Self::Number(number) => number as u32,
            Self::Star => 10,
            Self::Pound => 11,
            Self::A => 12,
            Self::B => 13,
            Self::C => 14,
            Self::D => 15,
            Self::Unknown(value) => value,
        }
    }

    /// Returns the printable digit, or `?` for an invalid/unknown value.
    pub fn as_char(self) -> char {
        match self {
            Self::Number(n) if n <= 9 => char::from(b'0' + n),
            Self::Number(_) => '?',
            Self::Star => '*',
            Self::Pound => '#',
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
            Self::Unknown(_) => '?',
        }
    }
}

impl From<u32> for Digit {
    fn from(value: u32) -> Self {
        Self::from_keypad(value)
    }
}

impl From<Digit> for u32 {
    fn from(value: Digit) -> Self {
        value.keypad_value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_versions_select_layout_profiles() {
        assert_eq!(ProtocolVersion::new(3).unwrap().layout(), LayoutProfile::V3);
        assert_eq!(
            ProtocolVersion::new(14).unwrap().layout(),
            LayoutProfile::V11
        );
        assert_eq!(ProtocolVersion::new(13).unwrap(), ProtocolVersion::V13);
        assert_eq!(ProtocolVersion::new(14).unwrap(), ProtocolVersion::V14);
        assert_eq!(
            ProtocolVersion::new(18).unwrap().layout(),
            LayoutProfile::V18
        );
        assert_eq!(
            ProtocolVersion::new(21).unwrap().layout(),
            LayoutProfile::V19
        );
        assert_eq!(
            ProtocolVersion::new(22).unwrap().layout(),
            LayoutProfile::V22
        );
        assert_eq!(
            ProtocolVersion::negotiate(99).unwrap(),
            ProtocolVersion::V22
        );
        assert!(ProtocolVersion::new(2).is_err());
    }

    #[test]
    fn dynamic_station_layout_boundaries_follow_session_negotiation() {
        assert!(!ProtocolVersion::V8.uses_dynamic_general_ui());
        assert!(ProtocolVersion::V9.uses_dynamic_general_ui());
        assert!(!ProtocolVersion::V8.uses_dynamic_speed_dial_status());
        assert!(ProtocolVersion::V9.uses_dynamic_speed_dial_status());

        assert_eq!(
            ProtocolVersion::V15.dynamic_call_info_layout(),
            DynamicCallInfoLayout::Fields12
        );
        assert_eq!(
            ProtocolVersion::V16.dynamic_call_info_layout(),
            DynamicCallInfoLayout::Fields13
        );
        assert_eq!(
            ProtocolVersion::V18.dynamic_call_info_layout(),
            DynamicCallInfoLayout::Fields13
        );
        assert_eq!(
            ProtocolVersion::V19.dynamic_call_info_layout(),
            DynamicCallInfoLayout::Fields15
        );

        let baseline = StationSessionContext::from(ProtocolVersion::V8);
        assert!(!baseline.uses_dynamic_general_ui());
        assert!(!baseline.uses_dynamic_feature_status());
        let negotiated =
            StationSessionContext::new(ProtocolVersion::V8, PhoneFeatures::DYNAMIC_MESSAGES);
        assert!(negotiated.uses_dynamic_general_ui());
        assert!(negotiated.uses_dynamic_feature_status());
        assert!(negotiated.uses_dynamic_speed_dial_status());
    }

    #[test]
    fn extensible_values_preserve_unknown_numbers() {
        let codec = Codec::from(0xfeed);
        assert_eq!(codec, Codec::Unknown(0xfeed));
        assert_eq!(codec.wire_value(), 0xfeed);
        let state = CallState::from(1000);
        assert_eq!(state.wire_value(), 1000);
    }

    #[test]
    fn soft_key_events_are_template_positions() {
        assert_eq!(SoftKey::from(1), SoftKey::Redial);
        assert_eq!(SoftKey::from(13), SoftKey::Conference);
        assert_eq!(SoftKey::from(32), SoftKey::Dial);
        assert_eq!(SoftKey::from(201), SoftKey::Unknown(201));
    }

    #[test]
    fn automatic_dtmf_uses_only_an_advertised_rfc2833_capability() {
        assert_eq!(
            DtmfMode::Auto.resolve(PhoneFeatures::RFC2833),
            DtmfMode::Rfc2833
        );
        assert_eq!(
            DtmfMode::Auto.telephone_event_payload(PhoneFeatures::RFC2833),
            RFC2833_TELEPHONE_EVENT_PAYLOAD
        );
        assert_eq!(
            DtmfMode::Auto.resolve(PhoneFeatures::empty()),
            DtmfMode::Skinny
        );
        assert_eq!(
            DtmfMode::Auto.telephone_event_payload(PhoneFeatures::empty()),
            0
        );
        assert_eq!(
            DtmfMode::Skinny.telephone_event_payload(PhoneFeatures::RFC2833),
            0
        );
        assert_eq!(
            DtmfMode::Rfc2833.telephone_event_payload(PhoneFeatures::empty()),
            RFC2833_TELEPHONE_EVENT_PAYLOAD
        );
    }

    #[test]
    fn phone_feature_bits_match_the_three_register_feature_bytes() {
        // CP-7961G firmware SCCP41.9-4-2SR3-1S advertises protocol/features
        // 16 00 72 85. The final feature byte carries dynamic messages,
        // RFC2833, and abbreviated dial.
        let advertised = PhoneFeatures::from_bits_retain(0x8572_0000);
        assert!(advertised.contains(PhoneFeatures::PORT_REQUEST));
        assert!(advertised.contains(PhoneFeatures::UTF8));
        assert!(advertised.contains(PhoneFeatures::DYNAMIC_MESSAGES));
        assert!(advertised.contains(PhoneFeatures::RFC2833));
        assert!(advertised.contains(PhoneFeatures::ABBREVIATED_DIAL));
        assert!(!advertised.contains(PhoneFeatures::INTERNAL_CM_MEDIA));
        assert!(!advertised.contains(PhoneFeatures::MULTIPLE_ACTIVE_CALLS));
        assert!(
            PhoneFeatures::from_bits_retain(1 << 30).contains(PhoneFeatures::MULTIPLE_ACTIVE_CALLS)
        );
    }

    #[test]
    fn every_named_wire_enum_value_is_unique_and_round_trips() {
        macro_rules! assert_wire_enum {
            ($type:ty) => {{
                let mut values = std::collections::HashSet::new();
                for value in <$type>::ALL_KNOWN {
                    assert!(
                        values.insert(value.wire_value()),
                        "duplicate {} value {value:?}",
                        stringify!($type)
                    );
                    assert_eq!(<$type>::from(value.wire_value()), *value);
                    assert!(value.is_known());
                }
            }};
        }

        assert_wire_enum!(DeviceType);
        assert_wire_enum!(Codec);
        assert_wire_enum!(CallState);
        assert_wire_enum!(CallType);
        assert_wire_enum!(AlarmSeverity);
        assert_wire_enum!(MediaStatus);
        assert_wire_enum!(Stimulus);
        assert_wire_enum!(SoftKey);
        assert_wire_enum!(KeyMode);
        assert_wire_enum!(RingerMode);
        assert_wire_enum!(RingDuration);
        assert_wire_enum!(LampMode);
        assert_wire_enum!(Tone);
        assert_wire_enum!(ToneDirection);
        assert_wire_enum!(MediaPathId);
        assert_wire_enum!(MediaPathEvent);
        assert_wire_enum!(MediaPathCapability);
        assert_wire_enum!(MediaType);
        assert_wire_enum!(MediaTransport);
        assert_wire_enum!(QosDirection);
        assert_wire_enum!(QosReservationStyle);
        assert_wire_enum!(QosErrorCode);
        assert_wire_enum!(RsvpErrorCode);
        assert_wire_enum!(EndOfAnnouncementAck);
        assert_wire_enum!(AnnouncementPlayMode);
        assert_wire_enum!(AnnouncementPlayStatus);
        assert_wire_enum!(MessageWaitingResult);
        assert_wire_enum!(IpAddressType);
        assert_wire_enum!(ResetType);
        assert_wire_enum!(DtmfMode);
        assert_wire_enum!(CallForwardKind);
        assert_wire_enum!(CallPriority);
        assert_wire_enum!(NotificationPriority);
        assert_wire_enum!(CallInfoVisibility);
        assert_wire_enum!(CallSecurityState);
        assert_wire_enum!(BusyLampFieldState);
        assert_wire_enum!(SubscriptionCause);
        assert_wire_enum!(VideoFormat);
        assert_wire_enum!(MiscCommandType);
        assert_wire_enum!(EchoCancellation);
        assert_wire_enum!(SilenceSuppression);
        assert_wire_enum!(G723BitRate);
        assert_wire_enum!(UnregisterStatus);
        assert_wire_enum!(ButtonType);
        assert_wire_enum!(EncryptionMethod);
        assert_wire_enum!(EncryptionCapability);
        assert_wire_enum!(CallHistoryDisposition);
        assert_wire_enum!(SpeakerMode);
        assert_wire_enum!(MicrophoneMode);
        assert_wire_enum!(ConferenceResourceType);
        assert_wire_enum!(CreateConferenceResult);
        assert_wire_enum!(DeleteConferenceResult);
        assert_wire_enum!(ModifyConferenceResult);
        assert_wire_enum!(AddParticipantResult);
        assert_wire_enum!(AuditParticipantResult);
    }

    #[test]
    fn codec_metadata_covers_static_and_cisco_dynamic_payloads() {
        assert_eq!(Codec::Pcmu.rtp_payload_type(), Some(0));
        assert_eq!(Codec::G711Ulaw56k.rtp_payload_type(), Some(0));
        assert_eq!(Codec::Pcma.rtp_payload_type(), Some(8));
        assert_eq!(Codec::G72248k.rtp_payload_type(), Some(9));
        assert_eq!(Codec::Wideband256k.rtp_payload_type(), Some(25));
        assert_eq!(Codec::Ilbc.rtp_payload_type(), Some(97));
        assert_eq!(Codec::G7221_32k.rtp_payload_type(), Some(102));
        assert_eq!(Codec::Opus.rtp_payload_type(), Some(107));
        assert_eq!(Codec::G726_32k.rtp_payload_type(), Some(112));
        assert_eq!(Codec::Wideband256k.sample_rate(), Some(16_000));
        assert_eq!(Codec::H264.kind(), CodecKind::Video);
        assert_eq!(Codec::ClearChannel.kind(), CodecKind::Data);
    }
}
