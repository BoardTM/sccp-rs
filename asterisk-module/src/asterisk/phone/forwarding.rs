//! Forwarding expiry and shared-line no-answer orchestration.

use super::{
    Access, AsteriskBackend, ForwardingOperation, ForwardingRouteReason, Instant, LogLevel,
    MutexExt as _, PbxCallId, ast_log, controller_step, execute_effects, publish_line,
};
use crate::runtime::backend::SupplementaryBackend as _;

pub async fn expire_no_answer_routes(access: &Access, now: Instant) {
    let expired = access
        .shared
        .no_answer_timers
        .lock_unpoisoned()
        .claim_expired(now);
    for timer in expired {
        let (line, claimed) = controller_step(&access.shared.controller, |controller| {
            (
                controller
                    .pbx_call(timer.call_id)
                    .map(|call| call.line.clone()),
                controller.claim_ringing_forward(timer.call_id),
            )
        });
        if !claimed {
            let _ = access
                .shared
                .no_answer_timers
                .lock_unpoisoned()
                .cancel(timer.call_id, timer.id);
            continue;
        }
        let operation = ForwardingOperation {
            call_id: timer.call_id,
            context: timer.context,
            destination: timer.destination,
            reason: ForwardingRouteReason::NoAnswer,
        };
        if let Err(error) = AsteriskBackend::new(access).forward(&operation) {
            controller_step(&access.shared.controller, |controller| {
                controller.rollback_ringing_forward(timer.call_id)
            });
            let _ = access
                .shared
                .no_answer_timers
                .lock_unpoisoned()
                .cancel(timer.call_id, timer.id);
            ast_log(
                LogLevel::Warning,
                &format!(
                    "unable to apply no-answer routing for PBX call {}: {error}",
                    timer.call_id.0
                ),
            );
            continue;
        }
        if access
            .shared
            .no_answer_timers
            .lock_unpoisoned()
            .commit(timer.call_id, timer.id)
            .is_err()
        {
            controller_step(&access.shared.controller, |controller| {
                controller.rollback_ringing_forward(timer.call_id)
            });
            continue;
        }
        let effects = controller_step(&access.shared.controller, |controller| {
            controller.complete_ringing_forward(timer.call_id)
        });
        if let Some(line) = line {
            publish_line(access, &line);
        }
        execute_effects(access, effects).await;
    }
}

pub fn cancel_no_answer_timer(access: &Access, pbx_id: PbxCallId) -> bool {
    let mut timers = access.shared.no_answer_timers.lock_unpoisoned();
    let Some(timer_id) = timers.get(pbx_id).map(|timer| timer.id) else {
        return false;
    };
    timers.cancel_pending(pbx_id, timer_id).is_ok()
}

pub fn clear_no_answer_route(access: &Access, pbx_id: PbxCallId) {
    access
        .shared
        .no_answer_plans
        .lock_unpoisoned()
        .remove(&pbx_id);
    let mut timers = access.shared.no_answer_timers.lock_unpoisoned();
    if let Some(timer_id) = timers.get(pbx_id).map(|timer| timer.id) {
        let _ = timers.cancel(pbx_id, timer_id);
    }
}
