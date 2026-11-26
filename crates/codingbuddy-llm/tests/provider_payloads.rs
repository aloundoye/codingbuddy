//! E2E tests for multi-provider payload formats.
#![allow(clippy::unwrap_used)] // integration test helpers may panic
use codingbuddy_llm::LlmClientBlockingExt;

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
    let mut tool = sample_tool();
    tool.function.parameters["additionalProperties"] = json!(false);
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
        vec![tool],
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
    assert_eq!(
        declarations[0]["parametersJsonSchema"],
        req.tools[0].function.parameters
    );
    assert!(declarations[0].get("parameters").is_none());

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
                native_parts: None,
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
            }]},"finishReason":"STOP"}]
        })
        .to_string(),
    )
    .unwrap();
    let id = response.tool_calls[0].id.clone();
    assert!(!id.contains("opaque-provider-signature"));
    let native = response
        .transport_parts
        .into_iter()
        .find_map(|part| match part {
            codingbuddy_core::TransportPart::Native(native) => Some(native),
            _ => None,
        });
    let request = make_request(
        "gemini-test",
        vec![
            ChatMessage::User {
                content: "Read main.rs".into(),
            },
            ChatMessage::Assistant {
                content: None,
                native_parts: native,
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
    use codingbuddy_llm::ApiClient;
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
            .complete_chat_streaming_blocking(&request, std::sync::Arc::new(|_| {}));
        server.join().unwrap();
        assert_eq!(response.unwrap().text, "ready");
    }
}

#[test]
fn cancellation_releases_the_turn_while_waiting_for_response_headers() {
    use codingbuddy_llm::ApiClient;
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
        let result = client.complete_chat_streaming_blocking(&request, std::sync::Arc::new(|_| {}));
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

#[test]
fn delivery_one_requires_complete_calls_and_preserves_interleaving() {
    use codingbuddy_core::TransportPart;
    use codingbuddy_llm::ApiClient;

    let valid = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"before\",\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"fs_read\",\"arguments\":\"{\"}}]}}]}\n\n",
        "data: {\"choices\":[\n",
        "data: {\"delta\":{\"content\":\"between\",\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"fs_read\",\"arguments\":\"{}\"}},{\"index\":1,\"function\":{\"arguments\":\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let broken = [
        (
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "terminal response",
        ),
        ("data: {invalid}\n\n", "Malformed provider stream event"),
        (
            "event: error\ndata: {\"error\":{\"message\":\"fixture\"}}\n\n",
            "error during the response stream",
        ),
        (
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"fs_read\",\"arguments\":\"{\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "malformed tool arguments",
        ),
    ];
    for (events, failure) in std::iter::once((valid, "")).chain(broken) {
        let (endpoint, server) = delivery_one_loopback(events);
        let mut cfg = codingbuddy_core::LlmConfig::default();
        cfg.select_provider("ollama");
        cfg.providers.get_mut("ollama").unwrap().base_url = endpoint;
        cfg.max_retries = 0;
        let request = make_request(
            &cfg.active_base_model(),
            vec![ChatMessage::User {
                content: "test".into(),
            }],
            vec![ToolDefinition {
                function: FunctionDefinition {
                    parameters: json!({"type":"object"}),
                    ..sample_tool().function
                },
                ..sample_tool()
            }],
        );
        let result = ApiClient::new(cfg)
            .unwrap()
            .complete_chat_streaming_blocking(&request, std::sync::Arc::new(|_| {}));
        server.join().unwrap();
        if !failure.is_empty() {
            assert!(
                format!("{:#}", result.unwrap_err()).contains(failure),
                "{failure}"
            );
            continue;
        }
        let response = result.unwrap();
        assert_eq!(
            response
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert!(matches!(response.transport_parts.as_slice(), [
            TransportPart::Text(before), TransportPart::Call(b), TransportPart::Text(between), TransportPart::Call(a)
        ] if before == "before" && b == "b" && between == "between" && a == "a"));
    }
}

fn delivery_one_loopback(events: impl Into<String>) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let events = events.into();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
        }
        reader.read_exact(&mut vec![0; length]).unwrap();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}", events.len()).unwrap();
    });
    (endpoint, worker)
}

