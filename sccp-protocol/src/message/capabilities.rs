//! Structured station capability updates.
//!
//! [`CapabilityUpdate`] decodes the fixed-capacity capability tables while
//! retaining the original payload for byte-lossless re-encoding. Inspect its
//! audio, video, data, picture, and conference-resource views through the
//! accessor methods rather than depending on table offsets.

use std::sync::Arc;

use super::MediaCapability;
use super::values::{Codec, EncryptionCapability, IpAddressType, ReceiveTransmit};
use super::wire::CodecError;

const MAX_AUDIO_CAPABILITIES: usize = 18;
const MAX_VIDEO_CAPABILITIES: usize = 10;
const MAX_DATA_CAPABILITIES: usize = 5;
const MAX_CUSTOM_PICTURES: usize = 6;
const MAX_CONFERENCE_SERVICES: usize = 4;
const MAX_SERVICE_LAYOUTS: usize = 5;
const MAX_LEVEL_PREFERENCES: usize = 4;

const CUSTOM_PICTURES_OFFSET: usize = 20;
const CUSTOM_PICTURE_SIZE: usize = 20;
const CONFERENCE_OFFSET: usize = CUSTOM_PICTURES_OFFSET + MAX_CUSTOM_PICTURES * CUSTOM_PICTURE_SIZE;
const CONFERENCE_SERVICE_SIZE: usize = 40;
const CONFERENCE_SIZE: usize = 12 + MAX_CONFERENCE_SERVICES * CONFERENCE_SERVICE_SIZE;
const AUDIO_OFFSET: usize = CONFERENCE_OFFSET + CONFERENCE_SIZE;
const AUDIO_CAPABILITY_SIZE: usize = 16;
const VIDEO_OFFSET: usize = AUDIO_OFFSET + MAX_AUDIO_CAPABILITIES * AUDIO_CAPABILITY_SIZE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Wire-layout family used by a station capability update.
pub enum CapabilityUpdateVariant {
    /// Original 1,840-byte update layout. The frame version does not select
    /// between the two message 0x0030 body sizes.
    Version1,
    /// Message 0x0030 carrying the expanded 2,000-byte video layout.
    Version1ExpandedVideo,
    /// Fixed 2,000-byte update carried by message identifier `0x0043`.
    Version2,
    /// Progressively decoded update carried by message identifier `0x0044`.
    Version3,
}

impl CapabilityUpdateVariant {
    /// Returns the message identifier that carries this layout.
    pub const fn message_id(self) -> u32 {
        match self {
            Self::Version1 | Self::Version1ExpandedVideo => 0x0030,
            Self::Version2 => 0x0043,
            Self::Version3 => 0x0044,
        }
    }

    const fn video_entry_size(self, protocol: u32) -> usize {
        match self {
            Self::Version1 => 116,
            Self::Version1ExpandedVideo | Self::Version2 => 132,
            Self::Version3 if protocol < 17 => 136,
            Self::Version3 => 140,
        }
    }

    const fn data_entry_size(self) -> usize {
        match self {
            Self::Version3 => 20,
            _ => 16,
        }
    }

    const fn codec_parameter_words(self) -> usize {
        match self {
            Self::Version1 => 2,
            _ => 6,
        }
    }

    pub(crate) const fn minimum_payload_bytes(self, protocol: u32) -> usize {
        let data_offset = VIDEO_OFFSET + MAX_VIDEO_CAPABILITIES * self.video_entry_size(protocol);
        data_offset + MAX_DATA_CAPABILITIES * self.data_entry_size()
    }

