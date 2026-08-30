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
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[$variant_meta:meta])* $variant:ident = $value:expr),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)+
            /// Preserves an SCCP numeric value not recognized by this crate.
            /// Allows newer or vendor-specific values to round-trip without loss.
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
        /// No concrete device model was supplied; used as the zero/default device type.
        Undefined = 0,
        /// Legacy Cisco 30-button SP+ hardware station.
        Phone30SpPlus = 1,
        /// Legacy Cisco 12-button SP+ hardware station.
        Phone12SpPlus = 2,
        /// Legacy Cisco 12-button SP hardware station.
        Phone12Sp = 3,
        /// Legacy Cisco 12-button station without the SP feature set.
        Phone12 = 4,
        /// Legacy Cisco 30-button VIP hardware station.
        Phone30Vip = 5,
        /// Cisco Unified IP Phone 7910, an early basic SCCP desk phone.
        Cisco7910 = 6,
        /// Cisco Unified IP Phone 7960, a six-line SCCP desk phone.
        Cisco7960 = 7,
        /// Cisco Unified IP Phone 7940, a two-line SCCP desk phone.
        Cisco7940 = 8,
        /// Cisco Unified IP Conference Station 7935.
        Cisco7935 = 9,
        /// A Cisco Voice Gateway Controller phone endpoint representing an analog gateway port.
        Vgc = 10,
        /// Cisco ATA 186 analog telephone adapter.
        Ata186 = 12,
        /// Cisco ATA 188 two-port analog telephone adapter with an Ethernet pass-through port.
        Ata188 = 13,
        /// The virtual counterpart of the legacy 30 SP+ station, used without matching physical hardware.
        Virtual30SpPlus = 20,
        /// A software or application-controlled phone endpoint represented as a station device.
        PhoneApplication = 21,
        /// A generic analog-access gateway resource.
        AnalogAccess = 30,
        /// A first-generation digital-access PRI resource, historically named DigitalAccessTitan1.
        DigitalAccessPri = 40,
        /// A digital-access resource for a channelized T1 interface.
        DigitalAccessT1 = 41,
        /// A second-generation digital-access gateway resource carrying Cisco's Titan2 codename.
        DigitalAccessTitan2 = 42,
        /// Historical Cisco tables identify this device type as DigitalAccessLennon or WS-X6608 digital access.
        /// The Rust variant name appears to be swapped with `DigitalAccessLennon`.
        AnalogAccessElvis = 43,
        /// Historical Cisco tables identify this device type as AnalogAccessElvis or WS-X6624 analog access.
        /// The Rust variant name appears to be swapped with `AnalogAccessElvis`.
        DigitalAccessLennon = 47,
        /// A generic conference-bridge media resource rather than a handset.
        ConferenceBridge = 50,
        /// A conference-bridge implementation generation carrying Cisco's Yoko codename.
        ConferenceBridgeYoko = 51,
        /// A conference-bridge implementation generation carrying Cisco's Dixieland codename.
        ConferenceBridgeDixieland = 52,
        /// A conference-bridge implementation generation carrying Cisco's Summit codename.
        ConferenceBridgeSummit = 53,
        /// An H.225 call-signaling endpoint used by the H.323 stack.
        H225 = 60,
        /// An H.323 telephone endpoint represented in the device table.
        H323Phone = 61,
        /// An H.323 gateway or trunk endpoint rather than an SCCP station.
        H323Trunk = 62,
        /// A music-on-hold media resource.
        MusicOnHold = 70,
        /// A logical call-routing pilot rather than a physical endpoint.
        Pilot = 71,
        /// A CTI/TAPI-controlled port used by telephony applications.
        TapiPort = 72,
        /// A CTI/TAPI route point that applications use to receive and redirect calls.
        TapiRoutePoint = 73,
        /// A voicemail or voice-inbox port represented as a callable device.
        VoiceInbox = 80,
        /// The administrative endpoint associated with a voice-inbox service.
        VoiceInboxAdmin = 81,
        /// A media resource that injects announcements or call-progress prompts on a line.
        LineAnnunciator = 82,
        /// A software media-termination-point implementation carrying Cisco's Dixieland codename.
        SoftwareMtpDixieland = 83,
        /// A Cisco media-server resource providing media processing rather than station service.
        CiscoMediaServer = 84,
        /// A conference-bridge implementation generation carrying Cisco's Flint codename.
        ConferenceBridgeFlint = 85,
        /// A logical route list used to select trunks and gateways.
        RouteList = 90,
        /// Cisco's synthetic station type for registration and call-load testing.
        LoadSimulator = 100,
        /// A generic media termination point used to relay or adapt media signaling.
        MediaTerminationPoint = 110,
        /// A hardware media termination point carrying Cisco's Yoko generation name.
        MediaTerminationPointYoko = 111,
        /// A media termination point carrying Cisco's Dixieland generation name.
        MediaTerminationPointDixieland = 112,
        /// A media termination point carrying Cisco's Summit generation name.
        MediaTerminationPointSummit = 113,
        /// Cisco Unified IP Phone 7941G, a two-line programmable SCCP desk phone.
        Cisco7941 = 115,
        /// Cisco Unified IP Phone 7971G-GE, a color touch-screen SCCP desk phone with Gigabit Ethernet.
        Cisco7971 = 119,
        /// An analog station port controlled through MGCP.
        MgcpStation = 120,
        /// A trunk endpoint controlled through MGCP.
        MgcpTrunk = 121,
        /// An H.323 Registration, Admission, and Status proxy resource.
        RasProxy = 122,
        /// Cisco 7914 fourteen-button line expansion module attached to a compatible desk phone.
        CiscoAddon7914 = 124,
        /// A generic call-routing trunk whose more specific signaling family is not encoded here.
        Trunk = 125,
        /// An annunciator media resource that plays tones and recorded prompts.
        Annunciator = 126,
        /// A media bridge used to fork call audio for monitoring.
        MonitorBridge = 127,
        /// A recording media resource represented as a call-control device.
        Recorder = 128,
        /// A monitoring bridge implementation carrying Cisco's Yoko generation name.
        MonitorBridgeYoko = 129,
        /// A SIP signaling trunk represented in the common device-type namespace.
        SipTrunk = 131,
        /// Cisco 7915 expansion module operating in its 12-button layout.
        CiscoAddon7915_12 = 227,
        /// Cisco 7915 expansion module operating in its 24-button layout.
        CiscoAddon7915_24 = 228,
        /// Cisco 7916 expansion module operating in its 12-button layout.
        CiscoAddon7916_12 = 229,
        /// Cisco 7916 expansion module operating in its 24-button layout.
        CiscoAddon7916_24 = 230,
        /// Nokia E-series mobile phone running a Cisco-compatible SCCP client.
        NokiaESeries = 275,
        /// Cisco Unified IP Phone 7985G desktop video phone.
        Cisco7985 = 302,
        /// Cisco Unified IP Phone 7911G, a basic single-line SCCP desk phone.
        Cisco7911 = 307,
        /// Cisco Unified IP Phone 7961G-GE, the Gigabit Ethernet six-line model.
        Cisco7961Ge = 308,
        /// Cisco Unified IP Phone 7941G-GE, the Gigabit Ethernet two-line model.
        Cisco7941Ge = 309,
        /// Cisco Unified IP Phone 7931G, a desk phone with a large set of programmable line and feature keys.
        Cisco7931 = 348,
        /// Cisco Unified Wireless IP Phone 7921G.
        Cisco7921 = 365,
        /// Cisco Unified IP Phone 7906G, a basic single-line SCCP desk phone.
        Cisco7906 = 369,
        /// Nokia Internet Call Client acting as an SCCP software endpoint.
        NokiaIcc = 376,
        /// Cisco Unified IP Phone 7962G, a six-line monochrome SCCP desk phone.
        Cisco7962 = 404,
        /// Cisco Unified IP Conference Station 7937G.
        Cisco7937 = 431,
        /// Cisco Unified IP Phone 7942G, a two-line monochrome SCCP desk phone.
        Cisco7942 = 434,
        /// Cisco Unified IP Phone 7945G, a two-line color SCCP desk phone with Gigabit Ethernet.
        Cisco7945 = 435,
        /// Cisco Unified IP Phone 7965G, a six-line color SCCP desk phone with Gigabit Ethernet.
        Cisco7965 = 436,
        /// Cisco Unified IP Phone 7975G, an eight-line color touch-screen SCCP desk phone.
        Cisco7975 = 437,
        /// Cisco Unified Wireless IP Phone 7925G.
        Cisco7925 = 484,
        /// Cisco Unified IP Phone 6921, a two-line entry-level desk phone.
        Cisco6921 = 495,
        /// Cisco Unified IP Phone 6941, a four-line desk phone.
        Cisco6941 = 496,
        /// Cisco Unified IP Phone 6961, a twelve-line desk phone.
        Cisco6961 = 497,
        /// Cisco Unified SIP Phone 6901, a displayless single-line endpoint represented in the common device table.
        Cisco6901 = 547,
        /// Cisco Unified IP Phone 6911, a basic single-line endpoint.
        Cisco6911 = 548,
        /// Cisco Unified IP Phone 6945, a four-line desk phone with Gigabit Ethernet.
        Cisco6945 = 564,
        /// Cisco Unified Wireless IP Phone 7926G with an integrated barcode scanner.
        Cisco7926 = 577,
        /// Cisco Unified IP Phone 8945, a color video desk phone with Gigabit Ethernet.
        Cisco8945 = 585,
        /// Cisco Unified IP Phone 8941, a color video desk phone.
        Cisco8941 = 586,
        /// Cisco IP Communicator, the Windows software-phone implementation of a Cisco desk phone.
        CiscoIpCommunicator = 30016,
        /// Cisco Unified IP Phone 7905G, a basic single-line SCCP desk phone.
        Cisco7905 = 20000,
        /// Cisco Wireless IP Phone 7920, the first-generation Cisco SCCP Wi-Fi handset.
        Cisco7920 = 30002,
        /// Cisco Unified IP Phone 7970G, an eight-line color touch-screen SCCP desk phone.
        Cisco7970 = 30006,
        /// Cisco Unified IP Phone 7912G, a basic single-line SCCP desk phone with an Ethernet switch.
        Cisco7912 = 30007,
        /// Cisco Unified IP Phone 7902G, a displayless single-line SCCP desk phone.
        Cisco7902 = 30008,
        /// Cisco Unified IP Phone 7961G, a six-line programmable SCCP desk phone.
        Cisco7961 = 30018,
        /// Cisco Unified IP Conference Station 7936.
        Cisco7936 = 30019,
        /// A virtual SCCP phone endpoint representing an analog gateway port.
        AnalogGateway = 30027,
        /// A virtual SCCP phone endpoint representing an ISDN BRI gateway port.
        BriGateway = 30028,
        /// Cisco SPA521S small-business IP desk phone.
        Spa521s = 80000,
        /// Cisco SPA524SG small-business IP desk phone with Gigabit Ethernet.
        Spa524sg = 80001,
        /// Cisco SPA502G one-line small-business IP desk phone.
        Spa502g = 80003,
        /// Cisco SPA504G four-line small-business IP desk phone.
        Spa504g = 80004,
        /// Cisco SPA525G five-line small-business color IP desk phone.
        Spa525g = 80005,
        /// Cisco SPA508G eight-line small-business IP desk phone.
        Spa508g = 80006,
        /// Cisco SPA509G twelve-line small-business IP desk phone.
        Spa509g = 80007,
        /// Second-generation Cisco SPA525G2 five-line color IP desk phone.
        Spa525g2 = 80009,
        /// Cisco SPA303G three-line small-business IP desk phone.
        Spa303g = 80011,
        /// Cisco SPA512G one-line small-business IP desk phone with Gigabit Ethernet.
        Spa512g = 80012,
        /// Cisco SPA514G four-line small-business IP desk phone with Gigabit Ethernet.
        Spa514g = 80013,
        /// Cisco SPA500S 32-button sidecar expansion module.
        AddonSpa500s = 99991,
        /// Cisco SPA500DS digital sidecar expansion module.
        AddonSpa500ds = 99992,
        /// Cisco SPA932DS attendant-console expansion module.
        AddonSpa932ds = 99993,
        /// Cisco's explicit “not defined” sentinel used when no registered device type applies.
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
        /// Indicates that no media encoding is selected or advertised.
        None = 0x0000,
        /// Marks an implementation-specific media format.
        /// The identifier alone does not define an interoperable encoding.
        NonStandard = 0x0001,
        /// G.711 A-law PCM at 64 kbit/s, commonly used by E-carrier telephony systems.
        Pcma = 0x0002,
        /// G.711 A-law PCM carried through a 56 kbit/s bearer rather than a full 64 kbit/s channel.
        G711Alaw56k = 0x0003,
        /// G.711 µ-law PCM at 64 kbit/s, commonly used by North American and Japanese telephony systems.
        Pcmu = 0x0004,
        /// G.711 µ-law PCM carried through a 56 kbit/s bearer rather than a full 64 kbit/s channel.
        G711Ulaw56k = 0x0005,
        /// G.722 sub-band wideband speech using its 64 kbit/s mode.
        G72264k = 0x0006,
        /// G.722 sub-band wideband speech using its 56 kbit/s mode.
        G72256k = 0x0007,
        /// G.722 sub-band wideband speech using its 48 kbit/s mode.
        G72248k = 0x0008,
        /// G.723.1 low-bit-rate narrowband speech, normally encoded at 5.3 or 6.3 kbit/s.
        G7231 = 0x0009,
        /// G.728 low-delay CELP narrowband speech encoded at 16 kbit/s.
        G728 = 0x000a,
        /// Base G.729 CS-ACELP narrowband speech encoded at 8 kbit/s.
        G729 = 0x000b,
        /// The reduced-complexity Annex A profile of G.729 at 8 kbit/s.
        G729A = 0x000c,
        /// ISO/IEC 11172 MPEG-1 audio, retained as a legacy SCCP audio capability.
        Is11172 = 0x000d,
        /// ISO/IEC 13818 MPEG-2 audio, retained as a legacy SCCP audio capability.
        Is13818 = 0x000e,
        /// G.729 with Annex B voice-activity detection and comfort-noise generation.
        G729B = 0x000f,
        /// The reduced-complexity G.729 Annex A profile combined with Annex B silence suppression.
        G729Ab = 0x0010,
        /// GSM Full Rate cellular speech coding.
        /// This is distinct from the RTP GSM 06.10 capability represented by `Gsm`.
        GsmFullRate = 0x0012,
        /// GSM Half Rate cellular speech coding, trading speech quality for reduced radio bandwidth.
        GsmHalfRate = 0x0013,
        /// GSM Enhanced Full Rate cellular speech coding with improved quality over GSM Full Rate.
        GsmEnhancedFullRate = 0x0014,
        /// Uncompressed 16-bit linear wideband PCM at 16 kHz, producing a 256 kbit/s audio stream.
        Wideband256k = 0x0019,
        /// A transparent 64 kbit/s data bearer rather than an audio or video encoding.
        Data64k = 0x0020,
        /// A transparent 56 kbit/s data bearer for rate-limited digital trunks.
        Data56k = 0x0021,
        /// G.722.1 wideband transform audio at 32 kbit/s, commonly known as the Siren 7 family.
        G7221_32k = 0x0028,
        /// G.722.1 wideband transform audio at 24 kbit/s, commonly known as the Siren 7 family.
        G7221_24k = 0x0029,
        /// Generic Advanced Audio Coding capability without a fixed SCCP LATM bit-rate selector.
        Aac = 0x002a,
        /// AAC audio transported with MPEG-4 LATM at a nominal 128 kbit/s.
        /// Cisco capability tables associate this family with low-delay AAC.
        Mp4aLatm128 = 0x002b,
        /// AAC audio transported with MPEG-4 LATM at a nominal 64 kbit/s.
        Mp4aLatm64 = 0x002c,
        /// AAC audio transported with MPEG-4 LATM at a nominal 56 kbit/s.
        Mp4aLatm56 = 0x002d,
        /// AAC audio transported with MPEG-4 LATM at a nominal 48 kbit/s.
        Mp4aLatm48 = 0x002e,
        /// AAC audio transported with MPEG-4 LATM at a nominal 32 kbit/s.
        Mp4aLatm32 = 0x002f,
        /// AAC audio transported with MPEG-4 LATM at a nominal 24 kbit/s.
        Mp4aLatm24 = 0x0030,
        /// AAC audio transported with MPEG-4 LATM when the bit rate is negotiated elsewhere or unspecified.
        Mp4aLatm = 0x0031,
        /// The RTP GSM 06.10 full-rate narrowband speech format.
        /// This is the interoperable RTP profile rather than a cellular bearer-mode selector.
        Gsm = 0x0050,
        /// A Cisco-defined narrowband `ActiveVoice` audio capability.
        /// The audited SCCP sources name it but do not establish a public encoding specification.
        ActiveVoice = 0x0051,
        /// G.726 ADPCM narrowband speech encoded at 32 kbit/s.
        G726_32k = 0x0052,
        /// G.726 ADPCM narrowband speech encoded at 24 kbit/s.
        G726_24k = 0x0053,
        /// G.726 ADPCM narrowband speech encoded at 16 kbit/s.
        G726_16k = 0x0054,
        /// A later SCCP identifier for G.729 Annex B silence suppression.
        /// It remains wire-distinct from the legacy `G729B` identifier.
        G729AnnexB = 0x0055,
        /// iLBC packet-loss-resilient narrowband speech, normally using 20 ms or 30 ms frames.
        Ilbc = 0x0056,
        /// iSAC adaptive wideband speech coding designed for variable IP-network conditions.
        Isac = 0x0059,
        /// Opus interactive audio, supporting speech and full-band audio with adaptive bit rate.
        Opus = 0x005a,
        /// Adaptive Multi-Rate narrowband cellular speech coding.
        Amr = 0x0061,
        /// Adaptive Multi-Rate Wideband speech coding, also standardized as G.722.2.
        AmrWb = 0x0062,
        /// H.261 video for audiovisual services over constant-rate digital channels.
        H261 = 0x0064,
        /// Base H.263 low-bit-rate video coding.
        H263 = 0x0065,
        /// H.263+ video, incorporating the optional enhancements standardized in H.263 version 2.
        H263Plus = 0x0066,
        /// Base H.264/AVC video capability.
        H264 = 0x0067,
        /// The scalable-video-coding extension of H.264/AVC, allowing layered temporal, spatial, or quality streams.
        H264Svc = 0x0068,
        /// T.120 real-time data-conferencing traffic such as shared whiteboards and application data.
        /// It is a data capability, not a speech codec.
        T120 = 0x0069,
        /// H.224 low-rate data-link traffic used by applications such as H.281 far-end camera control.
        H224 = 0x006a,
        /// T.38 real-time facsimile relay, transporting decoded fax data instead of modem audio.
        T38Fax = 0x006b,
        /// A Cisco-defined `TOTE` payload capability with no public encoding semantics established here.
        /// Available SCCP sources disagree on whether to classify it as video or data.
        Tote = 0x006c,
        /// H.265/HEVC video, the successor to H.264/AVC with improved compression efficiency.
        H265 = 0x006d,
        /// Cisco's distinct `H264_UC` video capability.
        /// The audited sources do not define what the UC profile adds, so it is not treated as base H.264.
        H264Uc = 0x006e,
        /// Cisco X-V.150 modem relay associated with a G.711 µ-law voiceband-data path.
        /// Legacy chan-sccp sources identify it with modem traffic on VG224 gateways.
        Xv150ModemRelay711u = 0x006f,
        /// Cisco named-signaling-event mode for voiceband data carried over G.711 µ-law.
        NseVbd711u = 0x0070,
        /// Cisco X-V.150 modem relay associated with a G.729 Annex A voice path.
        /// Legacy chan-sccp sources identify it with modem traffic on VG224 gateways.
        Xv150ModemRelay729a = 0x0071,
        /// Cisco named-signaling-event mode for voiceband data associated with G.729 Annex A.
        NseVbd729a = 0x0072,
        /// Cisco's H.264 capability variant carrying forward-error-correction support.
        /// It is negotiated separately because the backend has no equivalent base-H.264 flag.
        H264Fec = 0x0073,
        /// A transparent clear-channel bearer that preserves arbitrary digital data without transcoding.
        ClearChannel = 0x0078,
        /// A Cisco media-resource capability representing a universal transcoder.
        /// It identifies a transformation service rather than a media encoding.
        UniversalTranscoder = 0x00de,
        /// DTMF carried as RTP `telephone-event` packets using a dynamically negotiated payload type.
        /// The historical name references RFC 2833; RFC 4733 later replaced it.
        DtmfOutOfBandRfc2833 = 0x0101,
        /// Cisco's proprietary RTP DTMF passthrough payload rather than audible in-band tones.
        DtmfPassthrough = 0x0102,
        /// A Cisco DTMF event capability whose RTP payload number is negotiated dynamically.
        DtmfDynamic = 0x0103,
        /// Cisco out-of-band DTMF signaling, carrying digits separately from the voice samples.
        DtmfOutOfBand = 0x0104,
        /// Cisco's historical “in-band RFC 2833” event-payload mode.
        /// Despite the label, it represents packetized digit events rather than acoustic DTMF audio.
        DtmfInBandRfc2833 = 0x0105,
        /// Conference-bridge tone events used by Cisco media resources.
        /// This is a control/event payload, not an audio codec.
        CfbTones = 0x0106,
        /// DTMF event signaling without a companion audio stream.
        DtmfNoAudio = 0x012b,
        /// The V.150.1 modem-relay media mode, carrying demodulated modem data across an IP network.
        V150ModemRelay = 0x012c,
        /// The V.150.1 Simple Packet Relay Transport used for reliable modem-relay data.
        V150Sprt = 0x012d,
        /// The V.150.1 State Signalling Events channel used to coordinate transitions among audio, VBD, and relay modes.
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
        /// Reports an unassigned or inactive station control; no feature action is implied.
        Unused = 0x00,
        /// Reports use of the redial control to call the most recently dialed destination.
        LastNumberRedial = 0x01,
        /// Reports activation of a provisioned speed-dial entry.
        SpeedDial = 0x02,
        /// Reports use of the hold control for the addressed call.
        Hold = 0x03,
        /// Reports use of the transfer control to start or complete a call transfer.
        Transfer = 0x04,
        /// Reports use of the call-forward-all control.
        ForwardAll = 0x05,
        /// Reports use of the call-forward-on-busy control.
        ForwardBusy = 0x06,
        /// Reports use of the call-forward-on-no-answer control.
        ForwardNoAnswer = 0x07,
        /// Reports activation of a legacy display-oriented station control.
        Display = 0x08,
        /// Reports selection of a line appearance or a call on that appearance.
        Line = 0x09,
        /// Reports activation of the T.120 text-chat application.
        T120Chat = 0x0a,
        /// Reports activation of the T.120 shared-whiteboard application.
        T120Whiteboard = 0x0b,
        /// Reports activation of T.120 application sharing.
        T120ApplicationSharing = 0x0c,
        /// Reports activation of T.120 conference file transfer.
        T120FileTransfer = 0x0d,
        /// Reports use of the station's video control.
        Video = 0x0e,
        /// Reports activation of a voicemail access key.
        Voicemail = 0x0f,
        /// Reports use of a combined answer/release control.
        AnswerRelease = 0x10,
        /// Reports use of the station's automatic-answer control.
        AutoAnswer = 0x11,
        /// Reports use of the call-selection control for multi-call operations.
        Select = 0x12,
        /// Reports use of the call-privacy control.
        Privacy = 0x13,
        /// Reports activation of a provisioned phone-service URL.
        ServiceUrl = 0x14,
        /// Reports activation of a speed dial that also monitors its target through BLF.
        BlfSpeedDial = 0x15,
        /// Reports a directed-park request targeting a specific park destination.
        DirectedPark = 0x16,
        /// Reports activation of an intercom appearance or intercom call.
        Intercom = 0x17,
        /// Reports use of the malicious-call identification feature.
        MaliciousCall = 0x1b,
        /// Reports application-defined programmable control B1; SCCP assigns no universal action.
        GenericAppB1 = 0x21,
        /// Reports application-defined programmable control B2; SCCP assigns no universal action.
        GenericAppB2 = 0x22,
        /// Reports application-defined programmable control B3; SCCP assigns no universal action.
        GenericAppB3 = 0x23,
        /// Reports application-defined programmable control B4; SCCP assigns no universal action.
        GenericAppB4 = 0x24,
        /// Reports application-defined programmable control B5; SCCP assigns no universal action.
        GenericAppB5 = 0x25,
        /// Reports activation of a generic feature whose lamp can expose multiple blink states.
        MultiblinkFeature = 0x26,
        /// Reports activation of the dial-in Meet-Me conference workflow.
        MeetMeConference = 0x7b,
        /// Reports use of the station conference control for an active call.
        Conference = 0x7d,
        /// Reports a request to park the current call.
        CallPark = 0x7e,
        /// Reports a request to answer a ringing call in the configured pickup scope.
        CallPickup = 0x7f,
        /// Reports a group-pickup request for a ringing call in another pickup group.
        GroupCallPickup = 0x80,
        /// Reports activation of the extension-mobility login or logout feature.
        Mobility = 0x81,
        /// Reports use of the do-not-disturb control.
        DoNotDisturb = 0x82,
        /// Reports a request to show or operate on the conference participant list.
        ConferenceList = 0x83,
        /// Reports a request to remove the most recently added conference participant.
        RemoveLastParticipant = 0x84,
        /// Reports activation of Cisco's call-quality reporting tool.
        QualityReportTool = 0x85,
        /// Reports a callback request for a busy or unavailable destination.
        Callback = 0x86,
        /// Reports use of Cisco's alternate call-pickup feature.
        OtherPickup = 0x87,
        /// Reports a request to change the call's video mode.
        VideoMode = 0x88,
        /// Reports use of the new-call control to allocate an outbound call.
        NewCall = 0x89,
        /// Reports use of the end-call control to release the addressed call.
        EndCall = 0x8a,
        /// Reports a hunt-group login or logout request.
        HuntGroupLogin = 0x8b,
        /// Reports activation of Cisco's call-queuing feature.
        Queuing = 0x8f,
        /// Reports activation of a parking-lot view or retrieval workflow.
        ParkingLot = 0xc0,
        /// Reports use of the station's fixed Messages key.
        Messages = 0xc2,
        /// Reports use of the station's fixed Directories key.
        Directory = 0xc3,
        /// Reports use of the station's fixed Applications or Services key.
        Application = 0xc5,
        /// Reports a headset-control state change from the station.
        Headset = 0xc6,
        /// Identifies input originating from the station's physical keypad control block.
        Keypad = 0xf0,
        /// Reports use of the station acoustic-echo-cancellation control.
        AcousticEchoCancellation = 0xfd,
        /// Preserves a station stimulus that Cisco marks as undefined rather than unassigned.
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
        /// Stops any current station tone; the server uses this as the explicit silent tone request.
        Silence = 0x00,
        /// The DTMF `1` signal: 697 Hz row tone combined with a 1,209 Hz column tone.
        Dtmf1 = 0x01,
        /// The DTMF `2` signal: 697 Hz row tone combined with a 1,336 Hz column tone.
        Dtmf2 = 0x02,
        /// The DTMF `3` signal: 697 Hz row tone combined with a 1,477 Hz column tone.
        Dtmf3 = 0x03,
        /// The DTMF `4` signal: 770 Hz row tone combined with a 1,209 Hz column tone.
        Dtmf4 = 0x04,
        /// The DTMF `5` signal: 770 Hz row tone combined with a 1,336 Hz column tone.
        Dtmf5 = 0x05,
        /// The DTMF `6` signal: 770 Hz row tone combined with a 1,477 Hz column tone.
        Dtmf6 = 0x06,
        /// The DTMF `7` signal: 852 Hz row tone combined with a 1,209 Hz column tone.
        Dtmf7 = 0x07,
        /// The DTMF `8` signal: 852 Hz row tone combined with a 1,336 Hz column tone.
        Dtmf8 = 0x08,
        /// The DTMF `9` signal: 852 Hz row tone combined with a 1,477 Hz column tone.
        Dtmf9 = 0x09,
        /// The DTMF `0` signal: 941 Hz row tone combined with a 1,336 Hz column tone.
        Dtmf0 = 0x0a,
        /// The DTMF `*` signal: 941 Hz row tone combined with a 1,209 Hz column tone.
        DtmfStar = 0x0e,
        /// The DTMF `#` signal: 941 Hz row tone combined with a 1,477 Hz column tone.
        DtmfPound = 0x0f,
        /// The DTMF `A` signal from the fourth keypad column used by AUTOVON and control systems.
        /// Combines a 697 Hz row tone with a 1,633 Hz column tone.
        DtmfA = 0x10,
        /// The DTMF `B` signal from the fourth keypad column used by AUTOVON and control systems.
        /// Combines a 770 Hz row tone with a 1,633 Hz column tone.
        DtmfB = 0x11,
        /// The DTMF `C` signal from the fourth keypad column used by AUTOVON and control systems.
        /// Combines an 852 Hz row tone with a 1,633 Hz column tone.
        DtmfC = 0x12,
        /// The DTMF `D` signal from the fourth keypad column used by AUTOVON and control systems.
        /// Combines a 941 Hz row tone with a 1,633 Hz column tone.
        DtmfD = 0x13,
        /// Dial tone indicating that the caller may dial an internal extension.
        InsideDial = 0x21,
        /// Dial tone indicating access to an external or public telephone network.
        OutsideDial = 0x22,
        /// Busy tone indicating that the called line cannot accept the call.
        LineBusy = 0x23,
        /// Audible ringback indicating that the remote endpoint is being alerted.
        Alerting = 0x24,
        /// Fast-busy or reorder tone indicating congestion or an unusable dialing sequence.
        Reorder = 0x25,
        /// Periodic warning beep informing participants that the call is being recorded.
        RecorderWarning = 0x26,
        /// Station feedback indicating that a recording device or recording service was detected.
        RecorderDetected = 0x27,
        /// Recall tone used when a held, parked, or transferred call reverts to the station.
        Reverting = 0x28,
        /// Loud off-hook warning tone played when the handset remains off hook without a call.
        ReceiverOffHook = 0x29,
        /// Intercept tone indicating that the supplied address or digit sequence is incomplete.
        PartialDial = 0x2a,
        /// Intercept tone indicating that the dialed number does not exist.
        NoSuchNumber = 0x2b,
        /// Special tone used while an operator performs busy-line verification.
        BusyVerification = 0x2c,
        /// Brief in-call alert indicating that another call is waiting.
        CallWaiting = 0x2d,
        /// Positive confirmation tone indicating that a requested feature was accepted.
        Confirmation = 0x2e,
        /// Tone indicating that the call is camped on a busy destination awaiting availability.
        CampOn = 0x2f,
        /// Dial tone presented after recall or hook flash so the caller can enter another destination.
        RecallDial = 0x30,
        /// Cisco's two-part “zip-zip” feature alert.
        /// The exact user-facing meaning depends on the call feature that requests it.
        ZipZip = 0x31,
        /// Cisco's short “zip” feature alert.
        /// The exact user-facing meaning depends on the call feature that requests it.
        Zip = 0x32,
        /// Cisco's paired positive/negative feature-feedback sound.
        BeepBonk = 0x33,
        /// Requests the station's built-in music tone rather than an RTP music-on-hold stream.
        Music = 0x34,
        /// Audible indication associated with placing or leaving a call on hold.
        Hold = 0x35,
        /// A station test tone used for diagnostics rather than normal call progress.
        Test = 0x36,
        /// Warning tone indicating that the call is being monitored.
        MonitorWarning = 0x37,
        /// Call-waiting alert used when another waiting call is added.
        AddCallWaiting = 0x40,
        /// Higher-priority call-waiting alert for precedence-aware call handling.
        PriorityCallWaiting = 0x41,
        /// Warning tone indicating that another party has barged into the call.
        BargeIn = 0x43,
        /// Distinctive alerting cadence used to distinguish a call class from normal ringing.
        DistinctAlert = 0x44,
        /// Priority alerting cadence used for a higher-precedence incoming call.
        PriorityAlert = 0x45,
        /// Short reminder ring for a held, parked, forwarded, or otherwise pending call.
        ReminderRing = 0x46,
        /// Ringback used by Multilevel Precedence and Preemption calls.
        PrecedenceRingback = 0x47,
        /// Warning tone indicating that a lower-precedence call is being preempted.
        Preemption = 0x48,
        /// Sentinel indicating that no call-progress tone is assigned.
        /// Unlike `Silence`, it does not request an active silence tone.
        NoTone = 0x7f,
        /// Conference-service greeting played when entering a Meet-Me flow.
        MeetMeGreeting = 0x80,
        /// Conference prompt indicating that the entered Meet-Me number is invalid.
        MeetMeNumberInvalid = 0x81,
        /// Conference prompt indicating that the entered Meet-Me number could not be used.
        MeetMeNumberFailed = 0x82,
        /// Conference prompt requesting the participant PIN.
        MeetMeEnterPin = 0x83,
        /// Conference prompt indicating that the participant PIN is invalid.
        MeetMeInvalidPin = 0x84,
        /// Conference prompt indicating that PIN validation failed.
        MeetMeFailedPin = 0x85,
        /// Conference prompt indicating that allocation of the conference bridge failed.
        MeetMeCfbFailed = 0x86,
        /// Conference prompt requesting an access code.
        MeetMeEnterAccessCode = 0x87,
        /// Conference prompt indicating that the supplied access code is invalid.
        MeetMeAccessCodeInvalid = 0x88,
        /// Conference prompt indicating that access-code validation failed.
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
        /// An unassigned physical slot that should not present an actionable station key.
        Unused = 0x00,
        /// A key that redials the most recently dialed destination.
        LastNumberRedial = 0x01,
        /// A programmable key bound to a configured speed-dial destination.
        SpeedDial = 0x02,
        /// The station hold key used to hold or resume the current call.
        Hold = 0x03,
        /// The station transfer key used to start or complete a call transfer.
        Transfer = 0x04,
        /// A feature key for configuring or toggling call-forward-all.
        ForwardAll = 0x05,
        /// A feature key for configuring or toggling call-forward-on-busy.
        ForwardBusy = 0x06,
        /// A feature key for configuring or toggling call-forward-on-no-answer.
        ForwardNoAnswer = 0x07,
        /// A legacy display-oriented station key whose behavior is phone-model specific.
        Display = 0x08,
        /// A line-appearance key representing a directory number and its calls.
        Line = 0x09,
        /// A key launching the T.120 text-chat application.
        T120Chat = 0x0a,
        /// A key launching the T.120 shared-whiteboard application.
        T120Whiteboard = 0x0b,
        /// A key launching T.120 application sharing.
        T120ApplicationSharing = 0x0c,
        /// A key launching T.120 conference file transfer.
        T120FileTransfer = 0x0d,
        /// A station key assigned to video control.
        Video = 0x0e,
        /// A programmable voicemail-access key, commonly paired with message-waiting indication.
        Voicemail = 0x0f,
        /// A combined key that answers an offered call or releases the current call.
        AnswerRelease = 0x10,
        /// A feature key controlling automatic call answer.
        AutoAnswer = 0x11,
        /// A key that selects calls for transfer, conference, or other multi-call operations.
        Select = 0x12,
        /// A generic programmable feature key whose concrete action is supplied by provisioning.
        Feature = 0x13,
        /// A programmable key opening a provisioned phone-service URL.
        ServiceUrl = 0x14,
        /// A speed-dial key whose lamp also displays the target's busy-lamp-field state.
        BlfSpeedDial = 0x15,
        /// A feature key that parks a call at a specified destination.
        DirectedPark = 0x16,
        /// A key representing an intercom appearance or intercom destination.
        Intercom = 0x17,
        /// A feature key for malicious-call identification.
        MaliciousCall = 0x1b,
        /// Application-defined programmable key B1; SCCP assigns no universal action.
        GenericAppB1 = 0x21,
        /// Application-defined programmable key B2; SCCP assigns no universal action.
        GenericAppB2 = 0x22,
        /// Application-defined programmable key B3; SCCP assigns no universal action.
        GenericAppB3 = 0x23,
        /// Application-defined programmable key B4; SCCP assigns no universal action.
        GenericAppB4 = 0x24,
        /// Application-defined programmable key B5; SCCP assigns no universal action.
        GenericAppB5 = 0x25,
        /// A generic feature key whose lamp can display multiple blink states.
        MultiblinkFeature = 0x26,
        /// A feature key entering the dial-in Meet-Me conference workflow.
        MeetMeConference = 0x7b,
        /// The station conference key used to build or manage an ad-hoc conference.
        Conference = 0x7d,
        /// A feature key that parks the current call.
        CallPark = 0x7e,
        /// A feature key that answers a ringing call in the configured pickup scope.
        CallPickup = 0x7f,
        /// A feature key for group pickup outside the station's immediate pickup group.
        GroupCallPickup = 0x80,
        /// A feature key for extension-mobility login, logout, or appearance control.
        Mobility = 0x81,
        /// A feature key that exposes and changes do-not-disturb state.
        DoNotDisturb = 0x82,
        /// A feature key opening the conference participant list.
        ConferenceList = 0x83,
        /// A conference key that removes the most recently added participant.
        RemoveLastParticipant = 0x84,
        /// A feature key launching Cisco's call-quality reporting tool.
        QualityReportTool = 0x85,
        /// A feature key requesting callback when a busy or unavailable destination becomes reachable.
        Callback = 0x86,
        /// A feature key for Cisco's alternate call-pickup workflow.
        OtherPickup = 0x87,
        /// A feature key that changes the call's video mode.
        VideoMode = 0x88,
        /// A key that creates a new outbound call appearance.
        NewCall = 0x89,
        /// A key that releases the selected call.
        EndCall = 0x8a,
        /// A feature key that logs the station into or out of a hunt group.
        HuntGroupLogin = 0x8b,
        /// A feature key for Cisco call-queuing behavior.
        Queuing = 0x8f,
        /// A feature key opening a parking-lot view or retrieval workflow.
        ParkingLot = 0xc0,
        /// The station's fixed Messages key rather than a programmable voicemail slot.
        Messages = 0xc2,
        /// The station's fixed Directories key.
        Directory = 0xc3,
        /// The station's fixed Applications or Services key.
        Application = 0xc5,
        /// The station's fixed headset-control key.
        Headset = 0xc6,
        /// A template entry representing the physical dialing keypad rather than one programmable key.
        Keypad = 0xf0,
        /// Cisco template placeholder for a multi-purpose programmable position.
        /// Provisioning is expected to replace it with a concrete button definition.
        PlaceholderMulti = 0xf1,
        /// Cisco template placeholder reserved for a line appearance.
        /// Provisioning is expected to replace it with a concrete line definition.
        PlaceholderLine = 0xf2,
        /// Cisco template placeholder reserved for a speed dial.
        /// Provisioning is expected to replace it with a concrete speed-dial definition.
        PlaceholderSpeedDial = 0xf3,
        /// Cisco template placeholder reserved for a monitored hint or BLF target.
        /// Provisioning is expected to replace it with a concrete definition.
        PlaceholderHint = 0xf4,
        /// Cisco template placeholder reserved for abbreviated dialing.
        /// Provisioning is expected to replace it with a concrete definition.
        PlaceholderAbbreviatedDial = 0xf5,
        /// A station control for acoustic echo-cancellation behavior.
        AcousticEchoCancellation = 0xfd,
        /// A button definition Cisco marks as undefined; it is distinct from an intentionally unused slot.
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
