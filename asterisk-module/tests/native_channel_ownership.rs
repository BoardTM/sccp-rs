#[path = "../src/asterisk/native/channel/ownership.rs"]
mod ownership;

use std::fs;
use std::path::PathBuf;

fn source(relative: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative))
        .unwrap_or_else(|error| panic!("unable to read {relative}: {error}"))
}

#[test]
fn native_hangup_dispatches_by_explicit_channel_ownership() {
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    let control = source("src/asterisk/native/channel/control.rs");
    let runtime = source("src/asterisk/runtime/channel.rs");

    assert!(allocation.contains("ownership: NativeChannelOwnership"));
    assert!(allocation.contains("NativeChannelOwnership::module_owned()"));
    assert!(runtime.contains("ChannelAllocationOwner::Asterisk"));
    assert!(runtime.contains("handoff_channel_to_asterisk"));

    let hard = control.find("Ok(HangupOwnership::Hard)").unwrap();
    let ast_hangup = control[hard..].find("sys::ast_hangup(").unwrap() + hard;
    let queued = control.find("Ok(HangupOwnership::Queued)").unwrap();
    let queue_hangup = control[queued..]
        .find("sys::ast_queue_hangup_with_cause(")
        .unwrap()
        + queued;
    assert!(hard < ast_hangup && queued < queue_hangup);
}

#[test]
fn pbx_start_transfers_ownership_and_failure_rolls_it_back() {
    let control = source("src/asterisk/native/channel/control.rs");
    let begin = control.find(".begin_pbx_start()").unwrap();
    let start = control.find("sys::ast_pbx_start(").unwrap();
    let rollback = control.find(".rollback_pbx_start(ownership)").unwrap();
    assert!(begin < start && start < rollback);
}
