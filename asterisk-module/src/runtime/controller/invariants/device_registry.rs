use super::super::*;

pub(super) fn error(controller: &Controller) -> Option<String> {
    for (device_id, device) in &controller.devices {
        if let Some(active_call) = device.active_call
            && controller
                .appearance_for_call(active_call)
                .is_none_or(|appearance| &appearance.device_id != device_id)
        {
            return Some(format!(
                "device {device_id} has invalid active call {active_call:?}"
            ));
        }
        if device.selected_calls.iter().any(|call_id| {
            controller
                .appearance_for_call(*call_id)
                .is_none_or(|appearance| &appearance.device_id != device_id)
        }) {
            return Some(format!("device {device_id} has an invalid selected call"));
        }
    }
    None
}
