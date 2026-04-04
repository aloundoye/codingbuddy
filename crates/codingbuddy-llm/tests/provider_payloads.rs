//! E2E tests for multi-provider payload formats.

use codingbuddy_core::{
    ChatMessage, ChatRequest, FunctionDefinition, LlmToolCall, ToolChoice, ToolDefinition,
};
use codingbuddy_llm::providers::{anthropic, google};
use serde_json::json;

/// Helper to build a ChatRequest with all fields populated (no Default).
fn make_request(
    model: &str,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages,
        tools,
        tool_choice: ToolChoice::auto(),
        max_tokens: 4096,
        temperature: Some(0.7),
        top_p: None,
        presence_penalty: None,
        frequency_penalty: None,
        logprobs: None,
        top_logprobs: None,
        thinking: None,
        images: vec![],
        provider_options: Default::default(),
        response_format: None,
    }
}

fn sample_tool() -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: "fs_read".to_string(),
            description: "Read a file from disk.".to_string(),
            strict: None,
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" }
                },
                "required": ["path"]
            }),
        },
    }
}

// --------------------------------------------------------------------------
// Test 1: Anthropic payload has system as top-level, cache_control on last tool
// --------------------------------------------------------------------------
#[test]
fn anthropic_payload_system_toplevel_and_cache_control_on_last_tool() {
    let req = make_request(
        "claude-sonnet-4-20250514",
        vec![
            ChatMessage::System {
                content: "You are a coding assistant.".to_string(),
            },
            ChatMessage::User {
                content: "Read main.rs".to_string(),
            },
        ],
        vec![sample_tool()],
    );
    let payload = anthropic::build_payload(&req, 4096).unwrap();

    // System prompt is a top-level array, not inside messages
    let system = payload
        .get("system")
        .expect("system must be top-level")
        .as_array()
        .expect("system must be an array");
    assert_eq!(system[0]["text"], "You are a coding assistant.");

    // Last system block gets cache_control
    let last_sys = system.last().unwrap();
    assert_eq!(
        last_sys["cache_control"],
        json!({"type": "ephemeral"}),
        "last system block must have cache_control"
    );

    // Messages should contain only the user turn (system is extracted)
    let msgs = payload["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], "user");

    // Tools array should exist with cache_control on the last tool
    let tools = payload["tools"].as_array().expect("tools must be present");
    assert_eq!(tools.len(), 1);
    let last_tool = tools.last().unwrap();
    assert_eq!(
        last_tool["cache_control"],
        json!({"type": "ephemeral"}),
        "last tool must have cache_control for prompt caching"
    );
    assert_eq!(last_tool["name"], "fs_read");
    assert_eq!(last_tool["input_schema"]["type"], "object");
}

// --------------------------------------------------------------------------
// Test 2: Google payload uses contents array and systemInstruction
// --------------------------------------------------------------------------
#[test]
fn google_payload_uses_contents_and_system_instruction() {
    let req = make_request(
        "gemini-2.5-flash",
        vec![
            ChatMessage::System {
                content: "You are a coding assistant.".to_string(),
            },
            ChatMessage::User {
                content: "Hello".to_string(),
            },
        ],
        vec![sample_tool()],
    );
    let payload = google::build_payload(&req, 8192).unwrap();

    // systemInstruction is a top-level object with parts
    let sys_instr = payload
        .get("systemInstruction")
        .expect("systemInstruction must be top-level");
    let parts = sys_instr["parts"].as_array().unwrap();
    assert_eq!(parts[0]["text"], "You are a coding assistant.");

    // contents is the message array (system is NOT in contents)
    let contents = payload["contents"]
        .as_array()
        .expect("contents must be an array");
    assert_eq!(
        contents.len(),
        1,
        "only user message, no system in contents"
    );
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "Hello");

    // Tools use functionDeclarations wrapper
    let tools = payload["tools"].as_array().expect("tools must be present");
    assert_eq!(tools.len(), 1);
    let declarations = tools[0]["functionDeclarations"]
        .as_array()
        .expect("functionDeclarations must be present");
    assert_eq!(declarations.len(), 1);
    assert_eq!(declarations[0]["name"], "fs_read");

    // generationConfig should contain maxOutputTokens
    assert_eq!(payload["generationConfig"]["maxOutputTokens"], 8192);
}

// --------------------------------------------------------------------------
// Test 3: Google endpoint includes model name
// --------------------------------------------------------------------------
#[test]
fn google_endpoint_includes_model_name() {
    let base = "https://generativelanguage.googleapis.com";

    let non_stream = google::endpoint(base, "gemini-2.5-flash", false);
    assert_eq!(
        non_stream,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
    );
    assert!(
        non_stream.contains("gemini-2.5-flash"),
        "model name must appear in the URL"
    );

    let stream = google::endpoint(base, "gemini-2.5-pro", true);
    assert!(
        stream.contains("gemini-2.5-pro"),
        "streaming URL must contain model name"
    );
    assert!(
        stream.contains("streamGenerateContent"),
        "streaming URL must use streamGenerateContent"
    );
    assert!(
        stream.contains("alt=sse"),
        "streaming URL must include alt=sse"
    );
}

// --------------------------------------------------------------------------
// Test 4: Anthropic payload converts tool results to tool_result blocks
// --------------------------------------------------------------------------
#[test]
fn anthropic_payload_converts_tool_results_to_tool_result_blocks() {
    let req = make_request(
        "claude-sonnet-4-20250514",
        vec![
            ChatMessage::User {
                content: "Read the file".to_string(),
            },
            ChatMessage::Assistant {
                content: Some("I'll read that file.".to_string()),
                tool_calls: vec![LlmToolCall {
                    id: "toolu_abc".to_string(),
                    name: "fs_read".to_string(),
                    arguments: r#"{"path":"src/main.rs"}"#.to_string(),
                }],
                reasoning_content: None,
            },
            ChatMessage::Tool {
                tool_call_id: "toolu_abc".to_string(),
                content: "fn main() { println!(\"hello\"); }".to_string(),
            },
        ],
        vec![],
    );
    let payload = anthropic::build_payload(&req, 4096).unwrap();
    let msgs = payload["messages"].as_array().unwrap();

    // Three messages: user, assistant, user (with tool_result)
    assert_eq!(msgs.len(), 3);

    // Assistant message has text + tool_use blocks
    let assistant = &msgs[1];
    assert_eq!(assistant["role"], "assistant");
    let assistant_blocks = assistant["content"].as_array().unwrap();
    assert_eq!(assistant_blocks.len(), 2);
    assert_eq!(assistant_blocks[0]["type"], "text");
    assert_eq!(assistant_blocks[0]["text"], "I'll read that file.");
    assert_eq!(assistant_blocks[1]["type"], "tool_use");
    assert_eq!(assistant_blocks[1]["id"], "toolu_abc");
    assert_eq!(assistant_blocks[1]["name"], "fs_read");
    assert_eq!(assistant_blocks[1]["input"], json!({"path": "src/main.rs"}));

    // Tool result is a user message with tool_result content block
    let tool_msg = &msgs[2];
    assert_eq!(tool_msg["role"], "user");
    let tool_blocks = tool_msg["content"].as_array().unwrap();
    assert_eq!(tool_blocks.len(), 1);
    assert_eq!(tool_blocks[0]["type"], "tool_result");
    assert_eq!(tool_blocks[0]["tool_use_id"], "toolu_abc");
    assert_eq!(
        tool_blocks[0]["content"],
        "fn main() { println!(\"hello\"); }"
    );
}
