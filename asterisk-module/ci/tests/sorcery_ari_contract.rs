use std::fs;

use serde_json::Value;

mod support;
use support::{crate_root, rust_item, source, workspace_source};

fn websocket_requests() -> Vec<Value> {
    let path = crate_root().join("ci/sorcery/rest-over-websocket.requests.jsonl");
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("unable to read {}: {error}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("invalid JSON on fixture line {}: {error}", index + 1)
            })
        })
        .collect()
}

#[test]
fn websocket_fixture_uses_the_standard_ari_rest_request_envelope() {
    let requests = websocket_requests();
    assert_eq!(requests.len(), 13);

    for request in &requests {
        assert_eq!(request["type"], "RESTRequest");
        assert!(request["transaction_id"].as_str().is_some());
        assert!(request["request_id"].as_str().is_some());
        assert!(matches!(
            request["method"].as_str(),
            Some("GET" | "PUT" | "DELETE")
        ));
        let uri = request["uri"].as_str().expect("request URI");
        assert!(!uri.starts_with("/ari/"));

        if uri == "/asterisk/variable" {
            assert_eq!(request["method"], "GET");
            assert_eq!(request["query_strings"][0]["name"], "variable");
            assert_eq!(request["query_strings"][0]["value"], "SCCP_CONFIG_STATUS");
            assert!(request.get("message_body").is_none());
            assert!(request.get("content_type").is_none());
            continue;
        }

        assert!(uri.starts_with("/asterisk/config/dynamic/chan_sccp2/"));

        if request["method"] == "PUT" {
            assert_eq!(request["content_type"], "application/json");
            let body: Value = serde_json::from_str(
                request["message_body"]
                    .as_str()
                    .expect("PUT message body must be a JSON string"),
            )
            .expect("PUT message body must contain JSON");
            let fields = body["fields"].as_array().expect("ConfigTuple array");
            assert!(!fields.is_empty());
            for field in fields {
                assert!(field["attribute"].as_str().is_some());
                assert!(field["value"].as_str().is_some());
            }
        } else {
            assert!(request.get("message_body").is_none());
            assert!(request.get("content_type").is_none());
        }
    }
}

#[test]
fn websocket_fixture_respects_cross_object_dependency_order() {
    let requests = websocket_requests();
    let operation = |index: usize| {
        (
            requests[index]["method"].as_str().unwrap(),
            requests[index]["uri"].as_str().unwrap(),
        )
    };

    assert_eq!(operation(0), ("GET", "/asterisk/variable"));
    assert_eq!(
        operation(1),
        ("PUT", "/asterisk/config/dynamic/chan_sccp2/line/1001")
    );
    assert_eq!(
        operation(3),
        (
            "PUT",
            "/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455"
        )
    );
    assert_eq!(
        operation(9),
        (
            "DELETE",
            "/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455"
        )
    );
    assert_eq!(
        operation(11),
        ("DELETE", "/asterisk/config/dynamic/chan_sccp2/line/1001")
    );

    let tombstone: Value = serde_json::from_str(requests[5]["message_body"].as_str().unwrap())
        .expect("tombstone body");
    assert_eq!(tombstone["fields"][0]["attribute"], "button.0002");
    assert_eq!(tombstone["fields"][0]["value"], "");
}

#[test]
fn distributed_examples_lock_the_public_sorcery_contract() {
    let example = source("sccp.conf.example");
    assert!(example.contains("configuration_source = file"));

    let guide = workspace_source("docs/DYNAMIC_CONFIGURATION.md");
    for required in [
        "configuration_source = sorcery",
        "[chan_sccp2]",
        "device = astdb,chan_sccp2",
        "line = astdb,chan_sccp2",
        "/ari/asterisk/config/dynamic/chan_sccp2/{device|line}/{id}",
        "/asterisk/config/dynamic/chan_sccp2/{device|line}/{id}",
        "SCCP_CONFIG_STATUS",
        "/asterisk/variable",
        "generation",
        "converged",
        "button.0001",
        "Create or update SCCP lines",
        "Remove obsolete SCCP devices",
    ] {
        assert!(guide.contains(required), "dynamic guide lost {required}");
    }
}