#[test]
fn gemini_stream_preserves_terminal_text_usage_and_tool_order() {
    use codingbuddy_core::{StreamChunk, TransportPart};
    use codingbuddy_llm::ApiClient;

    let first = json!({"candidates": [{"content": {"parts": [
        {"thought": true, "text": "private"},
        {"text": "Reading. "},
        {"functionCall": {"name": "fs_read", "args": {"path": "src/main.rs"}},
         "thoughtSignature": "signed"}
    ]}}]});
    let last = json!({
        "candidates": [{"content": {"parts": [{"text": "One moment."}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 7}
    });
    let (endpoint, server) = delivery_one_loopback(format!("data: {first}\n\ndata: {last}\n\n"));
    let mut config = codingbuddy_core::LlmConfig::default();
    config.select_provider("google");
    config.providers.get_mut("google").unwrap().base_url = endpoint;
    config.api_key = Some("fixture-key".into());
    let request = make_request(
        &config.active_base_model(),
        vec![ChatMessage::User {
            content: "Test".into(),
        }],
        vec![sample_tool()],
    );
    let chunks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = chunks.clone();
    let response = ApiClient::new(config)
        .unwrap()
        .complete_chat_streaming_blocking(
            &request,
            std::sync::Arc::new(move |event| {
                captured.lock().unwrap().push(event);
            }),
        )
        .unwrap();
    server.join().unwrap();

    assert_eq!(response.finish_reason, "tool_calls");
    assert_eq!(response.text, "Reading. One moment.");
    assert_eq!(response.usage.unwrap().completion_tokens, 7);
    assert_eq!(response.tool_calls.len(), 1);
    assert!(matches!(
        response.transport_parts.as_slice(),
        [TransportPart::Text(_), TransportPart::Call(_), TransportPart::Text(_), TransportPart::Native(native)]
            if native.blocks[2]["thoughtSignature"] == "signed"
    ));
    assert!(matches!(
        chunks.lock().unwrap().as_slice(),
        [StreamChunk::ContentDelta(first), StreamChunk::ContentDelta(last), StreamChunk::Done { reason: Some(reason) }]
            if first == "Reading. " && last == "One moment." && reason == "tool_calls"
    ));
}

#[test]
fn gemini_incomplete_stream_emits_visible_text_without_completion() {
    use codingbuddy_core::StreamChunk;
    use codingbuddy_llm::{ApiClient, error};

    for finish in [Some("MAX_TOKENS"), Some("SAFETY"), None] {
        let mut frame = json!({"candidates": [{"content": {"parts": [
            {"text": "Partial"},
            {"functionCall": {"name": "fs_read", "args": {"path": "src/main.rs"}}}
        ]}}]});
        if let Some(reason) = finish {
            frame["candidates"][0]["finishReason"] = json!(reason);
        }
        if finish == Some("MAX_TOKENS") {
            frame["candidates"][0]["content"]["parts"][1]["functionCall"]["args"] =
                json!("partial");
        }
        let (endpoint, server) = delivery_one_loopback(format!("data: {frame}\n\n"));
        let mut config = codingbuddy_core::LlmConfig::default();
        config.select_provider("google");
        config.providers.get_mut("google").unwrap().base_url = endpoint;
        config.api_key = Some("fixture-key".into());
        let request = make_request(
            &config.active_base_model(),
            vec![ChatMessage::User {
                content: "Test".into(),
            }],
            vec![sample_tool()],
        );
        let chunks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = chunks.clone();
        let failure = ApiClient::new(config)
            .unwrap()
            .complete_chat_streaming_blocking(
                &request,
                std::sync::Arc::new(move |event| {
                    captured.lock().unwrap().push(event);
                }),
            )
            .unwrap_err();
        server.join().unwrap();

        assert_eq!(
            failure
                .downcast_ref::<error::ProviderError>()
                .unwrap()
                .category,
            if finish.is_some() {
                error::FailureKind::IncompleteResponse
            } else {
                error::FailureKind::IncompleteStream
            }
        );
        assert!(matches!(
            chunks.lock().unwrap().as_slice(),
            [StreamChunk::ContentDelta(text)] if text == "Partial"
        ));
    }
}

#[test]
fn delivery_one_cancellation_closes_a_stalled_response_body() {
    use codingbuddy_llm::ApiClient;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::mpsc,
        time::Duration,
    };

    for responses in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            let event = if responses {
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"item\",\"content\":[]}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"item\",\"content_index\":0,\"delta\":\"first\"}\n\n"
            } else {
                "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n"
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n", event.len()).unwrap();
            closed_tx.send(stream.read(&mut [0; 1])).unwrap();
        });
        let token = codingbuddy_core::CancellationToken::new();
        let mut cfg = codingbuddy_core::LlmConfig::default();
        cfg.select_provider("ollama");
        cfg.providers.get_mut("ollama").unwrap().base_url = endpoint;
        if responses {
            cfg.providers.get_mut("ollama").unwrap().chat_protocol =
                Some("openai-responses".into());
        }
        let request = make_request(
            &cfg.active_base_model(),
            vec![ChatMessage::User {
                content: "test".into(),
            }],
            vec![],
        );
        let mut client = ApiClient::new(cfg).unwrap();
        client.set_cancel_token(token.clone());
        let response = client.complete_chat_streaming_blocking(
            &request,
            std::sync::Arc::new(move |chunk| {
                if matches!(chunk, codingbuddy_core::StreamChunk::ContentDelta(_)) {
                    token.cancel();
                }
            }),
        );
        assert!(format!("{:#}", response.unwrap_err()).contains("Turn interrupted"));
        let closed = closed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // Dropping a response with unread body bytes can close TCP with a reset
        // instead of FIN. Both prove cancellation released the connection;
        // a timeout or any additional request data must still fail this check.
        assert!(
            matches!(closed, Ok(0))
                || matches!(&closed, Err(error) if matches!(error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted)),
            "provider connection remained open after cancellation (responses={responses}): {closed:?}"
        );
        server.join().unwrap();
    }
}

