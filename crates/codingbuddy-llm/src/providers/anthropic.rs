//! Native Anthropic Messages API support.
//!
//! Anthropic's Messages API differs from OpenAI:
//! - System prompt is a top-level `system` field, not a message
//! - Tool calls use `tool_use` content blocks
//! - Tool results use `tool_result` content blocks in user messages
//! - Auth uses `x-api-key` header, not `Authorization: Bearer`
//! - Requires `anthropic-version` header
//! - Streaming uses `content_block_start`, `content_block_delta`, `message_delta` events

use anyhow::Result;
use codingbuddy_core::{ChatMessage, ChatRequest, LlmResponse, LlmToolCall, TokenUsage};
use serde_json::{Value, json};

/// Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Build an Anthropic Messages API payload from a ChatRequest.
pub fn build_payload(req: &ChatRequest, max_output_tokens: u32) -> Result<Value> {
    let mut system_parts: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for msg in &req.messages {
        match msg {
            ChatMessage::System { content } => {
                if !content.is_empty() {
                    system_parts.push(json!({ "type": "text", "text": content }));
                }
            }
            ChatMessage::User { content } => {
                messages.push(json!({
                    "role": "user",
                    "content": content
                }));
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                reasoning_content,
            } => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(reasoning) = reasoning_content
                    && !reasoning.is_empty()
                {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": reasoning
                    }));
                }
                if let Some(text) = content
                    && !text.is_empty()
                {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for tc in tool_calls {
                    let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": args
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                messages.push(json!({
                    "role": "assistant",
                    "content": blocks
                }));
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                // Tool results go in user messages as tool_result blocks.
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": content
                });
                if let Some(last) = messages.last_mut()
                    && last.get("role").and_then(|v| v.as_str()) == Some("user")
                    && let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut())
                {
                    arr.push(block);
                    continue;
                }
                messages.push(json!({
                    "role": "user",
                    "content": [block]
                }));
            }
        }
    }

    let mut payload = json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": max_output_tokens,
    });

    if !system_parts.is_empty() {
        // Add cache_control to the last system block for prompt caching
        if let Some(last) = system_parts.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        payload["system"] = json!(system_parts);
    }

    // Tool definitions
    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "input_schema": t.function.parameters
                })
            })
            .collect();
        // Add cache_control to the last tool for prompt caching
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        payload["tools"] = json!(tools);
    }

    // Thinking / extended thinking
    if let Some(ref thinking) = req.thinking
        && thinking.thinking_type == "enabled"
    {
        payload["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": thinking.budget_tokens.unwrap_or(8192)
        });
    }

    // Temperature (omit when thinking is enabled)
    let thinking_on = req
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "enabled");
    if !thinking_on {
        if let Some(temp) = req.temperature {
            payload["temperature"] = json!(temp);
        }
        if let Some(top_p) = req.top_p {
            payload["top_p"] = json!(top_p);
        }
    }

    Ok(payload)
}

/// Parse a non-streaming Anthropic Messages API response.
pub fn parse_response(body: &str) -> Result<LlmResponse> {
    let v: Value = serde_json::from_str(body)?;

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    if let Some(content) = v.get("content").and_then(|c| c.as_array()) {
        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                        reasoning.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("call_0")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or_default();
                    tool_calls.push(LlmToolCall {
                        id,
                        name,
                        arguments: serde_json::to_string(&input).unwrap_or_default(),
                    });
                }
                _ => {}
            }
        }
    }

    let stop_reason = v
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    let finish_reason = match stop_reason {
        "tool_use" => "tool_calls",
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        other => other,
    };

    let usage = parse_anthropic_usage(&v);

    Ok(LlmResponse {
        text,
        finish_reason: finish_reason.to_string(),
        reasoning_content: reasoning,
        tool_calls,
        usage: Some(usage),
        compatibility: None,
    })
}

