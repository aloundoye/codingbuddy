//! SSE streaming: payload parsing helpers and chat-stream execution.
//!
//! Extracted from `lib.rs` as part of the module split — pure mechanical
//! refactor, no behavior change. Parsing helpers are byte-identical moves;
//! `execute_chat_stream` is the extracted chat SSE consumption loop.

use anyhow::{Result, anyhow};
use codingbuddy_core::{LlmResponse, LlmToolCall, StreamCallback, StreamChunk};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::BufRead;

use crate::{protocol, providers};

pub(crate) fn parse_fim_non_streaming_payload(body: &str) -> Result<LlmResponse> {
    let value: Value = serde_json::from_str(body)?;
    let choice = value
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first());
    let Some(choice) = choice else {
        return Err(anyhow!(
            "unexpected non-streaming payload: missing choices[0]"
        ));
    };
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop")
        .to_string();
    let text = choice
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let usage = parse_usage_object(value.get("usage"));
    Ok(LlmResponse {
        text,
        finish_reason,
        reasoning_content: String::new(),
        tool_calls: vec![],
        usage,
        compatibility: None,
    })
}

pub(crate) fn parse_non_streaming_payload(body: &str) -> Result<LlmResponse> {
    let value: Value = serde_json::from_str(body)?;
    let choice = value
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first());
    let Some(choice) = choice else {
        return Err(anyhow!(
            "unexpected non-streaming payload: missing choices[0]"
        ));
    };
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop")
        .to_string();
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
            "unexpected non-streaming payload: missing message.content/reasoning_content/tool_calls"
        ));
    }
    let text = if content.is_empty() {
        reasoning_content.clone()
    } else {
        content
    };
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

/// Extract token usage from a DeepSeek API usage JSON object.
pub(crate) fn parse_usage_object(
    usage_value: Option<&Value>,
) -> Option<codingbuddy_core::TokenUsage> {
    let u = usage_value?;
    Some(codingbuddy_core::TokenUsage {
        prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        completion_tokens: u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        prompt_cache_hit_tokens: u
            .get("prompt_cache_hit_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        prompt_cache_miss_tokens: u
            .get("prompt_cache_miss_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

pub(crate) fn parse_streaming_payload(body: &str) -> Result<LlmResponse> {
    let mut content_out = String::new();
    let mut reasoning_out = String::new();
    let mut finish_reason: Option<String> = None;
    let mut tool_call_parts: BTreeMap<u64, StreamToolCall> = BTreeMap::new();
    let mut completed_tool_calls = Vec::new();
    let mut parsed_any = false;
    let mut usage: Option<codingbuddy_core::TokenUsage> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("data:") {
            continue;
        }
        let chunk = trimmed.trim_start_matches("data:").trim();
        if chunk == "[DONE]" {
            break;
        }
        let value: Value = serde_json::from_str(chunk)?;
        if let Some(usage_value) = value.get("usage")
            && !usage_value.is_null()
        {
            usage = parse_usage_object(Some(usage_value));
        }
        let choice = value
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first());
        let Some(choice) = choice else {
            continue;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            finish_reason = Some(reason.to_string());
            parsed_any = true;
        }
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                content_out.push_str(content);
                parsed_any = true;
            }
            if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                reasoning_out.push_str(reasoning);
                parsed_any = true;
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                merge_stream_tool_calls(tool_calls, &mut tool_call_parts);
                parsed_any = true;
            }
        }
        if let Some(message) = choice.get("message") {
            if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
                content_out.push_str(content);
                parsed_any = true;
            }
            if let Some(reasoning) = message.get("reasoning_content").and_then(|v| v.as_str()) {
                reasoning_out.push_str(reasoning);
                parsed_any = true;
            }
            if let Some(tool_calls) = message.get("tool_calls") {
                completed_tool_calls.extend(parse_tool_calls_array(tool_calls));
                parsed_any = true;
            }
        }
    }

    let mut tool_calls = tool_call_parts
        .into_iter()
        .filter_map(|(index, value)| {
            if value.name.trim().is_empty() {
                return None;
            }
            Some(LlmToolCall {
                id: value
                    .id
                    .unwrap_or_else(|| format!("tool_call_{}", index + 1)),
                name: value.name,
                arguments: value.arguments,
            })
        })
        .collect::<Vec<_>>();
    if !completed_tool_calls.is_empty() {
        tool_calls.extend(completed_tool_calls);
    }

    if parsed_any {
        let text = if !content_out.is_empty() {
            content_out
        } else {
            reasoning_out.clone()
        };
        Ok(LlmResponse {
            text,
            finish_reason: finish_reason.unwrap_or_else(|| "stop".to_string()),
            reasoning_content: reasoning_out,
            tool_calls,
            usage,
            compatibility: None,
        })
    } else {
        parse_non_streaming_payload(body)
    }
}

