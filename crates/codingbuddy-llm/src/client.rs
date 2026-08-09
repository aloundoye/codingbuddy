use anyhow::{Result, anyhow};
use chrono::{DateTime, NaiveDateTime, Utc};
use codingbuddy_core::{
    AppliedCompatibility, CancellationToken, ChatMessage, ChatRequest, FimRequest, LlmConfig,
    LlmRequest, LlmResponse, LlmToolCall, ProviderKind, StreamCallback, StreamChunk, ToolChoice,
};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{HeaderName, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::io::BufRead;
use std::thread;
use std::time::Duration;

use crate::payload;
use crate::protocol;
use crate::provider_transform;
use crate::providers;
use crate::retry;
use crate::streaming::{
    self, StreamToolCall, merge_stream_tool_calls, parse_fim_non_streaming_payload,
    parse_non_streaming_payload, parse_streaming_payload, parse_tool_calls_array,
    parse_usage_object,
};

/// Base delay for network/transport error retries (1s, 2s, 4s exponential backoff).
pub(crate) const NETWORK_RETRY_BASE_MS: u64 = 1000;

pub trait LlmClient {
    fn complete(&self, req: &LlmRequest) -> Result<LlmResponse>;

    /// Streaming variant that invokes `cb` for each token chunk as it arrives.
    /// Returns the fully assembled `LlmResponse` once the stream ends.
    fn complete_streaming(&self, req: &LlmRequest, cb: StreamCallback) -> Result<LlmResponse>;

    /// Chat completion with tool definitions (function calling).
    /// Sends a multi-turn conversation with tool schemas and returns the response.
    fn complete_chat(&self, req: &ChatRequest) -> Result<LlmResponse>;

    /// Streaming chat completion with tool definitions.
    fn complete_chat_streaming(&self, req: &ChatRequest, cb: StreamCallback)
    -> Result<LlmResponse>;

    /// Discover available models from the provider's API.
    /// Default: returns empty (no discovery support).
    fn list_models(&self) -> Vec<String> {
        Vec::new()
    }

    /// Beta FIM completion (Fill-In-The-Middle)
    fn complete_fim(&self, req: &FimRequest) -> Result<LlmResponse>;

    /// Beta FIM completion streaming (Fill-In-The-Middle)
    fn complete_fim_streaming(&self, req: &FimRequest, cb: StreamCallback) -> Result<LlmResponse>;
}

#[derive(Debug, Clone)]
pub struct ApiClient {
    pub(crate) cfg: LlmConfig,
    client: Client,
    /// Optional cancellation token — checked between SSE reads during streaming.
    /// When cancelled, returns partial response with content accumulated so far.
    pub(crate) cancel_token: Option<CancellationToken>,
    /// Cached API key resolved eagerly at construction time.
    /// Avoids env var races when the key is cleared during logout.
    pub(crate) api_key: Option<String>,
}

/// Resolve an API key from env var then config fallback.
fn resolve_key_from_config(cfg: &LlmConfig) -> Option<String> {
    let provider = cfg.active_provider();
    let env_key = provider.api_key_env.trim();
    let env_value = if env_key.is_empty() {
        None
    } else {
        std::env::var(env_key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    env_value.or_else(|| {
        cfg.api_key
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

impl ApiClient {
    pub fn new(cfg: LlmConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_seconds))
            .build()?;
        let api_key = resolve_key_from_config(&cfg);
        Ok(Self {
            cfg,
            client,
            cancel_token: None,
            api_key,
        })
    }

    /// Clear the cached API key (e.g. on logout). Subsequent requests will
    /// fail with "API key not set" rather than using a stale credential.
    pub fn clear_api_key(&mut self) {
        self.api_key = None;
    }

    /// Re-resolve the API key from env/config. Called after a 401 response
    /// in case the key was rotated or refreshed since construction.
    pub fn refresh_api_key(&mut self) {
        self.api_key = resolve_key_from_config(&self.cfg);
    }

    /// Attach a cancellation token that will be checked during streaming.
    pub fn set_cancel_token(&mut self, token: CancellationToken) {
        self.cancel_token = Some(token);
    }

    /// Discover available models from the provider's /v1/models endpoint.
    /// Returns a list of model IDs. Fails gracefully (empty vec) on error.
    fn discover_models_from_api(&self) -> Vec<String> {
        let provider = self.provider_config();
        let base = provider.base_url.trim_end_matches('/');
        let prefix = if provider.openai_compat_prefix {
            "/v1"
        } else {
            ""
        };
        let url = format!("{base}{prefix}/models");
        let mut req = self.client.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = match req.timeout(Duration::from_secs(5)).send() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let body: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        body.get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn provider_kind(&self) -> Result<ProviderKind> {
        self.cfg.active_provider_kind().ok_or_else(|| {
            anyhow!(
                "unsupported llm.provider='{}' (supported: deepseek, openai-compatible, anthropic, google, groq, openrouter, ollama)",
                self.cfg.provider
            )
        })
    }

    fn provider_config(&self) -> codingbuddy_core::ProviderConfig {
        self.cfg.active_provider()
    }

    fn apply_auth(
        &self,
        builder: reqwest::blocking::RequestBuilder,
        api_key: Option<&str>,
    ) -> reqwest::blocking::RequestBuilder {
        self.apply_auth_for_protocol(builder, api_key, protocol::ChatProtocol::OpenAiChat)
    }

    fn apply_auth_for_protocol(
        &self,
        builder: reqwest::blocking::RequestBuilder,
        api_key: Option<&str>,
        chat_protocol: protocol::ChatProtocol,
    ) -> reqwest::blocking::RequestBuilder {
        let provider_config = self.provider_config();
        let provider = self.provider_kind().unwrap_or(ProviderKind::Deepseek);
        let auth_strategy =
            protocol::select_auth_strategy(&provider_config, provider, chat_protocol);
        let mut builder = if provider == ProviderKind::Anthropic
            || chat_protocol == protocol::ChatProtocol::AnthropicMessages
        {
            builder
                .header("anthropic-version", providers::anthropic::ANTHROPIC_VERSION)
                .header(
                    "anthropic-beta",
                    "prompt-caching-2024-07-31,output-128k-2025-02-19",
                )
                .header("content-type", "application/json")
        } else {
            builder
        };
        builder = match auth_strategy {
            protocol::AuthStrategy::Bearer => match api_key {
                Some(key) if !key.trim().is_empty() => builder.bearer_auth(key),
                _ => builder,
            },
            protocol::AuthStrategy::XApiKey => match api_key {
                Some(key) if !key.trim().is_empty() => builder.header("x-api-key", key),
                _ => builder,
            },
            protocol::AuthStrategy::QueryApiKey
            | protocol::AuthStrategy::AwsSigV4
            | protocol::AuthStrategy::None => builder,
        };
        self.apply_custom_provider_headers(builder, &provider_config)
    }

    fn apply_custom_provider_headers(
        &self,
        mut builder: reqwest::blocking::RequestBuilder,
        provider: &codingbuddy_core::ProviderConfig,
    ) -> reqwest::blocking::RequestBuilder {
        for (name, value) in &provider.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            }
        }
        builder
    }

    pub(crate) fn resolve_request_api_key(&self) -> Result<Option<String>> {
        let provider = self.provider_config();
        let key = self.resolve_api_key();
        if key.is_some() {
            return Ok(key);
        }
        if provider.api_key_env.trim().is_empty() {
            return Ok(None);
        }
        Err(anyhow!(
            "{} not set and llm.api_key is empty",
            provider.api_key_env
        ))
    }

    pub(crate) fn resolved_endpoint(
        &self,
        is_chat: bool,
        is_fim: bool,
        is_strict_tools: bool,
    ) -> String {
        if !self.cfg.endpoint.is_empty() {
            return self.cfg.endpoint.clone();
        }
        let provider = self.provider_config();
        let base = provider.base_url.trim_end_matches('/');
        let prefix = if provider.openai_compat_prefix {
            "/v1"
        } else {
            ""
        };
        let path = match self.provider_kind().unwrap_or(ProviderKind::Deepseek) {
            ProviderKind::Deepseek => {
                if is_fim || is_strict_tools {
                    format!("{prefix}/beta/completions")
                } else if is_chat {
                    format!("{prefix}/chat/completions")
                } else {
                    format!("{prefix}/completions")
                }
            }
            ProviderKind::Anthropic => {
                return format!("{base}/v1/messages");
            }
            ProviderKind::Google => {
                // Google uses model-specific URLs; handled separately in build/send
                // Return a placeholder that gets replaced in complete_chat_inner
                if is_chat {
                    return format!("{base}/v1beta/openai/chat/completions");
                }
                return format!("{base}/v1beta/openai/completions");
            }
            ProviderKind::OpenAiCompatible
            | ProviderKind::Groq
            | ProviderKind::OpenRouter
            | ProviderKind::Ollama
            | ProviderKind::Azure
            | ProviderKind::Bedrock
            | ProviderKind::Vertex
            | ProviderKind::MistralApi
            | ProviderKind::Xai
            | ProviderKind::Together
            | ProviderKind::Copilot => {
                if is_chat {
                    format!("{prefix}/chat/completions")
                } else {
                    format!("{prefix}/completions")
                }
            }
        };
        format!("{base}{path}")
    }

    fn complete_inner(&self, req: &LlmRequest, api_key: Option<&str>) -> Result<LlmResponse> {
        let prepared = payload::build_simple_payload(req, &self.cfg);

        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            let response = self
                .apply_auth(
                    self.client
                        .post(self.resolved_endpoint(false, false, false)),
                    api_key,
                )
                .json(&prepared.payload)
                .send();

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());
                    let body = resp.text()?;
                    if status.is_success() {
                        let mut parsed = if self.cfg.stream {
                            parse_streaming_payload(&body)?
                        } else {
                            parse_non_streaming_payload(&body)?
                        };
                        if !prepared.compatibility.transforms.is_empty()
                            || !prepared.compatibility.degraded_inputs.is_empty()
                        {
                            parsed.compatibility = Some(prepared.compatibility.clone());
                        }
                        return Ok(parsed);
                    }

                    last_err = Some(with_compatibility_context(
                        format_api_error(status, &body, attempt, self.cfg.max_retries),
                        &prepared.compatibility,
                    ));
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        thread::sleep(retry_delay_ms(self.cfg.retry_base_ms, attempt, retry_after));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(with_compatibility_context(
                        format_transport_error(&e),
                        &prepared.compatibility,
                    ));
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        thread::sleep(retry_delay_ms(NETWORK_RETRY_BASE_MS, attempt, None));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("deepseek request failed without detailed error")))
    }

    fn complete_chat_inner(&self, req: &ChatRequest, api_key: Option<&str>) -> Result<LlmResponse> {
        let prepared = payload::build_chat_payload(req, &self.cfg)?;
        let capabilities = self.cfg.capabilities_for_model(&req.model).ok_or_else(|| {
            anyhow!(
                "unsupported llm.provider='{}' (supported: deepseek, openai-compatible, anthropic, google, groq, openrouter, ollama)",
                self.cfg.provider
            )
        })?;
        let provider = self.provider_config();
        let chat_protocol = protocol::select_chat_protocol(&provider, capabilities.provider);

        // Build provider-specific endpoint
        let endpoint = match chat_protocol {
            protocol::ChatProtocol::GeminiGenerateContent => {
                let base = provider.base_url.trim_end_matches('/');
                let url = providers::google::endpoint(base, &req.model, false);
                if let Some(key) = api_key {
                    providers::google::append_api_key(&url, key)
                } else {
                    url
                }
            }
            _ => self.resolved_endpoint(true, false, false),
        };

        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            let builder = self
                .apply_auth_for_protocol(self.client.post(&endpoint), api_key, chat_protocol)
                .json(&prepared.payload);
            let response = builder.send();

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());
                    let body = resp.text()?;
                    if status.is_success() {
                        let response = match chat_protocol {
                            protocol::ChatProtocol::AnthropicMessages => {
                                providers::anthropic::parse_response(&body)?
                            }
                            protocol::ChatProtocol::GeminiGenerateContent => {
                                providers::google::parse_response(&body)?
                            }
                            _ => {
                                let r = parse_non_streaming_payload(&body)?;
                                provider_transform::postprocess_chat_response_with_compatibility(
                                    r,
                                    &capabilities,
                                    &req.tools,
                                    prepared.compatibility.clone(),
                                )
                            }
                        };
                        return Ok(response);
                    }
                    last_err = Some(with_compatibility_context(
                        format_api_error(status, &body, attempt, self.cfg.max_retries),
                        &prepared.compatibility,
                    ));
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        thread::sleep(retry_delay_ms(self.cfg.retry_base_ms, attempt, retry_after));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(with_compatibility_context(
                        format_transport_error(&e),
                        &prepared.compatibility,
                    ));
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        thread::sleep(retry_delay_ms(NETWORK_RETRY_BASE_MS, attempt, None));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("chat request failed")))
    }

    fn complete_fim_inner(&self, req: &FimRequest, api_key: Option<&str>) -> Result<LlmResponse> {
        let payload = payload::build_fim_payload(req, &self.cfg);
        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            let response = self
                .apply_auth(
                    self.client.post(self.resolved_endpoint(false, true, false)),
                    api_key,
                )
                .json(&payload)
                .send();

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());
                    let body = resp.text()?;
                    if status.is_success() {
                        return parse_fim_non_streaming_payload(&body);
                    }
                    last_err = Some(format_api_error(
                        status,
                        &body,
                        attempt,
                        self.cfg.max_retries,
                    ));
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        std::thread::sleep(retry_delay_ms(
                            self.cfg.retry_base_ms,
                            attempt,
                            retry_after,
                        ));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(format_transport_error(&e));
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        std::thread::sleep(retry_delay_ms(NETWORK_RETRY_BASE_MS, attempt, None));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("fim request failed")))
    }

    fn complete_fim_streaming_inner(
        &self,
        req: &FimRequest,
        api_key: Option<&str>,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let mut payload = payload::build_fim_payload(req, &self.cfg);
        payload["stream"] = json!(true);
        payload["stream_options"] = json!({"include_usage": true});

        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            let response = self
                .apply_auth(
                    self.client.post(self.resolved_endpoint(false, true, false)),
                    api_key,
                )
                .json(&payload)
                .send();

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());

                    if status.is_success() {
                        let mut content_out = String::new();
                        let mut finish_reason: Option<String> = None;
                        let mut usage: Option<codingbuddy_core::TokenUsage> = None;

                        let reader = std::io::BufReader::new(resp);
                        for line_result in std::io::BufRead::lines(reader) {
                            if let Some(ref ct) = self.cancel_token
                                && ct.is_cancelled()
                            {
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
                            if let Some(reason) =
                                choice.get("finish_reason").and_then(|v| v.as_str())
                            {
                                finish_reason = Some(reason.to_string());
                            }
                            if let Some(text) = choice.get("text").and_then(|v| v.as_str()) {
                                cb(StreamChunk::ContentDelta(text.to_string()));
                                content_out.push_str(text);
                            }
                        }
                        if last_err.is_none() {
                            return Ok(LlmResponse {
                                text: content_out,
                                finish_reason: finish_reason.unwrap_or_else(|| "stop".to_string()),
                                reasoning_content: String::new(),
                                tool_calls: vec![],
                                usage,
                                compatibility: None,
                            });
                        }
                    } else {
                        let body = resp.text().unwrap_or_default();
                        last_err = Some(format_api_error(
                            status,
                            &body,
                            attempt,
                            self.cfg.max_retries,
                        ));
                    }
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        std::thread::sleep(retry_delay_ms(
                            self.cfg.retry_base_ms,
                            attempt,
                            retry_after,
                        ));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(format_transport_error(&e));
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        std::thread::sleep(retry_delay_ms(NETWORK_RETRY_BASE_MS, attempt, None));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("fim stream request failed")))
    }

    fn complete_chat_streaming_inner(
        &self,
        req: &ChatRequest,
        api_key: Option<&str>,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let mut prepared = payload::build_chat_payload(req, &self.cfg)?;
        let capabilities = self.cfg.capabilities_for_model(&req.model).ok_or_else(|| {
            anyhow!(
                "unsupported llm.provider='{}' (supported: deepseek, openai-compatible, anthropic, google, groq, openrouter, ollama)",
                self.cfg.provider
            )
        })?;
        let provider = self.provider_config();
        let chat_protocol = protocol::select_chat_protocol(&provider, capabilities.provider);
        // Native providers handle streaming differently
        let is_native = chat_protocol.is_native();
        if !is_native {
            prepared.payload["stream"] = json!(true);
            prepared.payload["stream_options"] = json!({"include_usage": true});
        } else if chat_protocol == protocol::ChatProtocol::AnthropicMessages {
            prepared.payload["stream"] = json!(true);
        }
        // Google uses ?alt=sse in the URL for streaming, no body flag needed

        // Build provider-specific endpoint for streaming
        let stream_endpoint = match chat_protocol {
            protocol::ChatProtocol::GeminiGenerateContent => {
                let base = provider.base_url.trim_end_matches('/');
                let url = providers::google::endpoint(base, &req.model, true);
                if let Some(key) = api_key {
                    providers::google::append_api_key(&url, key)
                } else {
                    url
                }
            }
            _ => self.resolved_endpoint(true, false, false),
        };

        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            let builder = self
                .apply_auth_for_protocol(self.client.post(&stream_endpoint), api_key, chat_protocol)
                .json(&prepared.payload);
            let response = builder.send();

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());

                    if status.is_success() {
                        let streaming_result = streaming::execute_chat_stream(
                            std::io::BufReader::new(resp),
                            chat_protocol,
                            self.cancel_token.as_ref(),
                            &cb,
                        )?;

                        let content_out = streaming_result.content;
                        let reasoning_out = streaming_result.reasoning;
                        let tool_calls = streaming_result.tool_calls;
                        let finish_reason = streaming_result.finish_reason;
                        let usage = streaming_result.usage;

                        let text = if !content_out.is_empty() {
                            content_out
                        } else if !reasoning_out.is_empty() {
                            reasoning_out.clone()
                        } else {
                            String::new()
                        };

                        let response = LlmResponse {
                            text,
                            finish_reason: finish_reason.unwrap_or_else(|| "stop".to_string()),
                            reasoning_content: reasoning_out,
                            tool_calls,
                            usage,
                            compatibility: Some(prepared.compatibility.clone()),
                        };
                        // Native providers handle their own format; OpenAI-compat needs postprocessing
                        if is_native {
                            return Ok(response);
                        }
                        return Ok(
                            provider_transform::postprocess_chat_response_with_compatibility(
                                response,
                                &capabilities,
                                &req.tools,
                                prepared.compatibility.clone(),
                            ),
                        );
                    }

                    let body = resp.text().unwrap_or_default();
                    last_err = Some(with_compatibility_context(
                        format_api_error(status, &body, attempt, self.cfg.max_retries),
                        &prepared.compatibility,
                    ));
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        thread::sleep(retry_delay_ms(self.cfg.retry_base_ms, attempt, retry_after));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(with_compatibility_context(
                        format_transport_error(&e),
                        &prepared.compatibility,
                    ));
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        thread::sleep(retry_delay_ms(NETWORK_RETRY_BASE_MS, attempt, None));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("chat streaming request failed")))
    }

    pub(crate) fn resolve_api_key(&self) -> Option<String> {
        // Prefer the cached key (resolved at construction time) to avoid
        // env var data races when logout clears the variable.
        if let Some(ref cached) = self.api_key {
            return Some(cached.clone());
        }
        // Fallback: re-check env + config (e.g. key set after construction).
        resolve_key_from_config(&self.cfg)
    }

    fn resolve_request_model(&self, requested: &str) -> Result<String> {
        let trimmed = requested.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("llm model must not be empty"));
        }
        Ok(trimmed.to_string())
    }

    /// Validate parameters that are incompatible with provider-managed thinking modes.
    fn validate_thinking_params(&self, req: &ChatRequest) -> Result<()> {
        let thinking_enabled = req
            .thinking
            .as_ref()
            .is_some_and(|t| t.thinking_type == "enabled")
            && self
                .cfg
                .capabilities_for_model(&req.model)
                .is_some_and(|caps| caps.thinking_capability.accepts_thinking_config());
        if !thinking_enabled {
            return Ok(());
        }
        if req.logprobs == Some(true) {
            return Err(anyhow!(
                "logprobs is incompatible with thinking mode; remove logprobs or disable thinking"
            ));
        }
        if req.top_logprobs.is_some() {
            return Err(anyhow!(
                "top_logprobs is incompatible with thinking mode; remove top_logprobs or disable thinking"
            ));
        }
        Ok(())
    }

    /// Validate chat request parameters against the resolved capability profile
    /// before any provider request is dispatched.
    pub(crate) fn validate_chat_request_contract(&self, req: &ChatRequest) -> Result<()> {
        let resolution = self
            .cfg
            .capability_resolution_for_model(&req.model)
            .ok_or_else(|| {
                anyhow!(
                    "unsupported llm.provider='{}' (supported: deepseek, openai-compatible, anthropic, google, groq, openrouter, ollama)",
                    self.cfg.provider
                )
            })?;
        let caps = resolution.capabilities;
        let applied_rules = if resolution.applied_rules.is_empty() {
            "none".to_string()
        } else {
            resolution.applied_rules.join(", ")
        };

        let has_assistant_tool_calls = req.messages.iter().any(|message| {
            matches!(
                message,
                ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty()
            )
        });
        let has_tool_messages = req
            .messages
            .iter()
            .any(|message| matches!(message, ChatMessage::Tool { .. }));
        let uses_tool_protocol =
            !req.tools.is_empty() || has_assistant_tool_calls || has_tool_messages;

        if uses_tool_protocol && !caps.supports_tool_calling {
            return Err(anyhow!(
                "model '{}' does not support tool-calling protocol (capability rules: {}); disable tools for this model",
                req.model,
                applied_rules
            ));
        }

        if req.tools.len() > caps.max_safe_tool_count {
            return Err(anyhow!(
                "request defines {} tools but model '{}' max_safe_tool_count is {} (capability rules: {}); reduce active tools before dispatch",
                req.tools.len(),
                req.model,
                caps.max_safe_tool_count,
                applied_rules
            ));
        }

        if req.tool_choice != ToolChoice::none() && !caps.supports_tool_choice {
            return Err(anyhow!(
                "model '{}' does not support tool_choice (capability rules: {}); set tool_choice to \"none\" or switch models",
                req.model,
                applied_rules
            ));
        }

        let thinking_requested = req
            .thinking
            .as_ref()
            .is_some_and(|t| t.thinking_type == "enabled");
        if thinking_requested && !caps.thinking_capability.accepts_thinking_config() {
            match caps.thinking_capability {
                codingbuddy_core::ThinkingCapability::ImplicitReasoning => {
                    // OpenAI o1/o3 style: reasoning is implicit, coarse hint via reasoning_effort
                    return Ok(());
                }
                codingbuddy_core::ThinkingCapability::NativeReasoning => {
                    return Err(anyhow!(
                        "model '{}' uses native reasoning mode and rejects explicit thinking config (capability rules: {}); remove thinking config for this model",
                        req.model,
                        applied_rules
                    ));
                }
                codingbuddy_core::ThinkingCapability::None => {
                    // OpenAI-compatible proxies may accept a coarse reasoning hint
                    // via `reasoning_effort`; handled by provider payload compatibility.
                    if caps.provider == ProviderKind::OpenAiCompatible {
                        return Ok(());
                    }
                    return Err(anyhow!(
                        "model '{}' does not support thinking config (capability rules: {}); disable thinking or switch models",
                        req.model,
                        applied_rules
                    ));
                }
                codingbuddy_core::ThinkingCapability::ExtendedThinking => {
                    // Should not reach here — accepts_thinking_config() is true
                }
            }
        }

        if !thinking_requested && req.top_logprobs.is_some() && req.logprobs != Some(true) {
            return Err(anyhow!(
                "top_logprobs requires logprobs=true; set logprobs or remove top_logprobs"
            ));
        }

        Ok(())
    }

    /// Streaming variant: reads the SSE response line-by-line, invoking `cb`
    /// for each content/reasoning delta, then returns the assembled response.
    fn complete_streaming_inner(
        &self,
        req: &LlmRequest,
        api_key: Option<&str>,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let mut prepared = payload::build_simple_payload(req, &self.cfg);
        // Force streaming on for the HTTP request
        prepared.payload["stream"] = json!(true);
        prepared.payload["stream_options"] = json!({"include_usage": true});

        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            let response = self
                .apply_auth(
                    self.client
                        .post(self.resolved_endpoint(false, false, false)),
                    api_key,
                )
                .json(&prepared.payload)
                .send();

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());

                    if status.is_success() {
                        // Read SSE line-by-line, invoking callback for each delta
                        let mut content_out = String::new();
                        let mut reasoning_out = String::new();
                        let mut finish_reason: Option<String> = None;
                        let mut tool_call_parts: BTreeMap<u64, StreamToolCall> = BTreeMap::new();
                        let mut completed_tool_calls = Vec::new();
                        let mut usage: Option<codingbuddy_core::TokenUsage> = None;

                        let reader = std::io::BufReader::new(resp);
                        for line_result in reader.lines() {
                            if let Some(ref ct) = self.cancel_token
                                && ct.is_cancelled()
                            {
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
                            if let Some(reason) =
                                choice.get("finish_reason").and_then(|v| v.as_str())
                            {
                                finish_reason = Some(reason.to_string());
                            }
                            if let Some(delta) = choice.get("delta") {
                                if let Some(content) = delta.get("content").and_then(|v| v.as_str())
                                {
                                    content_out.push_str(content);
                                    cb(StreamChunk::ContentDelta(content.to_string()));
                                }
                                if let Some(reasoning) =
                                    delta.get("reasoning_content").and_then(|v| v.as_str())
                                {
                                    reasoning_out.push_str(reasoning);
                                    cb(StreamChunk::ReasoningDelta(reasoning.to_string()));
                                }
                                if let Some(tool_calls) =
                                    delta.get("tool_calls").and_then(|v| v.as_array())
                                {
                                    merge_stream_tool_calls(tool_calls, &mut tool_call_parts);
                                }
                            }
                            if let Some(message) = choice.get("message") {
                                if let Some(content) =
                                    message.get("content").and_then(|v| v.as_str())
                                {
                                    content_out.push_str(content);
                                    cb(StreamChunk::ContentDelta(content.to_string()));
                                }
                                if let Some(reasoning) =
                                    message.get("reasoning_content").and_then(|v| v.as_str())
                                {
                                    reasoning_out.push_str(reasoning);
                                    cb(StreamChunk::ReasoningDelta(reasoning.to_string()));
                                }
                                if let Some(tool_calls) = message.get("tool_calls") {
                                    completed_tool_calls.extend(parse_tool_calls_array(tool_calls));
                                }
                            }
                        }

                        // If stream read failed, propagate the error
                        if let Some(err) = last_err.take() {
                            return Err(err);
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

                        let text = if !content_out.is_empty() {
                            content_out
                        } else {
                            reasoning_out.clone()
                        };
                        let mut response = LlmResponse {
                            text,
                            finish_reason: finish_reason.unwrap_or_else(|| "stop".to_string()),
                            reasoning_content: reasoning_out,
                            tool_calls,
                            usage,
                            compatibility: None,
                        };
                        if !prepared.compatibility.transforms.is_empty()
                            || !prepared.compatibility.degraded_inputs.is_empty()
                        {
                            response.compatibility = Some(prepared.compatibility.clone());
                        }
                        return Ok(response);
                    }

                    let body = resp.text().unwrap_or_default();
                    last_err = Some(with_compatibility_context(
                        format_api_error(status, &body, attempt, self.cfg.max_retries),
                        &prepared.compatibility,
                    ));
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        let delay = retry_delay_ms(self.cfg.retry_base_ms, attempt, retry_after);
                        if status == StatusCode::TOO_MANY_REQUESTS {
                            cb(StreamChunk::RateLimited {
                                wait_seconds: delay.as_secs().max(1),
                                attempt: attempt + 1,
                                max_attempts: self.cfg.max_retries,
                                provider: self.cfg.provider.clone(),
                            });
                        }
                        thread::sleep(delay);
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(with_compatibility_context(
                        format_transport_error(&e),
                        &prepared.compatibility,
                    ));
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        thread::sleep(retry_delay_ms(NETWORK_RETRY_BASE_MS, attempt, None));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("streaming request failed without detailed error")))
    }
}

