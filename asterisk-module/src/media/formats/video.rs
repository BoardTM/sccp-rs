//! Video format mapping and capability negotiation.

use sccp_protocol::{
    Codec, CodecKind, MultimediaPayload, MultimediaPictureFormat, MultimediaVideoCapability,
    MultimediaVideoCapabilityArm, ReceiveTransmit, RtpPayloadNumber, StationMediaCapabilities,
    VideoCapability, VideoFormat,
};
use thiserror::Error;

/// Video formats exposed by the channel backend.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PbxVideoFormat {
    H261,
    H263,
    H263Plus,
    H264,
    H265,
}

impl PbxVideoFormat {
    pub const ALL: [Self; 5] = [
        Self::H261,
        Self::H263,
        Self::H263Plus,
        Self::H264,
        Self::H265,
    ];

    pub(crate) const fn native_mask(self) -> u32 {
        1 << self as u8
    }

    /// RTP payload number installed in the independently owned video instance.
    ///
    /// Static assignments are retained for the formats which have one. Dynamic
    /// assignments are stable within this channel driver so the native RTP map
    /// and station command always receive the same number.
    pub fn payload_type(self) -> Option<RtpPayloadNumber> {
        match self {
            Self::H261 => RtpPayloadNumber::new(31).ok(),
            Self::H263 => RtpPayloadNumber::new(34).ok(),
            Self::H263Plus => RtpPayloadNumber::new(98).ok(),
            Self::H264 => RtpPayloadNumber::new(103).ok(),
            Self::H265 => None,
        }
    }
}

#[derive(Clone, Copy)]
struct VideoFormatMapping {
    codec: Codec,
    pbx: PbxVideoFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VideoMatch {
    pbx_format: PbxVideoFormat,
    capability_index: usize,
}

const VIDEO_FORMAT_MAPPINGS: [VideoFormatMapping; 5] = [
    VideoFormatMapping {
        codec: Codec::H261,
        pbx: PbxVideoFormat::H261,
    },
    VideoFormatMapping {
        codec: Codec::H263,
        pbx: PbxVideoFormat::H263,
    },
    VideoFormatMapping {
        codec: Codec::H263Plus,
        pbx: PbxVideoFormat::H263Plus,
    },
    VideoFormatMapping {
        codec: Codec::H264,
        pbx: PbxVideoFormat::H264,
    },
    VideoFormatMapping {
        codec: Codec::H265,
        pbx: PbxVideoFormat::H265,
    },
];

/// Known PBX formats and any unrecognized bits from a native capability mask.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedPbxVideoFormats {
    supported: Vec<PbxVideoFormat>,
    unknown_bits: u32,
}

impl DecodedPbxVideoFormats {
    pub fn supported(&self) -> &[PbxVideoFormat] {
        &self.supported
    }

    pub const fn unknown_bits(&self) -> u32 {
        self.unknown_bits
    }

    pub const fn is_exact(&self) -> bool {
        self.unknown_bits == 0
    }
}

impl AsRef<[PbxVideoFormat]> for DecodedPbxVideoFormats {
    fn as_ref(&self) -> &[PbxVideoFormat] {
        self.supported()
    }
}

impl IntoIterator for DecodedPbxVideoFormats {
    type Item = PbxVideoFormat;
    type IntoIter = std::vec::IntoIter<PbxVideoFormat>;

    fn into_iter(self) -> Self::IntoIter {
        self.supported.into_iter()
    }
}

impl<'a> IntoIterator for &'a DecodedPbxVideoFormats {
    type Item = &'a PbxVideoFormat;
    type IntoIter = std::slice::Iter<'a, PbxVideoFormat>;

    fn into_iter(self) -> Self::IntoIter {
        self.supported.iter()
    }
}

