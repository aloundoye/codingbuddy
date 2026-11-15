use anyhow::{Context, Result, ensure};
use codingbuddy_core::{LlmToolCall, NativeParts, TransportPart};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) const ANTHROPIC: &str = "anthropic-messages";
pub(crate) const GEMINI: &str = "gemini-generate-content";

pub(crate) fn native_response(
    protocol: &str,
    blocks: Vec<Value>,
) -> Result<(Vec<TransportPart>, Vec<LlmToolCall>)> {
    let mut parts = Vec::new();
    let mut calls = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let tool = if protocol == GEMINI {
            block.get("functionCall")
        } else {
            (block["type"] == "tool_use").then_some(block)
        };
        if let Some(tool) = tool {
            let id = tool["id"].as_str().map(str::to_owned).unwrap_or_else(|| {
                if protocol == GEMINI {
                    format!("codingbuddy-gemini-local:{index}")
                } else {
                    String::new()
                }
            });
            let name = tool["name"].as_str().unwrap_or_default().to_owned();
            let args = if protocol == GEMINI {
                tool.get("args")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}))
            } else {
                tool["input"].clone()
            };
            calls.push(LlmToolCall {
                id: id.clone(),
                name,
                arguments: args.to_string(),
            });
            parts.push(TransportPart::Call(id));
        } else if block["thought"] != true
            && let Some(text) = block["text"].as_str()
        {
            parts.push(TransportPart::Text(text.to_owned()));
        }
    }
    super::validate_completed_calls(&calls)?;
    parts.push(TransportPart::Native(NativeParts {
        protocol: protocol.into(),
        blocks,
    }));
    Ok((parts, calls))
}

#[derive(Default)]
pub(crate) struct AnthropicBlocks {
    active: BTreeMap<u64, (Value, String)>,
    completed: BTreeMap<u64, Value>,
}

impl AnthropicBlocks {
    pub(crate) fn frame(&mut self, event: &str, frame: &Value) -> Result<()> {
        if !matches!(
            event,
            "content_block_start" | "content_block_delta" | "content_block_stop"
        ) {
            return Ok(());
        }
        let index = frame["index"]
            .as_u64()
            .context("Provider content block index missing")?;
        match event {
            "content_block_start" => {
                let block = frame["content_block"].clone();
                ensure!(block.is_object(), "Malformed provider content block");
                ensure!(
                    !self.completed.contains_key(&index) && !self.active.contains_key(&index),
                    "Duplicate provider content block"
                );
                self.active.insert(index, (block, String::new()));
            }
            "content_block_delta" => {
                let (block, input) = self
                    .active
                    .get_mut(&index)
                    .context("Provider delta for an inactive content block")?;
                let delta = &frame["delta"];
                let field = match delta["type"].as_str() {
                    Some("text_delta") => "text",
                    Some("thinking_delta") => "thinking",
                    Some("signature_delta") => "signature",
                    Some("input_json_delta") => {
                        input.push_str(
                            delta["partial_json"]
                                .as_str()
                                .context("Malformed provider argument fragment")?,
                        );
                        return Ok(());
                    }
                    _ => return Ok(()),
                };
                let mut joined = block[field].as_str().unwrap_or_default().to_owned();
                joined.push_str(
                    delta[field]
                        .as_str()
                        .context("Malformed provider content fragment")?,
                );
                block[field] = Value::String(joined);
            }
            _ => {
                let (mut block, input) = self
                    .active
                    .remove(&index)
                    .context("Provider stopped an inactive content block")?;
                if !input.is_empty() {
                    block["input"] = serde_json::from_str(&input)
                        .context("Malformed provider tool arguments")?;
                }
                self.completed.insert(index, block);
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Vec<Value>> {
        ensure!(
            self.active.is_empty(),
            "Provider stopped with unfinished content blocks"
        );
        Ok(self.completed.into_values().collect())
    }
}
