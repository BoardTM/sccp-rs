//! Backend-neutral mapping and negotiation for station and PBX media formats.

mod audio;
mod video;

pub use audio::{
    AudioFormatError, AudioNegotiationError, NegotiatedAudio, PbxAudioFormat, negotiate_audio,
    pbx_audio_format, pbx_audio_formats_from_mask, unsupported_audio_reason,
};
pub use video::{
    DecodedPbxVideoFormats, NegotiatedVideo, OwnedNegotiatedVideo, PbxVideoFormat,
    VideoDescriptorError, VideoFormatError, VideoNegotiationError, decode_pbx_video_formats,
    negotiate_video, negotiate_video_owned, pbx_video_format, pbx_video_formats_from_mask,
    unsupported_video_reason,
};
