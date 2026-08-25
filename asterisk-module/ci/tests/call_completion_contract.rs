mod support;
use support::source;

#[test]
fn native_channel_tech_uses_asterisk_generic_ccss_lifecycle() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    let channel = source("src/asterisk/native/channel/completion.rs");
    let adapter = source("src/asterisk/adapters/completion.rs");

    assert!(driver.contains("technology.cc_callback = Some(call_completion)"));
    assert!(driver.contains("sys::AST_CC_MONITOR_GENERIC"));
    assert!(driver.contains("canonical_callback_target("));
    assert!(driver.contains("sys::ast_cc_config_params_destroy(self.0.as_ptr())"));
    assert!(channel.contains("sys::AST_CC_AGENT_GENERIC"));
    assert!(channel.contains("sys::AST_CC_MONITOR_GENERIC"));
    assert!(channel.contains("sys::ast_cc_get_current_core_id(channel.as_ptr())"));
    assert!(channel.contains("sys::ast_cc_request_is_within_limits()"));
    assert!(channel.contains("sys::ast_cc_agent_accept_request("));
    assert!(adapter.contains("impl<'a> CallCompletionBackend<AsteriskChannel<'a>>"));
    assert!(!driver.contains("rust_sccp_"));
}

#[test]
fn callback_soft_key_reaches_the_typed_native_request_adapter() {
    let adapter = source("src/asterisk/phone/calls/call_control.rs");
    let completion = source("src/call/completion.rs");

    assert!(adapter.contains("soft_key: SoftKey::Callback"));
    assert!(adapter.contains("AsteriskCallCompletion::new().request_owned("));
    assert!(adapter.contains("requested_device: device_id.as_str()"));
    assert!(adapter.contains_literal("Callback requested"));
    assert!(completion.contains_literal("Callback is not available for this call"));
}