// DeepSeek uses automatic server-side prefix caching — no client-side annotations needed.
// annotate_cache_control / apply_cache_annotations / payload_rejects_cache_control removed in P9.

impl LlmClient for ApiClient {
    fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let _provider = self.provider_kind()?;
        let key = self.resolve_request_api_key()?;
        let mut normalized_req = req.clone();
        normalized_req.model = self.resolve_request_model(&req.model)?;
        self.complete_inner(&normalized_req, key.as_deref())
    }

    fn complete_streaming(&self, req: &LlmRequest, cb: StreamCallback) -> Result<LlmResponse> {
        let _provider = self.provider_kind()?;
        let key = self.resolve_request_api_key()?;
        let mut normalized_req = req.clone();
        normalized_req.model = self.resolve_request_model(&req.model)?;
        self.complete_streaming_inner(&normalized_req, key.as_deref(), cb)
    }

    fn complete_chat(&self, req: &ChatRequest) -> Result<LlmResponse> {
        let _provider = self.provider_kind()?;
        let key = self.resolve_request_api_key()?;
        let mut normalized_req = req.clone();
        normalized_req.model = self.resolve_request_model(&req.model)?;
        self.validate_chat_request_contract(&normalized_req)?;
        self.validate_thinking_params(&normalized_req)?;
        self.complete_chat_inner(&normalized_req, key.as_deref())
    }

    fn complete_chat_streaming(
        &self,
        req: &ChatRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let _provider = self.provider_kind()?;
        let key = self.resolve_request_api_key()?;
        let mut normalized_req = req.clone();
        normalized_req.model = self.resolve_request_model(&req.model)?;
        self.validate_chat_request_contract(&normalized_req)?;
        self.validate_thinking_params(&normalized_req)?;
        self.complete_chat_streaming_inner(&normalized_req, key.as_deref(), cb)
    }

    fn complete_fim(&self, req: &FimRequest) -> Result<LlmResponse> {
        let _provider = self.provider_kind()?;
        let resolution = self
            .cfg
            .capability_resolution_for_model(&req.model)
            .ok_or_else(|| {
                anyhow!(
                    "unsupported llm.provider='{}' (supported: deepseek, openai-compatible, anthropic, google, groq, openrouter, ollama)",
                    self.cfg.provider
                )
            })?;
        if !resolution.capabilities.supports_fim {
            let applied_rules = if resolution.applied_rules.is_empty() {
                "none".to_string()
            } else {
                resolution.applied_rules.join(", ")
            };
            return Err(anyhow!(
                "model '{}' does not support fill-in-the-middle requests (capability rules: {}); switch to a model/profile with supports_fim=true",
                req.model,
                applied_rules
            ));
        }
        let key = self.resolve_request_api_key()?;
        let mut normalized_req = req.clone();
        normalized_req.model = self.resolve_request_model(&req.model)?;
        self.complete_fim_inner(&normalized_req, key.as_deref())
    }

    fn complete_fim_streaming(&self, req: &FimRequest, cb: StreamCallback) -> Result<LlmResponse> {
        let _provider = self.provider_kind()?;
        let resolution = self
            .cfg
            .capability_resolution_for_model(&req.model)
            .ok_or_else(|| {
                anyhow!(
                    "unsupported llm.provider='{}' (supported: deepseek, openai-compatible, anthropic, google, groq, openrouter, ollama)",
                    self.cfg.provider
                )
            })?;
        if !resolution.capabilities.supports_fim {
            let applied_rules = if resolution.applied_rules.is_empty() {
                "none".to_string()
            } else {
                resolution.applied_rules.join(", ")
            };
            return Err(anyhow!(
                "model '{}' does not support fill-in-the-middle requests (capability rules: {}); switch to a model/profile with supports_fim=true",
                req.model,
                applied_rules
            ));
        }
        let key = self.resolve_request_api_key()?;
        let mut normalized_req = req.clone();
        normalized_req.model = self.resolve_request_model(&req.model)?;
        self.complete_fim_streaming_inner(&normalized_req, key.as_deref(), cb)
    }

    fn list_models(&self) -> Vec<String> {
        self.discover_models_from_api()
    }
}