#[derive(Default)]
pub(crate) struct StreamToolCall {
    pub(crate) id: Option<String>,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

pub(crate) fn merge_stream_tool_calls(chunks: &[Value], out: &mut BTreeMap<u64, StreamToolCall>) {
    for (idx, item) in chunks.iter().enumerate() {
        let index = item
            .get("index")
            .and_then(|v| v.as_u64())
            .unwrap_or(idx as u64);
        let entry = out.entry(index).or_default();
        if let Some(id) = item.get("id").and_then(|v| v.as_str())
            && !id.trim().is_empty()
        {
            entry.id = Some(id.to_string());
        }
        if let Some(function) = item.get("function") {
            if let Some(name) = function.get("name").and_then(|v| v.as_str())
                && !name.trim().is_empty()
            {
                entry.name = name.to_string();
            }
            if let Some(arguments) = function.get("arguments").and_then(|v| v.as_str()) {
                entry.arguments.push_str(arguments);
            }
        }
    }
}

pub(crate) fn parse_tool_calls_array(value: &Value) -> Vec<LlmToolCall> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let name = item
                .get("function")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if name.trim().is_empty() {
                return None;
            }
            let arguments = item
                .get("function")
                .and_then(|v| v.get("arguments"))
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
                .unwrap_or_else(|| {
                    item.get("function")
                        .and_then(|v| v.get("arguments"))
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string())
                });
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
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

/// Result of consuming a chat SSE stream.
pub struct StreamingResult {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<LlmToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<codingbuddy_core::TokenUsage>,
    pub cancelled: bool,
}

