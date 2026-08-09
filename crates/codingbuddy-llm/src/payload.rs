//! Payload building for LLM requests.
//!
//! Extracted from `lib.rs` as part of the module split — pure mechanical
//! refactor, no behavior change.

use anyhow::{Result, anyhow};
use codingbuddy_core::{
    AppliedCompatibility, ChatRequest, FimRequest, LlmConfig, LlmRequest, ProviderKind,
    max_output_tokens_for_model,
};
use serde_json::{Value, json};
use std::ops::Deref;

use crate::{protocol, provider_transform, providers};

/// A prepared HTTP payload plus the compatibility context describing any
/// transformations applied while building it.
#[derive(Debug, Clone)]
pub struct PreparedPayload {
    pub payload: Value,
    pub compatibility: AppliedCompatibility,
}

impl Deref for PreparedPayload {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

/// Build the payload for a simple (non-chat) completion request.
pub fn build_simple_payload(req: &LlmRequest, cfg: &LlmConfig) -> PreparedPayload {
    let provider = cfg.active_provider();
    let capabilities = cfg.capabilities_for_model(&req.model).unwrap_or_else(|| {
        codingbuddy_core::model_capabilities(
            cfg.active_provider_kind().unwrap_or(ProviderKind::Deepseek),
            &req.model,
        )
    });
    let fast_mode = cfg.fast_mode;
    let max_cap = max_output_tokens_for_model(capabilities.provider, &req.model, false);
    if req.max_tokens > max_cap {
        eprintln!(
            "warning: requested max_tokens ({}) exceeds model limit for {} ({}); capping",
            req.max_tokens, req.model, max_cap
        );
    }
    let max_tokens = if fast_mode {
        req.max_tokens.min(2048)
    } else {
        req.max_tokens.min(max_cap)
    };
    let temperature = if fast_mode {
        cfg.temperature.min(0.2)
    } else {
        cfg.temperature
    };
    let mut messages = Vec::new();
    if !cfg.language.trim().is_empty() && !cfg.language.eq_ignore_ascii_case("en") {
        messages.push(json!({
            "role": "system",
            "content": format!(
                "Respond in {} unless the user explicitly asks for another language.",
                cfg.language
            )
        }));
    }
    let mut compatibility = AppliedCompatibility {
        provider: capabilities.provider.as_key().to_string(),
        family: capabilities.family.as_key().to_string(),
        ..AppliedCompatibility::default()
    };

    if req.images.is_empty() {
        messages.push(json!({"role": "user", "content": req.prompt}));
    } else {
        let mut parts = vec![json!({"type": "text", "text": req.prompt})];
        for img in &req.images {
            if img.mime.trim().is_empty() {
                parts.push(json!({
                    "type": "text",
                    "text": "ERROR: A provided image input is malformed (missing mime type). Explain this limitation to the user.",
                }));
                compatibility
                    .degraded_inputs
                    .push("malformed image input".to_string());
                continue;
            }
            if !capabilities.supports_image_input || img.base64_data.trim().is_empty() {
                let reason = if img.base64_data.trim().is_empty() {
                    format!("empty {} input", img.mime)
                } else {
                    format!("unsupported {} input", img.mime)
                };
                compatibility.degraded_inputs.push(reason);
                parts.push(json!({
                    "type": "text",
                    "text": format!(
                        "ERROR: The current model cannot consume the provided {} input. Explain this limitation to the user and continue without reading the image.",
                        img.mime
                    ),
                }));
                continue;
            }
            parts.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{}", img.mime, img.base64_data)}
            }));
        }
        messages.push(json!({"role": "user", "content": parts}));
    }
    // Model names pass through as-is for all providers.
    let model = req.model.as_str();

    let mut payload = json!({
        "model": model,
        "messages": messages,
        "temperature": temperature,
        "stream": cfg.stream,
        "max_tokens": max_tokens
    });
    provider_transform::apply_provider_payload_options_map(
        &mut payload,
        &req.provider_options,
        &req.model,
        &provider,
        &capabilities,
        &mut compatibility,
    );
    compatibility.transforms.sort();
    compatibility.transforms.dedup();
    PreparedPayload {
        payload,
        compatibility,
    }
}

/// Build the payload for a fill-in-the-middle completion request.
pub fn build_fim_payload(req: &FimRequest, _cfg: &LlmConfig) -> Value {
    let mut payload = json!({
        "model": &req.model,
        "prompt": req.prompt,
        "max_tokens": req.max_tokens.min(8192),
        "stream": false
    });
    if let Some(s) = &req.suffix {
        payload["suffix"] = json!(s);
    }
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
    payload
}

