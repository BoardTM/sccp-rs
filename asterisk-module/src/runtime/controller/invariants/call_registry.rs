use super::super::*;

pub(super) fn error(controller: &Controller) -> Option<String> {
    for (pbx_id, call) in &controller.call_registry.pbx {
        if pbx_id != &call.id {
            return Some(format!("PBX call key {pbx_id:?} does not match record"));
        }
        if call.pending_pickup.is_some() != (call.state == CallState::PickupCollecting) {
            return Some(format!(
                "PBX call {pbx_id:?} has inconsistent directed-pickup state"
            ));
        }
        let mut unique = HashSet::new();
        for appearance_id in &call.appearance_ids {
            if !unique.insert(*appearance_id) {
                return Some(format!("PBX call {pbx_id:?} repeats an appearance"));
            }
            if controller
                .call_registry
                .appearances
                .get(appearance_id)
                .is_none_or(|appearance| appearance.pbx_id != *pbx_id)
            {
                return Some(format!("PBX call {pbx_id:?} has a dangling appearance"));
            }
        }
        if let Some(active) = call.active_appearance {
            let Some(appearance) = controller.call_registry.appearances.get(&active) else {
                return Some(format!(
                    "PBX call {pbx_id:?} has a dangling active appearance"
                ));
            };
            if appearance.pbx_id != *pbx_id
                || !matches!(
                    appearance.state,
                    CallState::Collecting
                        | CallState::PickupCollecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::Parking
                        | CallState::Retrieving
                        | CallState::Held
                        | CallState::TransferCollecting
                )
            {
                return Some(format!(
                    "PBX call {pbx_id:?} has an invalid active appearance"
                ));
            }
        }
        let active_count = call
            .appearance_ids
            .iter()
            .filter_map(|id| controller.call_registry.appearances.get(id))
            .filter(|appearance| {
                matches!(
                    appearance.state,
                    CallState::Collecting
                        | CallState::PickupCollecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::Parking
                        | CallState::Retrieving
                        | CallState::Held
                        | CallState::TransferCollecting
                )
            })
            .count();
        if active_count > usize::from(call.active_appearance.is_some()) {
            return Some(format!(
                "PBX call {pbx_id:?} has multiple active appearances"
            ));
        }
    }
    for (appearance_id, appearance) in &controller.call_registry.appearances {
        if appearance_id != &appearance.id {
            return Some(format!(
                "appearance key {appearance_id:?} does not match record"
            ));
        }
        if controller.call_registry.by_sccp.get(&appearance.sccp_id) != Some(appearance_id) {
            return Some(format!(
                "call {:?} does not index appearance {appearance_id:?}",
                appearance.sccp_id
            ));
        }
        if !controller
            .call_registry
            .by_device
            .get(&appearance.device_id)
            .is_some_and(|ids| ids.contains(appearance_id))
        {
            return Some(format!(
                "device {} does not index appearance {appearance_id:?}",
                appearance.device_id
            ));
        }
        let Some(call) = controller.call_registry.pbx.get(&appearance.pbx_id) else {
            return Some(format!("appearance {appearance_id:?} has no PBX call"));
        };
        if !call.appearance_ids.contains(appearance_id) {
            return Some(format!(
                "appearance {appearance_id:?} is not owned by its PBX call"
            ));
        }
    }
    for (call_id, appearance_id) in &controller.call_registry.by_sccp {
        if controller
            .call_registry
            .appearances
            .get(appearance_id)
            .is_none_or(|appearance| appearance.sccp_id != *call_id)
        {
            return Some(format!("call {call_id:?} has a dangling index"));
        }
    }
    for (device_id, appearance_ids) in &controller.call_registry.by_device {
        for appearance_id in appearance_ids {
            if controller
                .call_registry
                .appearances
                .get(appearance_id)
                .is_none_or(|appearance| appearance.device_id != *device_id)
            {
                return Some(format!("device {device_id} has a dangling appearance"));
            }
        }
    }
    None
}
