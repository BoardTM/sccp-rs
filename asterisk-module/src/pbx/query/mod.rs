//! Typed, bounded dialplan query surfaces.
//!
//! - `SCCPDevice(device-or-current,field)` resolves a configured or registered
//!   station. Canonical fields are represented by
//!   [`device::DeviceQueryField`].
//! - `SCCPLine(line-or-current[,device-or-#N],field)` resolves logical-line
//!   fields or one appearance selected by exact device ID or deterministic
//!   one-based order. Omitting the appearance selector is valid only when the
//!   requested appearance is unambiguous; fields are represented by
//!   [`line::LineQueryField`].
//! - `SCCPChannel(target,field)` accepts `current`, `pbx:N`, `call:N`, a bare
//!   handset call ID, or an exact channel name. Fields are represented by
//!   [`channel::ChannelQueryField`].
//!
//! Current selectors require an exact channel owned by this module. Unknown,
//! malformed, ambiguous, or non-allowlisted requests fail rather than falling
//! back to another object. Results use stable appearance order. Restricted ANI
//! and RDNIS are empty, account codes expose presence only, and channel
//! variables expose a count only.

pub mod channel;
pub mod device;
pub mod line;
