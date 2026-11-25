//! Real transport, agent, and SQLite conformance for each certified route.
#![allow(clippy::unwrap_used)] // integration test helpers may panic
use anyhow::{Context, Result, ensure};
use codingbuddy_agent::{AgentEngine, ChatOptions};
use codingbuddy_core::{AppConfig, ChatMessage, EventKind};
use codingbuddy_llm::{certification, certification::FixtureRoute};
use codingbuddy_store::Store;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .expect("fixture worker panicked")
                .expect("fixture server");
        }
    }
}

fn configure(
    cfg: &mut codingbuddy_core::LlmConfig,
    route: &FixtureRoute,
    server: &Mock,
) -> Result<()> {
    cfg.select_provider(&route.provider);
    if route.provider == "custom-gateway" {
        let mut profile = cfg.active_provider();
        profile.kind = "openai-compatible".into();
        cfg.providers.insert(route.provider.clone(), profile);
    }
    let profile = cfg
        .providers
        .get_mut(&route.provider)
        .with_context(|| format!("Missing fixture profile for {}", route.provider))?;
    let suffix = reqwest::Url::parse(&profile.base_url)
        .ok()
        .map(|base| base.path().trim_end_matches('/').to_owned())
        .unwrap_or_default();
    profile.base_url = format!("{}{suffix}", server.base);
    profile.openai_compat_prefix = true;
    profile.chat_protocol = Some(route.protocol.clone());
    profile.auth_strategy = route.cloud.is_none().then(|| route.auth.clone());
    profile.cloud.clone_from(&route.cloud);
    if route.provider == "custom-gateway" {
        profile.gateway = Some(codingbuddy_core::GatewayProfile {
            chat_path: route.path.clone(),
            omit_stream_usage_option: true,
        });
        profile
            .headers
            .insert("X-Fixture-Profile".into(), "configured".into());
    }
    profile.account_id = route.cloud.as_ref().map(|_| "fixture-identity".into());
    profile.models.chat.clone_from(&route.model);
    cfg.api_key = Some("synthetic-certification-key".into());
    cfg.max_retries = 0;
    cfg.timeout_seconds = 5;
    Ok(())
}

fn fixture_client(
    cfg: codingbuddy_core::LlmConfig,
    server: &Mock,
) -> Result<codingbuddy_llm::ApiClient> {
    let cloud = cfg.active_provider().cloud.is_some();
    let client = codingbuddy_llm::ApiClient::new(cfg)?;
    if cloud {
        client.with_fixture_origin(&server.base)
    } else {
        Ok(client)
    }
}

fn bedrock_stream(response: &Value) -> Result<Vec<u8>> {
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    let mut output = Vec::new();
    let mut event = |kind: &str, value: Value| -> Result<()> {
        let message = Message::new_from_parts(
            vec![
                Header::new(":message-type", HeaderValue::String("event".into())),
                Header::new(":event-type", HeaderValue::String(kind.to_owned().into())),
            ],
            serde_json::to_vec(&value)?,
        );
        aws_smithy_eventstream::frame::write_message_to(&message, &mut output)?;
        Ok(())
    };
    event("messageStart", json!({"role":"assistant"}))?;
    for (index, block) in response["output"]["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        if let Some(tool) = block.get("toolUse") {
            event(
                "contentBlockStart",
                json!({"contentBlockIndex":index,"start":{"toolUse":{"name":tool["name"],"toolUseId":tool["toolUseId"]}}}),
            )?;
            let args = tool["input"].to_string();
            for input in [&args[..1], &args[1..]] {
                event(
                    "contentBlockDelta",
                    json!({"contentBlockIndex":index,"delta":{"toolUse":{"input":input}}}),
                )?;
            }
        } else {
            let delta = if let Some(reason) = block.get("reasoningContent") {
                json!({"reasoningContent":reason["reasoningText"]})
            } else {
                block.clone()
            };
            event(
                "contentBlockDelta",
                json!({"contentBlockIndex":index,"delta":delta}),
            )?;
        }
        event("contentBlockStop", json!({"contentBlockIndex":index}))?;
    }
    event("messageStop", json!({"stopReason":response["stopReason"]}))?;
    event("metadata", json!({"usage":response["usage"]}))?;
    Ok(output)
}

