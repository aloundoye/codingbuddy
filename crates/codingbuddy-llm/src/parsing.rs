//! Parsing provider responses

use super::*;

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
        transport_parts: Vec::new(),
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
    anyhow::ensure!(
        value.get("error").is_none_or(Value::is_null),
        "Provider sent a completion error"
    );
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
        .unwrap_or("missing_terminal")
        .to_string();
    let message = choice.get("message").cloned().unwrap_or_else(|| json!({}));
    let content = message
        .get("content")
        .and_then(|v| v.as_str())
        .or_else(|| message.get("refusal").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let reasoning_content = message
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let tool_calls = parse_completed_calls(message.get("tool_calls"))?;
    if content.is_empty() && reasoning_content.is_empty() && tool_calls.is_empty() {
        return Err(anyhow!(
            "unexpected non-streaming payload: missing message.content/reasoning_content/tool_calls"
        ));
    }
    anyhow::ensure!(
        finish_reason != "missing_terminal" || tool_calls.is_empty(),
        "Provider completion missing its terminal status"
    );
    let text = content;
    let usage = parse_usage_object(value.get("usage"));
    let mut transport_parts = Vec::new();
    if !text.is_empty() {
        transport_parts.push(TransportPart::Text(text.clone()));
    }
    transport_parts.extend(
        tool_calls
            .iter()
            .map(|call| TransportPart::Call(call.id.clone())),
    );
    if !reasoning_content.is_empty() {
        transport_parts.push(TransportPart::Native(codingbuddy_core::NativeParts {
            protocol: "openai-chat".into(),
            blocks: vec![json!({"reasoning_content":reasoning_content})],
        }));
    }
    Ok(LlmResponse {
        transport_parts,
        text,
        finish_reason: if finish_reason == "missing_terminal" {
            "stop".into()
        } else {
            finish_reason
        },
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
    let usage = usage_value.filter(|value| value.is_object())?;
    let input = usage["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = usage["prompt_cache_hit_tokens"]
        .as_u64()
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
        })
        .unwrap_or(0)
        .min(input);
    Some(codingbuddy_core::TokenUsage {
        prompt_tokens: input,
        completion_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
        prompt_cache_hit_tokens: cached,
        prompt_cache_miss_tokens: usage["prompt_cache_miss_tokens"]
            .as_u64()
            .unwrap_or(input - cached)
            .min(input - cached),
        reasoning_tokens: usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
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
            transport_parts: Vec::new(),
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

pub(crate) fn confirmed_chat_response(
    response: LlmResponse,
    protocol: protocol::ChatProtocol,
    tools: &[codingbuddy_core::ToolDefinition],
) -> Result<LlmResponse> {
    if !matches!(response.finish_reason.as_str(), "stop" | "tool_calls") {
        return Err(
            error::ProviderError::new(protocol, error::FailureKind::IncompleteResponse).into(),
        );
    }
    for call in &response.tool_calls {
        let definition = tools
            .iter()
            .find(|tool| tool.function.name == call.name)
            .ok_or_else(|| {
                error::ProviderError::new(protocol, error::FailureKind::MalformedResponse)
            })?;
        let args: Value = serde_json::from_str(&call.arguments).map_err(|_| {
            error::ProviderError::new(protocol, error::FailureKind::MalformedResponse)
        })?;
        let validator =
            jsonschema::validator_for(&definition.function.parameters).map_err(|_| {
                error::ProviderError::new(protocol, error::FailureKind::UnsupportedFeature)
            })?;
        if !validator.is_valid(&args) {
            return Err(
                error::ProviderError::new(protocol, error::FailureKind::MalformedResponse).into(),
            );
        }
    }
    Ok(response)
}

pub(crate) fn validate_completed_calls(calls: &[LlmToolCall]) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for call in calls {
        anyhow::ensure!(
            !call.id.trim().is_empty() && !call.name.trim().is_empty() && ids.insert(&call.id),
            "Provider returned an incomplete or duplicate tool call"
        );
        let arguments: Value = serde_json::from_str(&call.arguments)
            .context("Provider returned malformed tool arguments")?;
        anyhow::ensure!(
            arguments.is_object(),
            "Provider tool arguments must be an object"
        );
    }
    Ok(())
}

pub(crate) fn parse_completed_calls(value: Option<&Value>) -> Result<Vec<LlmToolCall>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let calls = value.as_array().context("Malformed provider tool calls")?;
    let parsed = calls
        .iter()
        .map(|call| {
            let id = call["id"].as_str().unwrap_or_default().into();
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into();
            let args = call
                .pointer("/function/arguments")
                .context("Provider tool arguments missing")?;
            let arguments = args
                .as_str()
                .map_or_else(|| args.to_string(), str::to_owned);
            Ok(LlmToolCall {
                id,
                name,
                arguments,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    validate_completed_calls(&parsed)?;
    Ok(parsed)
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
