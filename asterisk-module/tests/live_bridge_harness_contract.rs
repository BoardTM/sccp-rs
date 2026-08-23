use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn live_bridge_gate_is_feature_scoped_and_separate_from_artifact_builds() {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).unwrap();
    let module = fs::read_to_string(root().join("src/asterisk/mod.rs")).unwrap();
    let native = fs::read_to_string(root().join("src/asterisk/native/mod.rs")).unwrap();
    let driver = fs::read_to_string(root().join("src/asterisk/direct/channel_driver.rs")).unwrap();
    let artifact = fs::read_to_string(root().join("build-linux-x86_64.sh")).unwrap();
    let live = fs::read_to_string(root().join("live-tests/test-bridges.sh")).unwrap();
    let workflow = fs::read_to_string(
        root()
            .parent()
            .unwrap()
            .join(".github/workflows/asterisk-live-bridges.yml"),
    )
    .unwrap();

    assert!(manifest.contains("live-asterisk-tests = []"));
    assert!(module.contains("mod raw;"));
    assert!(native.contains("#[cfg(feature = \"live-asterisk-tests\")]"));
    assert!(driver.contains("crate::asterisk::raw::live_bridge_cli_entry()"));
    assert!(native.contains("live_bridge_tests::cli_entry()"));
    assert!(live.contains("SCCP_LIVE_BRIDGES=1"));
    assert!(!artifact.contains("live-asterisk-tests"));
    assert!(!artifact.contains("test-bridges.sh"));
    assert!(workflow.contains("22.7.0"));
    assert!(workflow.contains("23.4.1"));
    assert!(workflow.contains("live-tests/Dockerfile"));
}

#[test]
fn live_bridge_gate_uses_real_owned_native_boundaries() {
    let harness = fs::read_to_string(root().join("live-tests/bridge.rs")).unwrap();
    let bridge =
        fs::read_to_string(root().join("src/asterisk/native/bridge/conference.rs")).unwrap();

    for required in [
        "__ast_channel_alloc(",
        "ModuleReference::acquire(module_self())",
        "create_bridge(",
        "merge_consultation(",
        "merge_calls(",
        "merge_participant(",
        "admit_two_party_source(",
        "set_participant_muted(",
        "set_participant_music_on_hold(",
        "remove_participant_and_hangup(",
        "acquire_barge_bridge(",
        "prepare_conference_destination(",
        "module_use_count()",
        "bridge_count()",
        "reference_count()",
    ] {
        assert!(harness.contains(required), "live gate lost {required}");
    }
    assert!(
        harness.matches("admit_two_party_source(").count() >= 5,
        "every successful merge path must populate two-party source bridges"
    );
    assert!(bridge.contains("let Some(channel) = (unsafe { ChannelRef::acquire(channel) })"));
    assert!(bridge.contains("let _ = channel.into_raw()"));
}
