//! Stateless OpenAI Responses encoding and ordered output assembly.
use anyhow::{Context, Result, bail, ensure};
use codingbuddy_core::{
    ChatMessage, ChatRequest, LlmResponse, LlmToolCall, NativeParts, StreamChunk, TokenUsage,
    ToolChoice, TransportPart,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

pub const PROTOCOL: &str = "openai-responses";

pub fn build_payload(req: &ChatRequest) -> Result<Value> {
    ensure!(
        req.presence_penalty.is_none() && req.frequency_penalty.is_none(),
        "Responses does not support presence or frequency penalties"
    );
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in &req.messages {
        match message {
            ChatMessage::System { content } => instructions.push(content.as_str()),
            ChatMessage::User { content } => input.push(json!({"role":"user","content":content})),
            ChatMessage::Assistant {
                content,
                tool_calls,
                native_parts,
                ..
            } => {
                if let Some(native) = native_parts.as_ref().filter(|p| p.protocol == PROTOCOL) {
                    input.extend(native.blocks.clone());
                    continue;
                }
                if let Some(text) = content {
                    input.push(
                        json!({"role":"assistant","content":[{"type":"output_text","text":text}]}),
                    );
                }
                for call in tool_calls {
                    super::super::validate_completed_calls(std::slice::from_ref(call))?;
                    input.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments}));
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
                ..
            } => input.push(
                json!({"type":"function_call_output","call_id":tool_call_id,"output":content}),
            ),
        }
    }
    if !req.images.is_empty() {
        let user = input
            .iter_mut()
            .rev()
            .find(|item| item["role"] == "user")
            .context("Responses image input requires a user message")?;
        let text = user["content"]
            .as_str()
            .context("invalid user image message")?
            .to_owned();
        let mut parts = vec![json!({"type":"input_text","text":text})];
        for image in &req.images {
            ensure!(
                image.mime.starts_with("image/") && !image.base64_data.is_empty(),
                "Responses requires a supported image input"
            );
            parts.push(json!({"type":"input_image","image_url":format!("data:{};base64,{}",image.mime,image.base64_data)}));
        }
        user["content"] = json!(parts);
    }
    let tools: Vec<_> = req
        .tools
        .iter()
        .map(|tool| {
            let mut value = json!({"type":"function","name":tool.function.name,
            "description":tool.function.description,"parameters":tool.function.parameters});
            if let Some(strict) = tool.function.strict {
                value["strict"] = json!(strict);
            }
            value
        })
        .collect();
    let choice = match &req.tool_choice {
        ToolChoice::Mode(mode) => json!(mode),
        ToolChoice::Function { function, .. } => json!({"type":"function","name":function.name}),
    };
    let mut payload = json!({"model":req.model,"input":input,"store":false,
        "include":["reasoning.encrypted_content"],"max_output_tokens":req.max_tokens,
        "stream":false,"tools":tools,"tool_choice":choice});
    if !instructions.is_empty() {
        payload["instructions"] = json!(instructions.join("\n\n"));
    }
    if let Some(value) = req.temperature {
        payload["temperature"] = json!(value);
    }
    if let Some(value) = req.top_p {
        payload["top_p"] = json!(value);
    }
    if req
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "enabled")
    {
        payload["reasoning"] = json!({"effort":"medium"});
        payload
            .as_object_mut()
            .expect("object")
            .remove("temperature");
        payload.as_object_mut().expect("object").remove("top_p");
    }
    if req.logprobs == Some(true) {
        payload["include"]
            .as_array_mut()
            .expect("array")
            .push(json!("message.output_text.logprobs"));
        if let Some(top) = req.top_logprobs {
            ensure!(top <= 20, "Unsupported Responses top_logprobs value");
            payload["top_logprobs"] = json!(top);
        }
    }
    if let Some(format) = &req.response_format {
        let format = if format["type"] == "json_schema" {
            let mut schema = format["json_schema"]
                .as_object()
                .context("Malformed Responses JSON schema format")?
                .clone();
            schema.insert("type".into(), json!("json_schema"));
            Value::Object(schema)
        } else {
            format.clone()
        };
        payload["text"] = json!({"format":format});
    }
    Ok(payload)
}

/// Only implemented options may extend a Responses request. Storage and hosted
/// tool controls cannot override the stateless runner's contract.
pub(crate) fn apply_options(payload: &mut Value, layers: &[&Value]) -> Result<()> {
    for layer in layers {
        if layer.is_null() {
            continue;
        }
        let object = layer
            .as_object()
            .context("Unsupported Responses options shape")?;
        for (name, value) in object {
            match name.as_str() {
                "chat_protocol" | "protocol" => {}
                "reasoning" => {
                    let reasoning = value
                        .as_object()
                        .context("Unsupported Responses reasoning options")?;
                    for (key, value) in reasoning {
                        let valid = match key.as_str() {
                            "effort" => matches!(
                                value.as_str(),
                                Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh")
                            ),
                            "summary" => {
                                matches!(value.as_str(), Some("auto" | "concise" | "detailed"))
                            }
                            _ => false,
                        };
                        ensure!(valid, "Unsupported Responses reasoning option");
                    }
                    payload["reasoning"] = value.clone();
                }
                "parallel_tool_calls" => {
                    ensure!(value.is_boolean(), "Unsupported parallel_tool_calls value");
                    payload[name] = value.clone();
                }
                _ => bail!("Unsupported Responses option"),
            }
        }
    }
    Ok(())
}

