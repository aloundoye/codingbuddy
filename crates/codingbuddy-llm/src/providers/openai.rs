//! Native OpenAI Chat Completions API support.
//!
//! OpenAI's Chat Completions API is the de-facto baseline other providers
//! emulate, but it has its own quirks relative to Anthropic/Google:
//! - Endpoint: `POST /v1/chat/completions` with `Authorization: Bearer <key>`
//! - Messages use `role`/`content`; assistant tool calls use a `tool_calls`
//!   array; tool results are `role: "tool"` messages with `tool_call_id`
//! - o-series reasoning models (o1/o3/o4) require `max_completion_tokens`
//!   instead of `max_tokens` and reject sampling controls (temperature,
//!   top_p, penalties, logprobs); reasoning depth is controlled via
//!   `reasoning_effort` ("low"/"medium"/"high")
//! - Structured outputs are requested via `response_format`
//!   (`{"type": "json_object"}` or `{"type": "json_schema", ...}`); OpenAI
//!   rejects `logprobs`/`top_logprobs` together with it
//! - Images are `image_url` parts with data-URI payloads
//!
//! Notes:
//! - `build_payload` is intentionally stream-agnostic: streaming callers set
//!   the `stream` key on the prepared payload themselves.
//! - The `openai_compat_prefix` config flag (a `/v1` vs `/v1beta` prefix knob
//!   used by DeepSeek-style gateways) is honored in `client.rs`'s
//!   `resolved_endpoint`, not here — `endpoint()` always builds the canonical
//!   OpenAI path, per the plan signature `endpoint(base_url)`.
//! - Prediction (OpenAI's `prediction` parameter) is future work: no
//!   dedicated request field exists in `ChatRequest` yet. Until one lands,
//!   ad-hoc fields can be passed through `req.provider_options`.

use anyhow::{Result, anyhow};
use codingbuddy_core::{
    ChatMessage, ChatRequest, ImageContent, LlmResponse, LlmToolCall, ModelFamily, TokenUsage,
    detect_model_family,
};
use serde_json::{Value, json};

/// Build the OpenAI Chat Completions endpoint URL.
///
/// Appends `/v1/chat/completions` to the base URL, tolerating a trailing slash.
pub fn endpoint(base_url: &str) -> String {
    format!("{}/v1/chat/completions", base_url.trim_end_matches('/'))
}

/// Build an OpenAI-native Chat Completions payload from a ChatRequest.
///
/// `max_tokens` is the caller-capped output token budget; it is emitted as
/// `max_completion_tokens` for o-series models (OpenAI rejects `max_tokens`
/// there) and as `max_tokens` otherwise.
pub fn build_payload(req: &ChatRequest, max_tokens: u32) -> Result<Value> {
    let mut messages: Vec<Value> = Vec::new();

    for msg in &req.messages {
        match msg {
            ChatMessage::System { content } => {
                messages.push(json!({"role": "system", "content": content}));
            }
            ChatMessage::User { content } => {
                messages.push(json!({"role": "user", "content": content}));
            }
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                let mut message = json!({"role": "assistant"});
                if let Some(text) = content
                    && !text.is_empty()
                {
                    message["content"] = json!(text);
                }
                if let Some(reasoning) = reasoning_content
                    && !reasoning.is_empty()
                {
                    // o-series models return this field too; keeping it inside
                    // a tool loop preserves the model's logical thread.
                    message["reasoning_content"] = json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments,
                                }
                            })
                        })
                        .collect();
                    message["tool_calls"] = json!(calls);
                }
                if message.get("content").is_none() && message.get("tool_calls").is_none() {
                    message["content"] = json!("");
                }
                messages.push(message);
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
                ..
            } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                }));
            }
        }
    }

    if !req.images.is_empty() {
        attach_images(&mut messages, &req.images)?;
    }

    let o_series = is_o_series(&req.model);
    let thinking_enabled = req
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "enabled");
    let reasoning_mode = o_series || thinking_enabled;

    let mut payload = json!({
        "model": &req.model,
        "messages": messages,
    });

    if reasoning_mode {
        // o-series models reject `max_tokens` and all sampling controls;
        // reasoning depth is controlled with `reasoning_effort`.
        payload["max_completion_tokens"] = json!(max_tokens);
        payload["reasoning_effort"] = json!(if thinking_enabled { "high" } else { "medium" });
    } else {
        payload["max_tokens"] = json!(max_tokens);
        if let Some(temp) = req.temperature {
            payload["temperature"] = json!(temp);
        }
        if let Some(top_p) = req.top_p {
            payload["top_p"] = json!(top_p);
        }
        if let Some(pp) = req.presence_penalty {
            payload["presence_penalty"] = json!(pp);
        }
        if let Some(fp) = req.frequency_penalty {
            payload["frequency_penalty"] = json!(fp);
        }
    }

    // Structured outputs pass through verbatim (json_object / json_schema).
    // OpenAI rejects logprobs/top_logprobs together with response_format.
    if let Some(fmt) = &req.response_format {
        payload["response_format"] = fmt.clone();
    } else if !reasoning_mode {
        if let Some(logprobs) = req.logprobs {
            payload["logprobs"] = json!(logprobs);
        }
        if let Some(top_logprobs) = req.top_logprobs {
            payload["top_logprobs"] = json!(top_logprobs);
        }
    }

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        payload["tools"] = json!(tools);
        payload["tool_choice"] = serde_json::to_value(&req.tool_choice)?;
    }

    // Note: `stream` is intentionally not set here — streaming callers stamp
    // it onto the prepared payload after building.

    Ok(payload)
}