impl From<u32> for DecodedPbxVideoFormats {
    fn from(mask: u32) -> Self {
        Self {
            supported: VIDEO_FORMAT_MAPPINGS
                .iter()
                .filter(|mapping| mask & mapping.pbx.native_mask() != 0)
                .map(|mapping| mapping.pbx)
                .collect(),
            unknown_bits: VIDEO_FORMAT_MAPPINGS.iter().fold(mask, |unknown, mapping| {
                unknown & !mapping.pbx.native_mask()
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum VideoFormatError {
    #[error("{0:?} is not a video codec")]
    NotVideo(Codec),
    #[error("video codec {0:?} has no PBX format mapping")]
    Unsupported(Codec),
    #[error("video codec identifier {0:#x} is unknown")]
    Unknown(u32),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum VideoDescriptorError {
    #[error("video codec {0:?} has no command capability arm")]
    UnsupportedCodec(Codec),
    #[error("video capability has no operating-level preference")]
    MissingLevelPreference,
    #[error(
        "video codec {codec:?} needs {required} capability parameters, but the station supplied {actual}"
    )]
    MissingCodecParameters {
        codec: Codec,
        required: usize,
        actual: usize,
    },
}

impl TryFrom<Codec> for PbxVideoFormat {
    type Error = VideoFormatError;

    fn try_from(codec: Codec) -> Result<Self, Self::Error> {
        if let Codec::Unknown(value) = codec {
            return Err(VideoFormatError::Unknown(value));
        }
        VIDEO_FORMAT_MAPPINGS
            .iter()
            .find(|mapping| mapping.codec == codec)
            .map(|mapping| mapping.pbx)
            .ok_or_else(|| {
                if codec.kind() == CodecKind::Video {
                    VideoFormatError::Unsupported(codec)
                } else {
                    VideoFormatError::NotVideo(codec)
                }
            })
    }
}

/// A mutually supported format with the station's complete typed capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedVideo<'station> {
    pub pbx_format: PbxVideoFormat,
    capability: &'station VideoCapability,
}

impl<'station> NegotiatedVideo<'station> {
    pub const fn codec(&self) -> Codec {
        self.capability.codec
    }

    pub const fn capability(&self) -> &'station VideoCapability {
        self.capability
    }
}

impl AsRef<VideoCapability> for NegotiatedVideo<'_> {
    fn as_ref(&self) -> &VideoCapability {
        self.capability()
    }
}

/// A negotiated format that keeps its immutable station snapshot alive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedNegotiatedVideo {
    pub pbx_format: PbxVideoFormat,
    station: StationMediaCapabilities,
    capability_index: usize,
}

impl OwnedNegotiatedVideo {
    pub fn codec(&self) -> Codec {
        self.capability().codec
    }

    pub fn capability(&self) -> &VideoCapability {
        &self.station.video()[self.capability_index]
    }

    /// Builds the fully typed payload shared by receive-open and transmit-start
    /// commands from the selected station operating level.
    pub fn multimedia_payload(
        &self,
        payload_type: RtpPayloadNumber,
    ) -> Result<MultimediaPayload, VideoDescriptorError> {
        let capability = self.capability();
        let level = capability
            .level_preferences
            .first()
            .ok_or(VideoDescriptorError::MissingLevelPreference)?;
        let parameters = capability.codec_parameters.as_slice();
        let required_parameters = match capability.codec {
            Codec::H261 | Codec::H263 | Codec::H263Plus => 2,
            Codec::H264 => 6,
            codec => return Err(VideoDescriptorError::UnsupportedCodec(codec)),
        };
        if parameters.len() < required_parameters {
            return Err(VideoDescriptorError::MissingCodecParameters {
                codec: capability.codec,
                required: required_parameters,
                actual: parameters.len(),
            });
        }
        let arm = match capability.codec {
            Codec::H261 => MultimediaVideoCapabilityArm::H261 {
                temporal_spatial_trade_off_capability: parameters[0],
                still_image_transmission: parameters[1],
            },
            Codec::H263 => MultimediaVideoCapabilityArm::H263 {
                capability_bitfield: parameters[0],
                annex_n_and_w_future_use: parameters[1],
            },
            Codec::H263Plus => MultimediaVideoCapabilityArm::H263Plus {
                model_number: parameters[0],
                bandwidth: parameters[1],
            },
            Codec::H264 => MultimediaVideoCapabilityArm::H264 {
                profile: parameters[0],
                level: parameters[1],
                custom_max_mbps: parameters[2],
                custom_max_fs: parameters[3],
                custom_max_dpb: parameters[4],
                custom_max_br_and_cpb: parameters[5],
            },
            codec => return Err(VideoDescriptorError::UnsupportedCodec(codec)),
        };
        let command_capability = MultimediaVideoCapability::new(
            level.max_bit_rate,
            [MultimediaPictureFormat {
                format: VideoFormat::from(level.format),
                minimum_picture_interval: level.minimum_picture_interval,
            }],
            level.service_number,
            arm,
        )
        .expect("one picture format is within the protocol bound");
        Ok(MultimediaPayload::new(payload_type, command_capability))
    }
}