#[test]
fn delivery_one_route_scopes_endpoint_model_and_account() {
    use codingbuddy_llm::session_route;
    let mut config = codingbuddy_core::LlmConfig::default();
    let key = b"workspace-only-random-route-key-fixture";
    let model = config.active_base_model();
    config.api_key = Some("fixture-first-credential".into());
    let first = session_route(&config, &model, key).unwrap();
    config.api_key = Some("fixture-rotated-credential".into());
    assert_ne!(first, session_route(&config, &model, key).unwrap());

    config.providers.get_mut("deepseek").unwrap().account_id =
        Some("fixture-stable-account".into());
    let pinned = session_route(&config, &model, key).unwrap();
    config.api_key = Some("fixture-third-credential".into());
    assert_eq!(pinned, session_route(&config, &model, key).unwrap());
    assert_ne!(
        pinned,
        session_route(&config, "different-model", key).unwrap()
    );
    config.endpoint = "http://127.0.0.1:8761/v1/chat/completions".into();
    assert_ne!(pinned, session_route(&config, &model, key).unwrap());
}

#[test]
fn delivery_one_anthropic_replays_signed_interleaved_blocks() {
    use codingbuddy_core::TransportPart;
    use codingbuddy_llm::ApiClient;
    let events = concat!(
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"private-fixture\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"opaque-fixture\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"fs_read\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"b\",\"name\":\"fs_read\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"b\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let (endpoint, server) = delivery_one_loopback(events);
    let mut config = codingbuddy_core::LlmConfig::default();
    config.select_provider("anthropic");
    config.providers.get_mut("anthropic").unwrap().base_url = endpoint;
    config.api_key = Some("fixture-key".into());
    let model = config.active_base_model();
    let user = ChatMessage::User {
        content: "Test".into(),
    };
    let request = make_request(&model, vec![user.clone()], vec![sample_tool()]);
    let response = ApiClient::new(config)
        .unwrap()
        .complete_chat_streaming_blocking(&request, std::sync::Arc::new(|_| {}))
        .unwrap();
    server.join().unwrap();
    assert!(response.text.is_empty());
    assert_eq!(response.tool_calls[0].arguments, r#"{"path":"a"}"#);
    assert_eq!(response.tool_calls[1].arguments, r#"{"path":"b"}"#);
    let native_parts = response
        .transport_parts
        .into_iter()
        .find_map(|part| match part {
            TransportPart::Native(native) => Some(native),
            _ => None,
        });
    let assistant = ChatMessage::Assistant {
        content: None,
        reasoning_content: None,
        native_parts,
        tool_calls: response.tool_calls,
    };
    assert!(
        !serde_json::to_string(&assistant)
            .unwrap()
            .contains("opaque-fixture")
    );
    let next = make_request(
        &model,
        vec![
            user,
            assistant,
            ChatMessage::Tool {
                tool_call_id: "a".into(),
                tool_name: None,
                content: "result-a".into(),
            },
            ChatMessage::Tool {
                tool_call_id: "b".into(),
                tool_name: None,
                content: "result-b".into(),
            },
        ],
        vec![sample_tool()],
    );
    let payload = anthropic::build_payload(&next, 1024).unwrap();
    assert_eq!(
        payload["messages"][1]["content"][0]["signature"],
        "opaque-fixture"
    );
    assert_eq!(payload["messages"][2]["content"][0]["tool_use_id"], "a");
    assert_eq!(payload["messages"][2]["content"][1]["tool_use_id"], "b");
}

#[test]
fn responses_stateless_payload_keeps_wire_ids_and_private_order() {
    use codingbuddy_core::NativeParts;
    use codingbuddy_llm::providers::responses;
    let blocks = vec![
        json!({"type":"reasoning","id":"reason-item","encrypted_content":"PRIVATE","summary":[]}),
        json!({"type":"function_call","id":"provider-item","call_id":"provider-call","name":"fs_read","arguments":"{}","status":"completed"}),
    ];
    let mut req = make_request(
        "gpt-4.1-mini",
        vec![
            ChatMessage::System {
                content: "instructions".into(),
            },
            ChatMessage::User {
                content: "read".into(),
            },
            ChatMessage::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![],
                native_parts: Some(NativeParts {
                    protocol: responses::PROTOCOL.into(),
                    blocks: blocks.clone(),
                }),
            },
            ChatMessage::Tool {
                tool_call_id: "provider-call".into(),
                tool_name: Some("fs_read".into()),
                content: "result".into(),
            },
        ],
        vec![sample_tool()],
    );
    req.response_format = Some(
        json!({"type":"json_schema","json_schema":{"name":"answer","strict":true,"schema":{"type":"object"}}}),
    );
    let payload = responses::build_payload(&req).unwrap();
    assert_eq!(payload["store"], false);
    assert_eq!(payload["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(payload["instructions"], "instructions");
    assert_eq!(payload["input"][1], blocks[0]);
    assert_eq!(payload["input"][2], blocks[1]);
    assert_eq!(payload["input"][3]["call_id"], "provider-call");
    assert_eq!(payload["tools"][0]["name"], "fs_read");
    assert_eq!(payload["text"]["format"]["type"], "json_schema");
    assert!(payload.get("previous_response_id").is_none());
    assert!(payload.get("messages").is_none());
}

#[test]
fn responses_interleaved_calls_require_a_successful_terminal() {
    use codingbuddy_llm::providers::responses::Stream;
    let mut stream = Stream::default();
    let mut output = vec![];
    for index in 0..2 {
        let item = json!({"type":"function_call","id":format!("item-{index}"),"call_id":format!("call-{index}"),"name":"fs_read","arguments":""});
        stream
            .frame(
                &json!({"type":"response.output_item.added","output_index":index,"item":item}),
                &|_| {},
            )
            .unwrap();
        let mut item = item;
        item["arguments"] = json!("{}");
        item["status"] = json!("completed");
        output.push(item);
    }
    for index in [1, 0] {
        for delta in ["{", "}"] {
            assert!(stream.frame(&json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":format!("item-{index}"),"delta":delta}), &|_| {}).unwrap().is_none());
        }
        assert!(stream.frame(&json!({"type":"response.output_item.done","output_index":index,"item":output[index]}), &|_| {}).unwrap().is_none());
    }
    let completed =
        json!({"type":"response.completed","response":{"status":"completed","output":output}});
    let result = stream.frame(&completed, &|_| {}).unwrap().unwrap();
    assert_eq!(
        result
            .tool_calls
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["call-0", "call-1"]
    );
    for bad in [
        json!({"type":"response.failed"}),
        json!({"type":"response.incomplete"}),
        json!({"type":"response.cancelled"}),
        json!({"type":"error"}),
        json!({"type":"response.completed","response":{"status":"completed","output":output}}),
    ] {
        assert!(Stream::default().frame(&bad, &|_| {}).is_err());
    }
}

#[test]
fn responses_reject_changed_item_ids_and_arguments() {
    use codingbuddy_llm::providers::responses::Stream;
    for changed in [
        json!({"item_id":"changed","delta":"{}"}),
        json!({"item_id":"item","arguments":"different"}),
    ] {
        let mut stream = Stream::default();
        let item = json!({"id":"item","type":"function_call","call_id":"call","name":"fs_read","arguments":""});
        let added = json!({"type":"response.output_item.added","output_index":0,"item":item});
        stream.frame(&added, &|_| {}).unwrap();
        assert!(stream.frame(&added, &|_| {}).is_err());
        let mut event = changed;
        event["output_index"] = json!(0);
        event["type"] = json!(if event.get("delta").is_some() {
            "response.function_call_arguments.delta"
        } else {
            "response.function_call_arguments.done"
        });
        assert!(stream.frame(&event, &|_| {}).is_err());
    }
}

#[test]
fn responses_errors_are_typed_and_sanitized_over_real_transport() {
    use codingbuddy_llm::{
        ApiClient,
        error::{FailureKind, ProviderError},
    };
    for (events, expected) in [
        ("data: {invalid}\n\n", FailureKind::MalformedResponse),
        ("data: [DONE]\n\n", FailureKind::MalformedResponse),
        (
            "data: {\"type\":\"response.incomplete\"}\n\n",
            FailureKind::IncompleteResponse,
        ),
        (
            "data: {\"type\":\"response.failed\"}\n\n",
            FailureKind::ProviderFailure,
        ),
        (
            "data: {\"type\":\"error\",\"message\":\"SECRET\"}\n\n",
            FailureKind::ProviderFailure,
        ),
        (
            "data: {\"type\":\"response.created\"}\n\n",
            FailureKind::IncompleteStream,
        ),
    ] {
        let (base, worker) = delivery_one_loopback(events);
        let mut cfg = codingbuddy_core::LlmConfig::default();
        cfg.select_provider("openai-compatible");
        let provider = cfg.providers.get_mut("openai-compatible").unwrap();
        provider.base_url = base;
        provider.chat_protocol = Some("openai-responses".into());
        cfg.api_key = Some("synthetic-secret".into());
        cfg.max_retries = 0;
        let req = make_request(
            "gpt-4.1-mini",
            vec![ChatMessage::User {
                content: "hello".into(),
            }],
            vec![sample_tool()],
        );
        let error = ApiClient::new(cfg)
            .unwrap()
            .complete_chat_streaming_blocking(&req, std::sync::Arc::new(|_| {}))
            .unwrap_err();
        worker.join().unwrap();
        assert!(!error.to_string().contains("SECRET"));
        assert_eq!(
            error.downcast_ref::<ProviderError>().unwrap().category,
            expected
        );
    }
}

#[test]
fn responses_publish_confirmed_text_when_the_provider_sends_no_text_deltas() {
    use codingbuddy_core::StreamChunk;
    use codingbuddy_llm::ApiClient;

    let item = json!({
        "id": "message", "type": "message", "status": "completed",
        "content": [{"type": "output_text", "text": "Complete response."}]
    });
    let events = [
        json!({"type": "response.output_item.added", "output_index": 0, "item": {
            "id": "message", "type": "message", "content": []
        }}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"status": "completed", "output": [item]}}),
    ];
    let (base, worker) = delivery_one_loopback(
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>(),
    );
    let mut cfg = codingbuddy_core::LlmConfig::default();
    cfg.select_provider("openai-compatible");
    let provider = cfg.providers.get_mut("openai-compatible").unwrap();
    provider.base_url = base;
    provider.chat_protocol = Some("openai-responses".into());
    cfg.api_key = Some("fixture-key".into());
    let req = make_request(
        "gpt-4.1-mini",
        vec![ChatMessage::User {
            content: "hello".into(),
        }],
        vec![],
    );
    let chunks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = chunks.clone();

    let response = ApiClient::new(cfg)
        .unwrap()
        .complete_chat_streaming_blocking(
            &req,
            std::sync::Arc::new(move |chunk| captured.lock().unwrap().push(chunk)),
        )
        .unwrap();
    worker.join().unwrap();
    assert!(matches!(
        chunks.lock().unwrap().as_slice(),
        [StreamChunk::ContentDelta(text), StreamChunk::Done { .. }] if text == &response.text
    ));
    assert_eq!(response.text, "Complete response.");
}

#[test]
fn native_missing_usage_stays_unavailable() {
    assert!(
        anthropic::parse_response(
            r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#
        )
        .unwrap()
        .usage
        .is_none()
    );
    assert!(
        google::parse_response(
            r#"{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"ok"}]}}]}"#
        )
        .unwrap()
        .usage
        .is_none()
    );
}

#[test]
fn responses_output_limit_and_refusal_have_distinct_outcomes() {
    use codingbuddy_llm::{
        error::{FailureKind, ProviderError},
        providers::responses,
    };
    let error = responses::parse_response(&json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]})).unwrap_err();
    let typed = error.downcast_ref::<ProviderError>().unwrap();
    assert_eq!(typed.category, FailureKind::IncompleteResponse);
    assert_eq!(typed.provider_code.as_deref(), Some("max_output_tokens"));
    let refusal = responses::parse_response(&json!({"status":"completed","output":[{"type":"message","status":"completed","content":[{"type":"refusal","refusal":"Cannot help."}]}]})).unwrap();
    assert_eq!(refusal.text, "Cannot help.");
    assert!(refusal.tool_calls.is_empty());
}

#[test]
fn unreachable_route_reports_transport_failure_without_provider_text() {
    use codingbuddy_llm::{
        ApiClient,
        error::{FailureKind, ProviderError},
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut cfg = codingbuddy_core::LlmConfig::default();
    cfg.select_provider("ollama");
    cfg.providers.get_mut("ollama").unwrap().base_url = format!("http://{address}");
    cfg.max_retries = 0;
    cfg.timeout_seconds = 2;
    let req = make_request(
        &cfg.active_base_model(),
        vec![ChatMessage::User {
            content: "test".into(),
        }],
        vec![],
    );
    let client = ApiClient::new(cfg).unwrap();
    for streaming in [false, true] {
        let error = if streaming {
            client.complete_chat_streaming_blocking(&req, std::sync::Arc::new(|_| {}))
        } else {
            client.complete_chat_blocking(&req)
        }
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProviderError>().unwrap().category,
            FailureKind::Transport
        );
        assert!(!error.to_string().contains(&address.to_string()));
    }
}
