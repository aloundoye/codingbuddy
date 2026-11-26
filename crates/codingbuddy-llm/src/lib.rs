pub mod certification;
mod chat_transport;
mod cloud_identity;
pub mod error;
pub mod extension;
mod native_parts;
mod route;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, NaiveDateTime, Utc};
use codingbuddy_core::runtime::spawn_blocking;
use codingbuddy_core::{
    AppliedCompatibility, CancellationToken, ChatMessage, ChatRequest, FimRequest, LlmConfig,
    LlmRequest, LlmResponse, LlmToolCall, ProviderKind, StreamCallback, StreamChunk, ToolChoice,
    TransportPart, max_output_tokens_for_model,
};
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::io::BufRead;
use std::ops::Deref;

use std::time::Duration;

mod connection;
mod credentials;
mod http_support;
pub mod model_catalog;
mod parsing;
pub mod protocol;
mod provider_transform;
pub mod providers;
pub mod retry;
pub use connection::*;
pub use credentials::*;
use http_support::*;
use parsing::*;

#[derive(Debug, Clone)]
struct PreparedPayload {
    payload: Value,
    compatibility: AppliedCompatibility,
}

impl Deref for PreparedPayload {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

/// Base delay for network/transport error retries (1s, 2s, 4s exponential backoff).
const NETWORK_RETRY_BASE_MS: u64 = 1000;

/// A model provider. Request methods are async; implementations must not
/// block the calling task (run blocking work with
/// `codingbuddy_core::runtime::spawn_blocking`).
#[async_trait::async_trait]
pub trait LlmClient {
    /// Preserve the selected transport and cached account while attaching turn cancellation.
    fn with_cancellation(
        &self,
        _token: CancellationToken,
    ) -> Option<Box<dyn LlmClient + Send + Sync>> {
        None
    }
    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse>;

    /// Streaming variant that invokes `cb` for each token chunk as it arrives.
    /// Returns the fully assembled `LlmResponse` once the stream ends.
    async fn complete_streaming(&self, req: &LlmRequest, cb: StreamCallback)
    -> Result<LlmResponse>;

    /// Chat completion with tool definitions (function calling).
    /// Sends a multi-turn conversation with tool schemas and returns the response.
    async fn complete_chat(&self, req: &ChatRequest) -> Result<LlmResponse>;