/// Produce a user-friendly error from an LLM API HTTP response.
pub(crate) fn format_api_error(
    status: StatusCode,
    body: &str,
    attempt: u8,
    max_retries: u8,
) -> anyhow::Error {
    // Parse structured error from JSON body: {"error": {"type": "...", "message": "..."}}
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error_obj = parsed.as_ref().and_then(|v| v.get("error"));
    let error_type = error_obj
        .and_then(|e| e.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("unknown");
    let detail = error_obj
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| body.chars().take(200).collect());

    match status {
        StatusCode::BAD_REQUEST => anyhow!(
            "Invalid request (HTTP 400, type={}): {}\n\
             Check that your request parameters are valid for the selected model.",
            error_type,
            detail
        ),
        StatusCode::UNAUTHORIZED => anyhow!(
            "Invalid or missing API key (HTTP 401).\n\
             Set the active provider API key environment variable or configure llm.api_key in settings."
        ),
        StatusCode::PAYMENT_REQUIRED => anyhow!(
            "Insufficient balance or billing issue (HTTP 402). Check the active provider account."
        ),
        StatusCode::UNPROCESSABLE_ENTITY => {
            let param = error_obj
                .and_then(|e| e.get("param"))
                .and_then(|p| p.as_str())
                .unwrap_or("unknown");
            let code = error_obj
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .unwrap_or("unknown");
            anyhow!(
                "Invalid parameters (HTTP 422, type={}, code={}, param={}): {}\n\
                 Common causes: logprobs/top_logprobs with thinking mode, unsupported parameter \
                 combinations, or malformed tool definitions. Review your request configuration.",
                error_type,
                code,
                param,
                detail
            )
        }
        StatusCode::TOO_MANY_REQUESTS => anyhow!(
            "Rate limited (HTTP 429). Exhausted {}/{} retries. Try again shortly or reduce request frequency. Detail: {}",
            attempt + 1,
            max_retries + 1,
            detail
        ),
        StatusCode::INTERNAL_SERVER_ERROR | StatusCode::SERVICE_UNAVAILABLE => anyhow!(
            "LLM provider server error (HTTP {}). Exhausted {}/{} retries. The service may be temporarily unavailable. Detail: {}",
            status.as_u16(),
            attempt + 1,
            max_retries + 1,
            detail
        ),
        _ => anyhow!(
            "LLM API error (HTTP {}, type={}): {}",
            status.as_u16(),
            error_type,
            detail
        ),
    }
}

