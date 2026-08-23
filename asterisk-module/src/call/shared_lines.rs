//! Pure adapter planning for configured shared-line appearances.

use sccp_protocol::{CallId, Codec};

use crate::call::forwarding::{ForwardingContext, ForwardingDestination};
use crate::config::LineBinding;
#[cfg(test)]
use crate::config::ModuleConfig;
use crate::runtime::controller::InboundAppearance;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedNoAnswerRoute {
    pub context: ForwardingContext,
    pub destination: ForwardingDestination,
    pub timeout: std::time::Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NoAnswerPolicy {
    pub context: ForwardingContext,
    pub destination: Option<ForwardingDestination>,
    pub timeout_seconds: u32,
}

/// A PBX-level no-answer redirect is safe only when every handset that is
/// still ringing agrees on one destination. The latest timeout wins so one
/// appearance never shortens another device's configured ringing window.
pub(crate) fn plan_shared_no_answer_route(
    policies: impl IntoIterator<Item = NoAnswerPolicy>,
) -> Option<SharedNoAnswerRoute> {
    let mut policies = policies.into_iter();
    let first = policies.next()?;
    let destination = first.destination?;
    let mut timeout_seconds = first.timeout_seconds;
    for policy in policies {
        if policy.destination.as_ref() != Some(&destination) || policy.context != first.context {
            return None;
        }
        timeout_seconds = timeout_seconds.max(policy.timeout_seconds);
    }
    Some(SharedNoAnswerRoute {
        context: first.context,
        destination,
        timeout: std::time::Duration::from_secs(u64::from(timeout_seconds)),
    })
}

#[cfg(test)]
pub(crate) fn plan_inbound_appearances(
    config: &ModuleConfig,
    address: &str,
    eligible: impl FnMut(&LineBinding) -> bool,
    reserve_call_id: impl FnMut() -> CallId,
    select_codec: impl FnMut(&LineBinding) -> Option<Codec>,
) -> Vec<InboundAppearance> {
    let Some(target) = config.dial_target(address) else {
        return Vec::new();
    };
    let bindings: Vec<_> = if address.split('/').count() == 2 {
        vec![target.clone()]
    } else {
        config
            .appearances_for_line(&target.line.number)
            .cloned()
            .collect()
    };
    plan_inbound_bindings(bindings, eligible, reserve_call_id, select_codec)
}

/// Plan an already-resolved runtime binding set. Mobility uses this form to
/// append committed roaming appearances without mutating the normalized
/// configuration snapshot.
pub(crate) fn plan_inbound_bindings(
    bindings: impl IntoIterator<Item = LineBinding>,
    mut eligible: impl FnMut(&LineBinding) -> bool,
    mut reserve_call_id: impl FnMut() -> CallId,
    mut select_codec: impl FnMut(&LineBinding) -> Option<Codec>,
) -> Vec<InboundAppearance> {
    bindings
        .into_iter()
        .filter(|binding| eligible(binding))
        .filter_map(|binding| {
            let codec = select_codec(&binding)?;
            Some(InboundAppearance {
                call_id: reserve_call_id(),
                codec,
                binding,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use sccp_protocol::{
        AppearanceRingMode, CallState as HandsetCallState, DeviceId, DeviceRegistration,
        DeviceType, MediaEndpoint, ProtocolVersion,
    };

    use super::*;
    use crate::runtime::backend::{DriverEffect, HandsetEffect, PbxCallId, PbxEffect};
    use crate::runtime::controller::Controller;

    const SHARED_CONFIG: &str = r#"
[general]
bind = 127.0.0.1:2000
advertised_address = 127.0.0.1

[1001]
type = line
label = Shared

[SEP001122334455]
type = device
button = line,1001

[SEP112233445566]
type = device
button = line,1001,ring=silent

[SEP223344556677]
type = device
button = line,1001,ring=disabled
"#;

    #[test]
    fn shared_address_reserves_ids_only_for_eligible_appearances_in_config_order() {
        let config = ModuleConfig::parse(SHARED_CONFIG).unwrap();
        let next = Cell::new(40_u64);
        let planned = plan_inbound_appearances(
            &config,
            "1001",
            |binding| binding.appearance.ring_mode != AppearanceRingMode::Disabled,
            || {
                let id = next.get();
                next.set(id + 1);
                CallId(id)
            },
            |binding| {
                if binding.device_id.as_str() == "SEP001122334455" {
                    Some(Codec::Pcma)
                } else {
                    Some(Codec::Pcmu)
                }
            },
        );

        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].binding.device_id.as_str(), "SEP001122334455");
        assert_eq!(planned[0].call_id, CallId(40));
        assert_eq!(planned[0].codec, Codec::Pcma);
        assert_eq!(planned[1].binding.device_id.as_str(), "SEP112233445566");
        assert_eq!(planned[1].call_id, CallId(41));
        assert_eq!(planned[1].codec, Codec::Pcmu);
        assert_eq!(next.get(), 42);
    }

    #[test]
    fn explicit_device_address_plans_only_that_appearance() {
        let config = ModuleConfig::parse(SHARED_CONFIG).unwrap();
        let planned = plan_inbound_appearances(
            &config,
            "SEP112233445566/1001",
            |_| true,
            || CallId(77),
            |_| Some(Codec::Pcmu),
        );

        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].binding.device_id.as_str(), "SEP112233445566");
        assert_eq!(planned[0].call_id, CallId(77));
    }

    #[test]
    fn no_answer_route_requires_consensus_and_uses_the_longest_ring_window() {
        let route = plan_shared_no_answer_route([
            NoAnswerPolicy {
                context: ForwardingContext::new("from-sccp").unwrap(),
                destination: Some(ForwardingDestination::new("9000").unwrap()),
                timeout_seconds: 15,
            },
            NoAnswerPolicy {
                context: ForwardingContext::new("from-sccp").unwrap(),
                destination: Some(ForwardingDestination::new("9000").unwrap()),
                timeout_seconds: 30,
            },
        ])
        .unwrap();
        assert_eq!(route.destination.as_str(), "9000");
        assert_eq!(route.timeout, Duration::from_secs(30));

        assert!(
            plan_shared_no_answer_route([
                NoAnswerPolicy {
                    context: ForwardingContext::new("from-sccp").unwrap(),
                    destination: Some(ForwardingDestination::new("9000").unwrap()),
                    timeout_seconds: 15,
                },
                NoAnswerPolicy {
                    context: ForwardingContext::new("from-sccp").unwrap(),
                    destination: None,
                    timeout_seconds: 15,
                },
            ])
            .is_none()
        );
        assert!(
            plan_shared_no_answer_route([
                NoAnswerPolicy {
                    context: ForwardingContext::new("from-sccp").unwrap(),
                    destination: Some(ForwardingDestination::new("9000").unwrap()),
                    timeout_seconds: 15,
                },
                NoAnswerPolicy {
                    context: ForwardingContext::new("from-sccp").unwrap(),
                    destination: Some(ForwardingDestination::new("9001").unwrap()),
                    timeout_seconds: 15,
                },
            ])
            .is_none()
        );
    }

    #[test]
    fn fake_multi_phone_adapter_runs_configured_offer_answer_and_cleanup_path() {
        let config = ModuleConfig::parse(SHARED_CONFIG).unwrap();
        let mut controller = Controller::new(Duration::from_secs(1));
        for device_id in ["SEP001122334455", "SEP112233445566"] {
            controller.registered(DeviceRegistration {
                id: DeviceId::new(device_id).unwrap(),
                peer: "192.0.2.10:2000".parse().unwrap(),
                transport: sccp_protocol::StationTransport::Clear,
                reported_address: Some("192.0.2.10".parse().unwrap()),
                reported_ipv6_address: None,
                device_type: DeviceType::Cisco7962,
                protocol: ProtocolVersion::V22,
                firmware: "test".into(),
            });
        }
        let next = Cell::new(100_u64);
        let candidates = plan_inbound_appearances(
            &config,
            "1001",
            |binding| {
                binding.appearance.ring_mode != AppearanceRingMode::Disabled
                    && controller.is_registered(&binding.device_id)
            },
            || {
                let id = next.get();
                next.set(id + 1);
                CallId(id)
            },
            |_| Some(Codec::Pcmu),
        );
        let offers = controller.offer_inbound_call(PbxCallId(90), candidates);
        assert_eq!(
            offers.iter().map(|offer| offer.call_id).collect::<Vec<_>>(),
            [CallId(100), CallId(101)]
        );

        let effects = controller.phone_answer(CallId(101));
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
                .count(),
            0
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(100),
                state: HandsetCallState::RemoteMultiline,
                ..
            })
        )));
        assert!(controller.phone_answer(CallId(100)).is_empty());
        let acknowledged = controller.media_opened(
            CallId(101),
            MediaEndpoint {
                address: "192.0.2.20".parse().unwrap(),
                rtp_port: 20_000,
                rtcp_port: 20_001,
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 1,
                telephone_event_payload: 101,
            },
        );
        assert_eq!(
            acknowledged
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
                .count(),
            1
        );

        let cleanup = controller.pbx_hangup_with_effects(PbxCallId(90)).unwrap();
        assert_eq!(cleanup.effects.len(), 2);
        assert!(cleanup.effects.iter().all(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::OnHook,
                ..
            })
        )));
    }

    #[test]
    fn appearances_without_a_mutual_codec_are_not_offered_or_assigned_ids() {
        let config = ModuleConfig::parse(SHARED_CONFIG).unwrap();
        let reservations = Cell::new(0_u64);
        let planned = plan_inbound_appearances(
            &config,
            "1001",
            |_| true,
            || {
                reservations.set(reservations.get() + 1);
                CallId(reservations.get())
            },
            |binding| (binding.device_id.as_str() == "SEP112233445566").then_some(Codec::Pcma),
        );

        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].binding.device_id.as_str(), "SEP112233445566");
        assert_eq!(reservations.get(), 1);
    }

    #[test]
    fn runtime_binding_planner_appends_roaming_appearance_in_supplied_order() {
        let config = ModuleConfig::parse(SHARED_CONFIG).unwrap();
        let mut bindings = config
            .appearances_for_line("1001")
            .cloned()
            .collect::<Vec<_>>();
        let mut roaming = bindings[0].clone();
        roaming.device_id = DeviceId::new("SEP998877665544").unwrap();
        roaming.line_instance = 2;
        roaming.appearance.instance = 2;
        bindings.push(roaming);
        let next = Cell::new(700_u64);
        let planned = plan_inbound_bindings(
            bindings,
            |binding| binding.appearance.ring_mode != AppearanceRingMode::Disabled,
            || {
                let id = next.get();
                next.set(id + 1);
                CallId(id)
            },
            |_| Some(Codec::Pcmu),
        );

        assert_eq!(
            planned
                .iter()
                .map(|candidate| candidate.binding.device_id.as_str())
                .collect::<Vec<_>>(),
            ["SEP001122334455", "SEP112233445566", "SEP998877665544"]
        );
        assert_eq!(
            planned
                .iter()
                .map(|candidate| candidate.call_id)
                .collect::<Vec<_>>(),
            [CallId(700), CallId(701), CallId(702)]
        );
    }
}
