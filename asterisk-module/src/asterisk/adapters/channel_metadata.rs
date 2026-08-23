//! Typed composition adapter for Asterisk channel metadata.

use crate::call::metadata::CallMetadata;
use crate::pbx::channel_metadata::{
    ChannelMetadataError, dispatch_apply, dispatch_inherit, dispatch_snapshot,
};
use crate::pbx::party::AsteriskChannel;

use crate::asterisk::raw::channel::NativeChannelMetadataAdapter;

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskChannelMetadata;

impl AsteriskChannelMetadata {
    pub const fn new() -> Self {
        Self
    }

    pub fn snapshot(
        &self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<CallMetadata, ChannelMetadataError> {
        dispatch_snapshot(&NativeChannelMetadataAdapter, channel)
    }

    pub fn apply(
        &self,
        channel: &AsteriskChannel<'_>,
        metadata: &CallMetadata,
    ) -> Result<(), ChannelMetadataError> {
        dispatch_apply(&NativeChannelMetadataAdapter, channel, metadata)
    }

    pub fn inherit_variables(
        &self,
        parent: &AsteriskChannel<'_>,
        child: &AsteriskChannel<'_>,
    ) -> Result<(), ChannelMetadataError> {
        dispatch_inherit(&NativeChannelMetadataAdapter, parent, child)
    }
}