impl AsRef<VideoCapability> for OwnedNegotiatedVideo {
    fn as_ref(&self) -> &VideoCapability {
        self.capability()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum VideoNegotiationError {
    #[error("video negotiation requires a known media direction")]
    InvalidRequiredDirection,
    #[error("the configured preference list has no supported video codec")]
    NoRepresentableConfiguredCodec,
    #[error("the station and PBX have no mutually supported video codec and direction")]
    NoMutualCodec,
}

/// Decode a native video capability mask without discarding unknown bits.
pub fn decode_pbx_video_formats(mask: u32) -> DecodedPbxVideoFormats {
    mask.into()
}

/// Convert the known bits of a native video capability mask in stable order.
pub fn pbx_video_formats_from_mask(mask: u32) -> Vec<PbxVideoFormat> {
    decode_pbx_video_formats(mask).into_iter().collect()
}

/// Resolve a station video codec without substituting another wire codec.
pub fn pbx_video_format(codec: Codec) -> Result<PbxVideoFormat, VideoFormatError> {
    codec.try_into()
}

/// Explain why a known video codec has no native format mapping.
pub const fn unsupported_video_reason(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::H264Svc | Codec::H264Fec | Codec::H264Uc => {
            Some("the channel backend has no distinct format for this H.264 capability")
        }
        _ => None,
    }
}

/// Select the first configured codec supported by the station and PBX.
///
/// The configured list establishes preference. A station entry must match the
/// exact wire codec and contain every required direction bit. The returned
/// value borrows the complete capability so level preferences, codec parameters,
/// encryption support, and address family remain available without a partial
/// projection or an unnecessary clone.
pub fn negotiate_video<'station>(
    configured: &[Codec],
    station: &'station [VideoCapability],
    pbx: &[PbxVideoFormat],
    required_direction: ReceiveTransmit,
) -> Result<NegotiatedVideo<'station>, VideoNegotiationError> {
    let selected = select_video(configured, station, pbx, required_direction)?;
    Ok(NegotiatedVideo {
        pbx_format: selected.pbx_format,
        capability: &station[selected.capability_index],
    })
}

/// Select a video format that can cross an ownership or lock boundary.
///
/// The returned value retains the Arc-backed station snapshot and its selected
/// index. It does not copy the capability or retain an opaque stream-command
/// union.
pub fn negotiate_video_owned(
    configured: &[Codec],
    station: StationMediaCapabilities,
    pbx: &[PbxVideoFormat],
    required_direction: ReceiveTransmit,
) -> Result<OwnedNegotiatedVideo, VideoNegotiationError> {
    let selected = select_video(configured, station.video(), pbx, required_direction)?;
    Ok(OwnedNegotiatedVideo {
        pbx_format: selected.pbx_format,
        station,
        capability_index: selected.capability_index,
    })
}

fn select_video(
    configured: &[Codec],
    station: &[VideoCapability],
    pbx: &[PbxVideoFormat],
    required_direction: ReceiveTransmit,
) -> Result<VideoMatch, VideoNegotiationError> {
    let known_directions = ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT;
    if required_direction.is_empty() || required_direction.bits() & !known_directions.bits() != 0 {
        return Err(VideoNegotiationError::InvalidRequiredDirection);
    }

    let mut has_representable = false;
    for codec in configured.iter().copied() {
        let Ok(pbx_format) = pbx_video_format(codec) else {
            continue;
        };
        has_representable = true;
        if !pbx.contains(&pbx_format) {
            continue;
        }
        let Some(capability_index) = station.iter().position(|capability| {
            capability.codec == codec && capability.direction.contains(required_direction)
        }) else {
            continue;
        };
        return Ok(VideoMatch {
            pbx_format,
            capability_index,
        });
    }

    if has_representable {
        Err(VideoNegotiationError::NoMutualCodec)
    } else {
        Err(VideoNegotiationError::NoRepresentableConfiguredCodec)
    }
}

#[cfg(test)]
mod tests {
    use sccp_protocol::{EncryptionCapability, IpAddressType, VideoLevelPreference};

    use super::*;

    fn capability(codec: Codec, direction: ReceiveTransmit, marker: u32) -> VideoCapability {
        VideoCapability {
            codec,
            direction,
            level_preferences: vec![VideoLevelPreference {
                transmit_preference: marker,
                format: marker + 1,
                max_bit_rate: marker + 2,
                min_bit_rate: marker + 3,
                minimum_picture_interval: marker + 4,
                service_number: marker + 5,
            }],
            codec_parameters: vec![marker + 6, marker + 7],
            encryption_capability: Some(EncryptionCapability::Capable),
            address_type: Some(IpAddressType::Ipv4AndIpv6),
        }
    }

