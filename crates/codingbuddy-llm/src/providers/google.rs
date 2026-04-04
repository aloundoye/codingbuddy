//! Native Google Gemini API support.
//!
//! Gemini's GenerateContent API differs from OpenAI:
//! - Endpoint: `/v1beta/models/{model}:generateContent` (non-streaming)
//!   or `:streamGenerateContent?alt=sse` (streaming)
//! - Auth: API key as `?key=` query parameter
//! - Messages use `contents` array with `parts`
//! - System instruction is a top-level field
//! - Tools use `functionDeclarations`
//! - Tool calls are `functionCall` parts, results are `functionResponse` parts

use anyhow::Result;
use codingbuddy_core::{ChatMessage, ChatRequest, LlmResponse, LlmToolCall, TokenUsage};
use serde_json::{Value, json};

/// Build the Gemini API endpoint URL.
pub fn endpoint(base_url: &str, model: &str, streaming: bool) -> String {
    let base = base_url.trim_end_matches('/');
    if streaming {
        format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse")
    } else {
        format!("{base}/v1beta/models/{model}:generateContent")
    }
}

/// Append the API key as a query parameter to the URL.
pub fn append_api_key(url: &str, api_key: &str) -> String {
    if url.contains('?') {
        format!("{url}&key={api_key}")
    } else {
        format!("{url}?key={api_key}")
    }
}

/// Build a Gemini GenerateContent payload from a ChatRequest.
pub fn build_payload(req: &ChatRequest, max_output_tokens: u32) -> Result<Value> {
    let mut system_text = String::new();
    let mut contents: Vec<Value> = Vec::new();

    for msg in &req.messages {
        match msg {
            ChatMessage::System { content } => {
                if !content.is_empty() {
                    if !system_text.is_empty() {
                        system_text.push('\n');
                    }
                    system_text.push_str(content);
                }
            }
            ChatMessage::User { content } => {
                contents.push(json!({
                    "role": "user",
                    "parts": [{ "text": content }]
                }));
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(text) = content
                    && !text.is_empty()
                {
                    parts.push(json!({ "text": text }));
                }
                for tc in tool_calls {
                    let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
                    parts.push(json!({
                        "functionCall": {
                            "name": tc.name,
                            "args": args
                        }
                    }));
                }
                if parts.is_empty() {
                    parts.push(json!({ "text": "" }));
                }
                contents.push(json!({
                    "role": "model",
                    "parts": parts
                }));
            }
            ChatMessage::Tool {
                tool_call_id: _,
                content,
            } => {
                // Gemini uses functionResponse parts in "user" role messages.
                // Try to parse content as JSON; fall back to wrapping as {"result": text}
                let response_val: Value =
                    serde_json::from_str(content).unwrap_or_else(|_| json!({ "result": content }));
                // FIXME: ChatMessage::Tool lacks the tool name. Gemini's functionResponse
                // requires it. Using a placeholder — may cause Gemini to reject or
                // misattribute results if it validates against function declarations.
                let tool_name = "tool_response";
                let part = json!({
                    "functionResponse": {
                        "name": tool_name,
                        "response": response_val
                    }
                });
                // Append to previous user message if it has functionResponse parts
                if let Some(last) = contents.last_mut()
                    && last.get("role").and_then(|v| v.as_str()) == Some("user")
                    && let Some(parts) = last.get_mut("parts").and_then(|p| p.as_array_mut())
                    && parts.iter().any(|p| p.get("functionResponse").is_some())
                {
                    parts.push(part);
                    continue;
                }
                contents.push(json!({
                    "role": "user",
                    "parts": [part]
                }));
            }
        }
    }

    let mut payload = json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": max_output_tokens,
        }
    });

    if !system_text.is_empty() {
        payload["systemInstruction"] = json!({
            "parts": [{ "text": system_text }]
        });
    }

    // Temperature
    if let Some(temp) = req.temperature {
        payload["generationConfig"]["temperature"] = json!(temp);
    }
    if let Some(top_p) = req.top_p {
        payload["generationConfig"]["topP"] = json!(top_p);
    }

    // Tool definitions
    if !req.tools.is_empty() {
        let declarations: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut decl = json!({
                    "name": t.function.name,
                    "description": t.function.description,
                });
                // Sanitize parameters for Gemini compatibility
                let mut params = t.function.parameters.clone();
                crate::provider_transform::sanitize_gemini_schema(&mut params);
                decl["parameters"] = params;
                decl
            })
            .collect();
        payload["tools"] = json!([{
            "functionDeclarations": declarations
        }]);
    }

    Ok(payload)
}

/// Parse a non-streaming Gemini GenerateContent response.
pub fn parse_response(body: &str) -> Result<LlmResponse> {
    let v: Value = serde_json::from_str(body)?;

    let candidate = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first());

    let mut text = String::new();
    let mut tool_calls = Vec::new();

    if let Some(candidate) = candidate
        && let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
    {
        for (i, part) in parts.iter().enumerate() {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
            if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = fc.get("args").cloned().unwrap_or_default();
                tool_calls.push(LlmToolCall {
                    id: format!("call_{i}"),
                    name,
                    arguments: serde_json::to_string(&args).unwrap_or_default(),
                });
            }
        }
    }

    let finish_reason = if !tool_calls.is_empty() {
        "tool_calls"
    } else {
        let raw = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(|v| v.as_str())
            .unwrap_or("STOP");
        match raw {
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            "SAFETY" => "content_filter",
            _ => "stop",
        }
    };

    let usage = parse_gemini_usage(&v);

    Ok(LlmResponse {
        text,
        finish_reason: finish_reason.to_string(),
        reasoning_content: String::new(),
        tool_calls,
        usage: Some(usage),
        compatibility: None,
    })
}