/// Parse a non-streaming OpenAI Chat Completions response.
pub fn parse_response(body: &str) -> Result<LlmResponse> {
    let value: Value = serde_json::from_str(body)?;
    let choice = value
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first());
    let Some(choice) = choice else {
        return Err(anyhow!("unexpected OpenAI response: missing choices[0]"));
    };
    let message = choice.get("message").cloned().unwrap_or_else(|| json!({}));
    let content = message
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let reasoning_content = message
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let tool_calls = message
        .get("tool_calls")
        .map(parse_tool_calls_array)
        .unwrap_or_default();
    if content.is_empty() && reasoning_content.is_empty() && tool_calls.is_empty() {
        return Err(anyhow!(
            "unexpected OpenAI response: missing message.content/reasoning_content/tool_calls"
        ));
    }
    let text = if content.is_empty() {
        reasoning_content.clone()
    } else {
        content
    };
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop")
        .to_string();
    let usage = parse_usage_object(value.get("usage"));
    Ok(LlmResponse {
        text,
        finish_reason,
        reasoning_content,
        tool_calls,
        usage,
        compatibility: None,
    })
}

/// Whether the model is an OpenAI o-series (reasoning) model.
///
/// `detect_model_family` lumps `gpt-*` and o-series together under
/// `ModelFamily::OpenAi`, so o-series detection additionally requires the
/// `o1`/`o3`/`o4` name prefix (or a `reasoning` marker) that OpenAI's
/// reasoning models share — the same predicate the capability layer uses for
/// `prefers_max_completion_tokens`.
fn is_o_series(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    detect_model_family(model) == ModelFamily::OpenAi
        && (lower.starts_with("o1")
            || lower.starts_with("o3")
            || lower.starts_with("o4")
            || lower.contains("reasoning"))
}

/// Attach multimodal images to the last user message as data-URI parts,
/// mirroring the generic OpenAI-compatible payload path.
fn attach_images(messages: &mut Vec<Value>, images: &[ImageContent]) -> Result<()> {
    let user_idx = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(idx, _)| idx);
    let existing_text = user_idx
        .and_then(|idx| messages[idx].get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut parts: Vec<Value> = Vec::new();
    if !existing_text.is_empty() {
        parts.push(json!({"type": "text", "text": existing_text}));
    }
    for image in images {
        if image.mime.trim().is_empty() {
            return Err(anyhow!("cannot encode image input with empty mime type"));
        }
        if image.base64_data.trim().is_empty() {
            parts.push(json!({
                "type": "text",
                "text": format!(
                    "ERROR: A provided {} input is empty or corrupted. Explain this limitation to the user.",
                    image.mime
                ),
            }));
            continue;
        }
        parts.push(json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:{};base64,{}", image.mime, image.base64_data),
            }
        }));
    }
    if let Some(idx) = user_idx {
        messages[idx]["content"] = json!(parts);
    } else {
        messages.push(json!({"role": "user", "content": parts}));
    }
    Ok(())
}

