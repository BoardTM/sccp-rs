use super::super::*;

pub(super) fn error(controller: &Controller) -> Option<String> {
    if let Some(pbx_id) = controller
        .redirect_claims
        .iter()
        .find(|pbx_id| !controller.call_registry.pbx.contains_key(pbx_id))
    {
        return Some(format!("redirect claim for missing PBX call {pbx_id:?}"));
    }
    for transaction in controller.voicemail.transactions() {
        let appearance_is_owned = controller
            .appearance_for_call(transaction.handset_call_id)
            .is_some_and(|appearance| {
                appearance.pbx_id == transaction.pbx_call_id
                    && appearance.device_id == transaction.device_id
            });
        let owner_disconnected = !controller.devices.contains_key(&transaction.device_id);
        if !controller
            .redirect_claims
            .contains(&transaction.pbx_call_id)
            || !controller
                .call_registry
                .pbx
                .contains_key(&transaction.pbx_call_id)
            || (!appearance_is_owned && !owner_disconnected)
        {
            return Some(format!(
                "voicemail transaction {:?} has inconsistent ownership",
                transaction.id
            ));
        }
    }
    None
}