    /// Streaming chat completion with tool definitions.
    async fn complete_chat_streaming(
        &self,
        req: &ChatRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse>;

    /// Return the actual route of a client whose credential may already be cached.
    fn pinned_session_route(&self, _model: &str, _local_key: &[u8]) -> Option<Result<String>> {
        None
    }

    /// Discover available models from the provider's API.
    /// Default: returns empty (no discovery support).
    async fn list_models(&self) -> Vec<String> {
        Vec::new()
    }

    /// Beta FIM completion (Fill-In-The-Middle)
    async fn complete_fim(&self, req: &FimRequest) -> Result<LlmResponse>;

    /// Beta FIM completion streaming (Fill-In-The-Middle)
    async fn complete_fim_streaming(
        &self,
        req: &FimRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse>;
}

/// Blocking wrappers around [`LlmClient`] for synchronous callers outside a
/// turn: standalone commands and tests. Never call these from async code.
pub trait LlmClientBlockingExt: LlmClient + Sync {
    fn complete_blocking(&self, req: &LlmRequest) -> Result<LlmResponse> {
        codingbuddy_core::runtime::block_on(self.complete(req))
    }
    fn complete_streaming_blocking(
        &self,
        req: &LlmRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        codingbuddy_core::runtime::block_on(self.complete_streaming(req, cb))
    }
    fn complete_chat_blocking(&self, req: &ChatRequest) -> Result<LlmResponse> {
        codingbuddy_core::runtime::block_on(self.complete_chat(req))
    }
    fn complete_chat_streaming_blocking(
        &self,
        req: &ChatRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        codingbuddy_core::runtime::block_on(self.complete_chat_streaming(req, cb))
    }
    fn complete_fim_blocking(&self, req: &FimRequest) -> Result<LlmResponse> {
        codingbuddy_core::runtime::block_on(self.complete_fim(req))
    }
    fn complete_fim_streaming_blocking(
        &self,
        req: &FimRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        codingbuddy_core::runtime::block_on(self.complete_fim_streaming(req, cb))
    }
    fn list_models_blocking(&self) -> Vec<String> {
        codingbuddy_core::runtime::block_on(self.list_models())
    }
}

impl<T: LlmClient + Sync + ?Sized> LlmClientBlockingExt for T {}

#[derive(Clone)]
pub struct ApiClient {
    cfg: LlmConfig,
    /// Async HTTP client shared by every request from this ApiClient, so the
    /// connection pool is reused across calls.
    client: reqwest::Client,
    /// Optional cancellation token — checked between SSE reads during streaming.
    /// When cancelled, returns partial response with content accumulated so far.
    cancel_token: Option<CancellationToken>,
    /// Cached API key resolved eagerly at construction time.
    /// Avoids env var races when the key is cleared during logout.
    api_key: Option<String>,
    #[cfg(feature = "provider-fixtures")]
    fixture_origin: Option<reqwest::Url>,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiClient")
            .field("provider", &self.cfg.provider)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    pub fn new(cfg: LlmConfig) -> Result<Self> {
        let client = chat_transport::build_http_client(Duration::from_secs(cfg.timeout_seconds))?;
        if cfg.active_provider().cloud.is_some() {
            route::ChatRoute::resolve(&cfg, &cfg.active_base_model(), false)?;
        }
        let api_key = resolve_key_from_config(&cfg);
        Ok(Self {
            cfg,
            client,
            cancel_token: None,
            api_key,
            #[cfg(feature = "provider-fixtures")]
            fixture_origin: None,
        })
    }

    /// Run conformance against an in-process fixture with synthetic credentials.
    #[cfg(feature = "provider-fixtures")]
    pub fn with_fixture_origin(mut self, origin: &str) -> Result<Self> {
        let url = reqwest::Url::parse(origin)?;
        anyhow::ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some()
                && url.path() == "/"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Provider fixture must be an explicit IPv4 loopback origin"
        );
        self.fixture_origin = Some(url);
        Ok(self)
    }

    fn resolve_chat_route(&self, model: &str, streaming: bool) -> Result<route::ChatRoute> {
        let route = route::ChatRoute::resolve(&self.cfg, model, streaming)?;
        #[cfg(feature = "provider-fixtures")]
        if let Some(origin) = &self.fixture_origin {
            let mut route = route;
            let endpoint = reqwest::Url::parse(&route.endpoint)?;
            route.endpoint = format!(
                "{}{}{}",
                origin.origin().ascii_serialization(),
                endpoint.path(),
                endpoint
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default()
            );
            return Ok(route);
        }
        Ok(route)
    }

    fn chat_bearer(
        &self,
        route: &route::ChatRoute,
        transport: &chat_transport::ChatTransport,
    ) -> Result<String> {
        #[cfg(feature = "provider-fixtures")]
        if self.fixture_origin.is_some() {
            return Ok("synthetic-certification-key".into());
        }
        cloud_identity::bearer(
            route,
            transport,
            Duration::from_secs(self.cfg.timeout_seconds),
        )
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

    fn check_cancellation(&self) -> Result<()> {
        anyhow::ensure!(
            !self
                .cancel_token
                .as_ref()
                .is_some_and(|token| token.is_cancelled()),
            "Turn interrupted"
        );
        Ok(())
    }

    /// Attach a cancellation token that will be checked during streaming.
    pub fn set_cancel_token(&mut self, token: CancellationToken) {
        self.cancel_token = Some(token);
    }

    /// Discover available models from the provider's /v1/models endpoint.
    /// Returns a list of model IDs. Fails gracefully (empty vec) on error.
    fn discover_models_from_api(&self) -> Vec<String> {
        let provider = self.provider_config();
        if provider.cloud.is_some() || route::custom_headers(&provider).is_err() {
            return Vec::new();
        }
        if !provider.discovery.enabled {
            return Vec::new();
        }
        let base = provider.base_url.trim_end_matches('/');
        let prefix = if provider.openai_compat_prefix && !base.ends_with("/v1") {
            "/v1"
        } else {
            ""
        };
        let kind = self
            .cfg
            .active_provider_kind()
            .unwrap_or(ProviderKind::OpenAiCompatible);
        let protocol = protocol::select_chat_protocol(&provider, kind);
        let path = match (kind, protocol) {
            (ProviderKind::Ollama, _) => "/api/tags".to_owned(),
            (_, protocol::ChatProtocol::GeminiGenerateContent) => "/v1beta/models".to_owned(),
            (_, protocol::ChatProtocol::AnthropicMessages) => "/v1/models".to_owned(),
            _ => format!("{prefix}/models"),
        };
        let path = provider
            .discovery
            .models_endpoint
            .as_deref()
            .filter(|path| path.starts_with('/') && !path.starts_with("//"))
            .unwrap_or(&path);
        let base = if kind == ProviderKind::Ollama {
            base.strip_suffix("/v1").unwrap_or(base)
        } else {
            base
        };
        let url = format!("{base}{path}");
        let Ok(mut headers) = self.auth_headers_for_protocol(self.api_key.as_deref(), protocol)
        else {
            return Vec::new();
        };
        if protocol == protocol::ChatProtocol::GeminiGenerateContent
            && let Some(key) = &self.api_key
            && let Ok(value) = reqwest::header::HeaderValue::from_str(key)
        {
            headers.insert("x-goog-api-key", value);
        }
        let client = self.client.clone();
        let fetched = self
            .transport()
            .wait_with_timeout(Duration::from_secs(5), async move {
                client
                    .get(url)
                    .headers(headers)
                    .send()
                    .await?
                    .json::<serde_json::Value>()
                    .await
            });
        let body = match fetched {
            Ok(Ok(Ok(body))) => body,
            _ => return Vec::new(),
        };
        body.get("data")
            .or_else(|| body.get("models"))
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        m.get("id")
                            .or_else(|| m.get("name"))
                            .and_then(|v| v.as_str())
                            .map(|id| id.strip_prefix("models/").unwrap_or(id).to_owned())
                    })
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

    /// A transport sharing this client's connection pool and cancellation.
    fn transport(&self) -> chat_transport::ChatTransport {
        chat_transport::ChatTransport::with_client(self.client.clone(), self.cancel_token.clone())
    }

    fn retry_policy<'a>(
        &'a self,
        progress: Option<&'a StreamCallback>,
    ) -> chat_transport::RetryPolicy<'a> {
        chat_transport::RetryPolicy {
            provider: &self.cfg.provider,
            max_retries: self.cfg.max_retries,
            retry_base_ms: self.cfg.retry_base_ms,
            progress,
        }
    }