    #[test]
    fn table_maps_every_backend_video_format_exactly_once() {
        assert_eq!(
            VIDEO_FORMAT_MAPPINGS
                .iter()
                .map(|mapping| (mapping.codec, mapping.pbx))
                .collect::<Vec<_>>(),
            [
                (Codec::H261, PbxVideoFormat::H261),
                (Codec::H263, PbxVideoFormat::H263),
                (Codec::H263Plus, PbxVideoFormat::H263Plus),
                (Codec::H264, PbxVideoFormat::H264),
                (Codec::H265, PbxVideoFormat::H265),
            ]
        );
        assert_eq!(
            VIDEO_FORMAT_MAPPINGS
                .iter()
                .map(|mapping| mapping.pbx)
                .collect::<Vec<_>>(),
            PbxVideoFormat::ALL
        );
    }

    #[test]
    fn every_known_video_codec_is_mapped_or_explicitly_unsupported() {
        for codec in Codec::ALL_KNOWN.iter().copied() {
            if codec.kind() != CodecKind::Video {
                continue;
            }
            assert!(
                pbx_video_format(codec).is_ok() ^ unsupported_video_reason(codec).is_some(),
                "{codec:?} must have exactly one adapter mapping decision"
            );
        }
    }

    #[test]
    fn format_errors_distinguish_unknown_unsupported_and_non_video_codecs() {
        assert_eq!(
            pbx_video_format(Codec::Unknown(0xfeed)),
            Err(VideoFormatError::Unknown(0xfeed))
        );
        assert_eq!(
            pbx_video_format(Codec::H264Svc),
            Err(VideoFormatError::Unsupported(Codec::H264Svc))
        );
        assert_eq!(
            pbx_video_format(Codec::Pcmu),
            Err(VideoFormatError::NotVideo(Codec::Pcmu))
        );
    }

    #[test]
    fn native_masks_preserve_known_order_and_report_unknown_bits() {
        let known_mask = VIDEO_FORMAT_MAPPINGS
            .iter()
            .fold(0, |mask, mapping| mask | mapping.pbx.native_mask());
        let decoded = decode_pbx_video_formats((1 << 4) | (1 << 1) | (1 << 12));
        assert_eq!(
            decoded.supported(),
            [PbxVideoFormat::H263, PbxVideoFormat::H265]
        );
        assert_eq!(decoded.unknown_bits(), 1 << 12);
        assert!(!decoded.is_exact());

        assert!(pbx_video_formats_from_mask(0).is_empty());
        assert_eq!(pbx_video_formats_from_mask(known_mask), PbxVideoFormat::ALL);
        assert!(decode_pbx_video_formats(known_mask).is_exact());
    }

    #[test]
    fn negotiation_uses_configured_preference_then_exact_intersection() {
        let station = [
            capability(
                Codec::H263,
                ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
                10,
            ),
            capability(
                Codec::H264,
                ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
                20,
            ),
        ];
        let negotiated = negotiate_video(
            &[Codec::H264, Codec::H263],
            &station,
            &PbxVideoFormat::ALL,
            ReceiveTransmit::TRANSMIT,
        )
        .unwrap();
        assert_eq!(negotiated.codec(), Codec::H264);
        assert_eq!(negotiated.pbx_format, PbxVideoFormat::H264);
        assert_eq!(negotiated.capability(), &station[1]);

        let fallback = negotiate_video(
            &[Codec::H264, Codec::H263],
            &station,
            &[PbxVideoFormat::H263],
            ReceiveTransmit::RECEIVE,
        )
        .unwrap();
        assert_eq!(fallback.codec(), Codec::H263);
    }

    #[test]
    fn negotiation_preserves_all_typed_station_parameters() {
        let expected = capability(
            Codec::H265,
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
            50,
        );
        let negotiated = negotiate_video(
            &[Codec::H265],
            std::slice::from_ref(&expected),
            &[PbxVideoFormat::H265],
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
        )
        .unwrap();

        assert_eq!(negotiated.capability(), &expected);
    }

    #[test]
    fn borrowed_and_owned_negotiation_select_the_same_shared_capability() {
        let station = StationMediaCapabilities::new(
            Vec::new(),
            vec![
                capability(Codec::H263, ReceiveTransmit::RECEIVE, 30),
                capability(
                    Codec::H265,
                    ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
                    40,
                ),
            ],
        );
        let borrowed = negotiate_video(
            &[Codec::H265, Codec::H263],
            station.video(),
            &PbxVideoFormat::ALL,
            ReceiveTransmit::TRANSMIT,
        )
        .unwrap();
        let owned = negotiate_video_owned(
            &[Codec::H265, Codec::H263],
            station.clone(),
            &PbxVideoFormat::ALL,
            ReceiveTransmit::TRANSMIT,
        )
        .unwrap();

        assert_eq!(owned.pbx_format, borrowed.pbx_format);
        assert_eq!(owned.codec(), borrowed.codec());
        assert_eq!(owned.capability(), borrowed.capability());
        assert!(std::ptr::eq(owned.capability(), borrowed.capability()));
    }