/// Produce a user-friendly error from a transport/network failure.
pub(crate) fn format_transport_error(err: &reqwest::Error) -> anyhow::Error {
    let inner_msg = err
        .source()
        .map(|e| e.to_string())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_dns = inner_msg.contains("dns")
        || inner_msg.contains("resolve")
        || inner_msg.contains("name or service not known")
        || inner_msg.contains("no such host")
        || inner_msg.contains("getaddrinfo");

    if err.is_timeout() {
        anyhow!(
            "Request timed out. The configured LLM endpoint did not respond in time.\n\
             Retrying with exponential backoff. If this persists, try increasing \
             llm.timeout_seconds in your config."
        )
    } else if is_dns {
        anyhow!(
            "DNS resolution failed. Could not resolve the configured LLM hostname.\n\
             Check your internet connection and DNS settings. \
             Retrying with exponential backoff."
        )
    } else if err.is_connect() {
        anyhow!(
            "Connection refused. Could not reach the configured LLM endpoint.\n\
             Check your network connection and firewall settings. \
             Retrying with exponential backoff."
        )
    } else {
        anyhow!("Network error: {err}. Retrying with exponential backoff if retries remain.")
    }
}

pub(crate) fn compatibility_error_context(compatibility: &AppliedCompatibility) -> String {
    let mut parts = vec![format!(
        "provider={} family={}",
        compatibility.provider, compatibility.family
    )];
    if !compatibility.transforms.is_empty() {
        parts.push(format!("transforms={}", compatibility.transforms.join(",")));
    }
    if !compatibility.degraded_inputs.is_empty() {
        parts.push(format!(
            "degraded_inputs={}",
            compatibility.degraded_inputs.join(",")
        ));
    }
    parts.join(" ")
}