    pub(crate) const fn maximum_payload_bytes(self, protocol: u32) -> usize {
        if matches!(self, Self::Version3) {
            2_380
        } else {
            self.minimum_payload_bytes(protocol)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One station-provided custom video picture format.
pub struct CustomPictureFormat {
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// Encoded pixel aspect-ratio value.
    pub pixel_aspect_ratio: u32,
    /// Pixel-clock conversion numerator.
    pub pixel_clock_conversion: u32,
    /// Pixel-clock conversion divisor.
    pub pixel_clock_divisor: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Capacity and layouts for one conference service number.
pub struct ConferenceServiceResource {
    pub layouts: Vec<u32>,
    pub service_number: u32,
    pub max_streams: u32,
    pub max_conferences: u32,
    pub active_conference_on_registration: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Aggregate conference resources advertised by a station.
pub struct ConferenceResource {
    pub active_streams_on_registration: u32,
    /// Maximum bandwidth in the protocol's rate units.
    pub max_bandwidth: u32,
    pub services: Vec<ConferenceServiceResource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One quality/rate preference within a video codec capability.
pub struct VideoLevelPreference {
    /// Preference word whose interpretation includes transmit selection.
    pub transmit_preference: u32,
    pub format: u32,
    pub max_bit_rate: u32,
    pub min_bit_rate: u32,
    pub minimum_picture_interval: u32,
    pub service_number: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One advertised video codec and its supported operating levels.
pub struct VideoCapability {
    pub codec: Codec,
    pub direction: ReceiveTransmit,
    pub level_preferences: Vec<VideoLevelPreference>,
    /// Codec-specific words. Their interpretation is selected by `codec`.
    pub codec_parameters: Vec<u32>,
    /// Optional encryption support in layouts that carry it.
    pub encryption_capability: Option<EncryptionCapability>,
    /// Optional network-address family in layouts that carry it.
    pub address_type: Option<IpAddressType>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One advertised non-audio/non-video data capability.
pub struct DataCapability {
    pub payload_capability: u32,
    pub direction: ReceiveTransmit,
    /// Capability-specific data retained as a wire word.
    pub protocol_dependent_data: u32,
    pub max_bit_rate: u32,
    /// Optional encryption support in layouts that carry it.
    pub encryption_capability: Option<EncryptionCapability>,
}

/// Application-facing audio and video capabilities for one station session.
///
/// Clones share the immutable capability tables. Protocol-only fields and the
/// preserved wire payload are deliberately excluded from this runtime view.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StationMediaCapabilities {
    audio: Arc<[MediaCapability]>,
    video: Arc<[VideoCapability]>,
}

impl StationMediaCapabilities {
    /// Builds an immutable snapshot from typed capability tables.
    pub fn new(audio: Vec<MediaCapability>, video: Vec<VideoCapability>) -> Self {
        Self {
            audio: audio.into(),
            video: video.into(),
        }
    }

    pub fn audio(&self) -> &[MediaCapability] {
        &self.audio
    }

    pub fn video(&self) -> &[VideoCapability] {
        &self.video
    }

    pub fn is_empty(&self) -> bool {
        self.audio.is_empty() && self.video.is_empty()
    }
}

impl From<Vec<MediaCapability>> for StationMediaCapabilities {
    fn from(audio: Vec<MediaCapability>) -> Self {
        Self::new(audio, Vec::new())
    }
}

/// A decoded fixed-layout capability update. The original payload is retained
/// so decoding and re-encoding is byte-lossless even for reserved fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityUpdate {
    variant: CapabilityUpdateVariant,
    rtp_payload_format: u32,
    custom_picture_formats: Vec<CustomPictureFormat>,
    conference: ConferenceResource,
    audio: Vec<MediaCapability>,
    video: Vec<VideoCapability>,
    data: Vec<DataCapability>,
    trailing_words: Vec<u32>,
    raw_payload: Vec<u8>,
}

impl CapabilityUpdate {
    /// Moves the typed media tables into an application-facing snapshot.
    ///
    /// Picture, conference, data, trailing, and raw wire fields remain codec
    /// concerns and are discarded by this projection.
    pub fn into_media_capabilities(self) -> StationMediaCapabilities {
        StationMediaCapabilities::new(self.audio, self.video)
    }

    pub const fn variant(&self) -> CapabilityUpdateVariant {
        self.variant
    }

    pub const fn rtp_payload_format(&self) -> u32 {
        self.rtp_payload_format
    }

    pub fn custom_picture_formats(&self) -> &[CustomPictureFormat] {
        &self.custom_picture_formats
    }

    pub const fn conference(&self) -> &ConferenceResource {
        &self.conference
    }

    pub fn audio(&self) -> &[MediaCapability] {
        &self.audio
    }

    pub fn video(&self) -> &[VideoCapability] {
        &self.video
    }

    pub fn data(&self) -> &[DataCapability] {
        &self.data
    }

    /// Returns complete trailing words understood structurally but not yet named.
    pub fn trailing_words(&self) -> &[u32] {
        &self.trailing_words
    }

    pub(crate) fn raw_payload(&self) -> &[u8] {
        &self.raw_payload
    }

    pub(crate) fn decode(
        variant: CapabilityUpdateVariant,
        protocol: u32,
        payload: &[u8],
    ) -> Result<Self, CodecError> {
        let message_id = variant.message_id();
        let video_size = variant.video_entry_size(protocol);
        let data_offset = VIDEO_OFFSET + MAX_VIDEO_CAPABILITIES * video_size;
        let trailing_offset = data_offset + MAX_DATA_CAPABILITIES * variant.data_entry_size();
        let required = if matches!(variant, CapabilityUpdateVariant::Version3) {
            20
        } else {
            variant.minimum_payload_bytes(protocol)
        };
        require_len(payload, required, message_id)?;
        let maximum = variant.maximum_payload_bytes(protocol);
        if payload.len() > maximum {
            return Err(CodecError::TrailingBytes {
                message_id,
                count: payload.len() - maximum,
            });
        }
        let cursor = CapabilityCursor::new(payload, message_id);

        let audio_count = bounded_wire_count(
            cursor.word(0)?,
            MAX_AUDIO_CAPABILITIES,
            "audio capabilities",
            message_id,
        )?;
        let video_count = bounded_wire_count(
            cursor.word(4)?,
            MAX_VIDEO_CAPABILITIES,
            "video capabilities",
            message_id,
        )?;
        let data_count = bounded_wire_count(
            cursor.word(8)?,
            MAX_DATA_CAPABILITIES,
            "data capabilities",
            message_id,
        )?;
        let rtp_payload_format = cursor.word(12)?;
        let picture_count = bounded_wire_count(
            cursor.word(16)?,
            MAX_CUSTOM_PICTURES,
            "custom picture formats",
            message_id,
        )?;

        require_declared_entries(
            payload,
            CUSTOM_PICTURES_OFFSET,
            picture_count,
            CUSTOM_PICTURE_SIZE,
            message_id,
        )?;
        require_declared_entries(
            payload,
            AUDIO_OFFSET,
            audio_count,
            AUDIO_CAPABILITY_SIZE,
            message_id,
        )?;
        require_declared_entries(payload, VIDEO_OFFSET, video_count, video_size, message_id)?;
        require_declared_entries(
            payload,
            data_offset,
            data_count,
            variant.data_entry_size(),
            message_id,
        )?;

        let mut custom_picture_formats = Vec::with_capacity(picture_count);
        for index in 0..picture_count {
            let offset = CUSTOM_PICTURES_OFFSET + index * CUSTOM_PICTURE_SIZE;
            custom_picture_formats.push(decode_picture_entry(&cursor, offset)?);
        }

        let service_count = match cursor.optional_word(CONFERENCE_OFFSET + 8) {
            Some(count) => bounded_wire_count(
                count,
                MAX_CONFERENCE_SERVICES,
                "conference services",
                message_id,
            )?,
            None => 0,
        };
        let mut services = Vec::with_capacity(service_count);
        require_declared_entries(
            payload,
            CONFERENCE_OFFSET + 12,
            service_count,
            CONFERENCE_SERVICE_SIZE,
            message_id,
        )?;
        for index in 0..service_count {
            let offset = CONFERENCE_OFFSET + 12 + index * CONFERENCE_SERVICE_SIZE;
            services.push(decode_conference_service_entry(&cursor, offset)?);
        }
        let conference = ConferenceResource {
            active_streams_on_registration: cursor.optional_word(CONFERENCE_OFFSET).unwrap_or(0),
            max_bandwidth: cursor.optional_word(CONFERENCE_OFFSET + 4).unwrap_or(0),
            services,
        };

        let mut audio = Vec::with_capacity(audio_count);
        for index in 0..audio_count {
            let offset = AUDIO_OFFSET + index * AUDIO_CAPABILITY_SIZE;
            audio.push(decode_audio_entry(&cursor, offset)?);
        }

        let mut video = Vec::with_capacity(video_count);
        for index in 0..video_count {
            let offset = VIDEO_OFFSET + index * video_size;
            video.push(decode_video_entry(&cursor, offset, variant, protocol)?);
        }

        let mut data = Vec::with_capacity(data_count);
        for index in 0..data_count {
            let offset = data_offset + index * variant.data_entry_size();
            data.push(decode_data_entry(&cursor, offset, variant)?);
        }

        let trailing = payload.get(trailing_offset..).unwrap_or_default();
        let trailing_words = (0..trailing.len() / 4)
            .map(|index| {
                let offset = index * 4;
                u32::from_le_bytes([
                    trailing[offset],
                    trailing[offset + 1],
                    trailing[offset + 2],
                    trailing[offset + 3],
                ])
            })
            .collect();

        Ok(Self {
            variant,
            rtp_payload_format,
            custom_picture_formats,
            conference,
            audio,
            video,
            data,
            trailing_words,
            raw_payload: payload.to_vec(),
        })
    }
}

/// Offset-aware bounded reader for the fixed capability tables. Each family
/// decoder receives this cursor instead of indexing the raw body directly.
struct CapabilityCursor<'a> {
    payload: &'a [u8],
    message_id: u32,
}

impl<'a> CapabilityCursor<'a> {
    const fn new(payload: &'a [u8], message_id: u32) -> Self {
        Self {
            payload,
            message_id,
        }
    }

    fn bytes(&self, offset: usize, length: usize) -> Result<&'a [u8], CodecError> {
        let end = offset.checked_add(length).ok_or(CodecError::Truncated {
            message_id: self.message_id,
            needed: usize::MAX,
            actual: self.payload.len(),
        })?;
        require_len(self.payload, end, self.message_id)?;
        Ok(&self.payload[offset..end])
    }

    fn word(&self, offset: usize) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(
            self.bytes(offset, 4)?
                .try_into()
                .expect("bounded capability word"),
        ))
    }

    fn optional_word(&self, offset: usize) -> Option<u32> {
        self.payload
            .get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
    }
}

fn decode_picture_entry(
    cursor: &CapabilityCursor<'_>,
    offset: usize,
) -> Result<CustomPictureFormat, CodecError> {
    Ok(CustomPictureFormat {
        width: cursor.word(offset)?,
        height: cursor.word(offset + 4)?,
        pixel_aspect_ratio: cursor.word(offset + 8)?,
        pixel_clock_conversion: cursor.word(offset + 12)?,
        pixel_clock_divisor: cursor.word(offset + 16)?,
    })
}

fn decode_conference_service_entry(
    cursor: &CapabilityCursor<'_>,
    offset: usize,
) -> Result<ConferenceServiceResource, CodecError> {
    let layout_count = bounded_wire_count(
        cursor.word(offset)?,
        MAX_SERVICE_LAYOUTS,
        "conference service layouts",
        cursor.message_id,
    )?;
    let layouts = (0..layout_count)
        .map(|layout| cursor.word(offset + 4 + layout * 4))
        .collect::<Result<_, _>>()?;
    Ok(ConferenceServiceResource {
        layouts,
        service_number: cursor.word(offset + 24)?,
        max_streams: cursor.word(offset + 28)?,
        max_conferences: cursor.word(offset + 32)?,
        active_conference_on_registration: cursor.word(offset + 36)?,
    })
}

fn decode_audio_entry(
    cursor: &CapabilityCursor<'_>,
    offset: usize,
) -> Result<MediaCapability, CodecError> {
    Ok(MediaCapability {
        codec: Codec::from(cursor.word(offset)?),
        max_packet_ms: cursor.word(offset + 4)?,
        codec_parameters: cursor
            .bytes(offset + 8, 8)?
            .try_into()
            .expect("bounded audio capability parameters"),
    })
}

fn decode_video_entry(
    cursor: &CapabilityCursor<'_>,
    offset: usize,
    variant: CapabilityUpdateVariant,
    protocol: u32,
) -> Result<VideoCapability, CodecError> {
    let level_count = bounded_wire_count(
        cursor.word(offset + 8)?,
        MAX_LEVEL_PREFERENCES,
        "video level preferences",
        cursor.message_id,
    )?;
    let level_preferences = (0..level_count)
        .map(|level| {
            let level_offset = offset + 12 + level * 24;
            Ok(VideoLevelPreference {
                transmit_preference: cursor.word(level_offset)?,
                format: cursor.word(level_offset + 4)?,
                max_bit_rate: cursor.word(level_offset + 8)?,
                min_bit_rate: cursor.word(level_offset + 12)?,
                minimum_picture_interval: cursor.word(level_offset + 16)?,
                service_number: cursor.word(level_offset + 20)?,
            })
        })
        .collect::<Result<_, CodecError>>()?;
    let parameters_offset =
        offset + 108 + usize::from(variant == CapabilityUpdateVariant::Version3) * 4;
    let codec_parameters = (0..variant.codec_parameter_words())
        .map(|parameter| cursor.word(parameters_offset + parameter * 4))
        .collect::<Result<_, _>>()?;
    Ok(VideoCapability {
        codec: Codec::from(cursor.word(offset)?),
        direction: ReceiveTransmit::from_bits_retain(cursor.word(offset + 4)?),
        level_preferences,
        codec_parameters,
        encryption_capability: (variant == CapabilityUpdateVariant::Version3)
            .then(|| cursor.word(offset + 108).map(EncryptionCapability::from))
            .transpose()?,
        address_type: (variant == CapabilityUpdateVariant::Version3 && protocol >= 17)
            .then(|| cursor.word(offset + 136).map(IpAddressType::from))
            .transpose()?,
    })
}

fn decode_data_entry(
    cursor: &CapabilityCursor<'_>,
    offset: usize,
    variant: CapabilityUpdateVariant,
) -> Result<DataCapability, CodecError> {
    Ok(DataCapability {
        payload_capability: cursor.word(offset)?,
        direction: ReceiveTransmit::from_bits_retain(cursor.word(offset + 4)?),
        protocol_dependent_data: cursor.word(offset + 8)?,
        max_bit_rate: cursor.word(offset + 12)?,
        encryption_capability: (variant == CapabilityUpdateVariant::Version3)
            .then(|| cursor.word(offset + 16).map(EncryptionCapability::from))
            .transpose()?,
    })
}

fn bounded_wire_count(
    count: u32,
    maximum: usize,
    field: &'static str,
    message_id: u32,
) -> Result<usize, CodecError> {
    let count = usize::try_from(count).map_err(|_| CodecError::InvalidValue {
        message_id,
        field,
        value: u64::from(count),
    })?;
    if count > maximum {
        Err(CodecError::CountTooLarge {
            message_id,
            field,
            count,
            maximum,
        })
    } else {
        Ok(count)
    }
}

fn require_len(payload: &[u8], needed: usize, message_id: u32) -> Result<(), CodecError> {
    if payload.len() < needed {
        Err(CodecError::Truncated {
            message_id,
            needed,
            actual: payload.len(),
        })
    } else {
        Ok(())
    }
}

fn require_declared_entries(
    payload: &[u8],
    offset: usize,
    count: usize,
    entry_size: usize,
    message_id: u32,
) -> Result<(), CodecError> {
    if count == 0 {
        Ok(())
    } else {
        require_len(payload, offset + count * entry_size, message_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ClientMessage;
    use crate::message::values::ProtocolVersion;
    use crate::message::wire::{Frame, FrameDecoder};

    fn put(payload: &mut [u8], offset: usize, value: u32) {
        payload[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn fixture(source: &str) -> Vec<u8> {
        source
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("valid fixture byte"))
            .collect()
    }

    fn assert_declared_table_boundaries(
        protocol: u32,
        count_offset: usize,
        table_offset: usize,
        entry_size: usize,
        maximum: usize,
        decoded_count: impl Fn(&CapabilityUpdate) -> usize,
    ) {
        for count in 1..=maximum {
            let needed = table_offset + count * entry_size;
            let mut truncated = vec![0; needed - 1];
            put(&mut truncated, count_offset, count as u32);
            assert!(matches!(
                CapabilityUpdate::decode(
                    CapabilityUpdateVariant::Version3,
                    protocol,
                    &truncated
                ),
                Err(CodecError::Truncated {
                    needed: actual_needed,
                    actual,
                    ..
                }) if actual_needed == needed && actual == needed - 1
            ));

            let mut complete = vec![0; needed];
            put(&mut complete, count_offset, count as u32);
            let update =
                CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, protocol, &complete)
                    .unwrap();
            assert_eq!(decoded_count(&update), count);
            assert_eq!(update.raw_payload(), complete);
        }
    }

    #[test]
    fn version_three_update_exposes_every_capability_family() {
        let mut payload = vec![0; 2_380];
        put(&mut payload, 0, 1);
        put(&mut payload, 4, 1);
        put(&mut payload, 8, 1);
        put(&mut payload, 12, 101);
        put(&mut payload, 16, 1);
        for (index, value) in [640, 480, 1, 2, 3].into_iter().enumerate() {
            put(&mut payload, CUSTOM_PICTURES_OFFSET + index * 4, value);
        }
        put(&mut payload, CONFERENCE_OFFSET, 1);
        put(&mut payload, CONFERENCE_OFFSET + 4, 2_048);
        put(&mut payload, CONFERENCE_OFFSET + 8, 1);
        put(&mut payload, CONFERENCE_OFFSET + 12, 1);
        put(&mut payload, CONFERENCE_OFFSET + 16, 7);
        put(&mut payload, CONFERENCE_OFFSET + 36, 9);
        put(&mut payload, CONFERENCE_OFFSET + 40, 2);
        put(&mut payload, CONFERENCE_OFFSET + 44, 1);
        put(&mut payload, CONFERENCE_OFFSET + 48, 0);
        put(&mut payload, AUDIO_OFFSET, Codec::Pcmu.wire_value());
        put(&mut payload, AUDIO_OFFSET + 4, 4);
        payload[AUDIO_OFFSET + 8..AUDIO_OFFSET + 16].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        put(&mut payload, VIDEO_OFFSET, Codec::H264.wire_value());
        put(&mut payload, VIDEO_OFFSET + 4, 3);
        put(&mut payload, VIDEO_OFFSET + 8, 1);
        for (index, value) in [1, 5, 4_000, 128, 2, 7].into_iter().enumerate() {
            put(&mut payload, VIDEO_OFFSET + 12 + index * 4, value);
        }
        put(&mut payload, VIDEO_OFFSET + 108, 1);
        for (index, value) in [66, 31, 120, 240, 360, 480].into_iter().enumerate() {
            put(&mut payload, VIDEO_OFFSET + 112 + index * 4, value);
        }
        put(&mut payload, VIDEO_OFFSET + 136, 2);
        let data_offset = VIDEO_OFFSET + MAX_VIDEO_CAPABILITIES * 140;
        for (index, value) in [0x120, 3, 8, 64_000, 1].into_iter().enumerate() {
            put(&mut payload, data_offset + index * 4, value);
        }
        put(
            &mut payload,
            data_offset + MAX_DATA_CAPABILITIES * 20,
            0xfeed_beef,
        );

        let update =
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 22, &payload).unwrap();
        assert_eq!(update.rtp_payload_format(), 101);
        assert_eq!(update.custom_picture_formats()[0].width, 640);
        assert_eq!(update.conference().services[0].layouts, [7]);
        assert_eq!(update.audio()[0].codec, Codec::Pcmu);
        assert_eq!(update.audio()[0].codec_parameters, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(update.video()[0].codec, Codec::H264);
        assert_eq!(
            update.video()[0].direction,
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT
        );
        assert_eq!(
            update.video()[0].encryption_capability,
            Some(EncryptionCapability::Capable)
        );
        assert_eq!(
            update.video()[0].address_type,
            Some(IpAddressType::Ipv4AndIpv6)
        );
        assert_eq!(update.data()[0].max_bit_rate, 64_000);
        assert_eq!(
            update.data()[0].encryption_capability,
            Some(EncryptionCapability::Capable)
        );
        assert_eq!(update.trailing_words()[0], 0xfeed_beef);
        assert_eq!(update.raw_payload(), payload);

        let expected_audio = update.audio().to_vec();
        let expected_video = update.video().to_vec();
        let media = update.into_media_capabilities();
        assert_eq!(media.audio(), expected_audio);
        assert_eq!(media.video(), expected_video);
    }

    #[test]
    fn version_three_accepts_every_bounded_progressive_length() {
        for size in 20..=2_380 {
            let payload = vec![0; size];
            let update =
                CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 22, &payload).unwrap();
            assert_eq!(update.raw_payload(), payload, "payload size {size}");
        }

        assert!(matches!(
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 22, &[0; 19]),
            Err(CodecError::Truncated { .. })
        ));
        assert!(matches!(
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 22, &[0; 2_381]),
            Err(CodecError::TrailingBytes { .. })
        ));
        assert!(
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 16, &[0; 2_380]).is_ok()
        );
        assert!(matches!(
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 16, &[0; 2_381]),
            Err(CodecError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn version_three_rejects_declared_tables_that_do_not_fit() {
        for (count_offset, needed) in [
            (0, AUDIO_OFFSET + AUDIO_CAPABILITY_SIZE),
            (4, VIDEO_OFFSET + 140),
            (8, VIDEO_OFFSET + MAX_VIDEO_CAPABILITIES * 140 + 20),
            (16, CUSTOM_PICTURES_OFFSET + CUSTOM_PICTURE_SIZE),
        ] {
            let mut payload = vec![0; 20];
            put(&mut payload, count_offset, 1);
            assert!(matches!(
                CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 22, &payload),
                Err(CodecError::Truncated {
                    needed: actual_needed,
                    actual: 20,
                    ..
                }) if actual_needed == needed
            ));
        }

        assert_declared_table_boundaries(
            22,
            16,
            CUSTOM_PICTURES_OFFSET,
            CUSTOM_PICTURE_SIZE,
            MAX_CUSTOM_PICTURES,
            |update| update.custom_picture_formats().len(),
        );
        assert_declared_table_boundaries(
            22,
            0,
            AUDIO_OFFSET,
            AUDIO_CAPABILITY_SIZE,
            MAX_AUDIO_CAPABILITIES,
            |update| update.audio().len(),
        );
        for (protocol, video_size) in [(16, 136), (17, 140)] {
            assert_declared_table_boundaries(
                protocol,
                4,
                VIDEO_OFFSET,
                video_size,
                MAX_VIDEO_CAPABILITIES,
                |update| update.video().len(),
            );
            assert_declared_table_boundaries(
                protocol,
                8,
                VIDEO_OFFSET + MAX_VIDEO_CAPABILITIES * video_size,
                20,
                MAX_DATA_CAPABILITIES,
                |update| update.data().len(),
            );
        }
    }

    #[test]
    fn version_three_rejects_declared_conference_services_that_do_not_fit() {
        for count in 1..=MAX_CONFERENCE_SERVICES {
            let needed = CONFERENCE_OFFSET + 12 + count * CONFERENCE_SERVICE_SIZE;
            let mut truncated = vec![0; needed - 1];
            put(&mut truncated, CONFERENCE_OFFSET + 8, count as u32);
            assert!(matches!(
                CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 22, &truncated),
                Err(CodecError::Truncated {
                    needed: actual_needed,
                    actual,
                    ..
                }) if actual_needed == needed && actual == needed - 1
            ));

            let mut complete = vec![0; needed];
            put(&mut complete, CONFERENCE_OFFSET + 8, count as u32);
            let update =
                CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 22, &complete).unwrap();
            assert_eq!(update.conference().services.len(), count);
        }
    }

