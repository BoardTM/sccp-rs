//! Alarm, location, accessory, and unsupported-message events.

use super::super::{
    Access, DriverEffect, LogLevel, PhoneAlarmTelemetry, PhoneDeviceEvent, PhoneDeviceEventKind,
    PhoneLocationTelemetry, alarm_event, ast_log, publish_ami_event, xml_alarm_event,
};

pub(super) async fn handle_telemetry_event(
    access: &Access,
    event: PhoneDeviceEvent,
) -> Vec<DriverEffect> {
    let PhoneDeviceEvent {
        device_id,
        session_generation: _,
        event,
    } = event;
    match event {
        PhoneDeviceEventKind::Alarm { severity, text, .. } => {
            ast_log(
                LogLevel::Warning,
                &format!(
                    "SCCP alarm from {device_id} ({severity:?}, {} bytes)",
                    text.len()
                ),
            );
            publish_ami_event(access, &alarm_event(&device_id, severity));
            Vec::new()
        }
        PhoneDeviceEventKind::XmlAlarm { telemetry } => {
            if let Some(summary) = telemetry.summary() {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "typed phone alarm from {device_id} ({:?}, reason {:?})",
                        summary.kind, summary.reason_for_out_of_service
                    ),
                );
                publish_ami_event(access, &xml_alarm_event(&device_id, summary));
            } else if let PhoneAlarmTelemetry::Opaque(alarm) = telemetry {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "opaque phone alarm from {device_id} ({} bytes)",
                        alarm.as_bytes().len()
                    ),
                );
            }
            Vec::new()
        }
        PhoneDeviceEventKind::LocationInformation { telemetry } => {
            if let Some(summary) = telemetry.summary() {
                ast_log(
                    LogLevel::Debug,
                    &format!(
                        "typed phone location information from {device_id} ({:?}, off-premises {})",
                        summary.kind, summary.off_premises
                    ),
                );
            } else if let PhoneLocationTelemetry::Opaque(location) = telemetry {
                ast_log(
                    LogLevel::Debug,
                    &format!(
                        "opaque phone location information from {device_id} ({} bytes)",
                        location.as_bytes().len()
                    ),
                );
            }
            Vec::new()
        }
        PhoneDeviceEventKind::HeadsetStatusChanged { .. }
        | PhoneDeviceEventKind::MediaPathChanged { .. } => Vec::new(),
        PhoneDeviceEventKind::UnhandledMessage { message } => {
            ast_log(
                LogLevel::Debug,
                &format!("unhandled SCCP message from {device_id}: {message:?}"),
            );
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey { .. } | PhoneDeviceEventKind::LineButton { .. } => {
            Vec::new()
        }
        _ => unreachable!("telemetry event was classified before dispatch"),
    }
}