#[test]
fn vendored_asterisk_supports_dynamic_config_over_rest_websocket() {
    let dynamic_api = source("asterisk/rest-api/api-docs/asterisk.json");
    assert!(dynamic_api.contains("/asterisk/config/dynamic/{configClass}/{objectType}/{id}"));
    assert!(dynamic_api.contains("/asterisk/variable"));
    assert!(dynamic_api.contains("List[ConfigTuple]"));

    let events_api = source("asterisk/rest-api/api-docs/events.json");
    for field in [
        "RESTRequest",
        "transaction_id",
        "request_id",
        "message_body",
        "RESTResponse",
        "status_code",
    ] {
        assert!(events_api.contains(field), "ARI schema lost {field}");
    }

    let ari_sample = source("asterisk/configs/samples/ari.conf.sample");
    assert!(ari_sample.contains("type = outbound_websocket"));
    assert!(ari_sample.contains("websocket_client_id"));
    assert!(ari_sample.contains("local_ari_user"));
}

#[test]
fn reconciliation_status_is_ordered_and_published_through_ari_variable_state() {
    let convergence = source("src/config/convergence.rs");
    assert!(convergence.contains("pub generation: u64"));
    assert!(convergence.contains("ConfigReconciliationState::Converged"));
    assert!(convergence.contains("ConfigReconciliationState::Failed"));
    assert!(convergence.contains_in_order(&[
        "let _serial = lock_unpoisoned(&self.serial)",
        "let result = apply()",
        "publish(status)",
    ]));

    let lifecycle = source("src/asterisk/runtime/lifecycle.rs");
    let tracked = rust_item(&lifecycle, "fn tracked_sorcery_reload");
    assert!(tracked.contains("reconcile_with"));
    assert!(tracked.contains("publish_config_reconciliation_status"));

    let system = source("src/asterisk/native/system.rs");
    assert!(system.contains("CONFIG_STATUS_VARIABLE: &CStr = c\"SCCP_CONFIG_STATUS\""));
    assert!(system.contains("pbx_builtin_setvar_helper"));
}

#[test]
fn live_harness_exercises_real_ari_and_runtime_convergence() {
    let harness = source("ci/sorcery/test-live-ari.sh");
    for required in [
        "res_sorcery_astdb.so",
        "res_http_websocket.so",
        "res_ari.so",
        "res_ari_asterisk.so",
        "configuration_source = sorcery",
        "/asterisk/config/dynamic/chan_sccp2/line/1001",
        "/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455",
        "button.0002",
        "SCCP_CONFIG_STATUS",
        "config-status-converged",
        "config-status-failed",
        "database get SCCP/config last-known-good",
        "sccp show devices",
        "sccp show lines",
        "module unload chan_sccp2.so",
    ] {
        assert!(harness.contains(required), "live harness lost {required}");
    }

    let docker = source("ci/Dockerfile");
    assert!(docker.contains("FROM asterisk-live AS sorcery-ari-test"));
    assert!(docker.contains("ci/sorcery/test-live-ari.sh"));

    let workflow = workspace_source(".github/workflows/asterisk-module.yml");
    assert!(workflow.contains("target: sorcery-ari-test"));
}

#[test]
fn sorcery_observers_are_reconciled_at_startup_and_drained_at_unload() {
    let exports = source("src/asterisk/exports.rs");
    let start = rust_item(&exports, "fn start_module");
    let stop = rust_item(&exports, "fn stop_module");

    assert!(start.contains_in_order(&[
        "*module = Some(started)",
        "drop(module)",
        "install_mwi(&access)",
        "source == ConfigurationSource::Sorcery",
        "reload_sorcery(",
    ]));
    assert!(stop.contains_in_order(&[".take()", "shutdown_observers()", "module.stop()",]));

    let descriptor = source("src/asterisk/direct/module_info.rs");
    assert!(descriptor.contains("info.optional_modules = OPTIONAL_MODULES"));
    assert!(!descriptor.contains("info.requires = OPTIONAL_MODULES"));
}
