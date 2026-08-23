use std::fs;
use std::path::PathBuf;

const RUST_NATIVE_MODULES: &[&str] = &[
    "bridge/mod.rs",
    "bridge/conference.rs",
    "bridge/parking.rs",
    "bridge/pickup.rs",
    "channel/mod.rs",
    "channel/allocation.rs",
    "channel/completion.rs",
    "channel/control.rs",
    "channel/media.rs",
    "channel/metadata.rs",
    "channel/ownership.rs",
    "channel/party_metadata.rs",
    "channel/video.rs",
    "dialplan.rs",
    "handles.rs",
    "http.rs",
    "manager.rs",
    "presence/mod.rs",
    "presence/hints.rs",
    "presence/mwi.rs",
    "realtime.rs",
    "recording.rs",
    "registry/mod.rs",
    "registry/callback.rs",
    "system.rs",
];

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn source(relative: &str) -> String {
    fs::read_to_string(crate_root().join(relative))
        .unwrap_or_else(|error| panic!("unable to read {relative}: {error}"))
}

fn rust_sources(directory: &std::path::Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
}

#[test]
fn domain_layers_do_not_depend_on_asterisk_bindings() {
    for directory in [
        "src/ami",
        "src/call",
        "src/config",
        "src/http",
        "src/media",
        "src/pbx",
        "src/presence",
        "src/state",
    ] {
        let mut files = Vec::new();
        rust_sources(&crate_root().join(directory), &mut files);
        for path in files {
            let contents = fs::read_to_string(&path).unwrap();
            assert!(
                !contents.contains("crate::asterisk"),
                "domain module imports Asterisk integration details: {}",
                path.display()
            );
            assert!(
                !contents.contains("ffi::sys"),
                "domain module imports generated bindings: {}",
                path.display()
            );
        }
    }
}

#[test]
fn asterisk_visibility_is_scoped_to_its_owning_module() {
    let library = source("src/lib.rs");
    assert!(library.contains("mod asterisk;"));
    assert!(
        !library.contains("pub mod asterisk;"),
        "the production Asterisk composition root must not become public API"
    );

    let mut files = Vec::new();
    rust_sources(&crate_root().join("src/asterisk"), &mut files);
    for path in files {
        let contents = fs::read_to_string(&path).unwrap();
        assert!(
            !contents.contains("pub(crate)"),
            "broad crate visibility escaped the Asterisk hierarchy: {}",
            path.display()
        );
        assert!(
            !contents.contains("pub(in crate::asterisk"),
            "private Asterisk ancestry should cap ordinary module APIs: {}",
            path.display()
        );
    }
}

#[test]
fn project_owned_internal_c_records_are_absent() {
    let mut files = Vec::new();
    rust_sources(&crate_root().join("src/asterisk"), &mut files);
    for path in files {
        let contents = fs::read_to_string(&path).unwrap();
        let relative = path
            .strip_prefix(crate_root().join("src/asterisk"))
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let count = contents.matches("#[repr(C)]").count();
        if relative == "native/http.rs" {
            assert_eq!(count, 1, "HTTP may define only its opaque libc FILE");
            assert!(contents.contains("#[repr(C)]\nstruct File"));
        } else {
            assert_eq!(
                count, 0,
                "project-owned C-shaped record returned in {relative}"
            );
        }
    }
}

#[test]
fn rust_does_not_call_rust_through_legacy_c_named_functions() {
    let mut files = Vec::new();
    rust_sources(&crate_root().join("src/asterisk"), &mut files);
    for path in files {
        let contents = fs::read_to_string(&path).unwrap();
        for legacy in ["rust_sccp_", "sccp_ast_"] {
            assert!(
                !contents.contains(legacy),
                "legacy internal C-ABI name {legacy} returned in {}",
                path.display()
            );
        }
    }
}