/// Consume a chat SSE stream from `reader`, invoking `cb` for each delta.
///
/// Handles Anthropic (`event:`/`data:` lines), Google, and OpenAI-compatible
/// formats, merges streaming tool calls, and checks `cancel_token` between
/// reads. Returns the assembled stream result; the caller maps it to an
/// `LlmResponse`.
pub fn execute_chat_stream(
    reader: impl BufRead,
    chat_protocol: protocol::ChatProtocol,
    cancel_token: Option<&codingbuddy_core::CancellationToken>,
    cb: &StreamCallback,
) -> Result<StreamingResult> {
    let is_native = chat_protocol.is_native();
    let mut last_err: Option<anyhow::Error> = None;
    let mut content_out = String::new();
    let mut reasoning_out = String::new();
    let mut finish_reason: Option<String> = None;
    let mut tool_call_parts: BTreeMap<u64, StreamToolCall> = BTreeMap::new();
    let mut completed_tool_calls: Vec<LlmToolCall> = Vec::new();
    let mut has_structured_tool_calls = false;
    let mut usage: Option<codingbuddy_core::TokenUsage> = None;
    let mut last_emitted_tool_index: u64 = 0;
    let mut cancelled = false;

    // Native provider streaming state
    let mut anthropic_event_type = String::new();
    let mut native_state = providers::NativeStreamState::default();

    for line_result in reader.lines() {
        if let Some(ct) = cancel_token
            && ct.is_cancelled()
        {
            cancelled = true;
            cb(StreamChunk::Done {
                reason: Some("cancelled".to_string()),
            });
            break;
        }
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                last_err = Some(anyhow!("stream read error: {e}"));
                break;
            }
        };
        let trimmed = line.trim();

        // Anthropic SSE uses "event:" prefix lines before "data:" lines
        if trimmed.starts_with("event:") {
            anthropic_event_type = trimmed.trim_start_matches("event:").trim().to_string();
            continue;
        }

        if !trimmed.starts_with("data:") {
            continue;
        }
        let chunk = trimmed.trim_start_matches("data:").trim();
        if chunk == "[DONE]" {
            cb(StreamChunk::Done { reason: None });
            break;
        }
        let value: Value = match serde_json::from_str(chunk) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // ── Native Anthropic streaming ──
        if chat_protocol == protocol::ChatProtocol::AnthropicMessages {
            let evt_type = if anthropic_event_type.is_empty() {
                value.get("type").and_then(|v| v.as_str()).unwrap_or("")
            } else {
                &anthropic_event_type
            };
            let raw = providers::anthropic::parse_streaming_event(evt_type, &value);
            let evt = providers::from_anthropic_event(raw);
            let done = native_state.handle_event(evt, &**cb);
            anthropic_event_type.clear();
            if done {
                break;
            }
            continue;
        }

        // ── Native Google streaming ──
        if chat_protocol == protocol::ChatProtocol::GeminiGenerateContent {
            let raw = providers::google::parse_streaming_chunk(&value);
            let events = providers::from_google_event(raw);
            let mut done = false;
            for evt in events {
                done = native_state.handle_event(evt, &**cb);
                if done {
                    break;
                }
            }
            if done {
                break;
            }
            continue;
        }

        // ── OpenAI-compatible streaming (default) ──
        if let Some(usage_value) = value.get("usage")
            && !usage_value.is_null()
        {
            usage = parse_usage_object(Some(usage_value));
        }
        let choice = value
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first());
        let Some(choice) = choice else {
            continue;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            finish_reason = Some(reason.to_string());
        }
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                content_out.push_str(content);
                if !has_structured_tool_calls {
                    cb(StreamChunk::ContentDelta(content.to_string()));
                }
            }
            if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                reasoning_out.push_str(reasoning);
                cb(StreamChunk::ReasoningDelta(reasoning.to_string()));
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                has_structured_tool_calls = true;
                merge_stream_tool_calls(tool_calls, &mut tool_call_parts);
                // Emit ToolCallReady for tool calls whose next index has started
                if tool_call_parts.len() > 1 {
                    for (&idx, tc) in &tool_call_parts {
                        if idx >= last_emitted_tool_index
                            && idx < *tool_call_parts.keys().last().unwrap_or(&0)
                            && !tc.name.is_empty()
                        {
                            cb(StreamChunk::ToolCallReady {
                                id: tc
                                    .id
                                    .clone()
                                    .unwrap_or_else(|| format!("tool_call_{}", idx + 1)),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            });
                            last_emitted_tool_index = idx + 1;
                        }
                    }
                }
            }
        }
        if let Some(message) = choice.get("message") {
            if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
                content_out.push_str(content);
                cb(StreamChunk::ContentDelta(content.to_string()));
            }
            if let Some(reasoning) = message.get("reasoning_content").and_then(|v| v.as_str()) {
                reasoning_out.push_str(reasoning);
                cb(StreamChunk::ReasoningDelta(reasoning.to_string()));
            }
            if let Some(tool_calls) = message.get("tool_calls") {
                completed_tool_calls.extend(parse_tool_calls_array(tool_calls));
            }
        }
    }

    if let Some(err) = last_err.take() {
        return Err(err);
    }

    // If content text was streamed to the TUI but we also
    // have structured tool calls, tell the TUI to clear the
    // noise text fragments that appeared before/between
    // tool calls.
    if has_structured_tool_calls && !content_out.is_empty() {
        cb(StreamChunk::ClearStreamingText);
    }

    let mut tool_calls: Vec<LlmToolCall> = tool_call_parts
        .into_iter()
        .filter_map(|(index, value)| {
            if value.name.trim().is_empty() {
                return None;
            }
            Some(LlmToolCall {
                id: value
                    .id
                    .unwrap_or_else(|| format!("tool_call_{}", index + 1)),
                name: value.name,
                arguments: value.arguments,
            })
        })
        .collect();
    if !completed_tool_calls.is_empty() {
        tool_calls.extend(completed_tool_calls);
    }

    // Merge native provider state into the response
    if is_native {
        if !native_state.content.is_empty() {
            content_out = native_state.content;
        }
        if !native_state.reasoning.is_empty() {
            reasoning_out = native_state.reasoning;
        }
        tool_calls.extend(native_state.tool_calls);
        if native_state.finish_reason.is_some() {
            finish_reason = native_state.finish_reason;
        }
        if native_state.usage.is_some() {
            usage = native_state.usage;
        }
    }

    Ok(StreamingResult {
        content: content_out,
        reasoning: reasoning_out,
        tool_calls,
        finish_reason,
        usage,
        cancelled,
    })
}