    fn auth_headers(&self, api_key: Option<&str>) -> Result<reqwest::header::HeaderMap> {
        self.auth_headers_for_protocol(api_key, protocol::ChatProtocol::OpenAiChat)
    }

    /// Authentication and provider headers for the legacy (non-route) paths.
    fn auth_headers_for_protocol(
        &self,
        api_key: Option<&str>,
        chat_protocol: protocol::ChatProtocol,
    ) -> Result<reqwest::header::HeaderMap> {
        use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
        let provider_config = self.provider_config();
        let provider = self.provider_kind().unwrap_or(ProviderKind::Deepseek);
        let auth_strategy =
            protocol::select_auth_strategy(&provider_config, provider, chat_protocol);
        let mut headers = HeaderMap::new();
        if provider == ProviderKind::Anthropic
            || chat_protocol == protocol::ChatProtocol::AnthropicMessages
        {
            headers.insert(
                "anthropic-version",
                HeaderValue::from_static(providers::anthropic::ANTHROPIC_VERSION),
            );
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("prompt-caching-2024-07-31,output-128k-2025-02-19"),
            );
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        let key = api_key.filter(|key| !key.trim().is_empty());
        let secret = |value: String| -> Result<HeaderValue> {
            let mut value = HeaderValue::from_str(&value)
                .map_err(|_| anyhow!("API key contains characters not allowed in a header"))?;
            value.set_sensitive(true);
            Ok(value)
        };
        match auth_strategy {
            protocol::AuthStrategy::Bearer => {
                if let Some(key) = key {
                    headers.insert(AUTHORIZATION, secret(format!("Bearer {key}"))?);
                }
            }
            protocol::AuthStrategy::XApiKey => {
                if let Some(key) = key {
                    headers.insert("x-api-key", secret(key.to_string())?);
                }
            }
            protocol::AuthStrategy::QueryApiKey
                if chat_protocol == protocol::ChatProtocol::GeminiGenerateContent
                    && provider_config.auth_strategy.is_none() =>
            {
                if let Some(key) = api_key {
                    headers.insert("x-goog-api-key", secret(key.to_string())?);
                }
            }
            protocol::AuthStrategy::QueryApiKey
            | protocol::AuthStrategy::AwsSigV4
            | protocol::AuthStrategy::AzureApiKey
            | protocol::AuthStrategy::AzureEntra
            | protocol::AuthStrategy::GoogleAdc
            | protocol::AuthStrategy::None => {}
        }
        if let Ok(custom) = route::custom_headers(&provider_config) {
            headers.extend(custom);
        }
        Ok(headers)
    }

