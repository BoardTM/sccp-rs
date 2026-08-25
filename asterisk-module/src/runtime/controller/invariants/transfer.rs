use super::super::*;

pub(super) fn error(controller: &Controller) -> Option<String> {
    for transaction in controller.transfers.transactions() {
        if !controller.devices.contains_key(&transaction.device_id)
            || controller
                .appearance_for_call(transaction.source.handset_call_id)
                .is_none_or(|appearance| {
                    appearance.pbx_id != transaction.source.pbx_call_id
                        || appearance.device_id != transaction.device_id
                })
            || transaction.consultation.is_none_or(|consultation| {
                controller
                    .appearance_for_call(consultation.handset_call_id)
                    .is_none_or(|appearance| {
                        appearance.pbx_id != consultation.pbx_call_id
                            || appearance.device_id != transaction.device_id
                    })
            })
        {
            return Some(format!(
                "transfer {:?} has inconsistent call identities",
                transaction.id
            ));
        }
        if transaction.mode == TransferMode::Consultation
            && transaction.phase != TransferPhase::Completing
            && (controller
                .call_registry
                .pbx
                .get(&transaction.source.pbx_call_id)
                .is_none_or(|call| call.state != CallState::Held)
                || transaction.consultation.is_none_or(|consultation| {
                    controller
                        .call_registry
                        .pbx
                        .get(&consultation.pbx_call_id)
                        .is_none_or(|call| {
                            !matches!(
                                call.state,
                                CallState::Collecting
                                    | CallState::TransferCollecting
                                    | CallState::Calling
                                    | CallState::Ringing
                                    | CallState::Connected
                            )
                        })
                }))
        {
            return Some(format!(
                "transfer {:?} has inconsistent call states",
                transaction.id
            ));
        }
    }
    None
}
