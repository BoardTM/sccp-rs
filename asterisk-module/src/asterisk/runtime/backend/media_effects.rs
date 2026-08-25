//! media effects backend-effect translation.

use super::{
    AsteriskBackend, AsteriskBackendError, Codec, LocalEncryptionCapabilities, MediaBackend,
    MediaEndpoint, NonNull, PbxCallId, local_media_endpoint, native_channel, with_channel,
};

impl MediaBackend for AsteriskBackend<'_> {
    fn audio_encryption_capabilities(&self) -> LocalEncryptionCapabilities {
        // The adapter must not report a profile until it can install and own
        // both directions of the protected stream.
        LocalEncryptionCapabilities::default()
    }

    fn configure_media(
        &self,
        call_id: PbxCallId,
        remote: MediaEndpoint,
        codec: Codec,
    ) -> Result<MediaEndpoint, Self::Error> {
        let endpoint = std::net::SocketAddr::new(remote.address, remote.rtp_port);
        let result = with_channel(self.access, call_id, |channel| {
            NonNull::new(channel).ok_or(()).and_then(|channel| unsafe {
                native_channel::set_remote_media(channel, endpoint).map_err(|_| ())
            })
        });
        Self::typed_operation_result("set remote media", call_id, result)?;
        local_media_endpoint(self.access, call_id, codec).ok_or_else(|| {
            AsteriskBackendError::Failed {
                operation: "get local media",
                calls: call_id.0.to_string(),
            }
        })
    }
}