    fn resolve_request_api_key(&self) -> Result<Option<String>> {
        let provider = self.provider_config();
        anyhow::ensure!(
            provider.cloud.is_none(),
            "This cloud route supports chat requests only"
        );
        route::custom_headers(&provider)?;
        if self
            .cfg
            .active_provider()
            .auth_strategy
            .as_deref()
            .and_then(protocol::parse_auth_strategy)
            == Some(protocol::AuthStrategy::None)
        {
            return Ok(None);
        }
        let key = self.resolve_api_key();
        if key.is_some() {
            return Ok(key);
        }
        if !provider_requires_key(&self.cfg) {
            return Ok(None);
        }
        Err(anyhow!(
            "{} not set and llm.api_key is empty",
            provider.api_key_env
        ))
    }

    fn resolved_endpoint(&self, is_chat: bool, is_fim: bool, is_strict_tools: bool) -> String {
        resolved_endpoint(&self.cfg, is_chat, is_fim, is_strict_tools)
    }

    fn complete_inner(&self, req: &LlmRequest, api_key: Option<&str>) -> Result<LlmResponse> {
        let prepared = self.build_payload(req);
        let transport = self.transport();
        let endpoint = self.resolved_endpoint(false, false, false);
        let headers = self.auth_headers(api_key)?;
        let max_retries = self.cfg.max_retries;
        let response = transport.send_with_retry(
            &self.retry_policy(None),
            || transport.post(&endpoint, headers.clone(), &prepared.payload),
            |status, body, attempt| {
                with_compatibility_context(
                    format_api_error(status, &redact_key(body, api_key), attempt, max_retries),
                    &prepared.compatibility,
                )
            },
            |error| {
                with_compatibility_context(format_transport_error(error), &prepared.compatibility)
            },
        )?;
        let body = transport.text(response)?;
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
        Ok(parsed)
    }

    fn complete_native_adapter(
        &self,
        req: &ChatRequest,
        route: &route::ChatRoute,
        adapter: &dyn extension::ProviderAdapterV1,
        callback: Option<StreamCallback>,
    ) -> Result<LlmResponse> {
        let prepared = self.build_chat_payload_for_route(req, route)?;
        let bytes = serde_json::to_vec(&prepared.payload)?;
        let timeout = Duration::from_secs(self.cfg.timeout_seconds);
        let transport = self.transport();
        let headers = adapter.authenticate(
            &extension::ProviderRequest {
                route,
                transport: &transport,
                timeout,
                #[cfg(feature = "provider-fixtures")]
                fixture: self.fixture_origin.is_some(),
            },
            &bytes,
        )?;
        let response = transport.post_bytes(&route.endpoint, headers, bytes)?;
        let status = response.status();
        if !status.is_success() {
            let body = transport.text(response)?;
            return Err(error::ProviderError::http(route.protocol, status.as_u16(), &body).into());
        }
        let mut result = if let Some(callback) = callback {
            adapter.decode_stream(&mut transport.reader(response), callback)?
        } else {
            adapter.decode_response(&serde_json::from_str(&transport.text(response)?)?)?
        };
        result.compatibility = Some(prepared.compatibility);
        Ok(result)
    }