pub(crate) fn with_compatibility_context(
    err: anyhow::Error,
    compatibility: &AppliedCompatibility,
) -> anyhow::Error {
    anyhow!("{err}. {}", compatibility_error_context(compatibility))
}

pub(crate) fn should_retry_status(status: StatusCode) -> bool {
    retry::RetryCategory::from_status(status).should_retry()
}

pub(crate) fn should_retry_transport_error(err: &reqwest::Error) -> bool {
    retry::classify_transport_error(err).should_retry()
}

/// Parse `retry-after-ms` (milliseconds) header, returns value in seconds for
/// consistency with `parse_retry_after_seconds`. Sub-second precision is preserved
/// by rounding up to the nearest second.
pub(crate) fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get("retry-after-ms")?.to_str().ok()?.trim();
    let ms = value.parse::<f64>().ok()?;
    // Convert ms to seconds, rounding up (at least 1s for any positive value)
    Some(((ms / 1000.0).ceil() as u64).max(1))
}

/// Combined retry-after parser: tries `retry-after-ms` first, then `Retry-After`.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    parse_retry_after_ms(headers).or_else(|| parse_retry_after_seconds(headers.get(RETRY_AFTER)))
}

pub(crate) fn parse_retry_after_seconds(
    header: Option<&reqwest::header::HeaderValue>,
) -> Option<u64> {
    let value = header?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    parse_retry_after_http_date(value)
}