/// Build the payload for a chat completion request.
pub fn build_chat_payload(req: &ChatRequest, cfg: &LlmConfig) -> Result<PreparedPayload> {
    let provider = cfg.active_provider();
    let capabilities = cfg.capabilities_for_model(&req.model).ok_or_else(|| {
        anyhow!(
            "unsupported llm.provider='{}' (supported: deepseek, openai-compatible, anthropic, google, groq, openrouter, ollama)",
            cfg.provider
        )
    })?;
    let chat_protocol = protocol::select_chat_protocol(&provider, capabilities.provider);

    // Native providers: build their own payload format
    match chat_protocol {
        protocol::ChatProtocol::OpenAiResponses | protocol::ChatProtocol::BedrockConverse => {
            let adapter = protocol::adapters::adapter_for(chat_protocol);
            return Err(anyhow!(
                "chat protocol '{}' is registered ({:?}) but not enabled in the runtime execution path yet; set provider.chat_protocol='openai-chat' or use a provider with native support for this protocol",
                chat_protocol.as_key(),
                adapter.payload_shape
            ));
        }
        protocol::ChatProtocol::AnthropicMessages => {
            let max_cap = max_output_tokens_for_model(
                capabilities.provider,
                &req.model,
                req.thinking
                    .as_ref()
                    .is_some_and(|t| t.thinking_type == "enabled"),
            );
            let payload = providers::anthropic::build_payload(req, req.max_tokens.min(max_cap))?;
            return Ok(PreparedPayload {
                payload,
                compatibility: AppliedCompatibility {
                    provider: "anthropic".to_string(),
                    family: capabilities.family.as_key().to_string(),
                    transforms: vec![
                        "protocol:anthropic-messages".to_string(),
                        "native_anthropic_messages_api".to_string(),
                    ],
                    degraded_inputs: vec![],
                },
            });
        }
        protocol::ChatProtocol::GeminiGenerateContent => {
            let max_cap = max_output_tokens_for_model(capabilities.provider, &req.model, false);
            let payload = providers::google::build_payload(req, req.max_tokens.min(max_cap))?;
            return Ok(PreparedPayload {
                payload,
                compatibility: AppliedCompatibility {
                    provider: "google".to_string(),
                    family: capabilities.family.as_key().to_string(),
                    transforms: vec![
                        "protocol:gemini-generate-content".to_string(),
                        "native_gemini_generate_content_api".to_string(),
                    ],
                    degraded_inputs: vec![],
                },
            });
        }
        _ => {} // Fall through to OpenAI-compatible path
    }
    let requested_thinking = req
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "enabled");
    let thinking_enabled =
        requested_thinking && capabilities.thinking_capability.accepts_thinking_config();
    let prepared_messages = provider_transform::preflight_chat_messages(req, &capabilities)?;
    let mut compatibility = AppliedCompatibility {
        provider: capabilities.provider.as_key().to_string(),
        family: capabilities.family.as_key().to_string(),
        transforms: prepared_messages.transforms,
        degraded_inputs: prepared_messages.degraded_inputs,
    };
    compatibility
        .transforms
        .push(format!("protocol:{}", chat_protocol.as_key()));

    // DeepSeek uses automatic server-side prefix caching — no client-side annotations needed.

    let max_cap = max_output_tokens_for_model(capabilities.provider, &req.model, thinking_enabled);
    let mut payload = json!({
        "model": &req.model,
        "messages": prepared_messages.messages,
        "max_tokens": req.max_tokens.min(max_cap),
        "stream": false
    });
    // Providers with explicit thinking configs require these sampling
    // params to be omitted while thinking is enabled.
    if thinking_enabled {
        if req.logprobs == Some(true) || req.top_logprobs.is_some() {
            // Cannot return Err from a non-Result fn; the caller validates via
            // complete_chat which calls validate_thinking_params. We strip silently
            // here and let validate_thinking_params catch it early.
        }
    } else {
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

    if let Some(fmt) = &req.response_format {
        payload["response_format"] = fmt.clone();
    }

    if let Some(logprobs) = req.logprobs
        && !thinking_enabled
    {
        payload["logprobs"] = json!(logprobs);
    }
    if let Some(top_logprobs) = req.top_logprobs
        && !thinking_enabled
    {
        payload["top_logprobs"] = json!(top_logprobs);
    }
    // Safety net: deepseek-reasoner thinks natively and rejects both
    // `thinking` config AND `tool_choice` (HTTP 400). The callers should
    // already omit these, but guard here as the last line of defense.
    if capabilities.thinking_capability.accepts_thinking_config()
        && let Some(ref thinking) = req.thinking
    {
        payload["thinking"] = serde_json::to_value(thinking)?;
    }
    if capabilities.supports_tool_calling
        && let Some(prepared_tools) = provider_transform::prepare_chat_tools_with_compatibility(
            req,
            &capabilities,
            &provider.base_url,
            &cfg.endpoint,
            &mut compatibility,
        )?
    {
        payload["tools"] = json!(prepared_tools.tools);
        if !prepared_tools.shim_only {
            if capabilities.supports_parallel_tool_calls
                && matches!(
                    capabilities.provider,
                    ProviderKind::OpenAiCompatible | ProviderKind::Ollama
                )
            {
                // OpenAI-compatible payload knob; omit for providers that do not
                // advertise this field to avoid spurious 400s.
                payload["parallel_tool_calls"] = json!(true);
            }
            if capabilities.supports_tool_choice {
                payload["tool_choice"] = serde_json::to_value(&req.tool_choice)?;
            }
        }
    }
    provider_transform::apply_provider_payload_options(
        &mut payload,
        req,
        &provider,
        &capabilities,
        &mut compatibility,
    );
    provider_transform::apply_chat_payload_compatibility_with_tracking(
        &mut payload,
        req,
        &capabilities,
        &mut compatibility,
    );
    compatibility.transforms.sort();
    compatibility.transforms.dedup();
    Ok(PreparedPayload {
        payload,
        compatibility,
    })
}
