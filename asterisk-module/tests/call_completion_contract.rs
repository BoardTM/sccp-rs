#[test]
fn native_channel_tech_uses_asterisk_generic_ccss_lifecycle() {
    let driver = include_str!("../src/asterisk/direct/channel_driver.rs");
    let channel = include_str!("../src/asterisk/native/channel/completion.rs");
    let adapter = include_str!("../src/asterisk/adapters/completion.rs");

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
    let adapter = include_str!("../src/asterisk/phone/calls.rs");
    let completion = include_str!("../src/call/completion.rs");

    assert!(adapter.contains("soft_key: SoftKey::Callback"));
    assert!(adapter.contains("AsteriskCallCompletion::new().request_owned("));
    assert!(adapter.contains("requested_device: device_id.as_str()"));
    assert!(adapter.contains("Callback requested"));
    assert!(completion.contains("Callback is not available for this call"));
}
