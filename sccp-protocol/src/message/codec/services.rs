//! Family-specific codec helpers delegated to by the exhaustive central dispatch.

use super::*;

pub(super) fn encode_user_data(
    message: &UserDataMessage,
    message_id: u32,
) -> Result<Vec<u8>, CodecError> {
    if message.data.len() > 2000 {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "user data",
            count: message.data.len(),
            maximum: 2000,
        });
    }
    encode(
        message_id,
        &WireUserData {
            header: WireUserDataHeader {
                application_id: message.application_id,
                line_instance: message.line_instance,
                call_reference: message.call_reference,
                transaction_id: message.transaction_id,
                data_length: wire_count(message_id, "user data", message.data.len())?,
            },
            data: message.data.clone(),
        },
    )
}

pub(super) fn decode_user_data_v1(
    payload: &[u8],
    message_id: u32,
) -> Result<UserDataV1Message, CodecError> {
    let value: WireUserDataV1 = decode_zero_padded(message_id, payload)?;
    if value.data.len() > 2000 {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "user data",
            count: value.data.len(),
            maximum: 2000,
        });
    }
    Ok(UserDataV1Message {
        application_id: value.header.application_id,
        line_instance: value.header.line_instance,
        call_reference: value.header.call_reference,
        transaction_id: value.header.transaction_id,
        sequence_flag: value.sequence_flag,
        display_priority: value.display_priority,
        conference_id: value.conference_id,
        application_instance_id: value.application_instance_id,
        routing: value.routing,
        data: value.data,
    })
}

pub(super) fn encode_user_data_v1(
    message: &UserDataV1Message,
    message_id: u32,
) -> Result<Vec<u8>, CodecError> {
    if message.data.len() > 2000 {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "user data",
            count: message.data.len(),
            maximum: 2000,
        });
    }
    encode(
        message_id,
        &WireUserDataV1 {
            header: WireUserDataHeader {
                application_id: message.application_id,
                line_instance: message.line_instance,
                call_reference: message.call_reference,
                transaction_id: message.transaction_id,
                data_length: wire_count(message_id, "user data", message.data.len())?,
            },
            sequence_flag: message.sequence_flag,
            display_priority: message.display_priority,
            conference_id: message.conference_id,
            application_instance_id: message.application_instance_id,
            routing: message.routing,
            data: message.data.clone(),
        },
    )
}

pub(super) fn decode_port_response(
    payload: &[u8],
    protocol: u32,
    message_id: u32,
) -> Result<PortEndpoint, CodecError> {
    let (conference_id, call_reference, passthrough_party_id, address, rtp, rtcp, media_type) =
        match protocol {
            20.. => {
                let value: WirePortResponseV20 = decode(message_id, payload)?;
                (
                    value.base.conference_id,
                    value.base.call_reference,
                    value.base.passthrough_party_id,
                    value.base.address.to_ip(message_id)?,
                    value.base.rtp_port,
                    value.base.rtcp_port,
                    Some(MediaType::from(value.media_type)),
                )
            }
            _ => {
                let value: WirePortResponseV3 = decode(message_id, payload)?;
                (
                    value.conference_id,
                    value.call_reference,
                    value.passthrough_party_id,
                    value.address.to_ip(message_id)?,
                    value.rtp_port,
                    value.rtcp_port,
                    None,
                )
            }
        };
    Ok(PortEndpoint {
        conference_id,
        call_reference,
        passthrough_party_id,
        address,
        rtp_port: decode_port(rtp, message_id, "RTP port")?,
        rtcp_port: decode_port(rtcp, message_id, "RTCP port")?,
        media_type,
    })
}

pub(super) fn encode_port_response(
    endpoint: &PortEndpoint,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    match protocol.wire() {
        20.. => encode(
            wire_id::PORT_RESPONSE,
            &WirePortResponseV20 {
                base: WirePortResponse {
                    conference_id: endpoint.conference_id,
                    call_reference: endpoint.call_reference,
                    passthrough_party_id: endpoint.passthrough_party_id,
                    address: WireExtendedAddress::from_ip(endpoint.address),
                    rtp_port: u32::from(endpoint.rtp_port),
                    rtcp_port: u32::from(endpoint.rtcp_port),
                },
                media_type: endpoint
                    .media_type
                    .ok_or(CodecError::InvalidValue {
                        message_id: wire_id::PORT_RESPONSE,
                        field: "media type required from protocol 20",
                        value: 0,
                    })?
                    .wire_value(),
            },
        ),
        _ => encode(
            wire_id::PORT_RESPONSE,
            &WirePortResponseV3 {
                conference_id: endpoint.conference_id,
                call_reference: endpoint.call_reference,
                passthrough_party_id: endpoint.passthrough_party_id,
                address: WireIpv4Address::from_ip(
                    endpoint.address,
                    wire_id::PORT_RESPONSE,
                    "IP address family for this protocol version",
                )?,
                rtp_port: u32::from(endpoint.rtp_port),
                rtcp_port: u32::from(endpoint.rtcp_port),
            },
        ),
    }
}

pub(super) fn push_dynamic_text(
    output: &mut Vec<u8>,
    message_id: u32,
    field: &'static str,
    text: &str,
    maximum: usize,
) -> Result<(), CodecError> {
    if text.as_bytes().contains(&0) {
        return Err(CodecError::InvalidText);
    }
    if text.len() > maximum {
        return Err(CodecError::TextTooLong {
            message_id,
            field,
            actual: text.len(),
            maximum,
        });
    }
    output.extend_from_slice(text.as_bytes());
    output.push(0);
    Ok(())
}
