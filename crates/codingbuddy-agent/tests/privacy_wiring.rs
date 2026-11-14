//! Tests for tool-output privacy and keyword search registration.

use codingbuddy_agent::tool_loop::{ToolLoopConfig, ToolUseLoop};
use codingbuddy_core::{LlmResponse, LlmToolCall, StreamChunk, TokenUsage};
use codingbuddy_testkit::{ScriptedLlm, scripted_text_response as text_response};
use std::sync::{Arc, Mutex};

#[test]
fn index_query_tool_available() {
    // The index_query tool should be in the tool definitions
    let tools = codingbuddy_tools::tool_definitions();
    let has_index_query = tools.iter().any(|t| t.function.name == "index_query");
    assert!(
        has_index_query,
        "tool_definitions() should include index_query"
    );
}

#[test]
fn privacy_router_redacts_in_tool_loop() {
    let llm = ScriptedLlm::new(vec![
        // First response: call a tool
        LlmResponse {
            text: String::new(),
            finish_reason: "tool_calls".to_string(),
            reasoning_content: String::new(),
            tool_calls: vec![LlmToolCall {
                id: "call_1".to_string(),
                name: "fs_read".to_string(),
                arguments: r#"{"path": "secret.txt"}"#.to_string(),
            }],
            usage: Some(TokenUsage::default()),
            compatibility: None,
        },
        // Second response: text reply
        text_response("Here is the info"),
    ]);

    let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let warnings_clone = warnings.clone();

    // Create a privacy router with a custom regex that the output_scanner doesn't have
    let privacy_config = codingbuddy_policy::privacy::PrivacyConfig {
        enabled: true,
        sensitive_regex: vec![r"INTERNAL_SECRET_\d+".to_string()],
        policy: codingbuddy_policy::privacy::PrivacyPolicy::Redact,
        ..Default::default()
    };
    let privacy_router =
        Arc::new(codingbuddy_policy::privacy::PrivacyRouter::new(privacy_config).unwrap());

    // Tool host that returns content with a custom secret pattern
    struct SensitiveToolHost;
    impl codingbuddy_core::ToolHost for SensitiveToolHost {
        fn propose(&self, call: codingbuddy_core::ToolCall) -> codingbuddy_core::ToolProposal {
            codingbuddy_core::ToolProposal {
                invocation_id: uuid::Uuid::now_v7(),
                call,
                approved: true,
            }
        }
        fn execute(
            &self,
            _approved: codingbuddy_core::ApprovedToolCall,
        ) -> codingbuddy_core::ToolResult {
            codingbuddy_core::ToolResult {
                invocation_id: uuid::Uuid::now_v7(),
                success: true,
                output: serde_json::json!({
                    "content": "Config value: INTERNAL_SECRET_42 is set\nOther line is fine"
                }),
            }
        }
    }

    let tool_host: Arc<dyn codingbuddy_core::ToolHost + Send + Sync> = Arc::new(SensitiveToolHost);

    let config = ToolLoopConfig {
        privacy_router: Some(privacy_router),
        ..Default::default()
    };

    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        config,
        "system".to_string(),
        codingbuddy_tools::tool_definitions(),
    );

    loop_.set_stream_callback(Arc::new(move |chunk| {
        if let StreamChunk::SecurityWarning { message } = chunk {
            warnings_clone.lock().unwrap().push(message);
        }
    }));

    let result = loop_.run("Read secret.txt");
    assert!(result.is_ok());

    // Check that privacy warning was emitted
    let w = warnings.lock().unwrap();
    let has_privacy_warning = w.iter().any(|msg| msg.contains("Privacy router"));
    assert!(
        has_privacy_warning,
        "privacy router should emit a security warning when redacting"
    );
}
