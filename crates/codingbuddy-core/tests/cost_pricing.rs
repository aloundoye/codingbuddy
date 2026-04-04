//! E2E tests for model pricing lookup.

use codingbuddy_core::cost::get_pricing;

#[test]
fn claude_sonnet_pricing() {
    let p = get_pricing("claude-sonnet-4-20250514");
    assert!((p.input_per_million - 3.0).abs() < 0.01);
    assert!((p.output_per_million - 15.0).abs() < 0.01);
    assert!(p.cache_discount < 0.2);
}

#[test]
fn claude_opus_pricing() {
    let p = get_pricing("claude-opus-4-20250514");
    assert!((p.input_per_million - 15.0).abs() < 0.01);
    assert!((p.output_per_million - 75.0).abs() < 0.01);
}

#[test]
fn gpt4o_pricing() {
    let p = get_pricing("gpt-4o");
    assert!((p.input_per_million - 2.5).abs() < 0.01);
    assert!((p.output_per_million - 10.0).abs() < 0.01);
}

#[test]
fn deepseek_chat_pricing() {
    let p = get_pricing("deepseek-chat");
    assert!((p.input_per_million - 0.27).abs() < 0.01);
}

#[test]
fn gemini_flash_pricing() {
    let p = get_pricing("gemini-2.5-flash");
    assert!((p.input_per_million - 0.15).abs() < 0.01);
}

#[test]
fn unknown_model_falls_back_to_default() {
    let p = get_pricing("some-unknown-model-xyz");
    // Should fall back to DeepSeek Chat pricing
    assert!((p.input_per_million - 0.27).abs() < 0.01);
}

#[test]
fn o3_mini_before_o3_ordering() {
    // o3-mini should get its own price, not be caught by o3
    let mini = get_pricing("o3-mini");
    let full = get_pricing("o3");
    assert!(
        (mini.input_per_million - full.input_per_million).abs() > 0.5,
        "o3-mini ({}) and o3 ({}) should have different pricing",
        mini.input_per_million,
        full.input_per_million
    );
}
