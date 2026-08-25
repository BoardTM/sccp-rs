//! Family-specific codec helpers delegated to by the exhaustive central dispatch.

use super::*;

pub(super) fn qos_flow_from_wire(
    flow: WireQosFlow,
    message_id: u32,
) -> Result<QosFlow, CodecError> {
    Ok(QosFlow {
        conference_id: flow.conference_id.into(),
        call_reference: flow.call_reference.into(),
        passthrough_party_id: flow.passthrough_party_id.into(),
        address: Ipv4Addr::from(flow.address),
        port: decode_port(flow.port, message_id, "QoS media port")?,
    })
}

pub(super) fn qos_flow_to_wire(flow: QosFlow) -> WireQosFlow {
    WireQosFlow {
        conference_id: flow.conference_id.get(),
        call_reference: flow.call_reference.get(),
        passthrough_party_id: flow.passthrough_party_id.get(),
        address: flow.address.octets(),
        port: u32::from(flow.port),
    }
}

pub(super) fn qos_application_from_wire(
    value: WireQosApplicationIdentifier,
) -> Result<QosApplicationIdentifier, CodecError> {
    Ok(QosApplicationIdentifier {
        vendor_id: value.vendor_id.text()?,
        version: value.version.text()?,
        application_name: value.application_name.text()?,
        sub_application_id: value.sub_application_id.text()?,
    })
}

pub(super) fn qos_application_to_wire(
    message_id: u32,
    value: &QosApplicationIdentifier,
) -> Result<WireQosApplicationIdentifier, CodecError> {
    Ok(WireQosApplicationIdentifier {
        vendor_id: WireFixedText::new(message_id, "QoS vendor ID", &value.vendor_id)?,
        version: WireFixedText::new(message_id, "QoS application version", &value.version)?,
        application_name: WireFixedText::new(
            message_id,
            "QoS application name",
            &value.application_name,
        )?,
        sub_application_id: WireFixedText::new(
            message_id,
            "QoS sub-application ID",
            &value.sub_application_id,
        )?,
    })
}

pub(super) fn qos_traffic(
    compression_type: u32,
    average_bit_rate: u32,
    burst_size: u32,
    peak_rate: u32,
) -> QosTrafficSpecification {
    QosTrafficSpecification {
        codec: Codec::from(compression_type),
        average_bit_rate,
        burst_size,
        peak_rate,
    }
}
