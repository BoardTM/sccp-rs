#[path = "../../src/asterisk/native/channel/ownership.rs"]
mod ownership;

mod support;
use support::{rust_item, rust_match_arm, source};

#[test]
fn native_hangup_dispatches_by_explicit_channel_ownership() {
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    let control = source("src/asterisk/native/channel/control.rs");
    let runtime = source("src/asterisk/runtime/channel.rs");

    assert!(allocation.contains("ownership: NativeChannelOwnership"));
    assert!(allocation.contains("NativeChannelOwnership::module_owned()"));
    assert!(runtime.contains("ChannelAllocationOwner::Asterisk"));
    assert!(runtime.contains("handoff_channel_to_asterisk"));

    let hard = rust_match_arm(&control, "Ok(HangupOwnership::Hard)");
    assert!(hard.contains("sys::ast_hangup("));
    assert!(!hard.contains("sys::ast_queue_hangup_with_cause("));
    let queued = rust_match_arm(&control, "Ok(HangupOwnership::Queued)");
    assert!(queued.contains("sys::ast_queue_hangup_with_cause("));
    assert!(!queued.contains("sys::ast_hangup("));
}

#[test]
fn pbx_start_transfers_ownership_and_failure_rolls_it_back() {
    let control = source("src/asterisk/native/channel/control.rs");
    let begin = control.find(".begin_pbx_start()").unwrap();
    let start = control.find("sys::ast_pbx_start(").unwrap();
    let rollback = control.find(".rollback_pbx_start(ownership)").unwrap();
    assert!(begin < start && start < rollback);
}

#[test]
fn channel_request_stages_every_guarded_allocation_before_commit() {
    let exports = source("src/asterisk/exports.rs");
    let request = rust_item(&exports, "pub unsafe fn request_channel");
    let parsed = request.find("ParsedChannelRequest::parse").unwrap();
    let text = request.find("prepare_channel_allocation_text").unwrap();
    let guard = request.find("PreparedChannelRequest::new").unwrap();
    let offer = request.find("offer_inbound_call_with_policy").unwrap();
    let allocate = request.find("allocate_channel(").unwrap();
    let binding = request.find("channel_binding(&access, pbx_id)").unwrap();
    let commit = request.find("prepared.commit()").unwrap();
    assert!(parsed < text && text < guard && guard < offer);
    assert!(offer < allocate && allocate < binding && binding < commit);

    for stage in [
        "struct ParsedChannelRequest",
        "struct SelectedChannelPolicy",
        "struct PreparedChannelRequest",
    ] {
        assert!(exports.contains(stage), "request lost typed stage {stage}");
    }
    for propagated in [
        "requestor_metadata",
        "requestor_party",
        "request_video_formats",
        "assigned_ids",
        "ChannelAllocationOwner::Asterisk",
    ] {
        assert!(request.contains(propagated), "request lost {propagated}");
    }

    let rollback = rust_item(&exports, "impl Drop for PreparedChannelRequest");
    assert!(rollback.contains("remove_channel(self.access, self.pbx_id)"));
    assert!(rollback.contains("controller.pbx_hangup_with_effects(self.pbx_id)"));

    let channel = source("src/asterisk/runtime/channel.rs");
    let remove = rust_item(&channel, "pub fn remove_channel");
    assert!(remove.contains("forwarded_calls"));
    assert!(remove.contains("clear_no_answer_route(access, pbx_id)"));
    assert!(remove.contains("audio_packet_ms"));
}