/// Parse streaming SSE events from Anthropic's Messages API.
pub fn parse_streaming_event(event_type: &str, data: &Value) -> StreamEvent {
    match event_type {
        "content_block_start" => {
            if let Some(block) = data.get("content_block") {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("call_0")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        StreamEvent::ToolStart { id, name }
                    }
                    _ => StreamEvent::None,
                }
            } else {
                StreamEvent::None
            }
        }
        "content_block_delta" => {
            if let Some(delta) = data.get("delta") {
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        let text = delta
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        StreamEvent::TextDelta(text)
                    }
                    Some("thinking_delta") => {
                        let text = delta
                            .get("thinking")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        StreamEvent::ThinkingDelta(text)
                    }
                    Some("input_json_delta") => {
                        let json_str = delta
                            .get("partial_json")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        StreamEvent::ToolArgsDelta(json_str)
                    }
                    _ => StreamEvent::None,
                }
            } else {
                StreamEvent::None
            }
        }
        "message_delta" => {
            let stop_reason = data
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|v| v.as_str())
                .unwrap_or("end_turn");
            let finish = match stop_reason {
                "tool_use" => "tool_calls",
                "end_turn" | "stop_sequence" => "stop",
                "max_tokens" => "length",
                other => other,
            };
            StreamEvent::Finish(finish.to_string())
        }
        "message_start" => {
            if let Some(usage) = data.get("message").and_then(|m| m.get("usage")) {
                let input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cache_read = usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                StreamEvent::Usage {
                    input,
                    output: 0,
                    cache_hit: cache_read,
                }
            } else {
                StreamEvent::None
            }
        }
        "message_stop" => StreamEvent::Done,
        _ => StreamEvent::None,
    }
}

/// Events produced by Anthropic streaming parser.
pub enum StreamEvent {
    None,
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart {
        id: String,
        name: String,
    },
    ToolArgsDelta(String),
    Finish(String),
    Usage {
        input: u64,
        output: u64,
        cache_hit: u64,
    },
    Done,
}

fn parse_anthropic_usage(v: &Value) -> TokenUsage {
    let usage = v.get("usage");
    TokenUsage {
        prompt_tokens: usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        completion_tokens: usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        prompt_cache_hit_tokens: usage
            .and_then(|u| u.get("cache_read_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        prompt_cache_miss_tokens: usage
            .and_then(|u| u.get("cache_creation_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codingbuddy_core::ToolChoice;

    fn simple_chat_request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-4-20250514".to_string(),
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
    fn build_payload_extracts_system_prompt() {
        let req = simple_chat_request(vec![
            ChatMessage::System {
                content: "You are helpful.".to_string(),
            },
            ChatMessage::User {
                content: "Hello".to_string(),
            },
        ]);
        let payload = build_payload(&req, 4096).unwrap();
        assert!(payload.get("system").is_some());
        let system = payload["system"].as_array().unwrap();
        assert_eq!(system[0]["text"], "You are helpful.");
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn build_payload_converts_tool_results() {
        let req = simple_chat_request(vec![
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
            },
        ]);
        let payload = build_payload(&req, 4096).unwrap();
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
    }

    #[test]
    fn parse_response_extracts_tool_calls() {
        let body = r#"{
            "content": [
                {"type": "text", "text": "I'll read the file."},
                {"type": "tool_use", "id": "toolu_1", "name": "fs_read", "input": {"path": "src/main.rs"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        }"#;
        let resp = parse_response(body).unwrap();
        assert_eq!(resp.text, "I'll read the file.");
        assert_eq!(resp.finish_reason, "tool_calls");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "fs_read");
    }

    #[test]
    fn parse_response_maps_stop_reasons() {
        for (reason, expected) in [
            ("end_turn", "stop"),
            ("tool_use", "tool_calls"),
            ("max_tokens", "length"),
        ] {
            let body = format!(
                r#"{{"content":[{{"type":"text","text":"ok"}}],"stop_reason":"{}","usage":{{"input_tokens":10,"output_tokens":5}}}}"#,
                reason
            );
            let resp = parse_response(&body).unwrap();
            assert_eq!(resp.finish_reason, expected, "stop_reason={reason}");
        }
    }
}