fn incomplete(value: &Value) -> anyhow::Error {
    let mut error = crate::error::ProviderError::new(
        crate::protocol::ChatProtocol::OpenAiResponses,
        crate::error::FailureKind::IncompleteResponse,
    );
    error.provider_code = value
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        .filter(|reason| matches!(*reason, "max_output_tokens" | "content_filter"))
        .map(str::to_owned);
    error.into()
}

pub fn parse_response(value: &Value) -> Result<LlmResponse> {
    if !value["error"].is_null() {
        return Err(crate::error::ProviderError::http(
            crate::protocol::ChatProtocol::OpenAiResponses,
            200,
            &value.to_string(),
        )
        .into());
    }
    match value["status"].as_str() {
        Some("completed") => {}
        Some("incomplete") => return Err(incomplete(value)),
        Some("failed") => bail!("Provider response failed"),
        Some("cancelled") => bail!("Provider response cancelled"),
        _ => bail!("Provider completion missing its terminal status"),
    }
    let blocks = value["output"]
        .as_array()
        .context("Malformed Responses output")?;
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut parts = Vec::new();
    let mut item_ids = HashSet::new();
    for block in blocks {
        if let Some(id) = block["id"].as_str() {
            ensure!(item_ids.insert(id), "Duplicate provider item ID");
        }
        match block["type"].as_str() {
            Some("message") => {
                ensure!(
                    block["status"].is_null() || block["status"] == "completed",
                    "Unfinished provider message"
                );
                for content in block["content"]
                    .as_array()
                    .context("Malformed Responses message")?
                {
                    match content["type"].as_str() {
                        Some("output_text") => {
                            let value = content["text"]
                                .as_str()
                                .context("Malformed Responses text")?;
                            text.push_str(value);
                            parts.push(TransportPart::Text(value.into()));
                        }
                        Some("refusal") => {
                            let value = content["refusal"]
                                .as_str()
                                .context("Malformed provider refusal")?;
                            text.push_str(value);
                            parts.push(TransportPart::Text(value.into()));
                        }
                        _ => bail!("Unsupported Responses message part"),
                    }
                }
            }
            Some("function_call") => {
                ensure!(
                    block["status"].is_null() || block["status"] == "completed",
                    "Unfinished provider function call"
                );
                let id = block["call_id"]
                    .as_str()
                    .context("Missing provider call ID")?
                    .to_owned();
                let call = LlmToolCall {
                    id: id.clone(),
                    name: block["name"]
                        .as_str()
                        .context("Missing provider function name")?
                        .into(),
                    arguments: block["arguments"]
                        .as_str()
                        .context("Missing provider arguments")?
                        .into(),
                };
                parts.push(TransportPart::Call(id));
                calls.push(call);
            }
            Some("reasoning") => {}
            _ => bail!("Unsupported Responses output item"),
        }
    }
    super::super::validate_completed_calls(&calls)?;
    parts.push(TransportPart::Native(NativeParts {
        protocol: PROTOCOL.into(),
        blocks: blocks.clone(),
    }));
    let usage = value.get("usage").filter(|v| !v.is_null()).map(|u| {
        let input = u["input_tokens"].as_u64().unwrap_or(0);
        let cached = u
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(input);
        TokenUsage {
            prompt_tokens: input,
            completion_tokens: u["output_tokens"].as_u64().unwrap_or(0),
            prompt_cache_hit_tokens: cached,
            prompt_cache_miss_tokens: input - cached,
            reasoning_tokens: u
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        }
    });
    Ok(LlmResponse {
        text,
        tool_calls: calls,
        transport_parts: parts,
        reasoning_content: String::new(),
        finish_reason: if blocks.iter().any(|b| b["type"] == "function_call") {
            "tool_calls"
        } else {
            "stop"
        }
        .into(),
        usage,
        compatibility: None,
    })
}

#[derive(Default)]
pub struct Stream {
    items: BTreeMap<u64, Value>,
    done: HashSet<u64>,
    text: BTreeMap<(u64, u64), (String, String)>,
}

