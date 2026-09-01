//! Audio format mapping and capability negotiation.

use sccp_protocol::message::MediaCapability;
use sccp_protocol::{
    Codec, CodecKind, DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET, DEFAULT_AUDIO_PACKET_MS,
};
use thiserror::Error;

/// Audio formats currently exposed by the channel backend.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PbxAudioFormat {
    G711Ulaw,
    G711Alaw,
    G722,
    G723,
    G729,
    G726Aal2,
    Gsm,
    Slin16,
    Ilbc,
    Siren7,
    Opus,
}

impl PbxAudioFormat {
    pub const ALL: [Self; 11] = [
        Self::G711Ulaw,
        Self::G711Alaw,
        Self::G722,
        Self::G723,
        Self::G729,
        Self::G726Aal2,
        Self::Gsm,
        Self::Slin16,
        Self::Ilbc,
        Self::Siren7,
        Self::Opus,
    ];

    const fn capability_bit(self) -> u32 {
        match self {
            Self::G711Ulaw => 1 << 0,
            Self::G711Alaw => 1 << 1,
            Self::G722 => 1 << 2,
            Self::G723 => 1 << 3,
            Self::G729 => 1 << 4,
            Self::G726Aal2 => 1 << 5,
            Self::Gsm => 1 << 6,
            Self::Slin16 => 1 << 7,
            Self::Ilbc => 1 << 8,
            Self::Siren7 => 1 << 9,
            Self::Opus => 1 << 10,
        }
    }

    /// Station codec identifiers represented by this PBX format, in preferred
    /// order when the PBX does not distinguish their wire bit-rate variants.
    pub const fn station_codecs(self) -> &'static [Codec] {
        match self {
            Self::G711Ulaw => &[Codec::Pcmu, Codec::G711Ulaw56k],
            Self::G711Alaw => &[Codec::Pcma, Codec::G711Alaw56k],
            Self::G722 => &[Codec::G72264k, Codec::G72256k, Codec::G72248k],
            Self::G723 => &[Codec::G7231],
            Self::G729 => &[
                Codec::G729,
                Codec::G729A,
                Codec::G729B,
                Codec::G729Ab,
                Codec::G729AnnexB,
            ],
            Self::G726Aal2 => &[Codec::G726_32k],
            Self::Gsm => &[Codec::Gsm],
            Self::Slin16 => &[Codec::Wideband256k],
            Self::Ilbc => &[Codec::Ilbc],
            Self::Siren7 => &[Codec::G7221_32k],
            Self::Opus => &[Codec::Opus],
        }
    }
}