#[test]
fn every_rust_defined_c_callback_is_an_actual_asterisk_entrypoint() {
    let mut files = Vec::new();
    rust_sources(&crate_root().join("src/asterisk"), &mut files);
    let allowed_native = [
        ("direct/channel_driver.rs", "requester"),
        ("direct/channel_driver.rs", "call"),
        ("direct/channel_driver.rs", "hangup"),
        ("direct/channel_driver.rs", "answer"),
        ("direct/channel_driver.rs", "read"),
        ("direct/channel_driver.rs", "write"),
        ("direct/channel_driver.rs", "get_rtp_info"),
        ("direct/channel_driver.rs", "get_vrtp_info"),
        ("direct/channel_driver.rs", "update_peer"),
        ("direct/channel_driver.rs", "get_codec"),
        ("direct/channel_driver.rs", "indicate"),
        ("direct/channel_driver.rs", "send_digit_begin"),
        ("direct/channel_driver.rs", "send_digit_end"),
        ("direct/channel_driver.rs", "send_text"),
        ("direct/channel_driver.rs", "set_option"),
        ("direct/channel_driver.rs", "query_option"),
        ("direct/channel_driver.rs", "fixup"),
        ("direct/channel_driver.rs", "device_state"),
        ("direct/channel_driver.rs", "call_completion"),
        ("direct/channel_driver.rs", "$name"),
        ("direct/channel_driver.rs", "cli_reload"),
        ("direct/channel_driver.rs", "cli_forwarding"),
        ("direct/module_info.rs", "load"),
        ("direct/module_info.rs", "unload"),
        ("direct/module_info.rs", "reload"),
        ("direct/module_info.rs", "register_module"),
        ("direct/module_info.rs", "unregister_module"),
        ("direct/module_info.rs", "__internal_chan_sccp2_self"),
        ("native/bridge/parking.rs", "async_application_thread"),
        ("native/bridge/parking.rs", "parking_event"),
        ("native/dialplan.rs", "function_read"),
        ("native/dialplan.rs", "function_write"),
        ("native/dialplan.rs", "application_execute"),
        ("native/http.rs", "callback"),
        ("native/manager.rs", "manager_action"),
        ("native/presence/hints.rs", "hint_update"),
        ("native/presence/hints.rs", "hint_watcher_destroy"),
        ("native/presence/mwi.rs", "mwi_event"),
    ];
    for path in files {
        let relative = path
            .strip_prefix(crate_root().join("src/asterisk"))
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let contents = fs::read_to_string(&path).unwrap();
        for line in contents
            .lines()
            .filter(|line| line.contains("unsafe extern \"C\" fn "))
        {
            assert!(
                allowed_native
                    .iter()
                    .any(|(file, name)| relative == *file && line.contains(name)),
                "un-inventoried C callback in {relative}: {line}"
            );
        }
    }
}

#[test]
fn conference_destination_work_is_owned_by_the_rust_runtime() {
    let native = source("src/asterisk/native/bridge/conference.rs");
    assert!(!native.contains("ast_pthread_create"));
    assert!(!native.contains("extern \"C\" fn conference_application"));
    assert!(native.contains("pub struct ConferenceApplication"));
    assert!(native.contains("pub struct ConferenceApplicationCancellation"));

    let runtime = source("src/asterisk/runtime/backend.rs");
    assert!(runtime.contains("conference_destination_tasks"));
    assert!(runtime.contains("spawn_blocking"));
    assert!(runtime.contains("begin_shutdown"));
    let start = runtime
        .find("fn start_conference_destination")
        .expect("conference destination backend");
    let end = runtime[start..]
        .find("impl MediaBackend")
        .map(|offset| start + offset)
        .expect("conference destination backend boundary");
    let destination = &runtime[start..end];
    assert!(destination.contains("conference_destination_failed("));
    assert!(destination.contains("complete_conference_mutation(mutation)"));
}

#[test]
fn native_adapter_is_split_into_rust_owned_domains() {
    let native = source("src/asterisk/native/mod.rs");
    for module in RUST_NATIVE_MODULES {
        let contents = source(&format!("src/asterisk/native/{module}"));
        assert!(
            contents.lines().count() < 1_500,
            "src/asterisk/native/{module} has regrown into a monolith"
        );
    }
    for module in [
        "bridge",
        "channel",
        "dialplan",
        "handles",
        "http",
        "manager",
        "presence",
        "realtime",
        "recording",
        "registry",
    ] {
        assert!(
            native.contains(&format!("mod {module};")),
            "native module root does not compile {module}"
        );
    }
    let bridge = source("src/asterisk/native/bridge/mod.rs");
    for module in ["conference", "parking", "pickup"] {
        assert!(
            bridge.contains(&format!("mod {module};")),
            "bridge module root does not compile {module}"
        );
    }
    for module in ["channel_driver.rs", "handles.rs", "module_info.rs"] {
        assert!(
            crate_root()
                .join("src/asterisk/direct")
                .join(module)
                .is_file(),
            "missing direct Asterisk adapter module {module}"
        );
    }
}