    fn complete_chat_inner(
        &self,
        req: &ChatRequest,
        api_key: Option<&str>,
        route: &route::ChatRoute,
    ) -> Result<LlmResponse> {
        if let Some(adapter) = extension::registered_adapter(route.protocol) {
            return self.complete_native_adapter(req, route, adapter, None);
        }
        let mut prepared = self.build_chat_payload_for_route(req, route)?;
        let capabilities = &route.capabilities;
        let chat_protocol = route.protocol;
        let endpoint = &route.endpoint;
        let transport = self.transport();
        if let Some(deployment) = &route.deployment {
            prepared.payload["model"] = json!(deployment);
        }
        let cloud_token = match route.auth {
            protocol::AuthStrategy::AzureEntra | protocol::AuthStrategy::GoogleAdc => {
                Some(self.chat_bearer(route, &transport)?)
            }
            _ => None,
        };
        let headers = route.headers(cloud_token.as_deref().or(api_key))?;
        let response = transport.send_with_retry(
            &self.retry_policy(None),
            || transport.post(endpoint, headers.clone(), &prepared.payload),
            |status, body, _| {
                error::ProviderError::http(chat_protocol, status.as_u16(), body).into()
            },
            |_| error::ProviderError::new(chat_protocol, error::FailureKind::Transport).into(),
        )?;
        let status = response.status();
        let body = transport.text(response)?;
        let envelope: Value = serde_json::from_str(&body)?;
        if !envelope["error"].is_null() {
            return Err(error::ProviderError::http(chat_protocol, status.as_u16(), &body).into());
        }
        let response = match chat_protocol {
            protocol::ChatProtocol::OpenAiResponses => {
                providers::responses::parse_response(&serde_json::from_str(&body)?)?
            }
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
                    capabilities,
                    &req.tools,
                    prepared.compatibility.clone(),
                )
            }
        };
        Ok(response)
    }

    fn complete_fim_inner(&self, req: &FimRequest, api_key: Option<&str>) -> Result<LlmResponse> {
        let payload = self.build_fim_payload(req);
        let transport = self.transport();
        let endpoint = self.resolved_endpoint(false, true, false);
        let headers = self.auth_headers(api_key)?;
        let max_retries = self.cfg.max_retries;
        let response = transport.send_with_retry(
            &self.retry_policy(None),
            || transport.post(&endpoint, headers.clone(), &payload),
            |status, body, attempt| format_api_error(status, body, attempt, max_retries),
            format_transport_error,
        )?;
        parse_fim_non_streaming_payload(&transport.text(response)?)
    }

    fn resolve_api_key(&self) -> Option<String> {
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
    fn validate_thinking_params(&self, req: &ChatRequest, route: &route::ChatRoute) -> Result<()> {
        let thinking_enabled = req
            .thinking
            .as_ref()
            .is_some_and(|t| t.thinking_type == "enabled")
            && route
                .capabilities
                .thinking_capability
                .accepts_thinking_config();
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
    #[cfg(test)]
    fn validate_chat_request_contract(&self, req: &ChatRequest) -> Result<()> {
        self.validate_chat_request_for_route(
            req,
            &route::ChatRoute::resolve(&self.cfg, &req.model, false)?,
        )
    }

    fn validate_chat_request_for_route(
        &self,
        req: &ChatRequest,
        route: &route::ChatRoute,
    ) -> Result<()> {
        let caps = &route.capabilities;
        let applied_rules = if route.capability_rules.is_empty() {
            "none".to_string()
        } else {
            route.capability_rules.join(", ")
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
}

// DeepSeek uses automatic server-side prefix caching — no client-side annotations needed.
// annotate_cache_control / apply_cache_annotations / payload_rejects_cache_control removed in P9.

/// One log line per model request: route, size of the outcome, and timing.
/// Never logs prompt or response text, only lengths and counts.
fn log_llm_outcome(
    kind: &'static str,
    provider: &str,
    model: &str,
    started: std::time::Instant,
    result: &Result<LlmResponse>,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(response) => {
            let usage = response.usage.clone().unwrap_or_default();
            tracing::info!(
                target: "codingbuddy_llm::request",
                kind,
                provider,
                model,
                elapsed_ms,
                finish_reason = %response.finish_reason,
                text_chars = response.text.len(),
                tool_calls = response.tool_calls.len(),
                prompt_tokens = usage.prompt_tokens,
                completion_tokens = usage.completion_tokens,
                cache_hit_tokens = usage.prompt_cache_hit_tokens,
                "model request completed"
            );
        }
        Err(error) => {
            tracing::warn!(
                target: "codingbuddy_llm::request",
                kind,
                provider,
                model,
                elapsed_ms,
                error = %format!("{error:#}"),
                "model request failed"
            );
        }
    }
}

#[async_trait::async_trait]
impl LlmClient for ApiClient {
    fn with_cancellation(
        &self,
        token: CancellationToken,
    ) -> Option<Box<dyn LlmClient + Send + Sync>> {
        let mut client = self.clone();
        client.set_cancel_token(token);
        Some(Box::new(client))
    }
    fn pinned_session_route(&self, model: &str, local_key: &[u8]) -> Option<Result<String>> {
        Some(session_route_with_credential(
            &self.cfg,
            model,
            local_key,
            || self.resolve_api_key(),
        ))
    }

    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let (client, req) = (self.clone(), req.clone());
        spawn_blocking(move || client.complete_sync(&req)).await
    }

    async fn complete_streaming(
        &self,
        req: &LlmRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let (client, req) = (self.clone(), req.clone());
        spawn_blocking(move || client.complete_streaming_sync(&req, cb)).await
    }

    async fn complete_chat(&self, req: &ChatRequest) -> Result<LlmResponse> {
        let (client, req) = (self.clone(), req.clone());
        spawn_blocking(move || client.complete_chat_sync(&req)).await
    }

    async fn complete_chat_streaming(
        &self,
        req: &ChatRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let (client, req) = (self.clone(), req.clone());
        spawn_blocking(move || client.complete_chat_streaming_sync(&req, cb)).await
    }

    async fn complete_fim(&self, req: &FimRequest) -> Result<LlmResponse> {
        let (client, req) = (self.clone(), req.clone());
        spawn_blocking(move || client.complete_fim_sync(&req)).await
    }

    async fn complete_fim_streaming(
        &self,
        req: &FimRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let (client, req) = (self.clone(), req.clone());
        spawn_blocking(move || client.complete_fim_streaming_sync(&req, cb)).await
    }

    async fn list_models(&self) -> Vec<String> {
        let client = self.clone();
        spawn_blocking(move || client.list_models_sync()).await
    }
}

/// The request bodies behind [`LlmClient`]. They block on the shared
/// runtime (`ChatTransport::wait`) and read response streams synchronously,
/// so the trait runs them on the blocking pool; the turn's cancellation
/// token still stops them mid-request.
impl ApiClient {
    fn complete_sync(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let _provider = self.provider_kind()?;
        let key = self.resolve_request_api_key()?;
        let mut normalized_req = req.clone();
        normalized_req.model = self.resolve_request_model(&req.model)?;
        self.complete_inner(&normalized_req, key.as_deref())
    }

    fn complete_streaming_sync(&self, req: &LlmRequest, cb: StreamCallback) -> Result<LlmResponse> {
        let _provider = self.provider_kind()?;
        let key = self.resolve_request_api_key()?;
        let mut normalized_req = req.clone();
        normalized_req.model = self.resolve_request_model(&req.model)?;
        self.complete_streaming_inner(&normalized_req, key.as_deref(), cb)
    }

    fn complete_chat_sync(&self, req: &ChatRequest) -> Result<LlmResponse> {
        let started = std::time::Instant::now();
        let protocol = protocol::select_chat_protocol(
            &self.cfg.active_provider(),
            self.cfg
                .active_provider_kind()
                .unwrap_or(ProviderKind::OpenAiCompatible),
        );
        let result = (|| {
            let _provider = self.provider_kind()?;
            let mut normalized_req = req.clone();
            normalized_req.model = self.resolve_request_model(&req.model)?;
            let route = self.resolve_chat_route(&normalized_req.model, false)?;
            let key = if route.auth == protocol::AuthStrategy::None {
                None
            } else {
                self.resolve_api_key()
            };
            self.validate_chat_request_for_route(&normalized_req, &route)?;
            self.validate_thinking_params(&normalized_req, &route)?;
            confirmed_chat_response(
                self.complete_chat_inner(&normalized_req, key.as_deref(), &route)?,
                protocol,
                &normalized_req.tools,
            )
        })()
        .map_err(|error| error::ProviderError::classify(protocol, error));
        log_llm_outcome("chat", &self.cfg.provider, &req.model, started, &result);
        result
    }

    fn complete_chat_streaming_sync(
        &self,
        req: &ChatRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let started = std::time::Instant::now();
        let protocol = protocol::select_chat_protocol(
            &self.cfg.active_provider(),
            self.cfg
                .active_provider_kind()
                .unwrap_or(ProviderKind::OpenAiCompatible),
        );
        let terminal_cb = cb.clone();
        let visible = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let presented = visible.clone();
        let cb: StreamCallback = std::sync::Arc::new(move |chunk| {
            if matches!(&chunk, StreamChunk::Done { .. }) {
                return;
            }
            if matches!(&chunk, StreamChunk::ContentDelta(text) if !text.is_empty()) {
                presented.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            cb(chunk);
        });
        let result = (|| {
            let _provider = self.provider_kind()?;
            let mut normalized_req = req.clone();
            normalized_req.model = self.resolve_request_model(&req.model)?;
            let route = self.resolve_chat_route(&normalized_req.model, self.cfg.stream)?;
            let key = if route.auth == protocol::AuthStrategy::None {
                None
            } else {
                self.resolve_api_key()
            };
            self.validate_chat_request_for_route(&normalized_req, &route)?;
            self.validate_thinking_params(&normalized_req, &route)?;
            if !self.cfg.stream {
                return confirmed_chat_response(
                    self.complete_chat_inner(&normalized_req, key.as_deref(), &route)?,
                    protocol,
                    &normalized_req.tools,
                );
            }
            confirmed_chat_response(
                self.complete_chat_streaming_inner(&normalized_req, key.as_deref(), cb, &route)?,
                protocol,
                &normalized_req.tools,
            )
        })()
        .map_err(|error| {
            let mut error = error::ProviderError::classify(protocol, error);
            if visible.load(std::sync::atomic::Ordering::SeqCst)
                && let Some(provider) = error.downcast_mut::<error::ProviderError>()
            {
                provider.retryable = false;
            }
            error
        });
        if let Ok(response) = &result {
            if !visible.load(std::sync::atomic::Ordering::SeqCst) && !response.text.is_empty() {
                terminal_cb(StreamChunk::ContentDelta(response.text.clone()));
            }
            terminal_cb(StreamChunk::Done {
                reason: Some(response.finish_reason.clone()),
            });
        }
        log_llm_outcome(
            "chat_stream",
            &self.cfg.provider,
            &req.model,
            started,
            &result,
        );
        result
    }

    fn complete_fim_sync(&self, req: &FimRequest) -> Result<LlmResponse> {
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

    fn complete_fim_streaming_sync(
        &self,
        req: &FimRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
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

    fn list_models_sync(&self) -> Vec<String> {
        self.discover_models_from_api()
    }
}

mod payload;
mod streaming;
#[cfg(test)]
mod tests;
