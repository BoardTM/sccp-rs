//! Native Asterisk channel ownership transitions.
//!
//! A phone-originated SCCP call allocates its Asterisk channel before digit
//! collection has committed a route.  Until `ast_pbx_start` succeeds, no PBX
//! worker exists to consume a queued hangup frame, so that channel remains the
//! module's responsibility and must be hard-hung up when abandoned.

use std::sync::atomic::{AtomicU8, Ordering};

const MODULE_OWNED: u8 = 0;
const ASTERISK_OWNED: u8 = 1;
const PBX_OWNED: u8 = 2;
const HARD_HANGUP_STARTED: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OwnershipTransitionError {
    InvalidState,
    HangupStarted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PbxStartOwnership {
    TransferredFromModule,
    AlreadyCoreOwned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HangupOwnership {
    Hard,
    Queued,
    AlreadyStarted,
}

pub(super) struct NativeChannelOwnership(AtomicU8);

impl NativeChannelOwnership {
    pub(super) const fn module_owned() -> Self {
        Self(AtomicU8::new(MODULE_OWNED))
    }

    pub(super) fn handoff_to_asterisk(&self) -> Result<(), OwnershipTransitionError> {
        self.0
            .compare_exchange(
                MODULE_OWNED,
                ASTERISK_OWNED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|state| match state {
                HARD_HANGUP_STARTED => OwnershipTransitionError::HangupStarted,
                _ => OwnershipTransitionError::InvalidState,
            })
    }

    pub(super) fn begin_pbx_start(&self) -> Result<PbxStartOwnership, OwnershipTransitionError> {
        loop {
            match self.0.load(Ordering::Acquire) {
                MODULE_OWNED => {
                    if self
                        .0
                        .compare_exchange(
                            MODULE_OWNED,
                            PBX_OWNED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Ok(PbxStartOwnership::TransferredFromModule);
                    }
                }
                ASTERISK_OWNED | PBX_OWNED => {
                    return Ok(PbxStartOwnership::AlreadyCoreOwned);
                }
                HARD_HANGUP_STARTED => return Err(OwnershipTransitionError::HangupStarted),
                _ => return Err(OwnershipTransitionError::InvalidState),
            }
        }
    }

    pub(super) fn rollback_pbx_start(
        &self,
        transition: PbxStartOwnership,
    ) -> Result<(), OwnershipTransitionError> {
        if transition == PbxStartOwnership::AlreadyCoreOwned {
            return Ok(());
        }
        self.0
            .compare_exchange(PBX_OWNED, MODULE_OWNED, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|state| match state {
                HARD_HANGUP_STARTED => OwnershipTransitionError::HangupStarted,
                _ => OwnershipTransitionError::InvalidState,
            })
    }

    pub(super) fn claim_hangup(&self) -> Result<HangupOwnership, OwnershipTransitionError> {
        loop {
            match self.0.load(Ordering::Acquire) {
                MODULE_OWNED => {
                    if self
                        .0
                        .compare_exchange(
                            MODULE_OWNED,
                            HARD_HANGUP_STARTED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Ok(HangupOwnership::Hard);
                    }
                }
                ASTERISK_OWNED | PBX_OWNED => return Ok(HangupOwnership::Queued),
                HARD_HANGUP_STARTED => return Ok(HangupOwnership::AlreadyStarted),
                _ => return Err(OwnershipTransitionError::InvalidState),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_collecting_channel_claims_exactly_one_hard_hangup() {
        let ownership = NativeChannelOwnership::module_owned();
        assert_eq!(ownership.claim_hangup(), Ok(HangupOwnership::Hard));
        assert_eq!(
            ownership.claim_hangup(),
            Ok(HangupOwnership::AlreadyStarted)
        );
    }

    #[test]
    fn requester_handoff_uses_queued_hangup() {
        let ownership = NativeChannelOwnership::module_owned();
        assert_eq!(ownership.handoff_to_asterisk(), Ok(()));
        assert_eq!(ownership.claim_hangup(), Ok(HangupOwnership::Queued));
    }

    #[test]
    fn successful_pbx_handoff_uses_queued_hangup() {
        let ownership = NativeChannelOwnership::module_owned();
        assert_eq!(
            ownership.begin_pbx_start(),
            Ok(PbxStartOwnership::TransferredFromModule)
        );
        assert_eq!(ownership.claim_hangup(), Ok(HangupOwnership::Queued));
    }

    #[test]
    fn failed_pbx_start_restores_hard_hangup_responsibility() {
        let ownership = NativeChannelOwnership::module_owned();
        let transition = ownership.begin_pbx_start().unwrap();
        assert_eq!(ownership.rollback_pbx_start(transition), Ok(()));
        assert_eq!(ownership.claim_hangup(), Ok(HangupOwnership::Hard));
    }
}