#[test]
fn attended_transfer_runs_off_the_serial_handset_event_loop() {
    let calls = source("src/asterisk/phone/calls.rs");
    let start = calls
        .find("pub(super) async fn execute_transfer_completion")
        .expect("transfer completion entrypoint");
    let end = calls[start..]
        .find("pub(super) async fn cancel_transfer")
        .map(|offset| start + offset)
        .expect("transfer completion boundary");
    let completion = &calls[start..end];
    assert!(completion.contains("tokio::task::spawn_blocking"));
    assert!(completion.contains("access.handle.spawn"));
    assert!(completion.contains("retain_two_channels"));
    assert!(completion.contains("Transfer in progress") || calls.contains("Transfer in progress"));
}

#[test]
fn build_uses_one_upstream_binding_surface_and_compiles_no_repository_c() {
    let build = source("build.rs");
    let manifest = source("Cargo.toml");
    let sys = source("src/asterisk/sys.rs");

    assert!(build.contains("sccp_asterisk_sys.h"));
    assert!(build.contains("asterisk_sys.rs"));
    assert!(sys.contains("/asterisk_sys.rs"));
    for retired in [
        "cc::Build",
        "NATIVE_SOURCES",
        "native/wrapper.h",
        "asterisk_shim.rs",
        "asterisk_raw.rs",
    ] {
        assert!(
            !build.contains(retired),
            "build.rs restored retired native path {retired}"
        );
    }
    assert!(
        !manifest
            .lines()
            .any(|line| line.trim_start().starts_with("cc =")),
        "the retired C compiler dependency returned"
    );
    assert!(
        !crate_root().join("src/asterisk/ffi.rs").exists(),
        "the retired flat FFI re-export facade returned"
    );
    assert!(!sys.contains("asterisk_shim.rs"));
    assert!(!sys.contains("asterisk_raw.rs"));

    let legacy_native = crate_root().join("native");
    if legacy_native.exists() {
        for entry in fs::read_dir(legacy_native).unwrap() {
            let path = entry.unwrap().path();
            assert!(
                !matches!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("c" | "h")
                ),
                "retired repository-owned native source remains: {}",
                path.display()
            );
        }
    }

    assert!(
        !crate_root().join("src/asterisk/abi.rs").exists(),
        "the retired project-owned Asterisk ABI catalog returned"
    );
    let persistence = source("src/asterisk/adapters/persistence.rs");
    assert!(persistence.contains("sys::ast_db_put"));
    assert!(persistence.contains("sys::ast_db_del"));
    let persistence_domain = source("src/state/persistence.rs");
    assert!(!persistence_domain.contains("crate::asterisk"));
    assert!(!persistence_domain.contains("ffi::sys::"));
}

#[test]
fn only_the_asterisk_module_self_hook_is_exported_explicitly() {
    let mut files = Vec::new();
    rust_sources(&crate_root().join("src"), &mut files);
    let mut exports = Vec::new();
    for path in files {
        let contents = fs::read_to_string(&path).unwrap();
        for _ in contents.match_indices("no_mangle") {
            exports.push(path.clone());
        }
        assert!(
            !contents.contains("export_name"),
            "unexpected explicit export name in {}",
            path.display()
        );
    }
    assert_eq!(
        exports.len(),
        1,
        "only Asterisk's module-self hook may be exported"
    );
    assert!(exports[0].ends_with("asterisk/direct/module_info.rs"));
    let module_info = fs::read_to_string(&exports[0]).unwrap();
    assert!(module_info.contains("fn __internal_chan_sccp2_self("));
}

