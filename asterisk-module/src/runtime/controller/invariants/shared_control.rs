use super::super::*;

pub(super) fn error(controller: &Controller) -> Option<String> {
    for (target_id, claim) in &controller.shared_control_claims {
        let Some(target) = controller.call_registry.pbx.get(target_id) else {
            return Some(format!("shared-control claim has no target {target_id:?}"));
        };
        match claim {
            SharedControlClaim::Steal(winner) => {
                if target.active_appearance != Some(*winner) {
                    return Some(format!(
                        "steal claim for {target_id:?} does not match its active appearance"
                    ));
                }
            }
            SharedControlClaim::Barge(bridge_id) => {
                if controller
                    .barges
                    .groups
                    .get(target_id)
                    .is_none_or(|group| group.bridge_id != *bridge_id)
                {
                    return Some(format!(
                        "barge claim for {target_id:?} has no matching group"
                    ));
                }
            }
        }
    }
    for (target_id, group) in &controller.barges.groups {
        if group.members.is_empty() {
            return Some(format!("barge group for {target_id:?} is empty"));
        }
        for call_id in &group.members {
            if controller
                .barges
                .by_handset
                .get(call_id)
                .is_none_or(|session| {
                    session.target_call_id != *target_id
                        || session.bridge_id != group.bridge_id
                        || session.mode != group.mode
                })
            {
                return Some(format!(
                    "barge group for {target_id:?} has dangling member {call_id:?}"
                ));
            }
        }
    }
    for (call_id, session) in &controller.barges.by_handset {
        if session.handset_call_id != *call_id
            || controller.barges.by_pbx.get(&session.barger_call_id) != Some(call_id)
            || controller
                .call_registry
                .pbx
                .get(&session.barger_call_id)
                .is_none_or(|call| {
                    !call.appearance_ids.is_empty() || call.state != CallState::Connected
                })
            || controller
                .appearance_for_call(*call_id)
                .is_none_or(|appearance| {
                    appearance.pbx_id != session.target_call_id
                        || appearance.state != CallState::Barged
                })
        {
            return Some(format!("barge session for {call_id:?} is inconsistent"));
        }
    }
    for (pbx_id, call_id) in &controller.barges.by_pbx {
        if controller
            .barges
            .by_handset
            .get(call_id)
            .is_none_or(|session| session.barger_call_id != *pbx_id)
        {
            return Some(format!("barge PBX index {pbx_id:?} is dangling"));
        }
    }
    None
}
