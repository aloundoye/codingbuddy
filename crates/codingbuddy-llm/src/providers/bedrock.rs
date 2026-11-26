use crate::{
    error::{FailureKind, ProviderError},
    protocol::ChatProtocol,
};
use anyhow::{Context, Result, ensure};
use codingbuddy_core::{
    ChatMessage, ChatRequest, LlmResponse, LlmToolCall, NativeParts, StreamCallback, StreamChunk,
    TokenUsage, ToolChoice, TransportPart,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const PROTOCOL: ChatProtocol = ChatProtocol::BedrockConverse;

fn failure(category: FailureKind) -> anyhow::Error {
    ProviderError::new(PROTOCOL, category).into()
}

pub(crate) fn endpoint(
    kind: codingbuddy_core::ProviderKind,
    deployment: &codingbuddy_core::CloudDeployment,
    model: &str,
    streaming: bool,
) -> Result<String> {
    let (
        codingbuddy_core::ProviderKind::Bedrock,
        codingbuddy_core::CloudDeployment::Bedrock { region, .. },
    ) = (kind, deployment)
    else {
        return Err(failure(FailureKind::UnsupportedFeature));
    };
    let region = crate::route::label(region)?;
    anyhow::ensure!(
        region.bytes().all(|b| !b.is_ascii_uppercase())
            && region.starts_with(|c: char| c.is_ascii_lowercase())
            && region.contains('-'),
        "Invalid AWS region"
    );
    anyhow::ensure!(
        !model.is_empty()
            && model.len() <= 2048
            && model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
            && !matches!(model, "." | ".."),
        "Invalid Bedrock model or inference profile ID"
    );
    let suffix = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    let mut url = reqwest::Url::parse(&format!("https://bedrock-runtime.{region}.{suffix}"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid Bedrock endpoint"))?
        .push("model")
        .push(model)
        .push(if streaming {
            "converse-stream"
        } else {
            "converse"
        });
    Ok(url.into())
}

pub(crate) fn encode(req: &ChatRequest) -> Result<Value> {
    ensure!(
        req.logprobs != Some(true)
            && req.top_logprobs.is_none()
            && req.response_format.is_none()
            && req.presence_penalty.is_none()
            && req.frequency_penalty.is_none(),
        failure(FailureKind::UnsupportedFeature)
    );
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for msg in &req.messages {
        let (role, mut blocks) = match msg {
            ChatMessage::System { content } => {
                if !content.is_empty() {
                    system.push(json!({"text":content}));
                }
                continue;
            }
            ChatMessage::User { content } => ("user", vec![json!({"text":content})]),
            ChatMessage::Tool {
                tool_call_id,
                content,
                ..
            } => (
                "user",
                vec![json!({"toolResult":{"toolUseId":tool_call_id,"content":[{"text":content}]}})],
            ),
            ChatMessage::Assistant {
                content,
                tool_calls,
                native_parts,
                ..
            } => {
                let mut blocks = Vec::new();
                if let Some(native) = native_parts
                    .as_ref()
                    .filter(|p| p.protocol == PROTOCOL.as_key())
                {
                    blocks.clone_from(&native.blocks);
                } else {
                    if let Some(text) = content.as_ref().filter(|text| !text.is_empty()) {
                        blocks.push(json!({"text":text}));
                    }
                    for call in tool_calls {
                        let input: Value = serde_json::from_str(&call.arguments)?;
                        blocks.push(json!({"toolUse":{"toolUseId": call.id,"name":call.name,"input":input}}));
                    }
                }
                ("assistant", blocks)
            }
        };
        if blocks.is_empty() {
            continue;
        }
        if let Some(last) = messages.last_mut().filter(|last| last["role"] == role)
            && let Some(content) = last["content"].as_array_mut()
        {
            content.append(&mut blocks);
        } else {
            messages.push(json!({"role":role,"content":blocks}));
        }
    }
    if !req.images.is_empty() {
        let last = messages
            .last_mut()
            .filter(|m| m["role"] == "user")
            .context("Bedrock images require a final user message")?;
        for image in &req.images {
            let format = match image.mime.as_str() {
                "image/jpeg" => "jpeg",
                "image/png" => "png",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => return Err(failure(FailureKind::UnsupportedFeature)),
            };
            last["content"]
                .as_array_mut()
                .ok_or_else(|| failure(FailureKind::UnsupportedFeature))?
                .push(json!({"image":{
                    "format":format,"source":{"bytes":image.base64_data}
                }}));
        }
    }
    let mut inference = json!({"maxTokens":req.max_tokens});
    if let Some(value) = req.temperature {
        inference["temperature"] = json!(value);
    }
    if let Some(value) = req.top_p {
        inference["topP"] = json!(value);
    }
    let mut payload = json!({"messages":messages,"inferenceConfig":inference});
    if !system.is_empty() {
        payload["system"] = json!(system);
    }
    if !req.tools.is_empty()
        && !matches!(&req.tool_choice, ToolChoice::Mode(mode) if mode == "none")
    {
        let choice = match &req.tool_choice {
            ToolChoice::Mode(mode) if mode == "auto" => json!({"auto":{}}),
            ToolChoice::Mode(mode) if mode == "required" => json!({"any":{}}),
            ToolChoice::Function { function, .. } => json!({"tool":{"name":function.name}}),
            _ => return Err(failure(FailureKind::UnsupportedFeature)),
        };
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                json!({"toolSpec":{
                    "name":tool.function.name,"description":tool.function.description,
                    "inputSchema":{"json":tool.function.parameters}
                }})
            })
            .collect();
        payload["toolConfig"] = json!({"tools":tools,"toolChoice":choice});
    }
    Ok(payload)
}

pub(crate) fn decode(value: &Value) -> Result<LlmResponse> {
    ensure!(
        value["output"]["message"]["role"] == "assistant",
        failure(FailureKind::MalformedResponse)
    );
    let blocks = value["output"]["message"]["content"]
        .as_array()
        .context("Missing Bedrock content")?;
    assemble(blocks.clone(), &value["stopReason"], &value["usage"])
}

fn assemble(blocks: Vec<Value>, reason: &Value, usage: &Value) -> Result<LlmResponse> {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut parts = Vec::new();
    for block in &blocks {
        if let Some(delta) = block["text"].as_str() {
            text.push_str(delta);
            parts.push(TransportPart::Text(delta.to_owned()));
        } else if let Some(tool) = block.get("toolUse") {
            let id = tool["toolUseId"]
                .as_str()
                .context("Missing Bedrock tool ID")?
                .to_owned();
            let name = tool["name"]
                .as_str()
                .context("Missing Bedrock tool name")?
                .to_owned();
            let arguments = tool
                .get("input")
                .context("Missing Bedrock tool input")?
                .to_string();
            parts.push(TransportPart::Call(id.clone()));
            calls.push(LlmToolCall {
                id,
                name,
                arguments,
            });
        } else {
            ensure!(
                block.get("reasoningContent").is_some(),
                failure(FailureKind::MalformedResponse)
            );
        }
    }
    crate::validate_completed_calls(&calls)?;
    let finish_reason = match reason.as_str() {
        Some("end_turn" | "stop_sequence") if calls.is_empty() => "stop",
        Some("tool_use") if !calls.is_empty() => "tool_calls",
        Some(
            "max_tokens"
            | "model_context_window_exceeded"
            | "guardrail_intervened"
            | "content_filtered",
        ) => return Err(failure(FailureKind::IncompleteResponse)),
        _ => return Err(failure(FailureKind::MalformedResponse)),
    };
    parts.push(TransportPart::Native(NativeParts {
        protocol: PROTOCOL.as_key().into(),
        blocks,
    }));
    let number = |key| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Ok(LlmResponse {
        text,
        tool_calls: calls,
        finish_reason: finish_reason.into(),
        transport_parts: parts,
        reasoning_content: String::new(),
        compatibility: None,
        usage: usage.is_object().then(|| TokenUsage {
            prompt_tokens: number("inputTokens")
                + number("cacheReadInputTokens")
                + number("cacheWriteInputTokens"),
            prompt_cache_hit_tokens: number("cacheReadInputTokens"),
            prompt_cache_miss_tokens: number("inputTokens") + number("cacheWriteInputTokens"),
            completion_tokens: number("outputTokens"),
            reasoning_tokens: 0,
        }),
    })
}

#[derive(Default)]
struct Block {
    value: Value,
    arguments: String,
    complete: bool,
}

async fn frame(
    input: &mut crate::chat_transport::ByteStream<'_>,
    remaining: &mut usize,
) -> Result<Option<(String, Value)>> {
    let mut size = [0; 4];
    if !input.read_exact(&mut size).await? {
        return Ok(None);
    }
    let length = u32::from_be_bytes(size) as usize;
    ensure!(
        (16..=16 * 1024 * 1024).contains(&length),
        failure(FailureKind::MalformedResponse)
    );
    *remaining = remaining
        .checked_sub(length)
        .context("Bedrock stream exceeds size limit")?;
    let mut bytes = vec![0; length];
    bytes[..4].copy_from_slice(&size);
    input.read_exact(&mut bytes[4..]).await?;
    let message = aws_smithy_eventstream::frame::read_message_from(bytes.as_slice())
        .map_err(|_| failure(FailureKind::MalformedResponse))?;
    let header = |name| {
        message
            .headers()
            .iter()
            .find(|h| h.name().as_str() == name)
            .and_then(|h| h.value().as_string().ok())
            .map(|s| s.as_str())
    };
    ensure!(
        header(":message-type") == Some("event"),
        failure(FailureKind::ProviderFailure)
    );
    Ok(Some((
        header(":event-type")
            .context("Missing Bedrock event type")?
            .to_owned(),
        serde_json::from_slice(message.payload())?,
    )))
}

pub(crate) async fn stream(
    input: &mut crate::chat_transport::ByteStream<'_>,
    cb: StreamCallback,
) -> Result<LlmResponse> {
    let mut blocks = BTreeMap::<u64, Block>::new();
    let mut started = false;
    let mut stopped = Value::Null;
    let mut usage = Value::Null;
    let mut remaining = 32 * 1024 * 1024;
    while let Some((event, value)) = frame(input, &mut remaining).await.map_err(|error| {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::UnexpectedEof)
        {
            failure(FailureKind::IncompleteStream)
        } else {
            error
        }
    })? {
        match event.as_str() {
            "messageStart" if !started && value["role"] == "assistant" => started = true,
            "messageStop"
                if started && stopped.is_null() && blocks.values().all(|b| b.complete) =>
            {
                stopped = value
                    .get("stopReason")
                    .filter(|v| v.is_string())
                    .context("Missing Bedrock stop reason")?
                    .clone();
            }
            "metadata" if !stopped.is_null() && usage.is_null() => {
                usage = value
                    .get("usage")
                    .filter(|v| v.is_object())
                    .context("Missing Bedrock stream usage")?
                    .clone();
            }
            "contentBlockStart" | "contentBlockDelta" | "contentBlockStop"
                if started && stopped.is_null() =>
            {
                let index = value["contentBlockIndex"]
                    .as_u64()
                    .context("Missing Bedrock block index")?;
                ensure!(index < 4096, failure(FailureKind::MalformedResponse));
                let block = blocks.entry(index).or_default();
                ensure!(!block.complete, failure(FailureKind::MalformedResponse));
                match event.as_str() {
                    "contentBlockStart" => {
                        ensure!(
                            block.value.is_null() && value["start"]["toolUse"].is_object(),
                            failure(FailureKind::MalformedResponse)
                        );
                        block.value = json!({"toolUse":value["start"]["toolUse"]});
                    }
                    "contentBlockDelta" => {
                        let delta = &value["delta"];
                        if let Some(text) = delta["text"].as_str() {
                            append_part(&mut block.value, "text", text)?;
                            cb(StreamChunk::ContentDelta(text.into()));
                        } else if let Some(input) = delta["toolUse"]["input"].as_str() {
                            ensure!(
                                block.value["toolUse"].is_object(),
                                failure(FailureKind::MalformedResponse)
                            );
                            append_string(&mut block.arguments, input)?;
                        } else if let Some(reasoning) = delta["reasoningContent"].as_object() {
                            if block.value.is_null() {
                                block.value = json!({"reasoningContent":{}});
                            }
                            let target = block
                                .value
                                .get_mut("reasoningContent")
                                .context("Mixed Bedrock content block types")?;
                            // Redacted bytes can be replayed as one opaque base64 value.
                            if let Some(data) =
                                reasoning.get("redactedContent").and_then(Value::as_str)
                            {
                                ensure!(
                                    target.as_object().is_some_and(|o| o.is_empty()),
                                    failure(FailureKind::MalformedResponse)
                                );
                                target["redactedContent"] = json!(data);
                            } else {
                                ensure!(
                                    target.get("redactedContent").is_none(),
                                    failure(FailureKind::MalformedResponse)
                                );
                                for (key, fragment) in reasoning {
                                    ensure!(
                                        matches!(key.as_str(), "text" | "signature"),
                                        failure(FailureKind::MalformedResponse)
                                    );
                                    if target["reasoningText"].is_null() {
                                        target["reasoningText"] = json!({});
                                    }
                                    append_part(
                                        &mut target["reasoningText"],
                                        key,
                                        fragment.as_str().context("Invalid reasoning fragment")?,
                                    )?;
                                }
                            }
                        } else {
                            return Err(failure(FailureKind::MalformedResponse));
                        }
                    }
                    _ => {
                        ensure!(
                            !block.value.is_null(),
                            failure(FailureKind::MalformedResponse)
                        );
                        if block.value["toolUse"].is_object() {
                            block.value["toolUse"]["input"] =
                                serde_json::from_str(&block.arguments)?;
                        }
                        block.complete = true;
                    }
                }
            }
            _ => return Err(failure(FailureKind::MalformedResponse)),
        }
    }
    ensure!(
        !stopped.is_null() && !usage.is_null(),
        failure(FailureKind::IncompleteStream)
    );
    ensure!(
        blocks.keys().copied().eq(0..blocks.len() as u64),
        failure(FailureKind::MalformedResponse)
    );
    assemble(
        blocks.into_values().map(|b| b.value).collect(),
        &stopped,
        &usage,
    )
}

fn append_string(buffer: &mut String, fragment: &str) -> Result<()> {
    ensure!(
        buffer.len().saturating_add(fragment.len()) <= 16 * 1024 * 1024,
        failure(FailureKind::MalformedResponse)
    );
    buffer.push_str(fragment);
    Ok(())
}

fn append_part(value: &mut Value, key: &str, fragment: &str) -> Result<()> {
    if value.is_null() {
        *value = json!({});
    }
    ensure!(
        value
            .as_object()
            .is_some_and(|object| object.keys().all(|existing| existing == key
                || matches!(
                    (existing.as_str(), key),
                    ("text", "signature") | ("signature", "text")
                ))),
        failure(FailureKind::MalformedResponse)
    );
    let text = &mut value[key];
    if text.is_null() {
        *text = Value::String(String::new());
    }
    let Value::String(text) = text else {
        return Err(failure(FailureKind::MalformedResponse));
    };
    append_string(text, fragment)
}
