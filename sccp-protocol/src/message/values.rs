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
        /// Identifies the `Undefined` station, gateway, or service device type.
        /// Uses SCCP device-type value `0` during registration and provisioning.
        Undefined = 0,
        /// Identifies the `Phone30SpPlus` station, gateway, or service device type.
        /// Uses SCCP device-type value `1` during registration and provisioning.
        Phone30SpPlus = 1,
        /// Identifies the `Phone12SpPlus` station, gateway, or service device type.
        /// Uses SCCP device-type value `2` during registration and provisioning.
        Phone12SpPlus = 2,
        /// Identifies the `Phone12Sp` station, gateway, or service device type.
        /// Uses SCCP device-type value `3` during registration and provisioning.
        Phone12Sp = 3,
        /// Identifies the `Phone12` station, gateway, or service device type.
        /// Uses SCCP device-type value `4` during registration and provisioning.
        Phone12 = 4,
        /// Identifies the `Phone30Vip` station, gateway, or service device type.
        /// Uses SCCP device-type value `5` during registration and provisioning.
        Phone30Vip = 5,
        /// Identifies the `Cisco7910` station, gateway, or service device type.
        /// Uses SCCP device-type value `6` during registration and provisioning.
        Cisco7910 = 6,
        /// Identifies the `Cisco7960` station, gateway, or service device type.
        /// Uses SCCP device-type value `7` during registration and provisioning.
        Cisco7960 = 7,
        /// Identifies the `Cisco7940` station, gateway, or service device type.
        /// Uses SCCP device-type value `8` during registration and provisioning.
        Cisco7940 = 8,
        /// Identifies the `Cisco7935` station, gateway, or service device type.
        /// Uses SCCP device-type value `9` during registration and provisioning.
        Cisco7935 = 9,
        /// Identifies the `Vgc` station, gateway, or service device type.
        /// Uses SCCP device-type value `10` during registration and provisioning.
        Vgc = 10,
        /// Identifies the `Ata186` station, gateway, or service device type.
        /// Uses SCCP device-type value `12` during registration and provisioning.
        Ata186 = 12,
        /// Identifies the `Ata188` station, gateway, or service device type.
        /// Uses SCCP device-type value `13` during registration and provisioning.
        Ata188 = 13,
        /// Identifies the `Virtual30SpPlus` station, gateway, or service device type.
        /// Uses SCCP device-type value `20` during registration and provisioning.
        Virtual30SpPlus = 20,
        /// Identifies the `PhoneApplication` station, gateway, or service device type.
        /// Uses SCCP device-type value `21` during registration and provisioning.
        PhoneApplication = 21,
        /// Identifies the `AnalogAccess` station, gateway, or service device type.
        /// Uses SCCP device-type value `30` during registration and provisioning.
        AnalogAccess = 30,
        /// Identifies the `DigitalAccessPri` station, gateway, or service device type.
        /// Uses SCCP device-type value `40` during registration and provisioning.
        DigitalAccessPri = 40,
        /// Identifies the `DigitalAccessT1` station, gateway, or service device type.
        /// Uses SCCP device-type value `41` during registration and provisioning.
        DigitalAccessT1 = 41,
        /// Identifies the `DigitalAccessTitan2` station, gateway, or service device type.
        /// Uses SCCP device-type value `42` during registration and provisioning.
        DigitalAccessTitan2 = 42,
        /// Identifies the `AnalogAccessElvis` station, gateway, or service device type.
        /// Uses SCCP device-type value `43` during registration and provisioning.
        AnalogAccessElvis = 43,
        /// Identifies the `DigitalAccessLennon` station, gateway, or service device type.
        /// Uses SCCP device-type value `47` during registration and provisioning.
        DigitalAccessLennon = 47,
        /// Identifies the `ConferenceBridge` station, gateway, or service device type.
        /// Uses SCCP device-type value `50` during registration and provisioning.
        ConferenceBridge = 50,
        /// Identifies the `ConferenceBridgeYoko` station, gateway, or service device type.
        /// Uses SCCP device-type value `51` during registration and provisioning.
        ConferenceBridgeYoko = 51,
        /// Identifies the `ConferenceBridgeDixieland` station, gateway, or service device type.
        /// Uses SCCP device-type value `52` during registration and provisioning.
        ConferenceBridgeDixieland = 52,
        /// Identifies the `ConferenceBridgeSummit` station, gateway, or service device type.
        /// Uses SCCP device-type value `53` during registration and provisioning.
        ConferenceBridgeSummit = 53,
        /// Identifies the `H225` station, gateway, or service device type.
        /// Uses SCCP device-type value `60` during registration and provisioning.
        H225 = 60,
        /// Identifies the `H323Phone` station, gateway, or service device type.
        /// Uses SCCP device-type value `61` during registration and provisioning.
        H323Phone = 61,
        /// Identifies the `H323Trunk` station, gateway, or service device type.
        /// Uses SCCP device-type value `62` during registration and provisioning.
        H323Trunk = 62,
        /// Identifies the `MusicOnHold` station, gateway, or service device type.
        /// Uses SCCP device-type value `70` during registration and provisioning.
        MusicOnHold = 70,
        /// Identifies the `Pilot` station, gateway, or service device type.
        /// Uses SCCP device-type value `71` during registration and provisioning.
        Pilot = 71,
        /// Identifies the `TapiPort` station, gateway, or service device type.
        /// Uses SCCP device-type value `72` during registration and provisioning.
        TapiPort = 72,
        /// Identifies the `TapiRoutePoint` station, gateway, or service device type.
        /// Uses SCCP device-type value `73` during registration and provisioning.
        TapiRoutePoint = 73,
        /// Identifies the `VoiceInbox` station, gateway, or service device type.
        /// Uses SCCP device-type value `80` during registration and provisioning.
        VoiceInbox = 80,
        /// Identifies the `VoiceInboxAdmin` station, gateway, or service device type.
        /// Uses SCCP device-type value `81` during registration and provisioning.
        VoiceInboxAdmin = 81,
        /// Identifies the `LineAnnunciator` station, gateway, or service device type.
        /// Uses SCCP device-type value `82` during registration and provisioning.
        LineAnnunciator = 82,
        /// Identifies the `SoftwareMtpDixieland` station, gateway, or service device type.
        /// Uses SCCP device-type value `83` during registration and provisioning.
        SoftwareMtpDixieland = 83,
        /// Identifies the `CiscoMediaServer` station, gateway, or service device type.
        /// Uses SCCP device-type value `84` during registration and provisioning.
        CiscoMediaServer = 84,
        /// Identifies the `ConferenceBridgeFlint` station, gateway, or service device type.
        /// Uses SCCP device-type value `85` during registration and provisioning.
        ConferenceBridgeFlint = 85,
        /// Identifies the `RouteList` station, gateway, or service device type.
        /// Uses SCCP device-type value `90` during registration and provisioning.
        RouteList = 90,
        /// Identifies the `LoadSimulator` station, gateway, or service device type.
        /// Uses SCCP device-type value `100` during registration and provisioning.
        LoadSimulator = 100,
        /// Identifies the `MediaTerminationPoint` station, gateway, or service device type.
        /// Uses SCCP device-type value `110` during registration and provisioning.
        MediaTerminationPoint = 110,
        /// Identifies the `MediaTerminationPointYoko` station, gateway, or service device type.
        /// Uses SCCP device-type value `111` during registration and provisioning.
        MediaTerminationPointYoko = 111,
        /// Identifies the `MediaTerminationPointDixieland` station, gateway, or service device type.
        /// Uses SCCP device-type value `112` during registration and provisioning.
        MediaTerminationPointDixieland = 112,
        /// Identifies the `MediaTerminationPointSummit` station, gateway, or service device type.
        /// Uses SCCP device-type value `113` during registration and provisioning.
        MediaTerminationPointSummit = 113,
        /// Identifies the `Cisco7941` station, gateway, or service device type.
        /// Uses SCCP device-type value `115` during registration and provisioning.
        Cisco7941 = 115,
        /// Identifies the `Cisco7971` station, gateway, or service device type.
        /// Uses SCCP device-type value `119` during registration and provisioning.
        Cisco7971 = 119,
        /// Identifies the `MgcpStation` station, gateway, or service device type.
        /// Uses SCCP device-type value `120` during registration and provisioning.
        MgcpStation = 120,
        /// Identifies the `MgcpTrunk` station, gateway, or service device type.
        /// Uses SCCP device-type value `121` during registration and provisioning.
        MgcpTrunk = 121,
        /// Identifies the `RasProxy` station, gateway, or service device type.
        /// Uses SCCP device-type value `122` during registration and provisioning.
        RasProxy = 122,
        /// Identifies the `CiscoAddon7914` station, gateway, or service device type.
        /// Uses SCCP device-type value `124` during registration and provisioning.
        CiscoAddon7914 = 124,
        /// Identifies the `Trunk` station, gateway, or service device type.
        /// Uses SCCP device-type value `125` during registration and provisioning.
        Trunk = 125,
        /// Identifies the `Annunciator` station, gateway, or service device type.
        /// Uses SCCP device-type value `126` during registration and provisioning.
        Annunciator = 126,
        /// Identifies the `MonitorBridge` station, gateway, or service device type.
        /// Uses SCCP device-type value `127` during registration and provisioning.
        MonitorBridge = 127,
        /// Identifies the `Recorder` station, gateway, or service device type.
        /// Uses SCCP device-type value `128` during registration and provisioning.
        Recorder = 128,
        /// Identifies the `MonitorBridgeYoko` station, gateway, or service device type.
        /// Uses SCCP device-type value `129` during registration and provisioning.
        MonitorBridgeYoko = 129,
        /// Identifies the `SipTrunk` station, gateway, or service device type.
        /// Uses SCCP device-type value `131` during registration and provisioning.
        SipTrunk = 131,
        /// Identifies the `CiscoAddon7915_12` station, gateway, or service device type.
        /// Uses SCCP device-type value `227` during registration and provisioning.
        CiscoAddon7915_12 = 227,
        /// Identifies the `CiscoAddon7915_24` station, gateway, or service device type.
        /// Uses SCCP device-type value `228` during registration and provisioning.
        CiscoAddon7915_24 = 228,
        /// Identifies the `CiscoAddon7916_12` station, gateway, or service device type.
        /// Uses SCCP device-type value `229` during registration and provisioning.
        CiscoAddon7916_12 = 229,
        /// Identifies the `CiscoAddon7916_24` station, gateway, or service device type.
        /// Uses SCCP device-type value `230` during registration and provisioning.
        CiscoAddon7916_24 = 230,
        /// Identifies the `NokiaESeries` station, gateway, or service device type.
        /// Uses SCCP device-type value `275` during registration and provisioning.
        NokiaESeries = 275,
        /// Identifies the `Cisco7985` station, gateway, or service device type.
        /// Uses SCCP device-type value `302` during registration and provisioning.
        Cisco7985 = 302,
        /// Identifies the `Cisco7911` station, gateway, or service device type.
        /// Uses SCCP device-type value `307` during registration and provisioning.
        Cisco7911 = 307,
        /// Identifies the `Cisco7961Ge` station, gateway, or service device type.
        /// Uses SCCP device-type value `308` during registration and provisioning.
        Cisco7961Ge = 308,
        /// Identifies the `Cisco7941Ge` station, gateway, or service device type.
        /// Uses SCCP device-type value `309` during registration and provisioning.
        Cisco7941Ge = 309,
        /// Identifies the `Cisco7931` station, gateway, or service device type.
        /// Uses SCCP device-type value `348` during registration and provisioning.
        Cisco7931 = 348,
        /// Identifies the `Cisco7921` station, gateway, or service device type.
        /// Uses SCCP device-type value `365` during registration and provisioning.
        Cisco7921 = 365,
        /// Identifies the `Cisco7906` station, gateway, or service device type.
        /// Uses SCCP device-type value `369` during registration and provisioning.
        Cisco7906 = 369,
        /// Identifies the `NokiaIcc` station, gateway, or service device type.
        /// Uses SCCP device-type value `376` during registration and provisioning.
        NokiaIcc = 376,
        /// Identifies the `Cisco7962` station, gateway, or service device type.
        /// Uses SCCP device-type value `404` during registration and provisioning.
        Cisco7962 = 404,
        /// Identifies the `Cisco7937` station, gateway, or service device type.
        /// Uses SCCP device-type value `431` during registration and provisioning.
        Cisco7937 = 431,
        /// Identifies the `Cisco7942` station, gateway, or service device type.
        /// Uses SCCP device-type value `434` during registration and provisioning.
        Cisco7942 = 434,
        /// Identifies the `Cisco7945` station, gateway, or service device type.
        /// Uses SCCP device-type value `435` during registration and provisioning.
        Cisco7945 = 435,
        /// Identifies the `Cisco7965` station, gateway, or service device type.
        /// Uses SCCP device-type value `436` during registration and provisioning.
        Cisco7965 = 436,
        /// Identifies the `Cisco7975` station, gateway, or service device type.
        /// Uses SCCP device-type value `437` during registration and provisioning.
        Cisco7975 = 437,
        /// Identifies the `Cisco7925` station, gateway, or service device type.
        /// Uses SCCP device-type value `484` during registration and provisioning.
        Cisco7925 = 484,
        /// Identifies the `Cisco6921` station, gateway, or service device type.
        /// Uses SCCP device-type value `495` during registration and provisioning.
        Cisco6921 = 495,
        /// Identifies the `Cisco6941` station, gateway, or service device type.
        /// Uses SCCP device-type value `496` during registration and provisioning.
        Cisco6941 = 496,
        /// Identifies the `Cisco6961` station, gateway, or service device type.
        /// Uses SCCP device-type value `497` during registration and provisioning.
        Cisco6961 = 497,
        /// Identifies the `Cisco6901` station, gateway, or service device type.
        /// Uses SCCP device-type value `547` during registration and provisioning.
        Cisco6901 = 547,
        /// Identifies the `Cisco6911` station, gateway, or service device type.
        /// Uses SCCP device-type value `548` during registration and provisioning.
        Cisco6911 = 548,
        /// Identifies the `Cisco6945` station, gateway, or service device type.
        /// Uses SCCP device-type value `564` during registration and provisioning.
        Cisco6945 = 564,
        /// Identifies the `Cisco7926` station, gateway, or service device type.
        /// Uses SCCP device-type value `577` during registration and provisioning.
        Cisco7926 = 577,
        /// Identifies the `Cisco8945` station, gateway, or service device type.
        /// Uses SCCP device-type value `585` during registration and provisioning.
        Cisco8945 = 585,
        /// Identifies the `Cisco8941` station, gateway, or service device type.
        /// Uses SCCP device-type value `586` during registration and provisioning.
        Cisco8941 = 586,
        /// Identifies the `CiscoIpCommunicator` station, gateway, or service device type.
        /// Uses SCCP device-type value `30016` during registration and provisioning.
        CiscoIpCommunicator = 30016,
        /// Identifies the `Cisco7905` station, gateway, or service device type.
        /// Uses SCCP device-type value `20000` during registration and provisioning.
        Cisco7905 = 20000,
        /// Identifies the `Cisco7920` station, gateway, or service device type.
        /// Uses SCCP device-type value `30002` during registration and provisioning.
        Cisco7920 = 30002,
        /// Identifies the `Cisco7970` station, gateway, or service device type.
        /// Uses SCCP device-type value `30006` during registration and provisioning.
        Cisco7970 = 30006,
        /// Identifies the `Cisco7912` station, gateway, or service device type.
        /// Uses SCCP device-type value `30007` during registration and provisioning.
        Cisco7912 = 30007,
        /// Identifies the `Cisco7902` station, gateway, or service device type.
        /// Uses SCCP device-type value `30008` during registration and provisioning.
        Cisco7902 = 30008,
        /// Identifies the `Cisco7961` station, gateway, or service device type.
        /// Uses SCCP device-type value `30018` during registration and provisioning.
        Cisco7961 = 30018,
        /// Identifies the `Cisco7936` station, gateway, or service device type.
        /// Uses SCCP device-type value `30019` during registration and provisioning.
        Cisco7936 = 30019,
        /// Identifies the `AnalogGateway` station, gateway, or service device type.
        /// Uses SCCP device-type value `30027` during registration and provisioning.
        AnalogGateway = 30027,
        /// Identifies the `BriGateway` station, gateway, or service device type.
        /// Uses SCCP device-type value `30028` during registration and provisioning.
        BriGateway = 30028,
        /// Identifies the `Spa521s` station, gateway, or service device type.
        /// Uses SCCP device-type value `80000` during registration and provisioning.
        Spa521s = 80000,
        /// Identifies the `Spa524sg` station, gateway, or service device type.
        /// Uses SCCP device-type value `80001` during registration and provisioning.
        Spa524sg = 80001,
        /// Identifies the `Spa502g` station, gateway, or service device type.
        /// Uses SCCP device-type value `80003` during registration and provisioning.
        Spa502g = 80003,
        /// Identifies the `Spa504g` station, gateway, or service device type.
        /// Uses SCCP device-type value `80004` during registration and provisioning.
        Spa504g = 80004,
        /// Identifies the `Spa525g` station, gateway, or service device type.
        /// Uses SCCP device-type value `80005` during registration and provisioning.
        Spa525g = 80005,
        /// Identifies the `Spa508g` station, gateway, or service device type.
        /// Uses SCCP device-type value `80006` during registration and provisioning.
        Spa508g = 80006,
        /// Identifies the `Spa509g` station, gateway, or service device type.
        /// Uses SCCP device-type value `80007` during registration and provisioning.
        Spa509g = 80007,
        /// Identifies the `Spa525g2` station, gateway, or service device type.
        /// Uses SCCP device-type value `80009` during registration and provisioning.
        Spa525g2 = 80009,
        /// Identifies the `Spa303g` station, gateway, or service device type.
        /// Uses SCCP device-type value `80011` during registration and provisioning.
        Spa303g = 80011,
        /// Identifies the `Spa512g` station, gateway, or service device type.
        /// Uses SCCP device-type value `80012` during registration and provisioning.
        Spa512g = 80012,
        /// Identifies the `Spa514g` station, gateway, or service device type.
        /// Uses SCCP device-type value `80013` during registration and provisioning.
        Spa514g = 80013,
        /// Identifies the `AddonSpa500s` station, gateway, or service device type.
        /// Uses SCCP device-type value `99991` during registration and provisioning.
        AddonSpa500s = 99991,
        /// Identifies the `AddonSpa500ds` station, gateway, or service device type.
        /// Uses SCCP device-type value `99992` during registration and provisioning.
        AddonSpa500ds = 99992,
        /// Identifies the `AddonSpa932ds` station, gateway, or service device type.
        /// Uses SCCP device-type value `99993` during registration and provisioning.
        AddonSpa932ds = 99993,
        /// Identifies the `NotDefined` station, gateway, or service device type.
        /// Uses SCCP device-type value `99999` during registration and provisioning.
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
        /// Selects the `None` media capability or payload format.
        /// Uses SCCP codec identifier `0x0000` in capability and media messages.
        None = 0x0000,
        /// Selects the `NonStandard` media capability or payload format.
        /// Uses SCCP codec identifier `0x0001` in capability and media messages.
        NonStandard = 0x0001,
        /// Selects the `Pcma` media capability or payload format.
        /// Uses SCCP codec identifier `0x0002` in capability and media messages.
        Pcma = 0x0002,
        /// Selects the `G711Alaw56k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0003` in capability and media messages.
        G711Alaw56k = 0x0003,
        /// Selects the `Pcmu` media capability or payload format.
        /// Uses SCCP codec identifier `0x0004` in capability and media messages.
        Pcmu = 0x0004,
        /// Selects the `G711Ulaw56k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0005` in capability and media messages.
        G711Ulaw56k = 0x0005,
        /// Selects the `G72264k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0006` in capability and media messages.
        G72264k = 0x0006,
        /// Selects the `G72256k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0007` in capability and media messages.
        G72256k = 0x0007,
        /// Selects the `G72248k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0008` in capability and media messages.
        G72248k = 0x0008,
        /// Selects the `G7231` media capability or payload format.
        /// Uses SCCP codec identifier `0x0009` in capability and media messages.
        G7231 = 0x0009,
        /// Selects the `G728` media capability or payload format.
        /// Uses SCCP codec identifier `0x000a` in capability and media messages.
        G728 = 0x000a,
        /// Selects the `G729` media capability or payload format.
        /// Uses SCCP codec identifier `0x000b` in capability and media messages.
        G729 = 0x000b,
        /// Selects the `G729A` media capability or payload format.
        /// Uses SCCP codec identifier `0x000c` in capability and media messages.
        G729A = 0x000c,
        /// Selects the `Is11172` media capability or payload format.
        /// Uses SCCP codec identifier `0x000d` in capability and media messages.
        Is11172 = 0x000d,
        /// Selects the `Is13818` media capability or payload format.
        /// Uses SCCP codec identifier `0x000e` in capability and media messages.
        Is13818 = 0x000e,
        /// Selects the `G729B` media capability or payload format.
        /// Uses SCCP codec identifier `0x000f` in capability and media messages.
        G729B = 0x000f,
        /// Selects the `G729Ab` media capability or payload format.
        /// Uses SCCP codec identifier `0x0010` in capability and media messages.
        G729Ab = 0x0010,
        /// Selects the `GsmFullRate` media capability or payload format.
        /// Uses SCCP codec identifier `0x0012` in capability and media messages.
        GsmFullRate = 0x0012,
        /// Selects the `GsmHalfRate` media capability or payload format.
        /// Uses SCCP codec identifier `0x0013` in capability and media messages.
        GsmHalfRate = 0x0013,
        /// Selects the `GsmEnhancedFullRate` media capability or payload format.
        /// Uses SCCP codec identifier `0x0014` in capability and media messages.
        GsmEnhancedFullRate = 0x0014,
        /// Selects the `Wideband256k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0019` in capability and media messages.
        Wideband256k = 0x0019,
        /// Selects the `Data64k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0020` in capability and media messages.
        Data64k = 0x0020,
        /// Selects the `Data56k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0021` in capability and media messages.
        Data56k = 0x0021,
        /// Selects the `G7221_32k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0028` in capability and media messages.
        G7221_32k = 0x0028,
        /// Selects the `G7221_24k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0029` in capability and media messages.
        G7221_24k = 0x0029,
        /// Selects the `Aac` media capability or payload format.
        /// Uses SCCP codec identifier `0x002a` in capability and media messages.
        Aac = 0x002a,
        /// Selects the `Mp4aLatm128` media capability or payload format.
        /// Uses SCCP codec identifier `0x002b` in capability and media messages.
        Mp4aLatm128 = 0x002b,
        /// Selects the `Mp4aLatm64` media capability or payload format.
        /// Uses SCCP codec identifier `0x002c` in capability and media messages.
        Mp4aLatm64 = 0x002c,
        /// Selects the `Mp4aLatm56` media capability or payload format.
        /// Uses SCCP codec identifier `0x002d` in capability and media messages.
        Mp4aLatm56 = 0x002d,
        /// Selects the `Mp4aLatm48` media capability or payload format.
        /// Uses SCCP codec identifier `0x002e` in capability and media messages.
        Mp4aLatm48 = 0x002e,
        /// Selects the `Mp4aLatm32` media capability or payload format.
        /// Uses SCCP codec identifier `0x002f` in capability and media messages.
        Mp4aLatm32 = 0x002f,
        /// Selects the `Mp4aLatm24` media capability or payload format.
        /// Uses SCCP codec identifier `0x0030` in capability and media messages.
        Mp4aLatm24 = 0x0030,
        /// Selects the `Mp4aLatm` media capability or payload format.
        /// Uses SCCP codec identifier `0x0031` in capability and media messages.
        Mp4aLatm = 0x0031,
        /// Selects the `Gsm` media capability or payload format.
        /// Uses SCCP codec identifier `0x0050` in capability and media messages.
        Gsm = 0x0050,
        /// Selects the `ActiveVoice` media capability or payload format.
        /// Uses SCCP codec identifier `0x0051` in capability and media messages.
        ActiveVoice = 0x0051,
        /// Selects the `G726_32k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0052` in capability and media messages.
        G726_32k = 0x0052,
        /// Selects the `G726_24k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0053` in capability and media messages.
        G726_24k = 0x0053,
        /// Selects the `G726_16k` media capability or payload format.
        /// Uses SCCP codec identifier `0x0054` in capability and media messages.
        G726_16k = 0x0054,
        /// Selects the `G729AnnexB` media capability or payload format.
        /// Uses SCCP codec identifier `0x0055` in capability and media messages.
        G729AnnexB = 0x0055,
        /// Selects the `Ilbc` media capability or payload format.
        /// Uses SCCP codec identifier `0x0056` in capability and media messages.
        Ilbc = 0x0056,
        /// Selects the `Isac` media capability or payload format.
        /// Uses SCCP codec identifier `0x0059` in capability and media messages.
        Isac = 0x0059,
        /// Selects the `Opus` media capability or payload format.
        /// Uses SCCP codec identifier `0x005a` in capability and media messages.
        Opus = 0x005a,
        /// Selects the `Amr` media capability or payload format.
        /// Uses SCCP codec identifier `0x0061` in capability and media messages.
        Amr = 0x0061,
        /// Selects the `AmrWb` media capability or payload format.
        /// Uses SCCP codec identifier `0x0062` in capability and media messages.
        AmrWb = 0x0062,
        /// Selects the `H261` media capability or payload format.
        /// Uses SCCP codec identifier `0x0064` in capability and media messages.
        H261 = 0x0064,
        /// Selects the `H263` media capability or payload format.
        /// Uses SCCP codec identifier `0x0065` in capability and media messages.
        H263 = 0x0065,
        /// Selects the `H263Plus` media capability or payload format.
        /// Uses SCCP codec identifier `0x0066` in capability and media messages.
        H263Plus = 0x0066,
        /// Selects the `H264` media capability or payload format.
        /// Uses SCCP codec identifier `0x0067` in capability and media messages.
        H264 = 0x0067,
        /// Selects the `H264Svc` media capability or payload format.
        /// Uses SCCP codec identifier `0x0068` in capability and media messages.
        H264Svc = 0x0068,
        /// Selects the `T120` media capability or payload format.
        /// Uses SCCP codec identifier `0x0069` in capability and media messages.
        T120 = 0x0069,
        /// Selects the `H224` media capability or payload format.
        /// Uses SCCP codec identifier `0x006a` in capability and media messages.
        H224 = 0x006a,
        /// Selects the `T38Fax` media capability or payload format.
        /// Uses SCCP codec identifier `0x006b` in capability and media messages.
        T38Fax = 0x006b,
        /// Selects the `Tote` media capability or payload format.
        /// Uses SCCP codec identifier `0x006c` in capability and media messages.
        Tote = 0x006c,
        /// Selects the `H265` media capability or payload format.
        /// Uses SCCP codec identifier `0x006d` in capability and media messages.
        H265 = 0x006d,
        /// Selects the `H264Uc` media capability or payload format.
        /// Uses SCCP codec identifier `0x006e` in capability and media messages.
        H264Uc = 0x006e,
        /// Selects the `Xv150ModemRelay711u` media capability or payload format.
        /// Uses SCCP codec identifier `0x006f` in capability and media messages.
        Xv150ModemRelay711u = 0x006f,
        /// Selects the `NseVbd711u` media capability or payload format.
        /// Uses SCCP codec identifier `0x0070` in capability and media messages.
        NseVbd711u = 0x0070,
        /// Selects the `Xv150ModemRelay729a` media capability or payload format.
        /// Uses SCCP codec identifier `0x0071` in capability and media messages.
        Xv150ModemRelay729a = 0x0071,
        /// Selects the `NseVbd729a` media capability or payload format.
        /// Uses SCCP codec identifier `0x0072` in capability and media messages.
        NseVbd729a = 0x0072,
        /// Selects the `H264Fec` media capability or payload format.
        /// Uses SCCP codec identifier `0x0073` in capability and media messages.
        H264Fec = 0x0073,
        /// Selects the `ClearChannel` media capability or payload format.
        /// Uses SCCP codec identifier `0x0078` in capability and media messages.
        ClearChannel = 0x0078,
        /// Selects the `UniversalTranscoder` media capability or payload format.
        /// Uses SCCP codec identifier `0x00de` in capability and media messages.
        UniversalTranscoder = 0x00de,
        /// Selects the `DtmfOutOfBandRfc2833` media capability or payload format.
        /// Uses SCCP codec identifier `0x0101` in capability and media messages.
        DtmfOutOfBandRfc2833 = 0x0101,
        /// Selects the `DtmfPassthrough` media capability or payload format.
        /// Uses SCCP codec identifier `0x0102` in capability and media messages.
        DtmfPassthrough = 0x0102,
        /// Selects the `DtmfDynamic` media capability or payload format.
        /// Uses SCCP codec identifier `0x0103` in capability and media messages.
        DtmfDynamic = 0x0103,
        /// Selects the `DtmfOutOfBand` media capability or payload format.
        /// Uses SCCP codec identifier `0x0104` in capability and media messages.
        DtmfOutOfBand = 0x0104,
        /// Selects the `DtmfInBandRfc2833` media capability or payload format.
        /// Uses SCCP codec identifier `0x0105` in capability and media messages.
        DtmfInBandRfc2833 = 0x0105,
        /// Selects the `CfbTones` media capability or payload format.
        /// Uses SCCP codec identifier `0x0106` in capability and media messages.
        CfbTones = 0x0106,
        /// Selects the `DtmfNoAudio` media capability or payload format.
        /// Uses SCCP codec identifier `0x012b` in capability and media messages.
        DtmfNoAudio = 0x012b,
        /// Selects the `V150ModemRelay` media capability or payload format.
        /// Uses SCCP codec identifier `0x012c` in capability and media messages.
        V150ModemRelay = 0x012c,
        /// Selects the `V150Sprt` media capability or payload format.
        /// Uses SCCP codec identifier `0x012d` in capability and media messages.
        V150Sprt = 0x012d,
        /// Selects the `V150Sse` media capability or payload format.
        /// Uses SCCP codec identifier `0x012e` in capability and media messages.
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
        /// Represents the `Unused` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x00` in station input messages.
        Unused = 0x00,
        /// Represents the `LastNumberRedial` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x01` in station input messages.
        LastNumberRedial = 0x01,
        /// Represents the `SpeedDial` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x02` in station input messages.
        SpeedDial = 0x02,
        /// Represents the `Hold` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x03` in station input messages.
        Hold = 0x03,
        /// Represents the `Transfer` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x04` in station input messages.
        Transfer = 0x04,
        /// Represents the `ForwardAll` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x05` in station input messages.
        ForwardAll = 0x05,
        /// Represents the `ForwardBusy` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x06` in station input messages.
        ForwardBusy = 0x06,
        /// Represents the `ForwardNoAnswer` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x07` in station input messages.
        ForwardNoAnswer = 0x07,
        /// Represents the `Display` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x08` in station input messages.
        Display = 0x08,
        /// Represents the `Line` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x09` in station input messages.
        Line = 0x09,
        /// Represents the `T120Chat` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x0a` in station input messages.
        T120Chat = 0x0a,
        /// Represents the `T120Whiteboard` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x0b` in station input messages.
        T120Whiteboard = 0x0b,
        /// Represents the `T120ApplicationSharing` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x0c` in station input messages.
        T120ApplicationSharing = 0x0c,
        /// Represents the `T120FileTransfer` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x0d` in station input messages.
        T120FileTransfer = 0x0d,
        /// Represents the `Video` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x0e` in station input messages.
        Video = 0x0e,
        /// Represents the `Voicemail` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x0f` in station input messages.
        Voicemail = 0x0f,
        /// Represents the `AnswerRelease` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x10` in station input messages.
        AnswerRelease = 0x10,
        /// Represents the `AutoAnswer` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x11` in station input messages.
        AutoAnswer = 0x11,
        /// Represents the `Select` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x12` in station input messages.
        Select = 0x12,
        /// Represents the `Privacy` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x13` in station input messages.
        Privacy = 0x13,
        /// Represents the `ServiceUrl` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x14` in station input messages.
        ServiceUrl = 0x14,
        /// Represents the `BlfSpeedDial` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x15` in station input messages.
        BlfSpeedDial = 0x15,
        /// Represents the `DirectedPark` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x16` in station input messages.
        DirectedPark = 0x16,
        /// Represents the `Intercom` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x17` in station input messages.
        Intercom = 0x17,
        /// Represents the `MaliciousCall` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x1b` in station input messages.
        MaliciousCall = 0x1b,
        /// Represents the `GenericAppB1` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x21` in station input messages.
        GenericAppB1 = 0x21,
        /// Represents the `GenericAppB2` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x22` in station input messages.
        GenericAppB2 = 0x22,
        /// Represents the `GenericAppB3` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x23` in station input messages.
        GenericAppB3 = 0x23,
        /// Represents the `GenericAppB4` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x24` in station input messages.
        GenericAppB4 = 0x24,
        /// Represents the `GenericAppB5` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x25` in station input messages.
        GenericAppB5 = 0x25,
        /// Represents the `MultiblinkFeature` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x26` in station input messages.
        MultiblinkFeature = 0x26,
        /// Represents the `MeetMeConference` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x7b` in station input messages.
        MeetMeConference = 0x7b,
        /// Represents the `Conference` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x7d` in station input messages.
        Conference = 0x7d,
        /// Represents the `CallPark` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x7e` in station input messages.
        CallPark = 0x7e,
        /// Represents the `CallPickup` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x7f` in station input messages.
        CallPickup = 0x7f,
        /// Represents the `GroupCallPickup` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x80` in station input messages.
        GroupCallPickup = 0x80,
        /// Represents the `Mobility` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x81` in station input messages.
        Mobility = 0x81,
        /// Represents the `DoNotDisturb` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x82` in station input messages.
        DoNotDisturb = 0x82,
        /// Represents the `ConferenceList` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x83` in station input messages.
        ConferenceList = 0x83,
        /// Represents the `RemoveLastParticipant` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x84` in station input messages.
        RemoveLastParticipant = 0x84,
        /// Represents the `QualityReportTool` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x85` in station input messages.
        QualityReportTool = 0x85,
        /// Represents the `Callback` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x86` in station input messages.
        Callback = 0x86,
        /// Represents the `OtherPickup` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x87` in station input messages.
        OtherPickup = 0x87,
        /// Represents the `VideoMode` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x88` in station input messages.
        VideoMode = 0x88,
        /// Represents the `NewCall` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x89` in station input messages.
        NewCall = 0x89,
        /// Represents the `EndCall` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x8a` in station input messages.
        EndCall = 0x8a,
        /// Represents the `HuntGroupLogin` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x8b` in station input messages.
        HuntGroupLogin = 0x8b,
        /// Represents the `Queuing` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0x8f` in station input messages.
        Queuing = 0x8f,
        /// Represents the `ParkingLot` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0xc0` in station input messages.
        ParkingLot = 0xc0,
        /// Represents the `Messages` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0xc2` in station input messages.
        Messages = 0xc2,
        /// Represents the `Directory` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0xc3` in station input messages.
        Directory = 0xc3,
        /// Represents the `Application` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0xc5` in station input messages.
        Application = 0xc5,
        /// Represents the `Headset` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0xc6` in station input messages.
        Headset = 0xc6,
        /// Represents the `Keypad` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0xf0` in station input messages.
        Keypad = 0xf0,
        /// Represents the `AcousticEchoCancellation` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0xfd` in station input messages.
        AcousticEchoCancellation = 0xfd,
        /// Represents the `Undefined` physical or logical station stimulus.
        /// Uses SCCP stimulus value `0xff` in station input messages.
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
        /// Selects the `Silence` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x00` in tone and announcement messages.
        Silence = 0x00,
        /// Selects the `Dtmf1` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x01` in tone and announcement messages.
        Dtmf1 = 0x01,
        /// Selects the `Dtmf2` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x02` in tone and announcement messages.
        Dtmf2 = 0x02,
        /// Selects the `Dtmf3` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x03` in tone and announcement messages.
        Dtmf3 = 0x03,
        /// Selects the `Dtmf4` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x04` in tone and announcement messages.
        Dtmf4 = 0x04,
        /// Selects the `Dtmf5` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x05` in tone and announcement messages.
        Dtmf5 = 0x05,
        /// Selects the `Dtmf6` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x06` in tone and announcement messages.
        Dtmf6 = 0x06,
        /// Selects the `Dtmf7` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x07` in tone and announcement messages.
        Dtmf7 = 0x07,
        /// Selects the `Dtmf8` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x08` in tone and announcement messages.
        Dtmf8 = 0x08,
        /// Selects the `Dtmf9` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x09` in tone and announcement messages.
        Dtmf9 = 0x09,
        /// Selects the `Dtmf0` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x0a` in tone and announcement messages.
        Dtmf0 = 0x0a,
        /// Selects the `DtmfStar` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x0e` in tone and announcement messages.
        DtmfStar = 0x0e,
        /// Selects the `DtmfPound` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x0f` in tone and announcement messages.
        DtmfPound = 0x0f,
        /// Selects the `DtmfA` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x10` in tone and announcement messages.
        DtmfA = 0x10,
        /// Selects the `DtmfB` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x11` in tone and announcement messages.
        DtmfB = 0x11,
        /// Selects the `DtmfC` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x12` in tone and announcement messages.
        DtmfC = 0x12,
        /// Selects the `DtmfD` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x13` in tone and announcement messages.
        DtmfD = 0x13,
        /// Selects the `InsideDial` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x21` in tone and announcement messages.
        InsideDial = 0x21,
        /// Selects the `OutsideDial` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x22` in tone and announcement messages.
        OutsideDial = 0x22,
        /// Selects the `LineBusy` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x23` in tone and announcement messages.
        LineBusy = 0x23,
        /// Selects the `Alerting` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x24` in tone and announcement messages.
        Alerting = 0x24,
        /// Selects the `Reorder` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x25` in tone and announcement messages.
        Reorder = 0x25,
        /// Selects the `RecorderWarning` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x26` in tone and announcement messages.
        RecorderWarning = 0x26,
        /// Selects the `RecorderDetected` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x27` in tone and announcement messages.
        RecorderDetected = 0x27,
        /// Selects the `Reverting` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x28` in tone and announcement messages.
        Reverting = 0x28,
        /// Selects the `ReceiverOffHook` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x29` in tone and announcement messages.
        ReceiverOffHook = 0x29,
        /// Selects the `PartialDial` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x2a` in tone and announcement messages.
        PartialDial = 0x2a,
        /// Selects the `NoSuchNumber` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x2b` in tone and announcement messages.
        NoSuchNumber = 0x2b,
        /// Selects the `BusyVerification` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x2c` in tone and announcement messages.
        BusyVerification = 0x2c,
        /// Selects the `CallWaiting` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x2d` in tone and announcement messages.
        CallWaiting = 0x2d,
        /// Selects the `Confirmation` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x2e` in tone and announcement messages.
        Confirmation = 0x2e,
        /// Selects the `CampOn` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x2f` in tone and announcement messages.
        CampOn = 0x2f,
        /// Selects the `RecallDial` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x30` in tone and announcement messages.
        RecallDial = 0x30,
        /// Selects the `ZipZip` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x31` in tone and announcement messages.
        ZipZip = 0x31,
        /// Selects the `Zip` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x32` in tone and announcement messages.
        Zip = 0x32,
        /// Selects the `BeepBonk` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x33` in tone and announcement messages.
        BeepBonk = 0x33,
        /// Selects the `Music` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x34` in tone and announcement messages.
        Music = 0x34,
        /// Selects the `Hold` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x35` in tone and announcement messages.
        Hold = 0x35,
        /// Selects the `Test` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x36` in tone and announcement messages.
        Test = 0x36,
        /// Selects the `MonitorWarning` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x37` in tone and announcement messages.
        MonitorWarning = 0x37,
        /// Selects the `AddCallWaiting` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x40` in tone and announcement messages.
        AddCallWaiting = 0x40,
        /// Selects the `PriorityCallWaiting` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x41` in tone and announcement messages.
        PriorityCallWaiting = 0x41,
        /// Selects the `BargeIn` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x43` in tone and announcement messages.
        BargeIn = 0x43,
        /// Selects the `DistinctAlert` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x44` in tone and announcement messages.
        DistinctAlert = 0x44,
        /// Selects the `PriorityAlert` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x45` in tone and announcement messages.
        PriorityAlert = 0x45,
        /// Selects the `ReminderRing` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x46` in tone and announcement messages.
        ReminderRing = 0x46,
        /// Selects the `PrecedenceRingback` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x47` in tone and announcement messages.
        PrecedenceRingback = 0x47,
        /// Selects the `Preemption` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x48` in tone and announcement messages.
        Preemption = 0x48,
        /// Selects the `NoTone` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x7f` in tone and announcement messages.
        NoTone = 0x7f,
        /// Selects the `MeetMeGreeting` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x80` in tone and announcement messages.
        MeetMeGreeting = 0x80,
        /// Selects the `MeetMeNumberInvalid` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x81` in tone and announcement messages.
        MeetMeNumberInvalid = 0x81,
        /// Selects the `MeetMeNumberFailed` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x82` in tone and announcement messages.
        MeetMeNumberFailed = 0x82,
        /// Selects the `MeetMeEnterPin` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x83` in tone and announcement messages.
        MeetMeEnterPin = 0x83,
        /// Selects the `MeetMeInvalidPin` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x84` in tone and announcement messages.
        MeetMeInvalidPin = 0x84,
        /// Selects the `MeetMeFailedPin` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x85` in tone and announcement messages.
        MeetMeFailedPin = 0x85,
        /// Selects the `MeetMeCfbFailed` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x86` in tone and announcement messages.
        MeetMeCfbFailed = 0x86,
        /// Selects the `MeetMeEnterAccessCode` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x87` in tone and announcement messages.
        MeetMeEnterAccessCode = 0x87,
        /// Selects the `MeetMeAccessCodeInvalid` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x88` in tone and announcement messages.
        MeetMeAccessCodeInvalid = 0x88,
        /// Selects the `MeetMeAccessCodeFailed` station tone or tone-control behavior.
        /// Uses SCCP tone identifier `0x89` in tone and announcement messages.
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
        /// Defines a provisioned `Unused` button or template slot.
        /// Uses SCCP button-type value `0x00` in station button templates.
        Unused = 0x00,
        /// Defines a provisioned `LastNumberRedial` button or template slot.
        /// Uses SCCP button-type value `0x01` in station button templates.
        LastNumberRedial = 0x01,
        /// Defines a provisioned `SpeedDial` button or template slot.
        /// Uses SCCP button-type value `0x02` in station button templates.
        SpeedDial = 0x02,
        /// Defines a provisioned `Hold` button or template slot.
        /// Uses SCCP button-type value `0x03` in station button templates.
        Hold = 0x03,
        /// Defines a provisioned `Transfer` button or template slot.
        /// Uses SCCP button-type value `0x04` in station button templates.
        Transfer = 0x04,
        /// Defines a provisioned `ForwardAll` button or template slot.
        /// Uses SCCP button-type value `0x05` in station button templates.
        ForwardAll = 0x05,
        /// Defines a provisioned `ForwardBusy` button or template slot.
        /// Uses SCCP button-type value `0x06` in station button templates.
        ForwardBusy = 0x06,
        /// Defines a provisioned `ForwardNoAnswer` button or template slot.
        /// Uses SCCP button-type value `0x07` in station button templates.
        ForwardNoAnswer = 0x07,
        /// Defines a provisioned `Display` button or template slot.
        /// Uses SCCP button-type value `0x08` in station button templates.
        Display = 0x08,
        /// Defines a provisioned `Line` button or template slot.
        /// Uses SCCP button-type value `0x09` in station button templates.
        Line = 0x09,
        /// Defines a provisioned `T120Chat` button or template slot.
        /// Uses SCCP button-type value `0x0a` in station button templates.
        T120Chat = 0x0a,
        /// Defines a provisioned `T120Whiteboard` button or template slot.
        /// Uses SCCP button-type value `0x0b` in station button templates.
        T120Whiteboard = 0x0b,
        /// Defines a provisioned `T120ApplicationSharing` button or template slot.
        /// Uses SCCP button-type value `0x0c` in station button templates.
        T120ApplicationSharing = 0x0c,
        /// Defines a provisioned `T120FileTransfer` button or template slot.
        /// Uses SCCP button-type value `0x0d` in station button templates.
        T120FileTransfer = 0x0d,
        /// Defines a provisioned `Video` button or template slot.
        /// Uses SCCP button-type value `0x0e` in station button templates.
        Video = 0x0e,
        /// Defines a provisioned `Voicemail` button or template slot.
        /// Uses SCCP button-type value `0x0f` in station button templates.
        Voicemail = 0x0f,
        /// Defines a provisioned `AnswerRelease` button or template slot.
        /// Uses SCCP button-type value `0x10` in station button templates.
        AnswerRelease = 0x10,
        /// Defines a provisioned `AutoAnswer` button or template slot.
        /// Uses SCCP button-type value `0x11` in station button templates.
        AutoAnswer = 0x11,
        /// Defines a provisioned `Select` button or template slot.
        /// Uses SCCP button-type value `0x12` in station button templates.
        Select = 0x12,
        /// Defines a provisioned `Feature` button or template slot.
        /// Uses SCCP button-type value `0x13` in station button templates.
        Feature = 0x13,
        /// Defines a provisioned `ServiceUrl` button or template slot.
        /// Uses SCCP button-type value `0x14` in station button templates.
        ServiceUrl = 0x14,
        /// Defines a provisioned `BlfSpeedDial` button or template slot.
        /// Uses SCCP button-type value `0x15` in station button templates.
        BlfSpeedDial = 0x15,
        /// Defines a provisioned `DirectedPark` button or template slot.
        /// Uses SCCP button-type value `0x16` in station button templates.
        DirectedPark = 0x16,
        /// Defines a provisioned `Intercom` button or template slot.
        /// Uses SCCP button-type value `0x17` in station button templates.
        Intercom = 0x17,
        /// Defines a provisioned `MaliciousCall` button or template slot.
        /// Uses SCCP button-type value `0x1b` in station button templates.
        MaliciousCall = 0x1b,
        /// Defines a provisioned `GenericAppB1` button or template slot.
        /// Uses SCCP button-type value `0x21` in station button templates.
        GenericAppB1 = 0x21,
        /// Defines a provisioned `GenericAppB2` button or template slot.
        /// Uses SCCP button-type value `0x22` in station button templates.
        GenericAppB2 = 0x22,
        /// Defines a provisioned `GenericAppB3` button or template slot.
        /// Uses SCCP button-type value `0x23` in station button templates.
        GenericAppB3 = 0x23,
        /// Defines a provisioned `GenericAppB4` button or template slot.
        /// Uses SCCP button-type value `0x24` in station button templates.
        GenericAppB4 = 0x24,
        /// Defines a provisioned `GenericAppB5` button or template slot.
        /// Uses SCCP button-type value `0x25` in station button templates.
        GenericAppB5 = 0x25,
        /// Defines a provisioned `MultiblinkFeature` button or template slot.
        /// Uses SCCP button-type value `0x26` in station button templates.
        MultiblinkFeature = 0x26,
        /// Defines a provisioned `MeetMeConference` button or template slot.
        /// Uses SCCP button-type value `0x7b` in station button templates.
        MeetMeConference = 0x7b,
        /// Defines a provisioned `Conference` button or template slot.
        /// Uses SCCP button-type value `0x7d` in station button templates.
        Conference = 0x7d,
        /// Defines a provisioned `CallPark` button or template slot.
        /// Uses SCCP button-type value `0x7e` in station button templates.
        CallPark = 0x7e,
        /// Defines a provisioned `CallPickup` button or template slot.
        /// Uses SCCP button-type value `0x7f` in station button templates.
        CallPickup = 0x7f,
        /// Defines a provisioned `GroupCallPickup` button or template slot.
        /// Uses SCCP button-type value `0x80` in station button templates.
        GroupCallPickup = 0x80,
        /// Defines a provisioned `Mobility` button or template slot.
        /// Uses SCCP button-type value `0x81` in station button templates.
        Mobility = 0x81,
        /// Defines a provisioned `DoNotDisturb` button or template slot.
        /// Uses SCCP button-type value `0x82` in station button templates.
        DoNotDisturb = 0x82,
        /// Defines a provisioned `ConferenceList` button or template slot.
        /// Uses SCCP button-type value `0x83` in station button templates.
        ConferenceList = 0x83,
        /// Defines a provisioned `RemoveLastParticipant` button or template slot.
        /// Uses SCCP button-type value `0x84` in station button templates.
        RemoveLastParticipant = 0x84,
        /// Defines a provisioned `QualityReportTool` button or template slot.
        /// Uses SCCP button-type value `0x85` in station button templates.
        QualityReportTool = 0x85,
        /// Defines a provisioned `Callback` button or template slot.
        /// Uses SCCP button-type value `0x86` in station button templates.
        Callback = 0x86,
        /// Defines a provisioned `OtherPickup` button or template slot.
        /// Uses SCCP button-type value `0x87` in station button templates.
        OtherPickup = 0x87,
        /// Defines a provisioned `VideoMode` button or template slot.
        /// Uses SCCP button-type value `0x88` in station button templates.
        VideoMode = 0x88,
        /// Defines a provisioned `NewCall` button or template slot.
        /// Uses SCCP button-type value `0x89` in station button templates.
        NewCall = 0x89,
        /// Defines a provisioned `EndCall` button or template slot.
        /// Uses SCCP button-type value `0x8a` in station button templates.
        EndCall = 0x8a,
        /// Defines a provisioned `HuntGroupLogin` button or template slot.
        /// Uses SCCP button-type value `0x8b` in station button templates.
        HuntGroupLogin = 0x8b,
        /// Defines a provisioned `Queuing` button or template slot.
        /// Uses SCCP button-type value `0x8f` in station button templates.
        Queuing = 0x8f,
        /// Defines a provisioned `ParkingLot` button or template slot.
        /// Uses SCCP button-type value `0xc0` in station button templates.
        ParkingLot = 0xc0,
        /// Defines a provisioned `Messages` button or template slot.
        /// Uses SCCP button-type value `0xc2` in station button templates.
        Messages = 0xc2,
        /// Defines a provisioned `Directory` button or template slot.
        /// Uses SCCP button-type value `0xc3` in station button templates.
        Directory = 0xc3,
        /// Defines a provisioned `Application` button or template slot.
        /// Uses SCCP button-type value `0xc5` in station button templates.
        Application = 0xc5,
        /// Defines a provisioned `Headset` button or template slot.
        /// Uses SCCP button-type value `0xc6` in station button templates.
        Headset = 0xc6,
        /// Defines a provisioned `Keypad` button or template slot.
        /// Uses SCCP button-type value `0xf0` in station button templates.
        Keypad = 0xf0,
        /// Defines a provisioned `PlaceholderMulti` button or template slot.
        /// Uses SCCP button-type value `0xf1` in station button templates.
        PlaceholderMulti = 0xf1,
        /// Defines a provisioned `PlaceholderLine` button or template slot.
        /// Uses SCCP button-type value `0xf2` in station button templates.
        PlaceholderLine = 0xf2,
        /// Defines a provisioned `PlaceholderSpeedDial` button or template slot.
        /// Uses SCCP button-type value `0xf3` in station button templates.
        PlaceholderSpeedDial = 0xf3,
        /// Defines a provisioned `PlaceholderHint` button or template slot.
        /// Uses SCCP button-type value `0xf4` in station button templates.
        PlaceholderHint = 0xf4,
        /// Defines a provisioned `PlaceholderAbbreviatedDial` button or template slot.
        /// Uses SCCP button-type value `0xf5` in station button templates.
        PlaceholderAbbreviatedDial = 0xf5,
        /// Defines a provisioned `AcousticEchoCancellation` button or template slot.
        /// Uses SCCP button-type value `0xfd` in station button templates.
        AcousticEchoCancellation = 0xfd,
        /// Defines a provisioned `Undefined` button or template slot.
        /// Uses SCCP button-type value `0xff` in station button templates.
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
