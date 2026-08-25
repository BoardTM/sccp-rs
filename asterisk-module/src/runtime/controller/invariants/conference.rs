use super::super::*;

pub(super) fn error(controller: &Controller) -> Option<String> {
    for (consultation, session) in &controller.conferences.by_consultation {
        if consultation != &session.consultation_handset_call_id
            || session.participants.iter().len() < 2
            || session.participants.moderator_count() == 0
            || session
                .participants
                .by_pbx(session.original_call_id)
                .is_none_or(|participant| {
                    participant.pbx_call_id != session.original_call_id
                        || participant.handset_call_id != session.original_handset_call_id
                })
            || session.participants.iter().any(|participant| {
                controller.conferences.by_pbx.get(&participant.pbx_call_id) != Some(consultation)
            })
            || session.pending_invite.as_ref().is_some_and(|invite| {
                controller
                    .conferences
                    .by_pbx
                    .get(&invite.participant.pbx_call_id)
                    != Some(consultation)
            })
        {
            return Some(format!(
                "conference {:?} has inconsistent indexes",
                session.id
            ));
        }
        let Some(original) = controller.call_registry.pbx.get(&session.original_call_id) else {
            return Some(format!("conference {:?} has no original call", session.id));
        };
        let Some(consultation_call) = controller
            .call_registry
            .pbx
            .get(&session.consultation_call_id)
        else {
            return Some(format!(
                "conference {:?} has no consultation call",
                session.id
            ));
        };
        if controller
            .appearance_for_call(session.original_handset_call_id)
            .is_none_or(|appearance| {
                appearance.pbx_id != session.original_call_id
                    || appearance.device_id != session.device_id
            })
            || controller
                .appearance_for_call(session.consultation_handset_call_id)
                .is_none_or(|appearance| {
                    appearance.pbx_id != session.consultation_call_id
                        || appearance.device_id != session.device_id
                })
        {
            return Some(format!(
                "conference {:?} has inconsistent handset appearances",
                session.id
            ));
        }
        if session.participants.iter().any(|participant| {
            !controller
                .call_registry
                .pbx
                .contains_key(&participant.pbx_call_id)
                || controller
                    .appearance_for_call(participant.handset_call_id)
                    .is_none_or(|appearance| {
                        appearance.pbx_id != participant.pbx_call_id
                            || appearance.device_id != participant.device_id
                    })
        }) {
            return Some(format!(
                "conference {:?} has an inconsistent participant",
                session.id
            ));
        }
        if session.pending_invite.as_ref().is_some_and(|invite| {
            !controller
                .call_registry
                .pbx
                .contains_key(&invite.participant.pbx_call_id)
                || controller
                    .appearance_for_call(invite.participant.handset_call_id)
                    .is_none_or(|appearance| {
                        appearance.pbx_id != invite.participant.pbx_call_id
                            || appearance.device_id != invite.participant.device_id
                    })
                || session
                    .participants
                    .get(invite.moderator_id)
                    .is_none_or(|moderator| {
                        !moderator.moderator
                            || moderator.pbx_call_id != invite.moderator_call_id
                            || controller
                                .call_registry
                                .pbx
                                .get(&moderator.pbx_call_id)
                                .is_none_or(|call| call.state != CallState::Held)
                    })
        }) {
            return Some(format!(
                "conference {:?} has an inconsistent pending invite",
                session.id
            ));
        }
        if session
            .pending_participant_mutation
            .is_some_and(|mutation| {
                session.phase != ConferencePhase::Active
                    || session
                        .participants
                        .get(mutation.participant_id)
                        .is_none_or(|participant| {
                            participant.pbx_call_id != mutation.call_id
                                || match mutation.kind {
                                    ConferenceParticipantMutationKind::Mute(muted) => {
                                        participant.moderator || participant.muted == muted
                                    }
                                    ConferenceParticipantMutationKind::Remove => {
                                        participant.moderator
                                            || session.participants.iter().len() <= 2
                                    }
                                    ConferenceParticipantMutationKind::Moderator(moderator) => {
                                        participant.moderator == moderator
                                            || participant.held
                                            || (moderator && participant.muted)
                                            || (!moderator
                                                && session.participants.moderator_count() == 1)
                                    }
                                    ConferenceParticipantMutationKind::Hold(held) => {
                                        !participant.moderator || participant.held == held
                                    }
                                }
                        })
            })
        {
            return Some(format!(
                "conference {:?} has an inconsistent participant mutation",
                session.id
            ));
        }
        let states_are_valid = match session.phase {
            ConferencePhase::Consultation => {
                session.origin == ConferenceOrigin::Consultation
                    && original.state == CallState::Held
                    && matches!(
                        consultation_call.state,
                        CallState::Collecting | CallState::Calling | CallState::Connected
                    )
            }
            ConferencePhase::Merging if session.origin == ConferenceOrigin::Consultation => {
                original.state == CallState::Held && consultation_call.state == CallState::Connected
            }
            ConferencePhase::Merging => session.participants.iter().all(|participant| {
                controller
                    .call_registry
                    .pbx
                    .get(&participant.pbx_call_id)
                    .is_some_and(|call| {
                        matches!(call.state, CallState::Connected | CallState::Held)
                    })
            }),
            ConferencePhase::Active => {
                session.participants.iter().all(|participant| {
                    controller
                        .call_registry
                        .pbx
                        .get(&participant.pbx_call_id)
                        .is_some_and(|call| {
                            matches!(call.state, CallState::Connected | CallState::Held)
                        })
                }) && session.pending_invite.as_ref().is_none_or(|invite| {
                    controller
                        .call_registry
                        .pbx
                        .get(&invite.participant.pbx_call_id)
                        .is_some_and(|call| {
                            matches!(
                                call.state,
                                CallState::Collecting | CallState::Calling | CallState::Connected
                            )
                        })
                })
            }
        };
        if !states_are_valid {
            return Some(format!(
                "conference {:?} has inconsistent call states",
                session.id
            ));
        }
    }
    for (pbx_id, consultation) in &controller.conferences.by_pbx {
        if controller
            .conferences
            .by_consultation
            .get(consultation)
            .is_none_or(|session| {
                session.participants.by_pbx(*pbx_id).is_none()
                    && session
                        .pending_invite
                        .as_ref()
                        .is_none_or(|invite| invite.participant.pbx_call_id != *pbx_id)
            })
        {
            return Some(format!("conference PBX index {pbx_id:?} is dangling"));
        }
    }
    None
}