impl Stream {
    pub fn frame(
        &mut self,
        value: &Value,
        callback: &dyn Fn(StreamChunk),
    ) -> Result<Option<LlmResponse>> {
        match value["type"]
            .as_str()
            .context("Missing Responses event type")?
        {
            "response.output_item.added" => {
                let index = value["output_index"]
                    .as_u64()
                    .context("Missing provider output index")?;
                ensure!(
                    !self.items.contains_key(&index),
                    "Duplicate provider output index"
                );
                let item = value["item"].clone();
                ensure!(item["id"].as_str().is_some(), "Missing provider item ID");
                ensure!(
                    !self.items.values().any(|other| other["id"] == item["id"]),
                    "Duplicate provider item ID"
                );
                self.items.insert(index, item);
            }
            "response.function_call_arguments.delta" => {
                let item = self.active_item(value)?;
                ensure!(
                    item["type"] == "function_call",
                    "Argument delta on non-function item"
                );
                let mut args = item["arguments"].as_str().unwrap_or_default().to_owned();
                args.push_str(
                    value["delta"]
                        .as_str()
                        .context("Malformed argument delta")?,
                );
                item["arguments"] = json!(args);
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                ensure!(
                    self.active_item(value)?["type"] == "message",
                    "Text delta on non-message item"
                );
                let index = value["output_index"]
                    .as_u64()
                    .context("Missing output index")?;
                let content_index = value["content_index"]
                    .as_u64()
                    .context("Missing content index")?;
                let kind = if value["type"] == "response.refusal.delta" {
                    "refusal"
                } else {
                    "output_text"
                };
                let delta = value["delta"]
                    .as_str()
                    .context("Malformed provider text delta")?;
                let text = self
                    .text
                    .entry((index, content_index))
                    .or_insert_with(|| (kind.into(), String::new()));
                ensure!(text.0 == kind, "Changed streamed content type");
                text.1.push_str(delta);
                callback(StreamChunk::ContentDelta(delta.into()));
            }
            "response.function_call_arguments.done" => {
                let item = self.active_item(value)?;
                ensure!(
                    item["arguments"] == value["arguments"],
                    "Provider completed arguments differ from streamed arguments"
                );
            }
            "response.output_item.done" => {
                let index = value["output_index"]
                    .as_u64()
                    .context("Missing provider output index")?;
                let item = self
                    .items
                    .get(&index)
                    .context("Unknown provider output item")?;
                ensure!(
                    !self.done.contains(&index) && item["id"] == value["item"]["id"],
                    "Changed or duplicate provider output item"
                );
                if item["type"] == "function_call" {
                    ensure!(
                        item["call_id"] == value["item"]["call_id"]
                            && item["name"] == value["item"]["name"]
                            && item["arguments"] == value["item"]["arguments"],
                        "Provider changed a function call"
                    );
                }
                ensure!(
                    item["type"] == value["item"]["type"],
                    "Changed provider output type"
                );
                for ((item_index, content_index), (kind, text)) in &self.text {
                    if *item_index == index {
                        let part = &value["item"]["content"][*content_index as usize];
                        let field = if kind == "refusal" { "refusal" } else { "text" };
                        ensure!(
                            part["type"] == *kind && part[field].as_str() == Some(text.as_str()),
                            "Completed text differs from streamed text"
                        );
                    }
                }
                self.items.insert(index, value["item"].clone());
                self.done.insert(index);
            }
            "response.completed" => {
                ensure!(
                    self.done.len() == self.items.len(),
                    "Provider stopped with unfinished output items"
                );
                let response = &value["response"];
                let output = response["output"]
                    .as_array()
                    .context("Missing completed provider output")?;
                ensure!(
                    output.len() == self.items.len(),
                    "Completed output differs from streamed items"
                );
                for ((index, item), complete) in self.items.iter().zip(output) {
                    ensure!(
                        *index < output.len() as u64 && item == complete,
                        "Completed output differs from streamed items"
                    );
                }
                return parse_response(response).map(Some);
            }
            "response.cancelled" => bail!("Provider response cancelled"),
            "response.failed" => {
                return Err(crate::error::ProviderError::http(
                    crate::protocol::ChatProtocol::OpenAiResponses,
                    200,
                    &value["response"].to_string(),
                )
                .into());
            }
            "response.incomplete" => return Err(incomplete(&value["response"])),
            "error" => bail!("Provider sent an error during the response stream"),
            "response.created"
            | "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.refusal.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done" => {}
            _ => bail!("Unsupported Responses streaming event"),
        }
        Ok(None)
    }

    fn active_item(&mut self, value: &Value) -> Result<&mut Value> {
        let index = value["output_index"]
            .as_u64()
            .context("Missing provider output index")?;
        ensure!(
            !self.done.contains(&index),
            "Provider delta after item completion"
        );
        let item = self
            .items
            .get_mut(&index)
            .context("Unknown provider output item")?;
        ensure!(
            item["id"] == value["item_id"],
            "Provider changed an item ID"
        );
        Ok(item)
    }
}
