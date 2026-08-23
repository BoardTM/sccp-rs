//! Media addressing, codec policy, direct-media decisions, and recording.
//!
//! Audio negotiation maps every compatible Skinny codec to an Asterisk format,
//! preserves registered-station capability order, and delegates direct versus
//! translated selection to Asterisk's translator graph. RTP framing,
//! advertised address selection, NAT topology, direct-media eligibility,
//! jitter-buffer policy, announcements, and recording are typed independently.
//!
//! [`direct`] permits a peer-to-peer audio route only for connected,
//! bidirectionally open, codec-compatible endpoints in an allowed address
//! scope. NAT, forced jitter buffering, hold, parking, conference/barge,
//! recording, and announcements retain or reacquire a driver-owned media
//! anchor. Failed peer retargeting falls back to that anchor without ending the
//! call.
//!
//! Video configuration, station capabilities, and native format masks share a
//! typed negotiation table. Video RTP glue/direct media, flow control, and
//! fast-picture-update transport are not implemented, so configuring a video
//! codec does not yet create a video stream.

pub mod addressing;
pub mod codec_preference;
pub mod direct;
pub mod encryption;
pub mod formats;
pub mod recording;