#[test]
fn rust_asterisk_root_remains_a_small_composition_root() {
    let root = source("src/asterisk/mod.rs");
    assert!(root.lines().count() < 300);
    for module in ["adapters", "boundary", "direct", "raw", "runtime", "sys"] {
        assert!(
            root.contains(&format!("mod {module};")),
            "composition root lost {module} module"
        );
    }
    assert!(
        !root.contains("include!("),
        "composition still uses textual include fragments instead of modules"
    );
}

#[test]
fn protocol_string_policy_stays_in_rust() {
    let http_policy = source("src/http/mod.rs");
    for required in [
        "request_body_length",
        "http_status_title",
        "validate_response_header",
    ] {
        assert!(
            http_policy.contains(required),
            "HTTP policy lost {required}"
        );
    }
    let manager_policy = source("src/ami/manager.rs");
    for required in [
        "struct ManagerField",
        "public_value",
        "validate_field_value",
        "request_field_name_sensitive",
        "struct RequestFields",
    ] {
        assert!(
            manager_policy.contains(required),
            "AMI string policy lost {required}"
        );
    }

    let manager_edge = source("src/asterisk/native/manager.rs");
    for required in [
        "ManagerRequestField::new",
        "serialized.push_str",
        "REDACTED_MANAGER_VALUE",
        "(*message).headers",
    ] {
        assert!(
            manager_edge.contains(required),
            "AMI native serialization lost {required}"
        );
    }
}

#[test]
fn http_unlink_cannot_free_a_descriptor_selected_by_asterisk() {
    let http = source("src/asterisk/native/http.rs");
    for required in [
        "struct HttpRouteGate",
        "closing: AtomicBool",
        "readers: AtomicUsize",
        "fn close_and_drain_readers",
        "if gate.is_null() || !(*gate).enter()",
        "sys::ast_http_uri_unlink",
        "release_from_native::<HttpPayload>",
    ] {
        assert!(
            http.contains(required),
            "HTTP callback/unlink ownership lost {required}"
        );
    }
    let unregister = http
        .split_once("fn unregister_http")
        .expect("HTTP unregister implementation")
        .1;
    let close = unregister
        .find("close_and_drain_readers")
        .expect("close URI admission");
    let unlink = unregister.find("ast_http_uri_unlink").expect("unlink URI");
    let release = unregister
        .find("release_from_native")
        .expect("release callback payload");
    assert!(close < unlink && unlink < release);
    assert!(http.contains("Box::into_raw(gate)"));
    assert!(!unregister.contains("Box::from_raw"));
}

#[test]
fn proceeding_control_is_typed_at_the_actual_asterisk_callback() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    assert!(driver.contains("sys::AST_CONTROL_PROCEEDING"));
    assert!(driver.contains("ChannelIndication::Proceeding"));
    let exports = source("src/asterisk/exports.rs");
    assert!(exports.contains("ChannelIndication::Proceeding => RuntimeCallSignalKind::Proceeding"));
}

