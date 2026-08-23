use sccp_protocol::message::catalog::{PayloadLayout, PayloadSizeBounds};
use sccp_protocol::{
    AddParticipantResponse, BoundedBytes, ClientMessage, ControlMessage, DtmfPayloadIdentity,
    DtmfPayloadRequest, Frame, FrameDecoder, MessageId, ProtocolVersion, ServerMessage,
    XmlAlarmMessage,
};

fn encoded_frame(bytes: &[u8]) -> Frame {
    FrameDecoder::new().push(bytes).unwrap().remove(0)
}

#[test]
fn dtmf_subscription_words_round_trip_without_value_loss() {
    let identity = DtmfPayloadIdentity {
        payload_type: 101,
        conference_id: 0x1122_3344,
        passthrough_party_id: 0x5566_7788,
    };
    let request = DtmfPayloadRequest {
        payload_type: identity.payload_type,
        conference_id: identity.conference_id,
        passthrough_party_id: identity.passthrough_party_id,
        dtmf_type: 2,
    };

    for message in [
        ClientMessage::SubscribeDtmfPayloadResponse(identity),
        ClientMessage::UnsubscribeDtmfPayloadResponse(identity),
    ] {
        let frame = encoded_frame(&message.encode(ProtocolVersion::V22).unwrap());
        assert_eq!(frame.payload.len(), 12);
        assert_eq!(
            ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }

    for message in [
        ServerMessage::SubscribeDtmfPayloadRequest(request),
        ServerMessage::SubscribeDtmfPayloadError(identity),
        ServerMessage::UnsubscribeDtmfPayloadRequest(request),
        ServerMessage::UnsubscribeDtmfPayloadError(identity),
    ] {
        let frame = encoded_frame(&message.encode(ProtocolVersion::V22).unwrap());
        assert!(matches!(frame.payload.len(), 12 | 16));
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }
}

#[test]
fn add_participant_response_emits_canonical_identifier_storage() {
    let identifier = BoundedBytes::try_from(b"bridge-participant".as_slice()).unwrap();
    let message = ControlMessage::AddParticipantResponse(AddParticipantResponse {
        conference_id: 42.into(),
        call_reference: 100.into(),
        result: sccp_protocol::AddParticipantResult::Ok,
        bridge_participant_id: identifier,
    });
    let frame = encoded_frame(&message.encode(ProtocolVersion::V22).unwrap());
    assert_eq!(frame.payload.len(), 272);
    assert_eq!(&frame.payload[12..30], b"bridge-participant");
    assert!(frame.payload[30..].iter().all(|byte| *byte == 0));

    let progressive = Frame::new(
        22,
        MessageId::AddParticipantResponse.wire_value(),
        frame.payload[..30].to_vec(),
    );
    let ControlMessage::AddParticipantResponse(decoded) =
        ControlMessage::decode(progressive, ProtocolVersion::V22).unwrap()
    else {
        panic!("expected add-participant response");
    };
    assert_eq!(
        decoded.bridge_participant_id.as_bytes(),
        b"bridge-participant"
    );
}

#[test]
fn xml_alarm_preserves_every_bounded_frame_length() {
    for payload_len in [0, 1, 2_000, 2_004, 2_048] {
        let payload = vec![b'x'; payload_len];
        let decoded = ClientMessage::decode_with_version(
            Frame::new(22, MessageId::XmlAlarm.wire_value(), payload.clone()),
            ProtocolVersion::V22,
        )
        .unwrap();
        let ClientMessage::XmlAlarm(message) = &decoded else {
            panic!("expected XML alarm");
        };
        assert_eq!(message.wire_payload(), payload);
        assert_eq!(
            encoded_frame(&decoded.encode(ProtocolVersion::V22).unwrap()).payload,
            payload
        );
    }

    assert!(
        ClientMessage::decode_with_version(
            Frame::new(22, MessageId::XmlAlarm.wire_value(), vec![0; 2_049]),
            ProtocolVersion::V22,
        )
        .is_err()
    );

    let canonical = XmlAlarmMessage::from_xml(vec![b'x'; 2_000]).unwrap();
    assert_eq!(canonical.wire_payload().len(), 2_004);
    let terminated = XmlAlarmMessage::from_wire_payload(b"<alarm/>\0suffix".to_vec()).unwrap();
    assert_eq!(terminated.xml_bytes(), b"<alarm/>");
}

#[test]
fn catalog_exposes_service_bounds_without_fixed_form_labels() {
    let alarm = MessageId::XmlAlarm.contract().unwrap();
    assert_eq!(alarm.payload_layout, PayloadLayout::BoundedPreserved);
    assert_eq!(
        alarm.payload_size_bounds,
        Some(PayloadSizeBounds {
            minimum: 0,
            maximum: 2_048,
        })
    );

    let participant = MessageId::AddParticipantResponse.contract().unwrap();
    assert_eq!(participant.fixed_payload_bytes, Some(272));
    assert_eq!(
        participant.payload_size_bounds,
        Some(PayloadSizeBounds {
            minimum: 12,
            maximum: 272,
        })
    );

    assert_eq!(
        MessageId::AuditConferenceRequest
            .contract()
            .unwrap()
            .fixed_payload_bytes,
        Some(0)
    );
}