pub(crate) fn parse_retry_after_http_date(value: &str) -> Option<u64> {
    let retry_at = DateTime::parse_from_rfc2822(value)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT")
                .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        })
        .ok()?;
    let now = Utc::now();
    let delta = retry_at.signed_duration_since(now).num_seconds();
    Some(delta.max(0) as u64)
}

/// Compute retry delay with jitter.
/// Priority: `retry-after-ms` header > `Retry-After` header > exponential backoff.
/// Backoff is capped at 30s; header values are respected as-is.
pub(crate) fn retry_delay_ms(
    base_ms: u64,
    attempt: u8,
    retry_after_seconds: Option<u64>,
) -> Duration {
    if let Some(seconds) = retry_after_seconds {
        return Duration::from_millis(seconds.saturating_mul(1000));
    }
    // Exponential backoff with jitter: base * 2^attempt * (0.5..1.5)
    let exponent = u32::from(attempt);
    let base = base_ms as f64 * 2f64.powi(exponent as i32);
    // Simple deterministic jitter based on attempt number (avoids rand dependency)
    let jitter_factor = 0.5 + (((attempt as f64 * 1.618).fract()) * 1.0); // 0.5..1.5 range
    let delay_ms = (base * jitter_factor).min(30_000.0);
    Duration::from_millis(delay_ms.max(base_ms as f64) as u64)
}