#[test]
fn native_call_indications_use_one_ordered_rust_queue() {
    let exports = fs::read_to_string(crate_root().join("src/asterisk/exports.rs")).unwrap();
    let answer_and_indicate = exports
        .split_once("fn answer_channel")
        .expect("answer callback")
        .1
        .split_once("fn send_digit_begin_to_channel")
        .expect("end of indication callbacks")
        .0;
    assert!(answer_and_indicate.contains("enqueue_call_signal"));
    assert!(answer_and_indicate.contains("RuntimeCallSignalKind::Proceeding"));
    assert!(!answer_and_indicate.contains(".spawn("));
    let hangup = exports
        .split_once("fn hangup_channel")
        .expect("hangup callback")
        .1
        .split_once("fn answer_channel")
        .expect("end of hangup callback")
        .0;
    assert!(hangup.contains("RuntimeCallSignalKind::Hangup"));
    assert!(hangup.contains("if !access.enqueue_call_signal"));
    assert!(hangup.contains("handle_runtime_hangup_signal"));

    let management =
        fs::read_to_string(crate_root().join("src/asterisk/runtime/management.rs")).unwrap();
    assert!(management.contains("Mutex<RuntimeCallSignalQueue>"));
    let lifecycle =
        fs::read_to_string(crate_root().join("src/asterisk/runtime/lifecycle.rs")).unwrap();
    assert!(lifecycle.contains("checked_add(1)"));
    assert!(lifecycle.contains("queue.sender.send(signal)"));

    let services =
        fs::read_to_string(crate_root().join("src/asterisk/runtime/services.rs")).unwrap();
    assert!(services.contains("signal.sequence <= last_sequence"));
    assert!(services.contains("HashMap::<PbxCallId, mpsc::UnboundedSender<RuntimeCallSignal>>"));
    assert!(services.contains("handle_runtime_call_signal(&lane_access, signal).await"));
    assert!(services.contains("controller.pbx_progress_with_media_mode"));

    let backend = fs::read_to_string(crate_root().join("src/asterisk/runtime/backend.rs")).unwrap();
    assert!(backend.contains("PhoneCommandAction::SetCallState"));
    assert!(backend.contains("PhoneCommandAction::CommitOutboundCall"));
    assert!(backend.contains("PhoneCommandAction::PresentOutboundRinging"));
    assert!(backend.contains("PhoneCommandAction::OpenOutboundMedia"));
    let handset_failure = backend
        .split_once("EffectExecutionError::Handset { effect, .. } =>")
        .expect("handset failure handling")
        .1
        .split_once("match *effect")
        .expect("end of handset failure handling")
        .0;
    assert!(handset_failure.contains("terminate_failed_pbx_call"));
    let outbound_media = backend
        .split_once("async fn begin_outbound_media")
        .expect("coupled outbound media implementation")
        .1
        .split_once("fn receive_media_source")
        .expect("end of coupled outbound media implementation")
        .0;
    let open = outbound_media
        .find("PhoneCommandAction::OpenOutboundMedia")
        .expect("coupled open command");
    let progress = outbound_media
        .find("PhoneCommandAction::DisplayPrompt")
        .expect("coupled progress prompt");
    assert!(open < progress);
    assert!(outbound_media.contains("\"Call Progress\".into()"));

    let terminal_state = backend
        .split_once("HandsetEffect::SetCallState")
        .expect("handset state executor")
        .1
        .split_once("HandsetEffect::SetMicrophoneMode")
        .expect("end of handset state executor")
        .0;
    assert!(terminal_state.contains("state != PhoneCallState::OnHook"));
    assert!(terminal_state.contains("PhoneCommandAction::CloseCall"));
}

#[test]
fn unload_keeps_active_calls_subscriptions_and_conferences_in_one_ordered_drain() {
    let lifecycle =
        fs::read_to_string(crate_root().join("src/asterisk/runtime/lifecycle.rs")).unwrap();
    let stop = lifecycle
        .split_once("fn stop(mut self)")
        .expect("module stop implementation")
        .1;
    let ordered = [
        "manager_registrations",
        "http_registrations",
        "dialplan_registrations",
        "uninstall_blf(&self.access)",
        "self.event_task.abort()",
        "shutdown_conferences(&self.access).await",
        "shutdown_remote_hangups(&self.access).await",
        "shutdown_one_way_microphones(&self.access).await",
        "phone.shutdown().await",
        "registration_contexts",
        "self.parking_subscription.unsubscribe()",
        "self.runtime.shutdown_timeout",
    ];
    let mut offset = 0;
    for phase in ordered {
        let relative = stop[offset..]
            .find(phase)
            .unwrap_or_else(|| panic!("module unload lost phase {phase}"));
        offset += relative + phase.len();
    }

    let backend = fs::read_to_string(crate_root().join("src/asterisk/runtime/backend.rs")).unwrap();
    let conference_shutdown = backend
        .split_once("async fn shutdown_conferences")
        .expect("conference shutdown implementation")
        .1;
    for required in [
        "drain_conferences_for_shutdown",
        "cancel_conference_announcement",
        "execute_cleanup_effects",
        "remaining_bridges",
        "remaining_barge_bridges",
        "remaining_calls",
        "remove_channel",
    ] {
        assert!(
            conference_shutdown.contains(required),
            "conference/call unload lost {required}"
        );
    }

    let presence =
        fs::read_to_string(crate_root().join("src/asterisk/runtime/presence.rs")).unwrap();
    let blf_shutdown = presence
        .split_once("fn uninstall_blf")
        .expect("BLF shutdown implementation")
        .1;
    assert!(blf_shutdown.contains(".clear();"));

    let exports = fs::read_to_string(crate_root().join("src/asterisk/exports.rs")).unwrap();
    let unload = exports
        .split_once("fn stop_module")
        .expect("module unload export")
        .1;
    assert!(unload.contains(".take()"));
    assert!(unload.contains("module.stop()"));
}