    #[test]
    fn version_three_preserves_an_unstructured_suffix() {
        let trailing_offset =
            VIDEO_OFFSET + MAX_VIDEO_CAPABILITIES * 140 + MAX_DATA_CAPABILITIES * 20;
        let mut payload = vec![0; trailing_offset + 7];
        put(&mut payload, trailing_offset, 0xfeed_beef);
        payload[trailing_offset + 4..].copy_from_slice(&[0xaa, 0xbb, 0xcc]);

        let update =
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 22, &payload).unwrap();
        assert_eq!(update.trailing_words(), [0xfeed_beef]);
        assert_eq!(update.raw_payload(), payload);

        let encoded = ClientMessage::CapabilitiesUpdate(update)
            .encode(ProtocolVersion::V22)
            .unwrap();
        let frame = FrameDecoder::new().push(&encoded).unwrap().remove(0);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn version_three_video_entry_boundary_depends_on_protocol() {
        let mut before = vec![0; 2_060];
        put(&mut before, 4, 1);
        put(&mut before, VIDEO_OFFSET, Codec::H264.wire_value());
        let before =
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 16, &before).unwrap();
        assert_eq!(before.video().len(), 1);
        assert_eq!(before.video()[0].address_type, None);

        let mut from = vec![0; 2_100];
        put(&mut from, 4, 1);
        put(&mut from, VIDEO_OFFSET, Codec::H264.wire_value());
        put(&mut from, VIDEO_OFFSET + 136, 2);
        let from = CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, 17, &from).unwrap();
        assert_eq!(from.video().len(), 1);
        assert_eq!(
            from.video()[0].address_type,
            Some(IpAddressType::Ipv4AndIpv6)
        );
    }

    #[test]
    fn update_rejects_truncation_and_every_oversized_count() {
        assert!(matches!(
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version1, 3, &[0; 100]),
            Err(CodecError::Truncated { .. })
        ));

        for (offset, count) in [(0, 19), (4, 11), (8, 6), (16, 7)] {
            let mut payload = vec![0; 1_840];
            put(&mut payload, offset, count);
            assert!(matches!(
                CapabilityUpdate::decode(CapabilityUpdateVariant::Version1, 3, &payload),
                Err(CodecError::CountTooLarge { .. })
            ));
        }

        let mut services = vec![0; 1_840];
        put(&mut services, CONFERENCE_OFFSET + 8, 5);
        assert!(matches!(
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version1, 3, &services),
            Err(CodecError::CountTooLarge { .. })
        ));

        let mut layouts = vec![0; 1_840];
        put(&mut layouts, CONFERENCE_OFFSET + 8, 1);
        put(&mut layouts, CONFERENCE_OFFSET + 12, 6);
        assert!(matches!(
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version1, 3, &layouts),
            Err(CodecError::CountTooLarge { .. })
        ));

        let mut levels = vec![0; 1_840];
        put(&mut levels, 4, 1);
        put(&mut levels, VIDEO_OFFSET + 8, 5);
        assert!(matches!(
            CapabilityUpdate::decode(CapabilityUpdateVariant::Version1, 3, &levels),
            Err(CodecError::CountTooLarge { .. })
        ));
    }

    #[test]
    fn every_update_variant_round_trips_its_original_fixed_layout() {
        for (variant, protocol, size) in [
            (
                CapabilityUpdateVariant::Version1,
                ProtocolVersion::V3,
                1_840,
            ),
            (
                CapabilityUpdateVariant::Version1ExpandedVideo,
                ProtocolVersion::V16,
                2_000,
            ),
            (
                CapabilityUpdateVariant::Version2,
                ProtocolVersion::V22,
                2_000,
            ),
            (
                CapabilityUpdateVariant::Version3,
                ProtocolVersion::V22,
                2_380,
            ),
        ] {
            let payload = vec![0; size];
            let decoded = ClientMessage::decode_with_version(
                Frame::new(protocol.wire(), variant.message_id(), payload.clone()),
                protocol,
            )
            .unwrap();
            assert!(matches!(
                decoded,
                ClientMessage::CapabilitiesUpdate(ref update) if update.variant() == variant
            ));
            let encoded = decoded.encode(protocol).unwrap();
            let frame = FrameDecoder::new().push(&encoded).unwrap().remove(0);
            assert_eq!(frame.message_id, variant.message_id());
            assert_eq!(frame.payload, payload);
        }
    }

    #[test]
    fn v22_7961_legacy_body_size_overrides_the_modern_session_protocol() {
        let bytes = fixture(include_str!(
            "../../tests/fixtures/golden/update_capabilities_7961_v22_legacy.hex"
        ));
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.protocol_version, ProtocolVersion::V22.wire());
        assert_eq!(
            frame.message_id,
            CapabilityUpdateVariant::Version1.message_id()
        );
        assert_eq!(frame.payload.len(), 1_840);
        let decoded = ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap();

        assert!(matches!(
            decoded,
            ClientMessage::CapabilitiesUpdate(ref update)
                if update.variant() == CapabilityUpdateVariant::Version1
        ));
        let encoded = decoded.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&encoded).unwrap().remove(0);
        assert_eq!(frame.protocol_version, ProtocolVersion::V22.wire());
        assert_eq!(frame.payload.len(), 1_840);
        assert_eq!(encoded, bytes);
    }
}
