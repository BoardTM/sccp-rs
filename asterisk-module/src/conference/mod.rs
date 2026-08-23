//! Backend-neutral conference participant state and external snapshots.
//!
//! Conference creation starts from owned calls and commits only after native
//! bridge operations succeed. Consultation merge, selected-call join, and
//! moderator invite keep stable conference/participant/call identifiers. A
//! conference holds at most [`MAX_CONFERENCE_PARTICIPANTS`] participants and
//! always retains a moderator.
//!
//! Participant mutation validates exact membership and the current moderator
//! before native work. Mute, remove, promote/demote, explicit end, moderator
//! hold/resume, destination routing, and announcement completion report typed
//! outcomes so handset success UI is emitted only after controller commit.
//! Disconnect, PBX hangup, explicit end, failure compensation, and unload use
//! idempotent cleanup of bridge membership, calls, handset presentations, MOH,
//! announcements, and media-anchor leases.
//!
//! External snapshots are deterministically ordered and apply presentation
//! filtering before names or numbers leave the controller.

use std::collections::HashMap;

use sccp_protocol::{CallId, ConferenceId, DeviceId, ParticipantId};
use serde::Serialize;

use crate::runtime::backend::PbxCallId;

pub const MAX_CONFERENCE_PARTICIPANTS: usize = 16;

/// Party identity after presentation policy has been applied.
/// Empty values represent a withheld or unavailable identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConferenceParticipantIdentity {
    pub display_name: String,
    pub number: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceParticipant {
    pub id: ParticipantId,
    pub pbx_call_id: PbxCallId,
    pub handset_call_id: CallId,
    pub device_id: DeviceId,
    pub display_name: String,
    pub number: String,
    pub moderator: bool,
    pub muted: bool,
    pub held: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParticipantRegistryError {
    Full,
    DuplicateId,
    DuplicateCall,
    MissingModerator,
    UnknownParticipant,
    LastModerator,
    UnchangedRole,
}

/// Participant records in stable insertion order, with typed indexes for
/// handset actions and PBX lifecycle callbacks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConferenceParticipantRegistry {
    participants: Vec<ConferenceParticipant>,
    by_id: HashMap<ParticipantId, usize>,
    by_pbx: HashMap<PbxCallId, ParticipantId>,
}

impl ConferenceParticipantRegistry {
    pub fn new(
        participants: impl IntoIterator<Item = ConferenceParticipant>,
    ) -> Result<Self, ParticipantRegistryError> {
        let mut registry = Self::default();
        for participant in participants {
            registry.insert(participant)?;
        }
        if registry
            .participants
            .iter()
            .filter(|participant| participant.moderator)
            .count()
            == 0
        {
            return Err(ParticipantRegistryError::MissingModerator);
        }
        Ok(registry)
    }

