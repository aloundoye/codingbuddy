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
                tool_name: None,
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

#[test]
fn gemini_round_trips_thought_signature_with_the_function_result() {
    let response = google::parse_response(
        &json!({
            "candidates": [{"content": {"parts": [{
                "functionCall": {"name": "fs_read", "args": {"path": "main.rs"}},
                "thoughtSignature": "opaque-provider-signature"
            }]}}]
        })
        .to_string(),
    )
    .unwrap();
    let id = response.tool_calls[0].id.clone();
    let request = make_request(
        "gemini-test",
        vec![
            ChatMessage::User {
                content: "Read main.rs".into(),
            },
            ChatMessage::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: response.tool_calls,
            },
            ChatMessage::Tool {
                tool_call_id: id,
                content: "fn main() {}".into(),
                tool_name: Some("fs_read".into()),
            },
        ],
        vec![sample_tool()],
    );
    let payload = google::build_payload(&request, 1024).unwrap();
    assert_eq!(
        payload["contents"][1]["parts"][0]["thoughtSignature"],
        "opaque-provider-signature"
    );
    assert_eq!(
        payload["contents"][2]["parts"][0]["functionResponse"]["name"],
        "fs_read"
    );
}

#[test]
fn openai_compatible_presets_stream_from_their_own_endpoint() {
    use codingbuddy_llm::{ApiClient, LlmClient};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    for (provider, expected_path) in [
        ("openai-compatible", "/v1/chat/completions"),
        ("deepseek", "/chat/completions"),
        ("openrouter", "/v1/chat/completions"),
        ("ollama", "/v1/chat/completions"),
        ("custom-fixture", "/chat/completions"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = codingbuddy_core::LlmConfig::default();
        if provider == "custom-fixture" {
            let mut profile = cfg.providers["openai-compatible"].clone();
            profile.openai_compat_prefix = false;
            cfg.providers.insert(provider.into(), profile);
        }
        cfg.select_provider(provider);
        let profile = cfg.providers.get_mut(provider).unwrap();
        profile.base_url = format!("http://{}", listener.local_addr().unwrap());
        if provider != "ollama" {
            profile.api_key_env = "CODINGBUDDY_FIXTURE_PROVIDER_KEY".into();
            cfg.api_key = Some("fixture-key".into());
        }
        let model = cfg.active_base_model();
        cfg.timeout_seconds = 3;
        cfg.max_retries = 0;
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut header = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                header.push_str(&line);
            }
            assert!(
                header.starts_with(&format!("POST {expected_path} ")),
                "{header}"
            );
            assert_eq!(header.contains("Bearer fixture-key"), provider != "ollama");
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["stream"], true);
            let events = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"ready\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{events}", events.len()).unwrap();
        });
        let request = make_request(
            &model,
            vec![ChatMessage::User {
                content: "Say ready".into(),
            }],
            vec![],
        );
        let response = ApiClient::new(cfg)
            .unwrap()
            .complete_chat_streaming(&request, std::sync::Arc::new(|_| {}));
        server.join().unwrap();
        assert_eq!(response.unwrap().text, "ready");
    }
}

#[test]
fn cancellation_releases_the_turn_while_waiting_for_response_headers() {
    use codingbuddy_llm::{ApiClient, LlmClient};
    use std::{net::TcpListener, sync::mpsc, time::Duration};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = codingbuddy_core::LlmConfig::default();
    cfg.select_provider("ollama");
    cfg.providers.get_mut("ollama").unwrap().base_url =
        format!("http://{}", listener.local_addr().unwrap());
    cfg.timeout_seconds = 5;
    let request = make_request(
        &cfg.active_base_model(),
        vec![ChatMessage::User {
            content: "Wait for the mock".into(),
        }],
        vec![],
    );
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (close_tx, close_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        accepted_tx.send(()).unwrap();
        close_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let token = codingbuddy_core::CancellationToken::new();
    let mut client = ApiClient::new(cfg).unwrap();
    client.set_cancel_token(token.clone());
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = client.complete_chat_streaming(&request, std::sync::Arc::new(|_| {}));
        let _ = done_tx.send(result);
    });
    accepted_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    token.cancel();
    let result = done_rx.recv_timeout(Duration::from_secs(1));
    close_tx.send(()).unwrap();
    server.join().unwrap();
    worker.join().unwrap();
    assert!(
        result
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("interrupted")
    );
}