fn completion(route: &FixtureRoute, round: usize) -> Value {
    let call = round < 2;
    let id = if round == 0 {
        "call-first"
    } else {
        "call-second"
    };
    let path = if round == 0 {
        "first.txt"
    } else {
        "second.txt"
    };
    let text = "Certification complete.";
    let args = json!({"path":path});
    match route.protocol.as_str() {
        "openai-responses" => {
            json!({"id":format!("response-{round}"),"status":"completed","output":if call {
            vec![json!({"id":format!("reason-{round}"),"type":"reasoning","summary":[],"encrypted_content":"opaque-certification-fixture"}),
                 json!({"id":format!("item-{round}"),"type":"function_call","status":"completed","call_id":id,"name":"fs_read","arguments":args.to_string()})]
        } else {vec![json!({"id":format!("message-{round}"),"type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]})]},
            "usage":{"input_tokens":20,"output_tokens":8,"input_tokens_details":{"cached_tokens":5},"output_tokens_details":{"reasoning_tokens":3}}})
        }
        "bedrock-converse" => json!({
            "output":{"message":{"role":"assistant","content": if call { vec![
                json!({"reasoningContent":{"reasoningText":{"text":"private fixture","signature":"opaque-certification-fixture"}}}),
                json!({"toolUse":{"toolUseId":id,"name":"fs_read","input":args}})
            ] } else { vec![json!({"text":text})] }}},
            "stopReason":if call {"tool_use"} else {"end_turn"},
            "usage":{"inputTokens":10,"cacheReadInputTokens":5,"cacheWriteInputTokens":5,"outputTokens":8,"totalTokens":28}
        }),
        "anthropic-messages" => {
            json!({"id":format!("message-{round}"),"type":"message","role":"assistant","stop_reason":if call {"tool_use"} else {"end_turn"},
            "content":if call {vec![json!({"type":"thinking","thinking":"private fixture","signature":"opaque-certification-fixture"}),
                json!({"type":"redacted_thinking","data":"opaque-certification-fixture"}),
                json!({"type":"tool_use","id":id,"name":"fs_read","input":args})]} else {vec![json!({"type":"text","text":text})]},
            "usage":{"input_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":5,"output_tokens":8}})
        }
        "gemini-generate-content" => {
            json!({"candidates":[{"index":0,"finishReason":"STOP","content":{"role":"model","parts":if call {
            vec![json!({"thought":true,"text":"private fixture","thoughtSignature":"opaque-certification-fixture"}),
                json!({"functionCall":{"id":id,"name":"fs_read","args":args},"thoughtSignature":"opaque-certification-fixture"})]
        } else {vec![json!({"text":text})]}}}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":8,"cachedContentTokenCount":5,"thoughtsTokenCount":3}})
        }
        _ => json!({"choices":[{"index":0,"finish_reason":if call {"tool_calls"} else {"stop"},
            "message":if call {json!({"content":null,"reasoning_content":"opaque-certification-fixture","tool_calls":[{"id":id,"type":"function","function":{"name":"fs_read","arguments":args.to_string()}}]})} else {json!({"content":text})}}],
            "usage":{"prompt_tokens":20,"completion_tokens":8,"prompt_tokens_details":{"cached_tokens":5},"completion_tokens_details":{"reasoning_tokens":3}}}),
    }
}

fn sse(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn stream(route: &FixtureRoute, response: &Value) -> String {
    match route.protocol.as_str() {
        "openai-responses" => {
            let mut frames = String::new();
            for (index, item) in response["output"].as_array().unwrap().iter().enumerate() {
                let mut start = item.clone();
                if item["type"] == "function_call" {
                    start["arguments"] = json!("");
                }
                frames += &sse(
                    json!({"type":"response.output_item.added","output_index":index,"item":start}),
                );
                if item["type"] == "function_call" {
                    let args = item["arguments"].as_str().unwrap();
                    for delta in [&args[..1], &args[1..]] {
                        frames += &sse(
                            json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":item["id"],"delta":delta}),
                        );
                    }
                    frames += &sse(
                        json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":item["id"],"arguments":item["arguments"]}),
                    );
                } else if item["type"] == "message" {
                    frames += &sse(
                        json!({"type":"response.output_text.delta","output_index":index,"item_id":item["id"],"content_index":0,"delta":item["content"][0]["text"]}),
                    );
                }
                frames += &sse(
                    json!({"type":"response.output_item.done","output_index":index,"item":item}),
                );
            }
            frames + &sse(json!({"type":"response.completed","response":response}))
        }
        "anthropic-messages" => {
            let mut frames =
                sse(json!({"type":"message_start","message":{"usage":response["usage"]}}));
            for (index, block) in response["content"].as_array().unwrap().iter().enumerate() {
                frames +=
                    &sse(json!({"type":"content_block_start","index":index,"content_block":block}));
                frames += &sse(json!({"type":"content_block_stop","index":index}));
            }
            frames += &sse(
                json!({"type":"message_delta","delta":{"stop_reason":response["stop_reason"]},"usage":{"output_tokens":8}}),
            );
            frames + &sse(json!({"type":"message_stop"}))
        }
        "gemini-generate-content" => sse(response.clone()),
        _ => {
            let mut delta = response["choices"][0]["message"].clone();
            if let Some(calls) = delta["tool_calls"].as_array_mut() {
                for (index, call) in calls.iter_mut().enumerate() {
                    call["index"] = json!(index);
                }
            }
            sse(
                json!({"choices":[{"index":0,"delta":delta,"finish_reason":response["choices"][0]["finish_reason"]}],"usage":response["usage"]}),
            ) + "data: [DONE]\n\n"
        }
    }
}

fn mock(route: FixtureRoute) -> Result<Mock> {
    mock_outcome(route, None)
}

fn mock_outcome(route: FixtureRoute, override_payload: Option<String>) -> Result<Mock> {
    mock_config(route, override_payload, false)
}

fn mock_config(
    route: FixtureRoute,
    override_payload: Option<String>,
    live_probe: bool,
) -> Result<Mock> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let base = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let collected = requests.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let worker = std::thread::spawn(move || -> Result<()> {
        while !stopped.load(Ordering::SeqCst) {
            let (mut socket, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            socket.set_nonblocking(false)?;
            socket.set_read_timeout(Some(Duration::from_secs(15)))?;
            let mut reader = BufReader::new(&mut socket);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                ensure!(reader.read_line(&mut line)? > 0, "incomplete mock request");
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse()?;
                }
                head += &line;
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            let body: Value = serde_json::from_slice(&bytes)?;
            let streaming = body["stream"] == true
                || head.contains(":streamGenerateContent")
                || head.contains("/converse-stream ");
            let path = if streaming && route.protocol == "gemini-generate-content" {
                route
                    .path
                    .replace(":generateContent", ":streamGenerateContent?alt=sse")
            } else if streaming && route.protocol == "bedrock-converse" {
                format!("{}-stream", route.path)
            } else {
                route.path.clone()
            };
            ensure!(
                head.starts_with(&format!("POST {path} ")),
                "unexpected endpoint {head}"
            );
            let headers = head.to_lowercase();
            if route.provider == "custom-gateway" {
                ensure!(
                    headers.contains("x-fixture-profile: configured")
                        && body.get("stream_options").is_none(),
                    "configured gateway options missing"
                );
            }
            match route.auth.as_str() {
                "azure-api-key" => ensure!(headers.contains("\r\napi-key: synthetic-certification-key") || headers.contains("\napi-key: synthetic-certification-key"),
                    "missing Azure API key"),
                "aws-sigv4" => ensure!(
                    headers.contains("authorization: aws4-hmac-sha256 credential=syntheticaccess/20231114/us-east-1/bedrock/aws4_request")
                    && headers.contains("x-amz-security-token: synthetic-session"),
                    "missing Bedrock signature or session token"),
                "none" => ensure!(
                    !headers.contains("authorization:") && !headers.contains("x-api-key:"),
                    "unexpected auth"
                ),
                "x-api-key" => ensure!(
                    headers.contains("x-api-key: synthetic-certification-key"),
                    "missing Anthropic auth"
                ),
                "query-api-key" => ensure!(
                    headers.contains("x-goog-api-key: synthetic-certification-key"),
                    "missing Gemini auth"
                ),
                _ => ensure!(
                    headers.contains("authorization: bearer synthetic-certification-key"),
                    "missing bearer auth"
                ),
            }
            if route.provider == "azure" {
                ensure!(
                    body["model"] == "fixture-deploy",
                    "Azure sent the model profile in place of the deployment"
                );
            }
            let mut requests = collected.lock().unwrap();
            let round = requests.len();
            if round > 0 {
                let wire = body.to_string();
                let expected = if round == 1 {
                    "first fixture"
                } else {
                    "second fixture"
                };
                ensure!(
                    wire.contains(expected),
                    "missing linked tool result in {wire}"
                );
                if matches!(
                    route.protocol.as_str(),
                    "openai-responses"
                        | "anthropic-messages"
                        | "gemini-generate-content"
                        | "bedrock-converse"
                ) {
                    ensure!(
                        wire.contains("opaque-certification-fixture"),
                        "missing private continuation"
                    );
                }
            }
            if route.provider == "deepseek" && round > 0 {
                ensure!(
                    body.to_string().contains("opaque-certification-fixture") == (round < 3),
                    "DeepSeek reasoning lifecycle changed"
                );
            }
            if round == 3 {
                ensure!(
                    body.to_string().contains("Certification complete."),
                    "missing restart history"
                );
            }
            requests.push(body);
            drop(requests);
            let mut response = completion(&route, round);
            if live_probe && round == 2 {
                response = serde_json::from_str(
                    &response
                        .to_string()
                        .replace("Certification complete.", "VERIFIED"),
                )?;
            }
            if streaming && route.protocol == "bedrock-converse" {
                let mut payload = bedrock_stream(&response)?;
                if override_payload.as_deref() == Some("truncate-bedrock") {
                    payload.truncate(payload.len().saturating_sub(8));
                }
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                )?;
                socket.write_all(&payload)?;
                continue;
            }
            let (content_type, payload) = if let Some(payload) = &override_payload {
                (
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                    payload.clone(),
                )
            } else if streaming {
                ("text/event-stream", stream(&route, &response))
            } else {
                ("application/json", response.to_string())
            };
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            )?;
        }
        Ok(())
    });
    Ok(Mock {
        base,
        requests,
        stop,
        worker: Some(worker),
    })
}

#[test]
fn routes_complete_two_tool_rounds_and_restart() -> Result<()> {
    let mut evidence = Vec::new();
    for route in certification::routes() {
        for streaming in [false, true] {
            let server = mock(route.clone())?;
            let workspace = tempfile::tempdir()?;
            std::fs::write(workspace.path().join("first.txt"), "first fixture")?;
            std::fs::write(workspace.path().join("second.txt"), "second fixture")?;
            let mut cfg = AppConfig::default();
            configure(&mut cfg.llm, &route, &server)?;
            cfg.llm.stream = streaming;
            cfg.workspace_trust = Some(false);
            cfg.agent_loop.phase_loop_enabled = false;
            let engine = AgentEngine::new_with_config_and_llm(
                workspace.path(),
                cfg.clone(),
                Box::new(fixture_client(cfg.llm.clone(), &server)?),
            )?;
            let output = engine.chat_with_options(
                "Read first.txt then second.txt.",
                ChatOptions {
                    tools: true,
                    disable_team_orchestration: true,
                    ..Default::default()
                },
            )?;
            ensure!(
                output == "Certification complete.",
                "unexpected output for {}",
                route.id
            );
            drop(engine);
            let store = Store::new(workspace.path())?;
            let session = store.load_latest_session()?.context("session")?.session_id;
            let history = store.rebuild_from_events(session)?;
            ensure!(
                history
                    .chat_messages
                    .iter()
                    .filter(|message| matches!(message, ChatMessage::Tool { .. }))
                    .count()
                    == 2,
                "tool settlements missing"
            );
            let linked: Vec<_> = history
                .chat_messages
                .iter()
                .filter_map(|message| match message {
                    ChatMessage::Tool {
                        tool_call_id,
                        content,
                        ..
                    } => Some((
                        tool_call_id.as_str(),
                        content.contains("first fixture"),
                        content.contains("second fixture"),
                    )),
                    _ => None,
                })
                .collect();
            ensure!(
                linked == [("call-first", true, false), ("call-second", false, true)],
                "tool results paired incorrectly"
            );
            ensure!(
                !serde_json::to_string(&history.chat_messages)?
                    .contains("opaque-certification-fixture"),
                "portable history leaked private metadata"
            );
            let public = serde_json::to_string(&store.session_events(session)?)?;
            // Only public model events are checked. Operational events retain tool evidence.
            for event in store.session_events(session)? {
                if let EventKind::ModelStep { .. } = &event.kind {
                    ensure!(
                        !serde_json::to_string(&event)?.contains("opaque-certification-fixture"),
                        "private data leaked"
                    );
                }
            }
            let route_key =
                codingbuddy_llm::session_route(&cfg.llm, &route.model, &store.route_key()?)?;
            ensure!(
                store
                    .provider_history(session, &route_key)?
                    .chat_messages
                    .iter()
                    .any(|message| matches!(
                        message,
                        ChatMessage::Assistant {
                            native_parts: Some(_),
                            ..
                        }
                    )),
                "private replay missing"
            );
            ensure!(
                store.provider_history(session, "foreign-route").is_err(),
                "foreign replay accepted"
            );
            ensure!(
                public.contains("call-first") && public.contains("call-second"),
                "call IDs lost"
            );
            let client = fixture_client(cfg.llm.clone(), &server)?;
            let resumed =
                AgentEngine::new_with_config_and_llm(workspace.path(), cfg, Box::new(client))?;
            resumed.chat_with_options(
                "Continue.",
                ChatOptions {
                    tools: true,
                    session_id: Some(session),
                    disable_team_orchestration: true,
                    ..Default::default()
                },
            )?;
            ensure!(
                server.requests.lock().unwrap().len() == 4,
                "duplicate model request"
            );
            evidence.push(
                json!({"id":route.id,"model":route.model,"streaming":streaming,"passed":true}),
            );
        }
    }
    if let Some(path) = std::env::var_os("CODINGBUDDY_CERTIFICATION_REPORT") {
        let report = json!({"schema_version":1,"suite":"delivery-order-4","implementation_digest":certification::implementation_digest(),
            "revision":option_env!("CODINGBUDDY_BUILD_REVISION"),"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"passed":true,"routes":evidence});
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    }
    Ok(())
}

#[test]
fn route_usage_and_private_parts_are_normalized() -> Result<()> {
    use codingbuddy_core::{ChatRequest, ToolChoice};
    use codingbuddy_llm::LlmClient;
    for route in certification::routes() {
        for streaming in [false, true] {
            let server = mock(route.clone())?;
            let mut cfg = codingbuddy_core::LlmConfig::default();
            configure(&mut cfg, &route, &server)?;
            cfg.stream = streaming;
            cfg.max_retries = 0;
            let req = ChatRequest {
                model: route.model.clone(),
                messages: vec![ChatMessage::User {
                    content: "test".into(),
                }],
                tools: vec![codingbuddy_core::ToolDefinition {
                    tool_type: "function".into(),
                    function: codingbuddy_core::FunctionDefinition {
                        name: "fs_read".into(),
                        description: "Read a file".into(),
                        strict: None,
                        parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                    },
                }],
                tool_choice: ToolChoice::auto(),
                max_tokens: 512,
                temperature: None,
                top_p: None,
                presence_penalty: None,
                frequency_penalty: None,
                logprobs: None,
                top_logprobs: None,
                thinking: None,
                images: vec![],
                provider_options: Default::default(),
                response_format: None,
            };
            let client = fixture_client(cfg, &server)?;
            let response = if streaming {
                client.complete_chat_streaming(&req, Arc::new(|_| {}))?
            } else {
                client.complete_chat(&req)?
            };
            let usage = response.usage.context("missing fixture usage")?;
            ensure!(
                usage.prompt_tokens == 20
                    && usage.completion_tokens == 8
                    && usage.prompt_cache_hit_tokens == 5,
                "incorrect usage for {} streaming={streaming}: {usage:?}",
                route.id
            );
            if !matches!(route.provider.as_str(), "anthropic" | "bedrock") {
                ensure!(usage.reasoning_tokens == 3, "reasoning usage missing");
            }
            ensure!(
                response.tool_calls[0].id == "call-first",
                "provider ID was replaced"
            );
            ensure!(
                !response.text.contains("opaque"),
                "private reasoning exposed"
            );
        }
    }
    Ok(())
}

#[test]
fn unfinished_responses_never_dispatch_tools() -> Result<()> {
    let route = certification::routes()
        .into_iter()
        .find(|route| route.id == "openai-responses")
        .context("Responses fixture")?;
    let completed = completion(&route, 0);
    let frames = stream(&route, &completed);
    let terminal = sse(json!({"type":"response.completed","response":completed}));
    let prefix = frames.strip_suffix(&terminal).context("terminal frame")?;
    let mut invalid_schema = completed.clone();
    invalid_schema["output"][1]["arguments"] = json!("{\"wrong\":\"path\"}");
    for payload in [
        stream(&route, &invalid_schema),
        prefix.to_owned(),
        format!("{prefix}data: [DONE]\n\n"),
        format!("{prefix}{}", sse(json!({"type":"response.failed"}))),
        format!("{prefix}{}", sse(json!({"type":"response.incomplete"}))),
        format!("{prefix}data: {{invalid}}\n\n"),
        format!(
            "{prefix}{}",
            sse(json!({"type":"error","message":"PRIVATE"}))
        ),
    ] {
        let server = mock_outcome(route.clone(), Some(payload))?;
        let workspace = tempfile::tempdir()?;
        let mut cfg = AppConfig::default();
        cfg.llm.select_provider(&route.provider);
        let profile = cfg
            .llm
            .providers
            .get_mut(&route.provider)
            .context("profile")?;
        profile.base_url = server.base.clone();
        profile.chat_protocol = Some(route.protocol.clone());
        profile.models.chat = route.model.clone();
        cfg.llm.api_key = Some("synthetic-certification-key".into());
        cfg.llm.max_retries = 0;
        cfg.llm.stream = true;
        cfg.workspace_trust = Some(false);
        cfg.agent_loop.phase_loop_enabled = false;
        let engine = AgentEngine::new_with_config(workspace.path(), cfg)?;
        ensure!(
            engine
                .chat_with_options(
                    "Read first.txt.",
                    ChatOptions {
                        tools: true,
                        disable_team_orchestration: true,
                        ..Default::default()
                    }
                )
                .is_err(),
            "unfinished response accepted"
        );
        let store = Store::new(workspace.path())?;
        let session = store.load_latest_session()?.context("session")?.session_id;
        ensure!(
            store
                .rebuild_from_events(session)?
                .chat_messages
                .iter()
                .all(|message| !matches!(message, ChatMessage::Tool { .. })),
            "unfinished call executed"
        );
        ensure!(
            store
                .session_events(session)?
                .iter()
                .all(|event| !matches!(&event.kind, EventKind::ModelStep { .. })),
            "unconfirmed model step committed"
        );
        ensure!(
            server.requests.lock().unwrap().len() == 1,
            "failed stream retried"
        );
    }
    Ok(())
}

#[test]
fn incomplete_cloud_streams_never_dispatch_tools() -> Result<()> {
    for route in certification::routes()
        .into_iter()
        .filter(|route| route.cloud.is_some())
    {
        let mut response = completion(&route, 0);
        let payload = match route.protocol.as_str() {
            "bedrock-converse" => "truncate-bedrock".into(),
            "gemini-generate-content" => {
                response["candidates"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("finishReason");
                sse(response)
            }
            _ => {
                response["choices"][0]["finish_reason"] = Value::Null;
                stream(&route, &response).replace("data: [DONE]\n\n", "")
            }
        };
        let server = mock_outcome(route.clone(), Some(payload))?;
        let workspace = tempfile::tempdir()?;
        let mut cfg = AppConfig {
            workspace_trust: Some(false),
            ..Default::default()
        };
        configure(&mut cfg.llm, &route, &server)?;
        cfg.llm.stream = true;
        cfg.agent_loop.phase_loop_enabled = false;
        let client = fixture_client(cfg.llm.clone(), &server)?;
        let engine = AgentEngine::new_with_config_and_llm(workspace.path(), cfg, Box::new(client))?;
        let result = engine.chat_with_options(
            "Read first.txt.",
            ChatOptions {
                tools: true,
                ..Default::default()
            },
        );
        ensure!(result.is_err(), "{} accepted incomplete output", route.id);
        let store = Store::new(workspace.path())?;
        let session = store.load_latest_session()?.context("missing session")?;
        let state = store.rebuild_from_events(session.session_id)?;
        ensure!(
            state.tool_invocations.is_empty(),
            "{} dispatched an unfinished tool",
            route.id
        );
    }
    Ok(())
}

#[test]
fn explicit_probe_contract_is_bounded_and_uses_only_read_tools() -> Result<()> {
    // Ollama's installed model digest is deliberately reserved for its optional
    // real-server probe; these loopback fixtures exercise the other six routes.
    for route in certification::routes().into_iter().filter(|route| {
        route.provider != "ollama" && route.provider != "custom-gateway" && route.cloud.is_none()
    }) {
        let server = mock_config(route.clone(), None, true)?;
        let mut cfg = codingbuddy_core::LlmConfig::default();
        configure(&mut cfg, &route, &server)?;
        let evidence = certification::live_probe(&cfg, true)?;
        ensure!(
            evidence.tool_rounds == 2 && evidence.model == route.model,
            "probe scope changed"
        );
        ensure!(
            server.requests.lock().unwrap().len() == 3,
            "unbounded probe"
        );
    }
    Ok(())
}

#[test]
fn private_continuation_survives_restart_between_every_model_step() -> Result<()> {
    use codingbuddy_core::{ChatRequest, FunctionDefinition, ToolChoice, ToolDefinition};
    use codingbuddy_llm::LlmClient;
    use codingbuddy_store::NewEvent;
    for route in certification::routes() {
        for streaming in [false, true] {
            let server = mock(route.clone())?;
            let workspace = tempfile::tempdir()?;
            std::fs::write(workspace.path().join("first.txt"), "first fixture")?;
            std::fs::write(workspace.path().join("second.txt"), "second fixture")?;
            let mut cfg = codingbuddy_core::LlmConfig::default();
            configure(&mut cfg, &route, &server)?;
            cfg.stream = streaming;
            let session = uuid::Uuid::now_v7();
            {
                let store = Store::new(workspace.path())?;
                let key = codingbuddy_llm::session_route(&cfg, &route.model, &store.route_key()?)?;
                store.pin_route(session, &key)?;
                store.append_event(&NewEvent::new(
                    session,
                    EventKind::ChatTurn {
                        message: ChatMessage::User {
                            content: "Read both files.".into(),
                        },
                    },
                ))?;
            }
            for round in 0..3 {
                // Recreate both the client and SQLite store, preserving only disk state.
                let store = Store::new(workspace.path())?;
                let _writer = store.session_lease(session)?;
                let key = codingbuddy_llm::session_route(&cfg, &route.model, &store.route_key()?)?;
                let request = ChatRequest {
                    model: route.model.clone(),
                    messages: store.provider_history(session, &key)?.chat_messages,
                    tools: vec![ToolDefinition {
                        tool_type: "function".into(),
                        function: FunctionDefinition {
                            name: "fs_read".into(),
                            description: "Read a file".into(),
                            strict: None,
                            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                        },
                    }],
                    tool_choice: ToolChoice::auto(),
                    max_tokens: 512,
                    temperature: None,
                    top_p: None,
                    presence_penalty: None,
                    frequency_penalty: None,
                    logprobs: None,
                    top_logprobs: None,
                    thinking: None,
                    images: vec![],
                    provider_options: Default::default(),
                    response_format: None,
                };
                let client = fixture_client(cfg.clone(), &server)?;
                let response = if streaming {
                    client.complete_chat_streaming(&request, Arc::new(|_| {}))?
                } else {
                    client.complete_chat(&request)?
                };
                let invocations = store.model_step(
                    session,
                    None,
                    Some(response.text.clone()),
                    &response.tool_calls,
                    &response.transport_parts,
                )?;
                if round == 2 {
                    ensure!(
                        response.tool_calls.is_empty()
                            && response.text == "Certification complete.",
                        "restart failed final answer"
                    );
                } else {
                    ensure!(
                        response.tool_calls.len() == 1 && invocations.len() == 1,
                        "restart lost tool identity"
                    );
                    let call = &response.tool_calls[0];
                    let expected = if round == 0 {
                        "first.txt"
                    } else {
                        "second.txt"
                    };
                    ensure!(
                        serde_json::from_str::<Value>(&call.arguments)?["path"] == expected,
                        "unexpected replay tool"
                    );
                    store.dispatch_invocation(session, invocations[0])?;
                    store.settle_invocation(
                        session,
                        invocations[0],
                        ChatMessage::Tool {
                            tool_call_id: call.id.clone(),
                            tool_name: Some(call.name.clone()),
                            content: std::fs::read_to_string(workspace.path().join(expected))?,
                        },
                        "running",
                    )?;
                }
            }
            ensure!(
                server.requests.lock().unwrap().len() == 3,
                "restart duplicated a provider request"
            );
        }
    }
    Ok(())
}