    pub fn insert(
        &mut self,
        participant: ConferenceParticipant,
    ) -> Result<(), ParticipantRegistryError> {
        if self.participants.len() >= MAX_CONFERENCE_PARTICIPANTS {
            return Err(ParticipantRegistryError::Full);
        }
        if self.by_id.contains_key(&participant.id) {
            return Err(ParticipantRegistryError::DuplicateId);
        }
        if self.by_pbx.contains_key(&participant.pbx_call_id) {
            return Err(ParticipantRegistryError::DuplicateCall);
        }
        let index = self.participants.len();
        self.by_id.insert(participant.id, index);
        self.by_pbx.insert(participant.pbx_call_id, participant.id);
        self.participants.push(participant);
        Ok(())
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ConferenceParticipant> {
        self.participants.iter()
    }

    pub fn get(&self, participant_id: ParticipantId) -> Option<&ConferenceParticipant> {
        self.by_id
            .get(&participant_id)
            .and_then(|index| self.participants.get(*index))
    }

    pub fn set_muted(&mut self, participant_id: ParticipantId, muted: bool) -> bool {
        let Some(participant) = self
            .by_id
            .get(&participant_id)
            .and_then(|index| self.participants.get_mut(*index))
        else {
            return false;
        };
        participant.muted = muted;
        true
    }

    pub fn set_held(&mut self, participant_id: ParticipantId, held: bool) -> bool {
        let Some(participant) = self
            .by_id
            .get(&participant_id)
            .and_then(|index| self.participants.get_mut(*index))
        else {
            return false;
        };
        participant.held = held;
        true
    }

    pub fn remove(&mut self, participant_id: ParticipantId) -> Option<ConferenceParticipant> {
        let index = *self.by_id.get(&participant_id)?;
        let participant = self.participants.remove(index);
        self.by_id.remove(&participant_id);
        self.by_pbx.remove(&participant.pbx_call_id);
        for (index, participant) in self.participants.iter().enumerate().skip(index) {
            self.by_id.insert(participant.id, index);
        }
        Some(participant)
    }

    pub fn by_pbx(&self, call_id: PbxCallId) -> Option<&ConferenceParticipant> {
        self.by_pbx
            .get(&call_id)
            .and_then(|participant_id| self.get(*participant_id))
    }

    /// Replaces presentation data without changing participant identity or
    /// insertion order. Returns `false` when the call is not registered.
    pub fn update_identity(
        &mut self,
        call_id: PbxCallId,
        identity: ConferenceParticipantIdentity,
    ) -> bool {
        let Some(participant) = self
            .by_pbx
            .get(&call_id)
            .and_then(|participant_id| self.by_id.get(participant_id))
            .and_then(|index| self.participants.get_mut(*index))
        else {
            return false;
        };
        participant.display_name = identity.display_name;
        participant.number = identity.number;
        true
    }

    pub fn moderator(&self) -> Option<&ConferenceParticipant> {
        self.participants
            .iter()
            .find(|participant| participant.moderator)
    }

    pub fn moderator_count(&self) -> usize {
        self.participants
            .iter()
            .filter(|participant| participant.moderator)
            .count()
    }

    pub fn active_moderator_count(&self) -> usize {
        self.participants
            .iter()
            .filter(|participant| participant.moderator && !participant.held)
            .count()
    }

    pub fn set_moderator(
        &mut self,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> Result<(), ParticipantRegistryError> {
        let Some(index) = self.by_id.get(&participant_id).copied() else {
            return Err(ParticipantRegistryError::UnknownParticipant);
        };
        if self.participants[index].moderator == moderator {
            return Err(ParticipantRegistryError::UnchangedRole);
        }
        if !moderator && self.moderator_count() == 1 {
            return Err(ParticipantRegistryError::LastModerator);
        }
        self.participants[index].moderator = moderator;
        Ok(())
    }

    pub fn to_json(&self, conference_id: ConferenceId) -> Result<String, serde_json::Error> {
        #[derive(Serialize)]
        struct JsonParticipant<'a> {
            id: u32,
            call_id: u64,
            device_id: &'a str,
            display_name: &'a str,
            number: &'a str,
            moderator: bool,
            muted: bool,
            held: bool,
        }

        #[derive(Serialize)]
        struct JsonConference<'a> {
            conference_id: u32,
            moderator_id: u32,
            participants: Vec<JsonParticipant<'a>>,
        }

        let moderator_id = self
            .moderator()
            .map_or(0, |participant| participant.id.get());
        serde_json::to_string(&JsonConference {
            conference_id: conference_id.get(),
            moderator_id,
            participants: self
                .participants
                .iter()
                .map(|participant| JsonParticipant {
                    id: participant.id.get(),
                    call_id: participant.pbx_call_id.0,
                    device_id: participant.device_id.as_str(),
                    display_name: &participant.display_name,
                    number: &participant.number,
                    moderator: participant.moderator,
                    muted: participant.muted,
                    held: participant.held,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(id: u32, call_id: u64, moderator: bool) -> ConferenceParticipant {
        ConferenceParticipant {
            id: ParticipantId::new(id),
            pbx_call_id: PbxCallId(call_id),
            handset_call_id: CallId(call_id),
            device_id: DeviceId::new(format!("SEP{id:012}")).expect("generated device ID is valid"),
            display_name: format!("Caller {id}"),
            number: format!("10{id}"),
            moderator,
            muted: false,
            held: false,
        }
    }

    #[test]
    fn registry_preserves_ids_order_and_typed_indexes() {
        let registry = ConferenceParticipantRegistry::new([
            participant(7, 70, true),
            participant(11, 110, false),
        ])
        .unwrap();

        assert_eq!(
            registry.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            [ParticipantId::new(7), ParticipantId::new(11)]
        );
        assert_eq!(
            registry.by_pbx(PbxCallId(110)).map(|entry| entry.id),
            Some(ParticipantId::new(11))
        );
        assert_eq!(
            registry.moderator().map(|entry| entry.id),
            Some(ParticipantId::new(7))
        );
    }

    #[test]
    fn registry_rejects_duplicate_or_unmoderated_state_and_allows_multiple_moderators() {
        assert_eq!(
            ConferenceParticipantRegistry::new([participant(1, 1, false)]),
            Err(ParticipantRegistryError::MissingModerator)
        );
        assert_eq!(
            ConferenceParticipantRegistry::new(
                [participant(1, 1, true), participant(1, 2, false),]
            ),
            Err(ParticipantRegistryError::DuplicateId)
        );
        assert_eq!(
            ConferenceParticipantRegistry::new(
                [participant(1, 1, true), participant(2, 1, false),]
            ),
            Err(ParticipantRegistryError::DuplicateCall)
        );
        let registry =
            ConferenceParticipantRegistry::new([participant(1, 1, true), participant(2, 2, true)])
                .unwrap();
        assert_eq!(registry.moderator_count(), 2);
    }

    #[test]
    fn json_is_typed_stable_and_escaped() {
        let mut moderator = participant(1, 10, true);
        moderator.display_name = "Alice \"Admin\"".into();
        let registry =
            ConferenceParticipantRegistry::new([moderator, participant(2, 20, false)]).unwrap();

        let json = registry.to_json(ConferenceId::new(4)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["conference_id"], 4);
        assert_eq!(value["moderator_id"], 1);
        assert_eq!(value["participants"][0]["display_name"], "Alice \"Admin\"");
        assert_eq!(value["participants"][1]["id"], 2);
    }

    #[test]
    fn participant_mutation_preserves_typed_indexes_and_updates_json() {
        let mut registry = ConferenceParticipantRegistry::new([
            participant(1, 10, true),
            participant(2, 20, false),
        ])
        .unwrap();

        assert!(registry.set_muted(ParticipantId::new(2), true));
        assert!(!registry.set_muted(ParticipantId::new(99), true));
        assert_eq!(registry.active_moderator_count(), 1);
        assert!(registry.set_held(ParticipantId::new(1), true));
        assert_eq!(registry.active_moderator_count(), 0);
        assert!(!registry.set_held(ParticipantId::new(99), true));

        assert!(registry.get(ParticipantId::new(2)).unwrap().muted);
        assert!(registry.by_pbx(PbxCallId(20)).unwrap().muted);
        assert!(registry.get(ParticipantId::new(1)).unwrap().held);
        let value: serde_json::Value =
            serde_json::from_str(&registry.to_json(ConferenceId::new(4)).unwrap()).unwrap();
        assert_eq!(value["participants"][1]["muted"], true);
        assert_eq!(value["participants"][0]["held"], true);
    }

    #[test]
    fn identity_update_keeps_participant_id_and_insertion_order() {
        let mut registry = ConferenceParticipantRegistry::new([
            participant(1, 10, true),
            participant(2, 20, false),
            participant(3, 30, false),
        ])
        .unwrap();
        let ids = registry
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>();

        assert!(registry.update_identity(
            PbxCallId(20),
            ConferenceParticipantIdentity {
                display_name: "Updated caller".into(),
                number: "2200".into(),
            },
        ));
        let missing =
            registry.update_identity(PbxCallId(99), ConferenceParticipantIdentity::default());
        assert!(!missing);

        assert_eq!(
            registry
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            ids
        );
        let updated = registry.by_pbx(PbxCallId(20)).unwrap();
        assert_eq!(updated.id, ParticipantId::new(2));
        assert_eq!(updated.display_name, "Updated caller");
        assert_eq!(updated.number, "2200");
    }

    #[test]
    fn participant_removal_reindexes_without_changing_stable_ids() {
        let mut registry = ConferenceParticipantRegistry::new([
            participant(1, 10, true),
            participant(2, 20, false),
            participant(3, 30, false),
        ])
        .unwrap();

        assert_eq!(
            registry.remove(ParticipantId::new(2)).map(|entry| entry.id),
            Some(ParticipantId::new(2))
        );
        assert_eq!(
            registry.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            [ParticipantId::new(1), ParticipantId::new(3)]
        );
        assert_eq!(
            registry.by_pbx(PbxCallId(30)).map(|entry| entry.id),
            Some(ParticipantId::new(3))
        );
        assert!(registry.get(ParticipantId::new(2)).is_none());
    }

    #[test]
    fn moderator_role_changes_preserve_at_least_one_moderator() {
        let mut registry = ConferenceParticipantRegistry::new([
            participant(1, 10, true),
            participant(2, 20, false),
        ])
        .unwrap();

        assert_eq!(
            registry.set_moderator(ParticipantId::new(1), false),
            Err(ParticipantRegistryError::LastModerator)
        );
        registry.set_moderator(ParticipantId::new(2), true).unwrap();
        assert_eq!(registry.moderator_count(), 2);
        registry
            .set_moderator(ParticipantId::new(1), false)
            .unwrap();
        assert_eq!(registry.moderator_count(), 1);
        assert_eq!(
            registry.moderator().map(|participant| participant.id),
            Some(ParticipantId::new(2))
        );
        assert_eq!(
            registry.set_moderator(ParticipantId::new(2), true),
            Err(ParticipantRegistryError::UnchangedRole)
        );
    }
}