#[test]
fn conference_announcements_are_generated_by_owned_pbx_channels() {
    let controller = fs::read_to_string(crate_root().join("src/runtime/controller.rs")).unwrap();
    let backend = fs::read_to_string(crate_root().join("src/asterisk/runtime/backend.rs")).unwrap();
    let native =
        fs::read_to_string(crate_root().join("src/asterisk/native/channel/control.rs")).unwrap();

    assert!(controller.contains("PbxEffect::ConferenceAnnouncement"));
    assert!(!controller.contains("HandsetEffect::ConferenceAnnouncement"));
    assert!(backend.contains("native_channel::start_tone_pair"));
    assert!(backend.contains("native_channel::stop_tone_pair"));
    assert!(!backend.contains("PhoneCommandAction::StartAnnouncement"));
    assert!(!backend.contains("PhoneCommandAction::AnnouncementFinish"));
    assert!(!backend.contains("PhoneCommandAction::StopAnnouncement"));
    assert!(native.contains("sys::ast_tonepair_start"));
    assert!(native.contains("sys::ast_tonepair_stop"));
}

#[test]
fn monitor_soft_key_uses_the_owned_recording_transaction() {
    let calls = fs::read_to_string(crate_root().join("src/asterisk/phone/calls.rs")).unwrap();
    let services =
        fs::read_to_string(crate_root().join("src/asterisk/runtime/services.rs")).unwrap();

    assert!(calls.contains("soft_key: SoftKey::Monitor"));
    assert!(calls.contains("toggle_monitor_recording(access, recordings"));
    assert!(services.contains("plan_recording_toggle("));
    assert!(services.contains("recording_service_operation("));
    assert!(services.contains("PhoneCommandAction::SetRecordingStatus"));
}

#[test]
fn native_lifecycle_gate_stays_separate_from_artifact_builds() {
    let script = fs::read_to_string(crate_root().join("test-native-lifecycle.sh")).unwrap();
    for required in [
        "module load chan_sccp2.so",
        "module unload chan_sccp2.so",
        "core show channeltypes",
        "record_metrics \"$cycle_label-start\"",
        "/proc/$asterisk_pid/fd",
        "/proc/$asterisk_pid/task",
        "/proc/$asterisk_pid/status",
        "second_batch_rss + RSS_TOLERANCE_KB",
        "kill -0",
    ] {
        assert!(
            script.contains(required),
            "native lifecycle gate lost {required}"
        );
    }
    assert!(script.contains("WARMUP_CYCLES:-4"));
    assert!(script.contains("BATCH_CYCLES:-12"));
    assert!(script.contains("autoload = no"));
    assert!(!script.contains("autoload = yes"));

    let docker = fs::read_to_string(crate_root().join("Dockerfile.linux-x86_64")).unwrap();
    assert!(docker.contains("make include/asterisk/buildopts.h"));
    assert!(!docker.contains("make -j"));
    assert!(!docker.contains("make install"));
    assert!(!docker.contains("make basic-pbx"));
    assert!(!docker.contains("test-native-lifecycle.sh"));
    assert!(docker.contains("rust_sccp_|sccp_ast_"));

    let workflow = fs::read_to_string(
        crate_root()
            .parent()
            .unwrap()
            .join(".github/workflows/asterisk-module.yml"),
    )
    .unwrap();
    for version in ["22.7.0", "23.4.1"] {
        assert!(workflow.contains(version));
    }
    assert!(workflow.contains("make include/asterisk/buildopts.h"));
    assert!(!workflow.contains("make -j"));
    assert!(!workflow.contains("make install"));
    assert!(!workflow.contains("make basic-pbx"));
    assert!(!workflow.contains("test-native-lifecycle.sh"));
    assert!(workflow.contains("rust_sccp_|sccp_ast_"));
}
