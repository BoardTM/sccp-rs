//! Family-specific codec helpers delegated to by the exhaustive central dispatch.

use super::*;

pub(super) const MAX_CONFERENCE_PASSTHROUGH_DATA: usize = 2000;
pub(super) const MAX_AUDIT_CONFERENCE_ENTRIES: usize = 32;
pub(super) const MAX_AUDIT_PARTICIPANT_DATA: usize = 256;

pub(super) fn encode_participant_request(
    message_id: u32,
    conference_id: crate::types::ConferenceId,
    participant: &ConferenceParticipant,
) -> Result<WireParticipantRequest, CodecError> {
    Ok(WireParticipantRequest {
        conference_id: conference_id.get(),
        call_reference: participant.call_reference.get(),
        presentation_restrictions: participant.presentation_restrictions.bits(),
        participant_name: WireFixedText::new(message_id, "participant name", &participant.name)?,
        participant_number: WireFixedText::new(
            message_id,
            "participant number",
            &participant.number,
        )?,
        conference_name: WireFixedText::new(
            message_id,
            "conference name",
            &participant.conference_name,
        )?,
    })
}

pub(super) fn decode_participant_request(
    payload: &[u8],
    message_id: u32,
) -> Result<(crate::types::ConferenceId, ConferenceParticipant), CodecError> {
    validate_exact_payload(payload, message_id, 108)?;
    let value: WireParticipantRequest = decode(message_id, payload)?;
    Ok((
        value.conference_id.into(),
        ConferenceParticipant {
            call_reference: value.call_reference.into(),
            presentation_restrictions: PartyInformationRestrictions::from_bits_retain(
                value.presentation_restrictions,
            ),
            name: value.participant_name.text()?,
            number: value.participant_number.text()?,
            conference_name: value.conference_name.text()?,
        },
    ))
}

impl ConferenceParticipantChange {
    /// Place the typed participant change inside the standard V1 application
    /// envelope. The caller supplies application-specific routing metadata.
    pub fn to_user_data_v1(
        &self,
        routing: ParticipantChangeRouting,
    ) -> Result<UserDataV1Message, CodecError> {
        let data = encode(
            id::USER_TO_DEVICE_DATA_V1,
            &encode_participant_request(
                id::USER_TO_DEVICE_DATA_V1,
                self.conference_id,
                &self.participant,
            )?,
        )?;
        Ok(UserDataV1Message {
            application_id: routing.application_id.get(),
            line_instance: routing.line_instance,
            call_reference: self.participant.call_reference.get(),
            transaction_id: routing.transaction_id.get(),
            sequence_flag: routing.sequence_flag,
            display_priority: routing.display_priority,
            conference_id: self.conference_id.get(),
            application_instance_id: routing.application_instance_id.get(),
            routing: routing.routing,
            data,
        })
    }

    /// Decode a participant change from a V1 application envelope and reject
    /// mismatched envelope/payload call or conference identities.
    pub fn from_user_data_v1(message: &UserDataV1Message) -> Result<Self, CodecError> {
        let (conference_id, participant) =
            decode_participant_request(&message.data, id::USER_TO_DEVICE_DATA_V1)?;
        if message.conference_id != conference_id.get() {
            return Err(CodecError::InvalidValue {
                message_id: id::USER_TO_DEVICE_DATA_V1,
                field: "participant change conference ID",
                value: u64::from(message.conference_id),
            });
        }
        if message.call_reference != participant.call_reference.get() {
            return Err(CodecError::InvalidValue {
                message_id: id::USER_TO_DEVICE_DATA_V1,
                field: "participant change call reference",
                value: u64::from(message.call_reference),
            });
        }
        Ok(Self {
            conference_id,
            participant,
        })
    }
}

pub(super) fn validate_conference_data_length(
    payload: &[u8],
    message_id: u32,
    header_size: usize,
    length_offset: usize,
) -> Result<(), CodecError> {
    if payload.len() < header_size {
        return Err(CodecError::Truncated {
            message_id,
            needed: header_size,
            actual: payload.len(),
        });
    }
    let data_length = usize_from_wire(
        message_id,
        "conference passthrough data",
        u32::from_le_bytes(
            payload[length_offset..length_offset + 4]
                .try_into()
                .expect("validated conference header contains length word"),
        ),
    )?;
    if data_length > MAX_CONFERENCE_PASSTHROUGH_DATA {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "conference passthrough data",
            count: data_length,
            maximum: MAX_CONFERENCE_PASSTHROUGH_DATA,
        });
    }
    let needed = header_size + data_length;
    if payload.len() < needed {
        return Err(CodecError::Truncated {
            message_id,
            needed,
            actual: payload.len(),
        });
    }
    Ok(())
}

pub(super) fn validate_conference_data_for_encode(
    message_id: u32,
    passthrough_data: &[u8],
) -> Result<u32, CodecError> {
    if passthrough_data.len() > MAX_CONFERENCE_PASSTHROUGH_DATA {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "conference passthrough data",
            count: passthrough_data.len(),
            maximum: MAX_CONFERENCE_PASSTHROUGH_DATA,
        });
    }
    wire_count(
        message_id,
        "conference passthrough data",
        passthrough_data.len(),
    )
}