    #[test]
    fn negotiation_requires_exact_codec_and_requested_direction() {
        let station = [capability(Codec::H263Plus, ReceiveTransmit::RECEIVE, 5)];
        assert_eq!(
            negotiate_video(
                &[Codec::H263],
                &station,
                &[PbxVideoFormat::H263, PbxVideoFormat::H263Plus],
                ReceiveTransmit::RECEIVE,
            ),
            Err(VideoNegotiationError::NoMutualCodec)
        );
        assert_eq!(
            negotiate_video(
                &[Codec::H263Plus],
                &station,
                &[PbxVideoFormat::H263Plus],
                ReceiveTransmit::TRANSMIT,
            ),
            Err(VideoNegotiationError::NoMutualCodec)
        );
    }

    #[test]
    fn configured_h264_family_does_not_substitute_variant_capabilities() {
        let station = [capability(
            Codec::H264Svc,
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
            15,
        )];

        assert_eq!(
            negotiate_video(
                &[Codec::H264, Codec::H264Svc, Codec::H264Fec, Codec::H264Uc,],
                &station,
                &[PbxVideoFormat::H264],
                ReceiveTransmit::RECEIVE,
            ),
            Err(VideoNegotiationError::NoMutualCodec)
        );
    }

    #[test]
    fn negotiation_rejects_invalid_direction_and_unrepresentable_policy() {
        assert_eq!(
            negotiate_video(
                &[Codec::H264],
                &[],
                &PbxVideoFormat::ALL,
                ReceiveTransmit::empty(),
            ),
            Err(VideoNegotiationError::InvalidRequiredDirection)
        );
        assert_eq!(
            negotiate_video(
                &[Codec::H264Svc, Codec::Pcmu, Codec::Unknown(7)],
                &[],
                &PbxVideoFormat::ALL,
                ReceiveTransmit::RECEIVE,
            ),
            Err(VideoNegotiationError::NoRepresentableConfiguredCodec)
        );
    }

    #[test]
    fn selected_operating_level_builds_a_typed_h264_payload() {
        let mut h264 = capability(
            Codec::H264,
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
            10,
        );
        h264.codec_parameters = vec![64, 43, 40_500, 1_620, 8_100, 10_000];
        let selected = negotiate_video_owned(
            &[Codec::H264],
            StationMediaCapabilities::new(Vec::new(), vec![h264]),
            &[PbxVideoFormat::H264],
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
        )
        .unwrap();

        let payload = selected
            .multimedia_payload(PbxVideoFormat::H264.payload_type().unwrap())
            .unwrap();
        let command = payload.video_capability().unwrap();
        assert_eq!(payload.payload_number().get(), 103);
        assert_eq!(payload.codec(), Codec::H264);
        assert_eq!(command.bit_rate(), 12);
        assert_eq!(
            command.picture_formats()[0].format,
            VideoFormat::Unknown(11)
        );
        assert_eq!(command.picture_formats()[0].minimum_picture_interval, 14);
        assert_eq!(command.conference_service_number(), 15);
        assert_eq!(
            command.arm(),
            MultimediaVideoCapabilityArm::H264 {
                profile: 64,
                level: 43,
                custom_max_mbps: 40_500,
                custom_max_fs: 1_620,
                custom_max_dpb: 8_100,
                custom_max_br_and_cpb: 10_000,
            }
        );
    }

    #[test]
    fn descriptor_construction_fails_closed_without_evidenced_parameters() {
        let selected = negotiate_video_owned(
            &[Codec::H264],
            StationMediaCapabilities::new(
                Vec::new(),
                vec![capability(
                    Codec::H264,
                    ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
                    1,
                )],
            ),
            &[PbxVideoFormat::H264],
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
        )
        .unwrap();

        assert_eq!(
            selected.multimedia_payload(PbxVideoFormat::H264.payload_type().unwrap()),
            Err(VideoDescriptorError::MissingCodecParameters {
                codec: Codec::H264,
                required: 6,
                actual: 2,
            })
        );
        assert_eq!(PbxVideoFormat::H265.payload_type(), None);
    }
}