/// Parse an OpenAI `message.tool_calls` array into `LlmToolCall` values.
///
/// `arguments` is a JSON string per the API; if a gateway returns a raw
/// object instead, it is serialized back to a string.
fn parse_tool_calls_array(value: &Value) -> Vec<LlmToolCall> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let function = item.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if name.trim().is_empty() {
                return None;
            }
            let arguments = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| {
                    function
                        .and_then(|f| f.get("arguments"))
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "{}".to_string())
                });
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("tool_call_{}", idx + 1));
            Some(LlmToolCall {
                id,
                name,
                arguments,
            })
        })
        .collect()
}

/// Extract token usage from an OpenAI usage object.
///
/// Mirrors `streaming.rs::parse_usage_object`, plus the o-series-native
/// `prompt_tokens_details.cached_tokens` mapping into
/// `prompt_cache_hit_tokens` (o1/o3/o4 report cache hits there, not in the
/// legacy `prompt_cache_hit_tokens` field).
fn parse_usage_object(usage_value: Option<&Value>) -> Option<TokenUsage> {
    let u = usage_value?;
    Some(TokenUsage {
        prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
        completion_tokens: u
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        prompt_cache_hit_tokens: u
            .get("prompt_cache_hit_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                u.get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
            })
            .unwrap_or(0),
        prompt_cache_miss_tokens: u
            .get("prompt_cache_miss_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codingbuddy_core::{
        FunctionDefinition, ThinkingConfig, ToolChoice, ToolChoiceFunction, ToolDefinition,
    };

    fn simple_chat_request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "gpt-4o".to_string(),
            messages,
            tools: vec![],
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

    #[test]
    fn endpoint_builds_correct_urls() {
        assert_eq!(
            endpoint("https://api.openai.com"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint("https://api.openai.com/"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn build_payload_gpt4o_keeps_sampling_controls() {
        let req = simple_chat_request(vec![ChatMessage::User {
            content: "Hello".to_string(),
        }]);
        let payload = build_payload(&req, 4096).expect("payload builds");
        assert_eq!(payload["model"], "gpt-4o");
        assert_eq!(payload["max_tokens"], 4096);
        assert!(
            (payload["temperature"].as_f64().unwrap_or_default() - 0.7).abs() < 0.000_001,
            "temperature should be preserved"
        );
        assert!(payload.get("max_completion_tokens").is_none());
        assert!(payload.get("reasoning_effort").is_none());
        assert!(payload.get("stream").is_none());
    }

    #[test]
    fn build_payload_o_series_uses_reasoning_effort_and_max_completion_tokens() {
        let mut req = simple_chat_request(vec![ChatMessage::User {
            content: "Hello".to_string(),
        }]);
        req.model = "o3-mini".to_string();
        req.temperature = Some(0.9);
        req.top_p = Some(0.8);
        req.presence_penalty = Some(0.1);
        req.frequency_penalty = Some(0.2);
        req.logprobs = Some(true);
        req.top_logprobs = Some(3);
        let payload = build_payload(&req, 8192).expect("payload builds");
        assert_eq!(payload["max_completion_tokens"], 8192);
        assert_eq!(payload["reasoning_effort"], "medium");
        assert!(payload.get("max_tokens").is_none());
        assert!(payload.get("temperature").is_none());
        assert!(payload.get("top_p").is_none());
        assert!(payload.get("presence_penalty").is_none());
        assert!(payload.get("frequency_penalty").is_none());
        assert!(payload.get("logprobs").is_none());
        assert!(payload.get("top_logprobs").is_none());
    }

    #[test]
    fn build_payload_o_series_with_thinking_uses_high_effort() {
        let mut req = simple_chat_request(vec![ChatMessage::User {
            content: "Hello".to_string(),
        }]);
        req.model = "o1".to_string();
        req.thinking = Some(ThinkingConfig::enabled(16_384));
        let payload = build_payload(&req, 16_384).expect("payload builds");
        assert_eq!(payload["reasoning_effort"], "high");
        assert_eq!(payload["max_completion_tokens"], 16_384);
    }

    #[test]
    fn build_payload_passes_through_response_format_and_omits_logprobs() {
        let mut req = simple_chat_request(vec![ChatMessage::User {
            content: "Hello".to_string(),
        }]);
        req.logprobs = Some(true);
        req.top_logprobs = Some(5);
        req.response_format = Some(json!({"type": "json_object"}));
        let payload = build_payload(&req, 4096).expect("payload builds");
        assert_eq!(payload["response_format"]["type"], "json_object");
        assert!(payload.get("logprobs").is_none());
        assert!(payload.get("top_logprobs").is_none());
        assert!(payload.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_payload_maps_tool_calls_and_results() {
        let mut req = simple_chat_request(vec![
            ChatMessage::User {
                content: "Do it".to_string(),
            },
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![LlmToolCall {
                    id: "call_1".to_string(),
                    name: "fs_read".to_string(),
                    arguments: r#"{"path":"src/main.rs"}"#.to_string(),
                }],
                reasoning_content: None,
            },
            ChatMessage::Tool {
                tool_call_id: "call_1".to_string(),
                content: "fn main() {}".to_string(),
                tool_name: None,
            },
        ]);
        req.tools = vec![ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "fs_read".to_string(),
                description: "Read a file".to_string(),
                strict: None,
                parameters: json!({"type": "object"}),
            },
        }];
        req.tool_choice = ToolChoice::Function {
            choice_type: "function".to_string(),
            function: ToolChoiceFunction {
                name: "fs_read".to_string(),
            },
        };
        let payload = build_payload(&req, 4096).expect("payload builds");
        let msgs = payload["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["type"], "function");
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "fs_read");
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"src/main.rs"}"#
        );
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_1");
        assert_eq!(msgs[2]["content"], "fn main() {}");
        assert_eq!(payload["tools"][0]["type"], "function");
        assert_eq!(payload["tools"][0]["function"]["name"], "fs_read");
        assert_eq!(payload["tool_choice"]["function"]["name"], "fs_read");
    }

    #[test]
    fn build_payload_attaches_images_as_data_uri_parts() {
        let mut req = simple_chat_request(vec![ChatMessage::User {
            content: "Look".to_string(),
        }]);
        req.images = vec![ImageContent {
            mime: "image/png".to_string(),
            base64_data: "AAAA".to_string(),
        }];
        let payload = build_payload(&req, 4096).expect("payload builds");
        let content = payload["messages"][0]["content"]
            .as_array()
            .expect("parts array");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Look");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn build_payload_rejects_empty_image_mime() {
        let mut req = simple_chat_request(vec![ChatMessage::User {
            content: "Look".to_string(),
        }]);
        req.images = vec![ImageContent {
            mime: String::new(),
            base64_data: "AAAA".to_string(),
        }];
        assert!(build_payload(&req, 4096).is_err());
    }

    #[test]
    fn parse_response_happy_path_maps_usage() {
        let body = r#"{
            "choices": [{
                "message": {"role": "assistant", "content": "Hello there"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "completion_tokens_details": {"reasoning_tokens": 20},
                "prompt_tokens_details": {"cached_tokens": 30}
            }
        }"#;
        let resp = parse_response(body).expect("parse succeeds");
        assert_eq!(resp.text, "Hello there");
        assert_eq!(resp.finish_reason, "stop");
        assert!(resp.tool_calls.is_empty());
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.reasoning_tokens, 20);
        assert_eq!(usage.prompt_cache_hit_tokens, 30);
    }

    #[test]
    fn parse_response_extracts_tool_calls() {
        let body = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {"name": "fs_read", "arguments": "{\"path\": \"src/main.rs\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }"#;
        let resp = parse_response(body).expect("parse succeeds");
        assert_eq!(resp.finish_reason, "tool_calls");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_abc");
        assert_eq!(resp.tool_calls[0].name, "fs_read");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"path": "src/main.rs"}"#);
    }

    #[test]
    fn parse_response_reads_reasoning_content() {
        let body = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "Let me think about this..."
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }"#;
        let resp = parse_response(body).expect("parse succeeds");
        assert_eq!(resp.reasoning_content, "Let me think about this...");
        assert_eq!(resp.text, "Let me think about this...");
    }

    #[test]
    fn parse_response_errors_on_missing_choices() {
        let body = r#"{"error": {"message": "boom", "type": "server_error"}}"#;
        let err = parse_response(body).expect_err("missing choices should error");
        assert!(err.to_string().contains("missing choices[0]"));
    }
}
