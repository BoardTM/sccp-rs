//! Typed do-not-disturb transitions shared by handset and management controls.

use crate::config::DndButtonMode;
use crate::runtime::controller::DndMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DndMutation {
    Set(DndMode),
    Toggle(DndButtonMode),
}

impl DndMutation {
    pub const fn apply(self, current: DndMode) -> DndMode {
        match self {
            Self::Set(mode) => mode,
            Self::Toggle(DndButtonMode::Silent) => {
                if matches!(current, DndMode::Silent) {
                    DndMode::Off
                } else {
                    DndMode::Silent
                }
            }
            Self::Toggle(DndButtonMode::Reject) => {
                if matches!(current, DndMode::Reject) {
                    DndMode::Off
                } else {
                    DndMode::Reject
                }
            }
            Self::Toggle(DndButtonMode::Cycle) => match current {
                DndMode::Off => DndMode::Reject,
                DndMode::Reject => DndMode::Silent,
                DndMode::Silent => DndMode::Off,
            },
        }
    }
}

pub const fn default_button_mode(default: DndMode) -> DndButtonMode {
    match default {
        DndMode::Off => DndButtonMode::Cycle,
        DndMode::Silent => DndButtonMode::Silent,
        DndMode::Reject => DndButtonMode::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_management_updates_are_idempotent() {
        for mode in [DndMode::Off, DndMode::Silent, DndMode::Reject] {
            assert_eq!(DndMutation::Set(mode).apply(mode), mode);
            assert_eq!(DndMutation::Set(mode).apply(DndMode::Off), mode);
        }
    }

    #[test]
    fn cycle_and_fixed_buttons_have_exact_transitions() {
        assert_eq!(
            [DndMode::Off, DndMode::Reject, DndMode::Silent]
                .map(|mode| DndMutation::Toggle(DndButtonMode::Cycle).apply(mode)),
            [DndMode::Reject, DndMode::Silent, DndMode::Off]
        );
        assert_eq!(
            [DndMode::Off, DndMode::Silent, DndMode::Reject]
                .map(|mode| DndMutation::Toggle(DndButtonMode::Silent).apply(mode)),
            [DndMode::Silent, DndMode::Off, DndMode::Silent]
        );
        assert_eq!(
            [DndMode::Off, DndMode::Silent, DndMode::Reject]
                .map(|mode| DndMutation::Toggle(DndButtonMode::Reject).apply(mode)),
            [DndMode::Reject, DndMode::Reject, DndMode::Off]
        );
    }

    #[test]
    fn configured_default_selects_fixed_or_cycle_soft_key_behavior() {
        assert_eq!(default_button_mode(DndMode::Off), DndButtonMode::Cycle);
        assert_eq!(default_button_mode(DndMode::Silent), DndButtonMode::Silent);
        assert_eq!(default_button_mode(DndMode::Reject), DndButtonMode::Reject);
    }
}