/// Convert the native channel-request capability mask into ordered PBX audio
/// formats. Unknown future bits are ignored.
pub fn pbx_audio_formats_from_mask(mask: u32) -> Vec<PbxAudioFormat> {
    PbxAudioFormat::ALL
        .into_iter()
        .filter(|format| mask & format.capability_bit() != 0)
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AudioFormatError {
    #[error("{0:?} is not an audio codec")]
    NotAudio(Codec),
    #[error("audio codec {0:?} has no PBX format mapping")]
    Unsupported(Codec),
}

/// Explain why a Skinny audio capability cannot be represented by Asterisk
/// 22. Keeping this table beside the positive mapping makes catalog additions
/// fail tests until the native boundary has an explicit decision.
pub const fn unsupported_audio_reason(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::G728 => Some("Asterisk has no G.728 format"),
        Codec::Is11172 | Codec::Is13818 => Some("Asterisk has no matching ISO audio format"),
        Codec::GsmFullRate | Codec::GsmHalfRate | Codec::GsmEnhancedFullRate => {
            Some("cellular GSM variants are not Asterisk GSM 06.10")
        }
        Codec::G7221_24k => Some("Asterisk Siren7 represents only G.722.1 at 32 kbit/s"),
        Codec::Aac
        | Codec::Mp4aLatm128
        | Codec::Mp4aLatm64
        | Codec::Mp4aLatm56
        | Codec::Mp4aLatm48
        | Codec::Mp4aLatm32
        | Codec::Mp4aLatm24
        | Codec::Mp4aLatm => Some("Asterisk has no matching AAC/LATM audio format"),
        Codec::ActiveVoice => Some("Asterisk has no ActiveVoice format"),
        Codec::G726_24k | Codec::G726_16k => Some("Asterisk exposes only G.726 at 32 kbit/s"),
        Codec::Isac => Some("Asterisk has no iSAC format"),
        Codec::Amr | Codec::AmrWb => Some("Asterisk has no AMR audio format"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedAudio {
    pub codec: Codec,
    pub pbx_format: PbxAudioFormat,
    pub packet_ms: u32,
    pub max_frames_per_packet: u32,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AudioNegotiationError {
    #[error("the configured preference list has no supported audio codec")]
    NoRepresentableConfiguredCodec,
    #[error("the station and PBX have no mutually supported audio codec")]
    NoMutualCodec,
}

/// Resolve a station codec without silently substituting a different format.
pub const fn pbx_audio_format(codec: Codec) -> Result<PbxAudioFormat, AudioFormatError> {
    match codec {
        Codec::Pcmu | Codec::G711Ulaw56k => Ok(PbxAudioFormat::G711Ulaw),
        Codec::Pcma | Codec::G711Alaw56k => Ok(PbxAudioFormat::G711Alaw),
        Codec::G72264k | Codec::G72256k | Codec::G72248k => Ok(PbxAudioFormat::G722),
        Codec::G7231 => Ok(PbxAudioFormat::G723),
        Codec::G729 | Codec::G729A | Codec::G729B | Codec::G729Ab | Codec::G729AnnexB => {
            Ok(PbxAudioFormat::G729)
        }
        Codec::G726_32k => Ok(PbxAudioFormat::G726Aal2),
        Codec::Gsm => Ok(PbxAudioFormat::Gsm),
        Codec::Wideband256k => Ok(PbxAudioFormat::Slin16),
        Codec::Ilbc => Ok(PbxAudioFormat::Ilbc),
        Codec::G7221_32k => Ok(PbxAudioFormat::Siren7),
        Codec::Opus => Ok(PbxAudioFormat::Opus),
        codec if matches!(codec.kind(), CodecKind::Audio) => {
            Err(AudioFormatError::Unsupported(codec))
        }
        codec => Err(AudioFormatError::NotAudio(codec)),
    }
}

/// Select the first configured codec represented by both endpoints.
///
/// An absent station capability list means capability discovery has not
/// completed yet; in that case the configured and PBX preferences alone are
/// used. A present list is matched by exact station codec identifier because
/// identifiers sharing a PBX format can still have different wire behavior.
pub fn negotiate_audio(
    configured: &[Codec],
    station: Option<&[MediaCapability]>,
    pbx: &[PbxAudioFormat],
) -> Result<NegotiatedAudio, AudioNegotiationError> {
    let mut has_representable = false;
    let ordered = station
        .map(|capabilities| {
            capabilities
                .iter()
                .map(|capability| capability.codec)
                .filter(|codec| configured.contains(codec))
                .fold(Vec::new(), |mut codecs, codec| {
                    if !codecs.contains(&codec) {
                        codecs.push(codec);
                    }
                    codecs
                })
        })
        .unwrap_or_else(|| configured.to_vec());
    for codec in ordered {
        let Ok(pbx_format) = pbx_audio_format(codec) else {
            continue;
        };
        has_representable = true;
        if !pbx.contains(&pbx_format) {
            continue;
        }
        let station_max_packet_ms = station.and_then(|capabilities| {
            capabilities
                .iter()
                .find(|capability| capability.codec == codec)
                .map(|capability| capability.max_packet_ms)
        });
        if station.is_some()
            && station_max_packet_ms.is_none_or(|maximum| maximum < DEFAULT_AUDIO_PACKET_MS)
        {
            continue;
        }
        return Ok(NegotiatedAudio {
            codec,
            pbx_format,
            packet_ms: DEFAULT_AUDIO_PACKET_MS,
            max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
        });
    }
    if has_representable {
        Err(AudioNegotiationError::NoMutualCodec)
    } else {
        Err(AudioNegotiationError::NoRepresentableConfiguredCodec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_audio_codec_exposed_by_the_channel_backend() {
        for (codec, expected) in [
            (Codec::Pcmu, PbxAudioFormat::G711Ulaw),
            (Codec::G711Ulaw56k, PbxAudioFormat::G711Ulaw),
            (Codec::Pcma, PbxAudioFormat::G711Alaw),
            (Codec::G711Alaw56k, PbxAudioFormat::G711Alaw),
            (Codec::G72264k, PbxAudioFormat::G722),
            (Codec::G72256k, PbxAudioFormat::G722),
            (Codec::G72248k, PbxAudioFormat::G722),
            (Codec::G7231, PbxAudioFormat::G723),
            (Codec::G729, PbxAudioFormat::G729),
            (Codec::G729A, PbxAudioFormat::G729),
            (Codec::G729B, PbxAudioFormat::G729),
            (Codec::G729Ab, PbxAudioFormat::G729),
            (Codec::G729AnnexB, PbxAudioFormat::G729),
            (Codec::G726_32k, PbxAudioFormat::G726Aal2),
            (Codec::Gsm, PbxAudioFormat::Gsm),
            (Codec::Wideband256k, PbxAudioFormat::Slin16),
            (Codec::Ilbc, PbxAudioFormat::Ilbc),
            (Codec::G7221_32k, PbxAudioFormat::Siren7),
            (Codec::Opus, PbxAudioFormat::Opus),
        ] {
            assert_eq!(pbx_audio_format(codec), Ok(expected));
            assert!(expected.station_codecs().contains(&codec));
        }
    }

    #[test]
    fn every_known_audio_codec_is_mapped_or_has_an_explicit_reason() {
        for codec in Codec::ALL_KNOWN.iter().copied() {
            if codec.kind() != CodecKind::Audio {
                continue;
            }
            assert!(
                pbx_audio_format(codec).is_ok() ^ unsupported_audio_reason(codec).is_some(),
                "{codec:?} must have exactly one adapter mapping decision"
            );
        }
    }

    #[test]
    fn station_capabilities_map_without_losing_wire_variants() {
        let station = [
            Codec::Wideband256k,
            Codec::Pcmu,
            Codec::Pcma,
            Codec::G729B,
            Codec::G729Ab,
            Codec::G729,
            Codec::G729A,
            Codec::DtmfOutOfBandRfc2833,
        ];
        let mapped = station
            .into_iter()
            .filter_map(|codec| pbx_audio_format(codec).ok().map(|format| (codec, format)))
            .collect::<Vec<_>>();
        assert_eq!(
            mapped,
            [
                (Codec::Wideband256k, PbxAudioFormat::Slin16),
                (Codec::Pcmu, PbxAudioFormat::G711Ulaw),
                (Codec::Pcma, PbxAudioFormat::G711Alaw),
                (Codec::G729B, PbxAudioFormat::G729),
                (Codec::G729Ab, PbxAudioFormat::G729),
                (Codec::G729, PbxAudioFormat::G729),
                (Codec::G729A, PbxAudioFormat::G729),
            ]
        );
    }

    #[test]
    fn never_substitutes_an_unsupported_audio_codec() {
        assert_eq!(
            pbx_audio_format(Codec::G7221_24k),
            Err(AudioFormatError::Unsupported(Codec::G7221_24k))
        );
        assert_eq!(
            pbx_audio_format(Codec::G726_24k),
            Err(AudioFormatError::Unsupported(Codec::G726_24k))
        );
    }

    #[test]
    fn distinguishes_non_audio_and_unknown_identifiers() {
        assert_eq!(
            pbx_audio_format(Codec::H264),
            Err(AudioFormatError::NotAudio(Codec::H264))
        );
        assert_eq!(
            pbx_audio_format(Codec::Unknown(0xfeed)),
            Err(AudioFormatError::NotAudio(Codec::Unknown(0xfeed)))
        );
    }

    #[test]
    fn negotiation_preserves_station_preference_order_within_policy() {
        let station = [
            MediaCapability {
                codec: Codec::Pcma,
                max_packet_ms: 40,
                codec_parameters: [0; 8],
            },
            MediaCapability {
                codec: Codec::Pcmu,
                max_packet_ms: 40,
                codec_parameters: [0; 8],
            },
        ];
        assert_eq!(
            negotiate_audio(
                &[Codec::Pcmu, Codec::Pcma],
                Some(&station),
                &PbxAudioFormat::ALL,
            ),
            Ok(NegotiatedAudio {
                codec: Codec::Pcma,
                pbx_format: PbxAudioFormat::G711Alaw,
                packet_ms: 20,
                max_frames_per_packet: 0,
            })
        );
    }

    #[test]
    fn negotiation_requires_exact_station_codec_and_pbx_format() {
        let station = [MediaCapability {
            codec: Codec::G711Ulaw56k,
            max_packet_ms: 40,
            codec_parameters: [0; 8],
        }];
        assert_eq!(
            negotiate_audio(
                &[Codec::Pcmu, Codec::G711Ulaw56k],
                Some(&station),
                &[PbxAudioFormat::G711Ulaw],
            ),
            Ok(NegotiatedAudio {
                codec: Codec::G711Ulaw56k,
                pbx_format: PbxAudioFormat::G711Ulaw,
                packet_ms: 20,
                max_frames_per_packet: 0,
            })
        );
        assert_eq!(
            negotiate_audio(
                &[Codec::G711Ulaw56k],
                Some(&station),
                &[PbxAudioFormat::G711Alaw],
            ),
            Err(AudioNegotiationError::NoMutualCodec)
        );
    }

    #[test]
    fn negotiation_distinguishes_unrepresentable_configuration() {
        assert_eq!(
            negotiate_audio(&[Codec::Isac, Codec::H264], None, &PbxAudioFormat::ALL),
            Err(AudioNegotiationError::NoRepresentableConfiguredCodec)
        );
    }

    #[test]
    fn negotiation_keeps_packet_duration_out_of_the_transmit_frame_count() {
        let station = [MediaCapability {
            codec: Codec::Pcmu,
            max_packet_ms: 40,
            codec_parameters: [0; 8],
        }];
        assert_eq!(
            negotiate_audio(&[Codec::Pcmu], Some(&station), &PbxAudioFormat::ALL),
            Ok(NegotiatedAudio {
                codec: Codec::Pcmu,
                pbx_format: PbxAudioFormat::G711Ulaw,
                packet_ms: 20,
                max_frames_per_packet: 0,
            })
        );
    }

    #[test]
    fn negotiation_rejects_an_invalid_zero_packet_duration_capability() {
        let station = [MediaCapability {
            codec: Codec::Pcmu,
            max_packet_ms: 0,
            codec_parameters: [0; 8],
        }];
        assert_eq!(
            negotiate_audio(&[Codec::Pcmu], Some(&station), &PbxAudioFormat::ALL,),
            Err(AudioNegotiationError::NoMutualCodec)
        );
    }

    #[test]
    fn native_capability_masks_preserve_known_format_order() {
        assert_eq!(
            pbx_audio_formats_from_mask((1 << 2) | (1 << 0) | (1 << 9)),
            [
                PbxAudioFormat::G711Ulaw,
                PbxAudioFormat::G722,
                PbxAudioFormat::Siren7,
            ]
        );
        assert!(pbx_audio_formats_from_mask(0).is_empty());
        assert_eq!(
            pbx_audio_formats_from_mask((1 << PbxAudioFormat::ALL.len()) - 1),
            PbxAudioFormat::ALL
        );
    }
}