/// Parse streaming SSE events from Gemini's streamGenerateContent.
///
/// Gemini streams JSON objects (one per SSE "data:" line) with `candidates` array.
pub fn parse_streaming_chunk(data: &Value) -> StreamEvent {
    let candidate = data
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first());

    let Some(candidate) = candidate else {
        // Check for usage-only chunk
        if data.get("usageMetadata").is_some() {
            return parse_usage_event(data);
        }
        return StreamEvent::None;
    };

    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array());

    let Some(parts) = parts else {
        if let Some(reason) = candidate.get("finishReason").and_then(|v| v.as_str()) {
            let finish = match reason {
                "STOP" => "stop",
                "MAX_TOKENS" => "length",
                _ => "stop",
            };
            return StreamEvent::Finish(finish.to_string());
        }
        return StreamEvent::None;
    };

    let mut events = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
            events.push(StreamEvent::TextDelta(t.to_string()));
        }
        if let Some(fc) = part.get("functionCall") {
            let name = fc
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = fc.get("args").cloned().unwrap_or_default();
            events.push(StreamEvent::ToolCall {
                id: format!("call_{i}"),
                name,
                arguments: serde_json::to_string(&args).unwrap_or_default(),
            });
        }
    }

    if let Some(reason) = candidate.get("finishReason").and_then(|v| v.as_str()) {
        let finish = match reason {
            "STOP"
                if events
                    .iter()
                    .any(|e| matches!(e, StreamEvent::ToolCall { .. })) =>
            {
                "tool_calls"
            }
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            _ => "stop",
        };
        events.push(StreamEvent::Finish(finish.to_string()));
    }

    if events.len() == 1 {
        events.into_iter().next().unwrap_or(StreamEvent::None)
    } else if events.is_empty() {
        StreamEvent::None
    } else {
        StreamEvent::Multiple(events)
    }
}

fn parse_usage_event(data: &Value) -> StreamEvent {
    if let Some(usage) = data.get("usageMetadata") {
        let input = usage
            .get("promptTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("candidatesTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cached = usage
            .get("cachedContentTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        StreamEvent::Usage {
            input,
            output,
            cache_hit: cached,
        }
    } else {
        StreamEvent::None
    }
}

/// Events produced by Gemini streaming parser.
pub enum StreamEvent {
    None,
    TextDelta(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Finish(String),
    Usage {
        input: u64,
        output: u64,
        cache_hit: u64,
    },
    Multiple(Vec<StreamEvent>),
}

fn parse_gemini_usage(v: &Value) -> TokenUsage {
    let usage = v.get("usageMetadata");
    TokenUsage {
        prompt_tokens: usage
            .and_then(|u| u.get("promptTokenCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        completion_tokens: usage
            .and_then(|u| u.get("candidatesTokenCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        prompt_cache_hit_tokens: usage
            .and_then(|u| u.get("cachedContentTokenCount"))
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
            model: "gemini-2.5-flash".to_string(),
            messages,
            tools: vec![],
            tool_choice: ToolChoice::auto(),
            max_tokens: 4096,
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
        }
    }

    #[test]
    fn build_payload_uses_contents_format() {
        let req = simple_chat_request(vec![
            ChatMessage::System {
                content: "Be helpful.".to_string(),
            },
            ChatMessage::User {
                content: "Hello".to_string(),
            },
        ]);
        let payload = build_payload(&req, 4096).unwrap();

        assert!(payload.get("systemInstruction").is_some());
        let contents = payload["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn parse_response_extracts_function_calls() {
        let body = r#"{
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "I'll read that file."},
                        {"functionCall": {"name": "fs_read", "args": {"path": "src/main.rs"}}}
                    ],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50}
        }"#;
        let resp = parse_response(body).unwrap();
        assert_eq!(resp.text, "I'll read that file.");
        assert_eq!(resp.finish_reason, "tool_calls");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "fs_read");
    }

    #[test]
    fn endpoint_builds_correct_urls() {
        let base = "https://generativelanguage.googleapis.com";
        assert_eq!(
            endpoint(base, "gemini-2.5-flash", false),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert!(endpoint(base, "gemini-2.5-flash", true).contains("streamGenerateContent"));
        assert!(endpoint(base, "gemini-2.5-flash", true).contains("alt=sse"));
    }

    #[test]
    fn sanitize_coerces_numeric_enums() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "level": {
                    "type": "integer",
                    "enum": [1, 2, 3]
                }
            }
        });
        crate::provider_transform::sanitize_gemini_schema(&mut schema);
        // The shared sanitizer converts integer enums to strings for Gemini
        let level = &schema["properties"]["level"];
        let enum_vals = level["enum"].as_array().unwrap();
        for v in enum_vals {
            assert!(
                v.is_string(),
                "enum values should be strings after sanitization"
            );
        }
    }
}
