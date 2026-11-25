pub mod certification;
mod chat_transport;
mod cloud_identity;
pub mod error;
pub mod extension;
mod native_parts;
mod route;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, NaiveDateTime, Utc};
use codingbuddy_core::{
    AppliedCompatibility, CancellationToken, ChatMessage, ChatRequest, FimRequest, LlmConfig,
    LlmRequest, LlmResponse, LlmToolCall, ProviderKind, StreamCallback, StreamChunk, ToolChoice,
    TransportPart, max_output_tokens_for_model,
};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::RETRY_AFTER;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::io::BufRead;
use std::ops::Deref;
use std::thread;
use std::time::Duration;

pub mod model_catalog;
pub mod protocol;
mod provider_transform;
pub mod providers;
pub mod retry;

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

pub trait LlmClient {
    /// Preserve the selected transport and cached account while attaching turn cancellation.
    fn with_cancellation(
        &self,
        _token: CancellationToken,
    ) -> Option<Box<dyn LlmClient + Send + Sync>> {
        None
    }
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

    /// Return the actual route of a client whose credential may already be cached.
    fn pinned_session_route(&self, _model: &str, _local_key: &[u8]) -> Option<Result<String>> {
        None
    }

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

#[derive(Clone)]
pub struct ApiClient {
    cfg: LlmConfig,
    client: Client,
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

/// Resolve an explicit credential, an environment key, or a saved key.
pub fn resolve_key_from_config(cfg: &LlmConfig) -> Option<String> {
    let provider = cfg.active_provider();
    let kind = cfg.active_provider_kind()?;
    let protocol = protocol::select_chat_protocol(&provider, kind);
    if protocol::select_auth_strategy(&provider, kind, protocol).uses_cloud_identity() {
        return None;
    }
    let env_key = provider.api_key_env.trim();
    let env_value = if env_key.is_empty() {
        None
    } else {
        std::env::var(env_key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    cfg.api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .or(env_value)
        .or_else(|| {
            (!env_key.is_empty())
                .then(|| credential_entry(cfg).ok()?.get_password().ok())
                .flatten()
        })
}

fn credential_entry(cfg: &LlmConfig) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "codingbuddy-provider",
        &cfg.connection_id(),
    )?)
}

pub fn save_provider_key(cfg: &LlmConfig, key: &str) -> Result<()> {
    Ok(credential_entry(cfg)?.set_password(key)?)
}

fn named_credential_entry(cfg: &LlmConfig, name: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "codingbuddy-provider-named",
        &format!("{}:{name}", cfg.connection_id()),
    )?)
}

fn credential_names_entry(cfg: &LlmConfig) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "codingbuddy-provider-key-names",
        &cfg.connection_id(),
    )?)
}

pub fn provider_key_names(cfg: &LlmConfig) -> Result<Vec<String>> {
    match credential_names_entry(cfg)?.get_password() {
        Ok(names) => Ok(serde_json::from_str(&names)?),
        Err(keyring::Error::NoEntry) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn load_named_provider_key(cfg: &LlmConfig, name: &str) -> Result<String> {
    Ok(named_credential_entry(cfg, name)?.get_password()?)
}

pub fn save_named_provider_key(cfg: &LlmConfig, name: &str, key: &str) -> Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty()
            && name.chars().count() <= 80
            && !name.chars().any(char::is_control),
        "Use a key name between 1 and 80 characters without control characters"
    );
    // Multiple terminal sessions can save keys for the same provider.
    let settings =
        codingbuddy_core::AppConfig::user_settings_path().context("Cannot locate user settings")?;
    let directory = settings.parent().context("Invalid user settings path")?;
    std::fs::create_dir_all(directory)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("named-credentials.lock"))?;
    lock.lock()?;
    let mut names = provider_key_names(cfg)?;
    anyhow::ensure!(
        !names.iter().any(|saved| saved == name),
        "That key name already exists"
    );
    names.push(name.to_owned());
    names.sort_unstable();
    let names = serde_json::to_string(&names)?;
    let index = credential_names_entry(cfg)?;
    let entry = named_credential_entry(cfg, name)?;
    entry.set_password(key)?;
    if let Err(error) = index.set_password(&names) {
        let _ = entry.delete_credential();
        return Err(error.into());
    }
    Ok(())
}

pub fn remove_provider_key(cfg: &LlmConfig) -> Result<()> {
    match credential_entry(cfg)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn provider_requires_key(cfg: &LlmConfig) -> bool {
    let provider = cfg.active_provider();
    let Some(kind) = cfg.active_provider_kind() else {
        return false;
    };
    let protocol = protocol::select_chat_protocol(&provider, kind);
    let auth = protocol::select_auth_strategy(&provider, kind, protocol);
    !provider.api_key_env.trim().is_empty()
        && auth != protocol::AuthStrategy::None
        && !auth.uses_cloud_identity()
}

#[derive(Debug)]
pub enum ConnectionError {
    UnknownProvider(String),
    UnsupportedProtocol {
        provider: String,
        protocol: &'static str,
    },
    MissingCredential {
        provider: String,
        variable: String,
    },
    RejectedCredential,
    BillingRequired,
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownProvider(provider) => write!(
                f,
                "Unknown provider '{provider}'. Use /connect to configure it."
            ),
            Self::UnsupportedProtocol { provider, protocol } => write!(
                f,
                "{protocol} is unavailable for {provider}. Choose another connection in /connect."
            ),
            Self::MissingCredential { provider, variable } => write!(
                f,
                "{variable} is required for {provider}. Set it or use /connect."
            ),
            Self::BillingRequired => write!(
                f,
                "Insufficient balance or billing issue (HTTP 402). Check your provider balance or switch providers with /connect."
            ),
            Self::RejectedCredential => write!(
                f,
                "Invalid or missing API key (HTTP 401). Set the active provider API key environment variable, update llm.api_key, or use /connect."
            ),
        }
    }
}

impl std::error::Error for ConnectionError {}

pub fn check_provider(cfg: &LlmConfig) -> Result<()> {
    cfg.active_provider_kind()
        .ok_or_else(|| ConnectionError::UnknownProvider(cfg.provider.clone()))?;
    let route = route::ChatRoute::resolve(cfg, &cfg.active_base_model(), false)?;
    if provider_requires_key(cfg) && resolve_key_from_config(cfg).is_none() {
        return Err(ConnectionError::MissingCredential {
            provider: cfg.provider.clone(),
            variable: cfg.active_provider().api_key_env,
        }
        .into());
    }
    if !route.auth.uses_cloud_identity() {
        route.headers(resolve_key_from_config(cfg).as_deref())?;
    }
    Ok(())
}

/// Stable workspace-keyed identity for replay. Credentials and the route key are never emitted.
pub fn session_route(cfg: &LlmConfig, model: &str, local_key: &[u8]) -> Result<String> {
    session_route_with_credential(cfg, model, local_key, || resolve_key_from_config(cfg))
}

fn session_route_with_credential(
    cfg: &LlmConfig,
    model: &str,
    local_key: &[u8],
    credential: impl FnOnce() -> Option<String>,
) -> Result<String> {
    let resolved = route::ChatRoute::resolve(cfg, model, false)?;
    let provider = resolved.provider;
    let chat_protocol = resolved.protocol;
    let auth = resolved.auth;
    let account = provider
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let credential = if account.is_none() && auth != protocol::AuthStrategy::None {
        credential()
    } else {
        None
    };
    let route = if provider.cloud.is_some() {
        serde_json::to_vec(&(
            "cloud-v1",
            cfg.provider.as_str(),
            chat_protocol.as_key(),
            auth.as_key(),
            resolved.endpoint,
            resolved.deployment,
            model,
            account,
            credential,
            &provider.cloud,
            cloud_identity::principal_hint(auth),
        ))?
    } else {
        serde_json::to_vec(&(
            cfg.provider.as_str(),
            chat_protocol.as_key(),
            resolved.endpoint,
            model,
            account,
            credential,
            &provider.headers,
        ))?
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, local_key);
    Ok(ring::hmac::sign(&key, &route)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub fn validate_base_url(value: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(value).map_err(|_| anyhow!("Enter a valid HTTP or HTTPS base URL"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Use an HTTP or HTTPS base URL without credentials, query parameters, or fragments"
    );
    Ok(url.as_str().trim_end_matches('/').into())
}

pub fn probe_provider(cfg: &LlmConfig) -> Result<()> {
    check_provider(cfg)?;
    let is_gemini = cfg.active_provider_kind().is_some_and(|kind| {
        protocol::select_chat_protocol(&cfg.active_provider(), kind)
            == protocol::ChatProtocol::GeminiGenerateContent
    });
    let mut cfg = cfg.clone();
    cfg.timeout_seconds = if is_gemini { 15 } else { 8 };
    cfg.max_retries = 0;
    let request = ChatRequest {
        model: cfg.active_base_model(),
        messages: vec![codingbuddy_core::ChatMessage::User {
            content: "Reply with OK.".into(),
        }],
        tools: Vec::new(),
        tool_choice: codingbuddy_core::ToolChoice::none(),
        // Gemini counts thinking tokens against the output limit.
        max_tokens: if is_gemini { 512 } else { 32 },
        temperature: None,
        top_p: None,
        presence_penalty: None,
        frequency_penalty: None,
        logprobs: None,
        top_logprobs: None,
        thinking: None,
        images: Vec::new(),
        provider_options: Default::default(),
        response_format: None,
    };
    ApiClient::new(cfg)?.complete_chat(&request)?;
    Ok(())
}

impl ApiClient {
    pub fn new(cfg: LlmConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_seconds))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
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

    fn wait_before_retry(&self, delay: Duration) -> Result<()> {
        if self.cancel_token.is_none() {
            thread::sleep(delay);
            return Ok(());
        }
        let started = std::time::Instant::now();
        while let Some(remaining) = delay.checked_sub(started.elapsed()) {
            self.check_cancellation()?;
            thread::sleep(remaining.min(Duration::from_millis(40)));
        }
        self.check_cancellation()
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
        let mut req =
            self.apply_auth_for_protocol(self.client.get(url), self.api_key.as_deref(), protocol);
        if protocol == protocol::ChatProtocol::GeminiGenerateContent
            && let Some(key) = &self.api_key
        {
            req = req.header("x-goog-api-key", key);
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
                if chat_protocol == protocol::ChatProtocol::GeminiGenerateContent
                    && provider_config.auth_strategy.is_none() =>
            {
                match api_key {
                    Some(key) => builder.header("x-goog-api-key", key),
                    None => builder,
                }
            }
            protocol::AuthStrategy::QueryApiKey
            | protocol::AuthStrategy::AwsSigV4
            | protocol::AuthStrategy::AzureApiKey
            | protocol::AuthStrategy::AzureEntra
            | protocol::AuthStrategy::GoogleAdc
            | protocol::AuthStrategy::None => builder,
        };
        self.apply_custom_provider_headers(builder, &provider_config)
    }

    fn apply_custom_provider_headers(
        &self,
        builder: reqwest::blocking::RequestBuilder,
        provider: &codingbuddy_core::ProviderConfig,
    ) -> reqwest::blocking::RequestBuilder {
        match route::custom_headers(provider) {
            Ok(headers) => builder.headers(headers),
            Err(_) => builder,
        }
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
                    let body = resp.text().map_err(reqwest::Error::without_url)?;
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
                        format_api_error(
                            status,
                            &redact_key(&body, api_key),
                            attempt,
                            self.cfg.max_retries,
                        ),
                        &prepared.compatibility,
                    ));
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
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
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
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

    fn build_payload(&self, req: &LlmRequest) -> PreparedPayload {
        let provider = self.provider_config();
        let capabilities = self
            .cfg
            .capabilities_for_model(&req.model)
            .unwrap_or_else(|| {
                codingbuddy_core::model_capabilities(
                    self.provider_kind().unwrap_or(ProviderKind::Deepseek),
                    &req.model,
                )
            });
        let fast_mode = self.cfg.fast_mode;
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
            self.cfg.temperature.min(0.2)
        } else {
            self.cfg.temperature
        };
        let mut messages = Vec::new();
        if !self.cfg.language.trim().is_empty() && !self.cfg.language.eq_ignore_ascii_case("en") {
            messages.push(json!({
                "role": "system",
                "content": format!(
                    "Respond in {} unless the user explicitly asks for another language.",
                    self.cfg.language
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
            "stream": self.cfg.stream,
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

    fn build_fim_payload(&self, req: &FimRequest) -> Value {
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

    #[cfg(test)]
    fn build_chat_payload(&self, req: &ChatRequest) -> Result<PreparedPayload> {
        let route = route::ChatRoute::resolve(&self.cfg, &req.model, false)?;
        self.build_chat_payload_for_route(req, &route)
    }

    fn build_chat_payload_for_route(
        &self,
        req: &ChatRequest,
        route: &route::ChatRoute,
    ) -> Result<PreparedPayload> {
        let provider = route.provider.clone();
        let capabilities = &route.capabilities;
        let chat_protocol = route.protocol;
        for tool in &req.tools {
            jsonschema::validator_for(&tool.function.parameters).map_err(|_| {
                error::ProviderError::new(chat_protocol, error::FailureKind::UnsupportedFeature)
            })?;
        }
        if let Some(adapter) = extension::registered_adapter(chat_protocol) {
            return Ok(PreparedPayload {
                payload: adapter.encode(req)?,
                compatibility: AppliedCompatibility {
                    provider: self.cfg.provider.clone(),
                    family: capabilities.family.as_key().into(),
                    transforms: vec![format!("protocol:{}", chat_protocol.as_key())],
                    degraded_inputs: vec![],
                },
            });
        }
        // Native providers: build their own payload format
        match chat_protocol {
            protocol::ChatProtocol::OpenAiResponses => {
                let mut payload = providers::responses::build_payload(req)?;
                let mut layers = vec![&provider.payload_options];
                for key in [
                    "default",
                    capabilities.provider.as_key(),
                    capabilities.family.as_key(),
                    req.model.as_str(),
                ] {
                    if let Some(options) = req.provider_options.get(key) {
                        layers.push(options);
                    }
                }
                providers::responses::apply_options(&mut payload, &layers)?;
                payload["max_output_tokens"] = json!(req.max_tokens.min(
                    max_output_tokens_for_model(capabilities.provider, &req.model, false)
                ));
                return Ok(PreparedPayload {
                    payload,
                    compatibility: AppliedCompatibility {
                        provider: self.cfg.provider.clone(),
                        family: capabilities.family.as_key().into(),
                        transforms: vec!["protocol:openai-responses".into()],
                        degraded_inputs: vec![],
                    },
                });
            }
            protocol::ChatProtocol::AnthropicMessages => {
                let max_cap = max_output_tokens_for_model(
                    capabilities.provider,
                    &req.model,
                    req.thinking
                        .as_ref()
                        .is_some_and(|t| t.thinking_type == "enabled"),
                );
                let payload =
                    providers::anthropic::build_payload(req, req.max_tokens.min(max_cap))?;
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
        let prepared_messages = provider_transform::preflight_chat_messages(req, capabilities)?;
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

        let max_cap =
            max_output_tokens_for_model(capabilities.provider, &req.model, thinking_enabled);
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
                capabilities,
                &provider.base_url,
                &self.cfg.endpoint,
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
            capabilities,
            &mut compatibility,
        );
        provider_transform::apply_chat_payload_compatibility_with_tracking(
            &mut payload,
            req,
            capabilities,
            &mut compatibility,
        );
        compatibility.transforms.sort();
        compatibility.transforms.dedup();
        Ok(PreparedPayload {
            payload,
            compatibility,
        })
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
        let transport = chat_transport::ChatTransport::new(timeout, self.cancel_token.clone())?;
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
        let transport = chat_transport::ChatTransport::new(
            Duration::from_secs(self.cfg.timeout_seconds),
            self.cancel_token.clone(),
        )?;
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
        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            self.check_cancellation()?;
            let response = transport.post(endpoint, headers.clone(), &prepared.payload)?;

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());
                    let body = transport.text(resp)?;
                    if status.is_success() {
                        let envelope: Value = serde_json::from_str(&body)?;
                        if !envelope["error"].is_null() {
                            return Err(error::ProviderError::http(
                                chat_protocol,
                                status.as_u16(),
                                &body,
                            )
                            .into());
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
                        return Ok(response);
                    }
                    last_err = Some(
                        error::ProviderError::http(chat_protocol, status.as_u16(), &body).into(),
                    );
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
                        self.wait_before_retry(retry_delay_ms(
                            self.cfg.retry_base_ms,
                            attempt,
                            retry_after,
                        ))?;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(
                        error::ProviderError::new(chat_protocol, error::FailureKind::Transport)
                            .into(),
                    );
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
                        self.wait_before_retry(retry_delay_ms(
                            NETWORK_RETRY_BASE_MS,
                            attempt,
                            None,
                        ))?;
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
        let payload = self.build_fim_payload(req);
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
                    let body = resp.text().map_err(reqwest::Error::without_url)?;
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
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
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
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
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
        let mut payload = self.build_fim_payload(req);
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
                                transport_parts: Vec::new(),
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
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
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
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
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
        route: &route::ChatRoute,
    ) -> Result<LlmResponse> {
        if let Some(adapter) = extension::registered_adapter(route.protocol) {
            return self.complete_native_adapter(req, route, adapter, Some(cb));
        }
        let mut prepared = self.build_chat_payload_for_route(req, route)?;
        let capabilities = &route.capabilities;
        let chat_protocol = route.protocol;
        let is_responses = chat_protocol == protocol::ChatProtocol::OpenAiResponses;
        let is_native = chat_protocol.is_native();
        if chat_protocol != protocol::ChatProtocol::GeminiGenerateContent {
            prepared.payload["stream"] = json!(true);
        }
        if !is_native
            && !is_responses
            && !route
                .provider
                .gateway
                .as_ref()
                .is_some_and(|g| g.omit_stream_usage_option)
        {
            prepared.payload["stream_options"] = json!({"include_usage":true});
        }
        let stream_endpoint = &route.endpoint;
        let transport = chat_transport::ChatTransport::new(
            Duration::from_secs(self.cfg.timeout_seconds),
            self.cancel_token.clone(),
        )?;
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
        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            self.check_cancellation()?;
            let response = transport.post(stream_endpoint, headers.clone(), &prepared.payload)?;

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());

                    if status.is_success() {
                        let mut content_out = String::new();
                        let mut reasoning_out = String::new();
                        let mut finish_reason: Option<String> = None;
                        let mut tool_call_parts: BTreeMap<u64, StreamToolCall> = BTreeMap::new();
                        let mut completed_tool_calls: Vec<LlmToolCall> = Vec::new();
                        let mut order = Vec::new();
                        let mut usage: Option<codingbuddy_core::TokenUsage> = None;
                        let mut saw_anthropic_stop = false;
                        let mut native_state = providers::NativeStreamState::default();
                        let mut anthropic_blocks = native_parts::AnthropicBlocks::default();
                        let mut gemini_blocks = Vec::new();
                        let mut native_usage = None;
                        let mut anthropic_usage = json!({});
                        let mut responses = providers::responses::Stream::default();
                        let mut reader = std::io::BufReader::new(transport.reader(resp));
                        while let Some(frame) = chat_transport::next_frame(&mut reader)
                            .context("Provider stream read failed")?
                        {
                            let anthropic_event_type = frame.event;
                            let chunk = frame.data.as_str();
                            if chunk == "[DONE]" {
                                if !is_native && !is_responses {
                                    break;
                                }
                                return Err(anyhow!("Unexpected [DONE] in native provider stream"));
                            }
                            let value: Value = serde_json::from_str(chunk)
                                .context("Malformed provider stream event")?;
                            if anthropic_event_type == "error"
                                || value.get("type").and_then(Value::as_str) == Some("error")
                                || !value.get("error").is_none_or(Value::is_null)
                            {
                                return Err(error::ProviderError::http(
                                    chat_protocol,
                                    status.as_u16(),
                                    &value.to_string(),
                                )
                                .into());
                            }

                            if is_responses {
                                if let Some(mut response) = responses.frame(&value, &*cb)? {
                                    response.compatibility = Some(prepared.compatibility.clone());
                                    self.check_cancellation()?;
                                    cb(StreamChunk::Done {
                                        reason: Some(response.finish_reason.clone()),
                                    });
                                    return Ok(response);
                                }
                                continue;
                            }

                            // ── Native Anthropic streaming ──
                            if chat_protocol == protocol::ChatProtocol::AnthropicMessages {
                                let evt_type = if anthropic_event_type.is_empty() {
                                    value.get("type").and_then(|v| v.as_str()).unwrap_or("")
                                } else {
                                    &anthropic_event_type
                                };
                                let usage_part = if evt_type == "message_start" {
                                    value.pointer("/message/usage")
                                } else {
                                    value.get("usage")
                                };
                                if let Some(fields) = usage_part.and_then(Value::as_object) {
                                    anthropic_usage
                                        .as_object_mut()
                                        .expect("object")
                                        .extend(fields.clone());
                                    native_usage =
                                        Some(providers::anthropic::parse_anthropic_usage(
                                            &json!({"usage":anthropic_usage}),
                                        ));
                                }
                                if evt_type == "content_block_start"
                                    && value.pointer("/content_block/type").and_then(Value::as_str)
                                        == Some("text")
                                    && let Some(text) =
                                        value.pointer("/content_block/text").and_then(Value::as_str)
                                    && !text.is_empty()
                                {
                                    cb(StreamChunk::ContentDelta(text.into()));
                                }
                                anthropic_blocks.frame(evt_type, &value)?;
                                let raw =
                                    providers::anthropic::parse_streaming_event(evt_type, &value);
                                let evt = providers::from_anthropic_event(raw);
                                if matches!(evt, providers::ProviderStreamEvent::Done) {
                                    saw_anthropic_stop = true;
                                    break;
                                }
                                native_state.handle_event(evt, &*cb);
                                continue;
                            }

                            // ── Native Google streaming ──
                            if chat_protocol == protocol::ChatProtocol::GeminiGenerateContent {
                                anyhow::ensure!(
                                    value["candidates"].as_array().is_none_or(|c| c.len() <= 1),
                                    "Expected one Gemini candidate"
                                );
                                if value["usageMetadata"].is_object() {
                                    native_usage =
                                        Some(providers::google::parse_gemini_usage(&value));
                                }
                                anyhow::ensure!(
                                    native_state.finish_reason.is_none()
                                        || value.pointer("/candidates/0").is_none(),
                                    "Gemini candidate after terminal event"
                                );
                                if let Some(parts) = value
                                    .pointer("/candidates/0/content/parts")
                                    .and_then(Value::as_array)
                                {
                                    gemini_blocks.extend(parts.iter().cloned());
                                }
                                let raw = providers::google::parse_streaming_chunk(&value);
                                for evt in providers::from_google_event(raw) {
                                    native_state.handle_event(evt, &*cb);
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
                            if let Some(reason) =
                                choice.get("finish_reason").and_then(|v| v.as_str())
                            {
                                finish_reason = Some(reason.to_string());
                            }
                            if let Some(delta) = choice.get("delta") {
                                if let Some(content) = delta
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .or_else(|| delta.get("refusal").and_then(Value::as_str))
                                {
                                    content_out.push_str(content);
                                    order.push(Ok::<_, u64>(TransportPart::Text(
                                        content.to_owned(),
                                    )));
                                    cb(StreamChunk::ContentDelta(content.to_string()));
                                }
                                if let Some(reasoning) =
                                    delta.get("reasoning_content").and_then(|v| v.as_str())
                                {
                                    reasoning_out.push_str(reasoning);
                                }
                                if let Some(tool_calls) =
                                    delta.get("tool_calls").and_then(|v| v.as_array())
                                {
                                    for (position, item) in tool_calls.iter().enumerate() {
                                        let id = item["id"].as_str();
                                        let index = item["index"]
                                            .as_u64()
                                            .or_else(|| {
                                                id.and_then(|id| {
                                                    tool_call_parts.iter().find_map(
                                                        |(index, entry)| {
                                                            (entry.id.as_deref() == Some(id))
                                                                .then_some(*index)
                                                        },
                                                    )
                                                })
                                            })
                                            .unwrap_or(position as u64);
                                        let entry =
                                            tool_call_parts.entry(index).or_insert_with(|| {
                                                order.push(Err(index));
                                                StreamToolCall::default()
                                            });
                                        if let Some(id) = id {
                                            anyhow::ensure!(
                                                entry
                                                    .id
                                                    .as_deref()
                                                    .is_none_or(|previous| previous == id),
                                                "Provider changed a tool call ID"
                                            );
                                            entry.id = Some(id.into());
                                        }
                                        if let Some(name) =
                                            item.pointer("/function/name").and_then(Value::as_str)
                                        {
                                            anyhow::ensure!(
                                                entry.name.is_empty() || entry.name == name,
                                                "Provider changed a tool call name"
                                            );
                                            entry.name = name.into();
                                        }
                                        if let Some(args) = item
                                            .pointer("/function/arguments")
                                            .and_then(Value::as_str)
                                        {
                                            entry.arguments.push_str(args);
                                        }
                                    }
                                }
                            }
                            if let Some(message) = choice.get("message") {
                                if let Some(content) = message["content"]
                                    .as_str()
                                    .or_else(|| message["refusal"].as_str())
                                {
                                    content_out.push_str(content);
                                    order.push(Ok(TransportPart::Text(content.into())));
                                    cb(StreamChunk::ContentDelta(content.to_string()));
                                }
                                if let Some(reasoning) = message["reasoning_content"].as_str() {
                                    reasoning_out.push_str(reasoning);
                                }
                                let calls = parse_completed_calls(message.get("tool_calls"))?;
                                order.extend(
                                    calls
                                        .iter()
                                        .map(|call| Ok(TransportPart::Call(call.id.clone()))),
                                );
                                completed_tool_calls.extend(calls);
                            }
                        }

                        self.check_cancellation()?;
                        let finished = match chat_protocol {
                            protocol::ChatProtocol::AnthropicMessages => {
                                saw_anthropic_stop && native_state.finish_reason.is_some()
                            }
                            protocol::ChatProtocol::GeminiGenerateContent => {
                                native_state.finish_reason.is_some()
                            }
                            _ => finish_reason.is_some(),
                        };
                        anyhow::ensure!(
                            finished,
                            "Provider stream ended before a terminal response"
                        );

                        if chat_protocol == protocol::ChatProtocol::GeminiGenerateContent
                            && !matches!(
                                native_state.finish_reason.as_deref(),
                                Some("stop" | "tool_calls")
                            )
                        {
                            return Err(error::ProviderError::new(
                                chat_protocol,
                                error::FailureKind::IncompleteResponse,
                            )
                            .into());
                        }

                        let mut tool_calls = Vec::new();
                        let mut transport_parts = Vec::new();
                        for part in order {
                            match part {
                                Ok(part) => transport_parts.push(part),
                                Err(index) => {
                                    let call = tool_call_parts
                                        .remove(&index)
                                        .expect("reserved tool index");
                                    let id = call.id.unwrap_or_default();
                                    transport_parts.push(TransportPart::Call(id.clone()));
                                    tool_calls.push(LlmToolCall {
                                        id,
                                        name: call.name,
                                        arguments: call.arguments,
                                    });
                                }
                            }
                        }
                        tool_calls.extend(completed_tool_calls);
                        if is_native {
                            let (protocol, blocks) = match chat_protocol {
                                protocol::ChatProtocol::AnthropicMessages => {
                                    (native_parts::ANTHROPIC, anthropic_blocks.finish()?)
                                }
                                _ => (native_parts::GEMINI, gemini_blocks),
                            };
                            (transport_parts, tool_calls) =
                                native_parts::native_response(protocol, blocks)?;
                            content_out = transport_parts
                                .iter()
                                .filter_map(|part| match part {
                                    TransportPart::Text(text) => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect();
                            reasoning_out = native_state.reasoning;
                            if protocol == native_parts::GEMINI
                                && native_state.finish_reason.as_deref() == Some("stop")
                                && !tool_calls.is_empty()
                            {
                                native_state.finish_reason = Some("tool_calls".into());
                            }
                            finish_reason = native_state.finish_reason;
                            usage = native_usage.or(native_state.usage);
                        }
                        if chat_protocol == protocol::ChatProtocol::OpenAiChat
                            && !reasoning_out.is_empty()
                        {
                            transport_parts.push(TransportPart::Native(
                                codingbuddy_core::NativeParts {
                                    protocol: "openai-chat".into(),
                                    blocks: vec![json!({"reasoning_content":reasoning_out})],
                                },
                            ));
                        }
                        validate_completed_calls(&tool_calls)?;
                        let text = content_out;

                        let reason = finish_reason
                            .ok_or_else(|| anyhow!("Provider finish reason missing"))?;
                        cb(StreamChunk::Done {
                            reason: Some(reason.clone()),
                        });
                        let response = LlmResponse {
                            text,
                            finish_reason: reason,
                            reasoning_content: reasoning_out,
                            tool_calls,
                            usage,
                            transport_parts,
                            compatibility: Some(prepared.compatibility.clone()),
                        };
                        // Native providers handle their own format; OpenAI-compat needs postprocessing
                        if is_native {
                            return Ok(response);
                        }
                        return Ok(
                            provider_transform::postprocess_chat_response_with_compatibility(
                                response,
                                capabilities,
                                &req.tools,
                                prepared.compatibility.clone(),
                            ),
                        );
                    }

                    let body = transport.text(resp)?;
                    last_err = Some(
                        error::ProviderError::http(chat_protocol, status.as_u16(), &body).into(),
                    );
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
                        self.wait_before_retry(retry_delay_ms(
                            self.cfg.retry_base_ms,
                            attempt,
                            retry_after,
                        ))?;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(
                        error::ProviderError::new(chat_protocol, error::FailureKind::Transport)
                            .into(),
                    );
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
                        self.wait_before_retry(retry_delay_ms(
                            NETWORK_RETRY_BASE_MS,
                            attempt,
                            None,
                        ))?;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("chat streaming request failed")))
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

    /// Streaming variant: reads the SSE response line-by-line, invoking `cb`
    /// for each content/reasoning delta, then returns the assembled response.
    fn complete_streaming_inner(
        &self,
        req: &LlmRequest,
        api_key: Option<&str>,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let mut prepared = self.build_payload(req);
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
                            transport_parts: Vec::new(),
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
                        format_api_error(
                            status,
                            &redact_key(&body, api_key),
                            attempt,
                            self.cfg.max_retries,
                        ),
                        &prepared.compatibility,
                    ));
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
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
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
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

    fn complete_chat_streaming(
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

fn redact_key<'a>(body: &'a str, key: Option<&str>) -> std::borrow::Cow<'a, str> {
    match key.filter(|key| !key.is_empty()) {
        Some(key) if body.contains(key) => body.replace(key, "***").into(),
        _ => body.into(),
    }
}

/// Produce a user-friendly error from an LLM API HTTP response.
fn format_api_error(status: StatusCode, body: &str, attempt: u8, max_retries: u8) -> anyhow::Error {
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
        StatusCode::UNAUTHORIZED => ConnectionError::RejectedCredential.into(),
        StatusCode::PAYMENT_REQUIRED => ConnectionError::BillingRequired.into(),
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
fn format_transport_error(err: &reqwest::Error) -> anyhow::Error {
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
        anyhow!("Network error. Check the configured endpoint and retry.")
    }
}

fn compatibility_error_context(compatibility: &AppliedCompatibility) -> String {
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

fn with_compatibility_context(
    err: anyhow::Error,
    compatibility: &AppliedCompatibility,
) -> anyhow::Error {
    if err.is::<ConnectionError>() {
        err
    } else {
        anyhow!("{err}. {}", compatibility_error_context(compatibility))
    }
}

/// Logged at every retry site so a slow or flaky provider shows up in the log
/// even when the request eventually succeeds.
fn log_retry(provider: &str, attempt: u8, max_attempts: u8, reason: &dyn std::fmt::Display) {
    tracing::warn!(
        target: "codingbuddy_llm::retry",
        provider,
        attempt = attempt + 1,
        max_attempts,
        reason = %reason,
        "retrying model request"
    );
}

fn should_retry_status(status: StatusCode) -> bool {
    retry::RetryCategory::from_status(status).should_retry()
}

fn should_retry_transport_error(err: &reqwest::Error) -> bool {
    retry::classify_transport_error(err).should_retry()
}

/// Parse `retry-after-ms` (milliseconds) header, returns value in seconds for
/// consistency with `parse_retry_after_seconds`. Sub-second precision is preserved
/// by rounding up to the nearest second.
fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get("retry-after-ms")?.to_str().ok()?.trim();
    let ms = value.parse::<f64>().ok()?;
    // Convert ms to seconds, rounding up (at least 1s for any positive value)
    Some(((ms / 1000.0).ceil() as u64).max(1))
}

/// Combined retry-after parser: tries `retry-after-ms` first, then `Retry-After`.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    parse_retry_after_ms(headers).or_else(|| parse_retry_after_seconds(headers.get(RETRY_AFTER)))
}

fn parse_retry_after_seconds(header: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let value = header?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    parse_retry_after_http_date(value)
}

fn parse_retry_after_http_date(value: &str) -> Option<u64> {
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

fn resolved_endpoint(
    cfg: &LlmConfig,
    is_chat: bool,
    is_fim: bool,
    is_strict_tools: bool,
) -> String {
    if !cfg.endpoint.is_empty() {
        return cfg.endpoint.clone();
    }
    let provider = cfg.active_provider();
    let base = provider.base_url.trim_end_matches('/');
    let prefix = if provider.openai_compat_prefix && !base.ends_with("/v1") {
        "/v1"
    } else {
        ""
    };
    let path = match cfg.active_provider_kind().unwrap_or(ProviderKind::Deepseek) {
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

/// Compute retry delay with jitter.
/// Priority: `retry-after-ms` header > `Retry-After` header > exponential backoff.
/// Backoff is capped at 30s; header values are respected as-is.
fn retry_delay_ms(base_ms: u64, attempt: u8, retry_after_seconds: Option<u64>) -> Duration {
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

fn parse_fim_non_streaming_payload(body: &str) -> Result<LlmResponse> {
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

fn parse_non_streaming_payload(body: &str) -> Result<LlmResponse> {
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
fn parse_usage_object(usage_value: Option<&Value>) -> Option<codingbuddy_core::TokenUsage> {
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

fn parse_streaming_payload(body: &str) -> Result<LlmResponse> {
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
struct StreamToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

fn merge_stream_tool_calls(chunks: &[Value], out: &mut BTreeMap<u64, StreamToolCall>) {
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

fn confirmed_chat_response(
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

fn validate_completed_calls(calls: &[LlmToolCall]) -> Result<()> {
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

fn parse_completed_calls(value: Option<&Value>) -> Result<Vec<LlmToolCall>> {
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

fn parse_tool_calls_array(value: &Value) -> Vec<LlmToolCall> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use codingbuddy_core::ChatMessage;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration as StdDuration;

    #[test]
    fn parses_non_streaming() {
        let body = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
        let got = parse_non_streaming_payload(body).expect("parse");
        assert_eq!(got.text, "hello");
        assert_eq!(got.finish_reason, "stop");
    }

    #[test]
    fn parses_streaming_sse_lines() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]";
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "hello");
    }

    #[test]
    fn parses_streaming_reasoning_content() {
        let body = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step1\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step2\"}}]}\n\ndata: [DONE]";
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "step1step2");
        assert_eq!(got.reasoning_content, "step1step2");
    }

    #[test]
    fn parses_non_streaming_tool_calls() {
        let body = r#"{
          "choices": [
            {
              "finish_reason": "tool_calls",
              "message": {
                "content": "",
                "tool_calls": [
                  {
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "fs.read", "arguments": "{\"path\":\"README.md\"}" }
                  }
                ]
              }
            }
          ]
        }"#;
        let got = parse_non_streaming_payload(body).expect("parse");
        assert_eq!(got.finish_reason, "tool_calls");
        assert_eq!(got.tool_calls.len(), 1);
        assert_eq!(got.tool_calls[0].name, "fs.read");
    }

    #[test]
    fn parses_streaming_tool_call_fragments() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"fs.read\",\"arguments\":\"{\\\"path\\\":\\\"REA\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"DME.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n"
        );
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.finish_reason, "tool_calls");
        assert_eq!(got.tool_calls.len(), 1);
        assert_eq!(got.tool_calls[0].name, "fs.read");
        assert_eq!(got.tool_calls[0].arguments, "{\"path\":\"README.md\"}");
    }

    #[test]
    fn fast_mode_caps_max_tokens_in_payload() {
        let cfg = LlmConfig {
            fast_mode: true,
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let payload = client.build_payload(&LlmRequest {
            unit: codingbuddy_core::LlmUnit::Planner,
            prompt: "hello".to_string(),
            model: "deepseek-chat".to_string(),
            max_tokens: 16_000,
            non_urgent: false,
            images: vec![],
            provider_options: Default::default(),
        });
        assert_eq!(payload["max_tokens"], 2048);
    }

    #[test]
    fn model_name_passes_through_unmodified_in_payload() {
        let client = ApiClient::new(LlmConfig::default()).expect("client");
        let payload = client.build_payload(&LlmRequest {
            unit: codingbuddy_core::LlmUnit::Planner,
            prompt: "hello".to_string(),
            model: "codingbuddy-v3.2".to_string(),
            max_tokens: 256,
            non_urgent: false,
            images: vec![],
            provider_options: Default::default(),
        });
        assert_eq!(payload["model"], "codingbuddy-v3.2");
    }

    #[test]
    fn truly_unsupported_provider_is_rejected() {
        let cfg = LlmConfig {
            provider: "invalid_provider_xyz".to_string(),
            api_key: Some("test-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let err = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hello".to_string(),
                model: "any".to_string(),
                max_tokens: 128,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect_err("truly unsupported provider should fail");
        assert!(err.to_string().contains("unsupported llm.provider"));
    }

    #[test]
    fn thinking_mode_rejects_logprobs() {
        let cfg = LlmConfig {
            api_key: Some("test-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: Some(true),
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(4096)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let err = client
            .complete_chat(&req)
            .expect_err("logprobs with thinking should fail");
        assert_eq!(
            err.downcast_ref::<error::ProviderError>()
                .expect("typed provider failure")
                .category,
            error::FailureKind::UnsupportedFeature
        );
    }

    #[test]
    fn thinking_mode_rejects_top_logprobs() {
        let cfg = LlmConfig {
            api_key: Some("test-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: Some(5),
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(4096)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let err = client
            .complete_chat(&req)
            .expect_err("top_logprobs with thinking should fail");
        assert_eq!(
            err.downcast_ref::<error::ProviderError>()
                .expect("typed provider failure")
                .category,
            error::FailureKind::UnsupportedFeature
        );
    }

    #[test]
    fn capability_contract_allows_openai_compat_thinking_shim() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(2048)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        client
            .validate_chat_request_contract(&req)
            .expect("openai-compatible thinking shim should be accepted");
    }

    #[test]
    fn capability_contract_rejects_ollama_thinking_config() {
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "qwen2.5-coder:7b".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(2048)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let err = client
            .validate_chat_request_contract(&req)
            .expect_err("ollama should reject thinking config");
        assert!(err.to_string().contains("does not support thinking config"));
    }

    #[test]
    fn capability_contract_rejects_tool_choice_when_unsupported() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-reasoner".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        };
        let err = client
            .validate_chat_request_contract(&req)
            .expect_err("tool_choice should be rejected");
        assert!(err.to_string().contains("does not support tool_choice"));
    }

    #[test]
    fn capability_contract_rejects_too_many_tools() {
        let mut cfg = LlmConfig::default();
        cfg.capability_overrides.models.insert(
            "deepseek@deepseek-chat".to_string(),
            codingbuddy_core::CapabilityOverride {
                max_safe_tool_count: Some(1),
                ..codingbuddy_core::CapabilityOverride::default()
            },
        );
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![
                codingbuddy_core::ToolDefinition {
                    tool_type: "function".to_string(),
                    function: codingbuddy_core::FunctionDefinition {
                        name: "fs_read".to_string(),
                        description: "Read file".to_string(),
                        strict: None,
                        parameters: serde_json::json!({"type": "object"}),
                    },
                },
                codingbuddy_core::ToolDefinition {
                    tool_type: "function".to_string(),
                    function: codingbuddy_core::FunctionDefinition {
                        name: "fs_list".to_string(),
                        description: "List dir".to_string(),
                        strict: None,
                        parameters: serde_json::json!({"type": "object"}),
                    },
                },
            ],
            tool_choice: codingbuddy_core::ToolChoice::auto(),
            max_tokens: 128,
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
        };
        let err = client
            .validate_chat_request_contract(&req)
            .expect_err("tool count should be rejected");
        assert!(err.to_string().contains("max_safe_tool_count"));
    }

    #[test]
    fn capability_contract_rejects_top_logprobs_without_logprobs() {
        let client = ApiClient::new(LlmConfig::default()).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: Some(3),
            thinking: None,
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let err = client
            .validate_chat_request_contract(&req)
            .expect_err("top_logprobs without logprobs should be rejected");
        assert!(
            err.to_string()
                .contains("top_logprobs requires logprobs=true")
        );
    }

    #[test]
    fn thinking_mode_strips_sampling_params_from_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            logprobs: None,
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(4096)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        // With thinking enabled, all sampling params must be omitted
        assert!(
            payload.get("temperature").is_none(),
            "temperature should be stripped"
        );
        assert!(payload.get("top_p").is_none(), "top_p should be stripped");
        assert!(
            payload.get("presence_penalty").is_none(),
            "presence_penalty should be stripped"
        );
        assert!(
            payload.get("frequency_penalty").is_none(),
            "frequency_penalty should be stripped"
        );
        // thinking config should be present
        assert!(
            payload.get("thinking").is_some(),
            "thinking should be present"
        );
    }

    #[test]
    fn non_thinking_mode_includes_sampling_params_in_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            logprobs: Some(true),
            top_logprobs: Some(3),
            thinking: None,
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert!(
            payload.get("temperature").is_some(),
            "temperature should be present"
        );
        assert!(payload.get("top_p").is_some(), "top_p should be present");
        assert!(
            payload.get("presence_penalty").is_some(),
            "presence_penalty should be present"
        );
        assert!(
            payload.get("frequency_penalty").is_some(),
            "frequency_penalty should be present"
        );
        assert_eq!(payload["logprobs"], true);
        assert_eq!(payload["top_logprobs"], 3);
    }

    #[test]
    fn reasoner_strips_tool_choice_from_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let tool = codingbuddy_core::ToolDefinition {
            tool_type: "function".to_string(),
            function: codingbuddy_core::FunctionDefinition {
                name: "fs_read".to_string(),
                description: "Read file".to_string(),
                strict: None,
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let req = ChatRequest {
            model: "deepseek-reasoner".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![tool],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        // Tools should be present
        assert!(payload.get("tools").is_some(), "tools should be present");
        // tool_choice must be stripped for reasoner (HTTP 400 otherwise)
        assert!(
            payload.get("tool_choice").is_none(),
            "tool_choice should be stripped for deepseek-reasoner"
        );
        // thinking config must also be stripped for reasoner (thinks natively)
        assert!(
            payload.get("thinking").is_none(),
            "thinking should be stripped for deepseek-reasoner"
        );
    }

    #[test]
    fn reasoner_strips_thinking_config_from_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-reasoner".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(16_384)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert!(
            payload.get("thinking").is_none(),
            "thinking config must be stripped for deepseek-reasoner (thinks natively)"
        );
    }

    #[test]
    fn non_reasoner_preserves_tool_choice_in_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let tool = codingbuddy_core::ToolDefinition {
            tool_type: "function".to_string(),
            function: codingbuddy_core::FunctionDefinition {
                name: "fs_read".to_string(),
                description: "Read file".to_string(),
                strict: None,
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![tool],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert!(payload.get("tools").is_some(), "tools should be present");
        assert!(
            payload.get("tool_choice").is_some(),
            "tool_choice should be present for non-reasoner"
        );
        assert_eq!(payload["tool_choice"], "required");
    }

    #[test]
    fn format_api_error_parses_structured_error() {
        let body = r#"{"error": {"type": "invalid_request_error", "message": "logprobs is not supported with reasoning"}}"#;
        let err = format_api_error(StatusCode::UNPROCESSABLE_ENTITY, body, 0, 3);
        let msg = err.to_string();
        assert!(msg.contains("HTTP 422"), "should mention 422: {msg}");
        assert!(
            msg.contains("invalid_request_error"),
            "should include error type: {msg}"
        );
        assert!(
            msg.contains("logprobs is not supported"),
            "should include detail: {msg}"
        );
    }

    #[test]
    fn format_api_error_400_includes_type() {
        let body = r#"{"error": {"type": "invalid_format", "message": "bad json"}}"#;
        let err = format_api_error(StatusCode::BAD_REQUEST, body, 0, 3);
        let msg = err.to_string();
        assert!(msg.contains("HTTP 400"), "should mention 400: {msg}");
        assert!(msg.contains("invalid_format"), "should include type: {msg}");
    }

    #[test]
    fn format_api_error_fallback_on_non_json_body() {
        let body = "something went wrong";
        let err = format_api_error(StatusCode::INTERNAL_SERVER_ERROR, body, 2, 3);
        let msg = err.to_string();
        assert!(
            msg.contains("something went wrong"),
            "should include raw body: {msg}"
        );
    }

    #[test]
    fn openai_compatible_payload_maps_thinking_to_reasoning_effort() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: Some(true),
            top_logprobs: Some(3),
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(4096)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["model"], "gpt-4o-mini");
        assert!(payload.get("thinking").is_none());
        assert_eq!(payload["reasoning_effort"], "medium");
        assert_eq!(payload["temperature"], 0.5);
        assert!(
            (payload["top_p"].as_f64().unwrap_or_default() - 0.9).abs() < 0.000_001,
            "top_p should be preserved"
        );
        assert_eq!(payload["logprobs"], true);
        assert_eq!(payload["top_logprobs"], 3);
    }

    #[test]
    fn openai_reasoning_family_with_thinking_strips_sampling_controls() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "o3-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            logprobs: Some(true),
            top_logprobs: Some(3),
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(16_384)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["reasoning_effort"], "high");
        assert!(payload.get("temperature").is_none());
        assert!(payload.get("top_p").is_none());
        assert!(payload.get("presence_penalty").is_none());
        assert!(payload.get("frequency_penalty").is_none());
        assert!(payload.get("logprobs").is_none());
        assert!(payload.get("top_logprobs").is_none());
    }

    #[test]
    fn openai_gemini_payload_adds_max_output_tokens_alias() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gemini-2.0-flash".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 256,
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
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["max_tokens"], 256);
        assert_eq!(payload["max_output_tokens"], 256);
    }

    #[test]
    fn openai_gemini_downgrades_required_tool_choice() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gemini-2.0-flash".to_string(),
            messages: vec![ChatMessage::User {
                content: "run tool".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn litellm_proxy_with_tool_history_gets_placeholder_tool_payload() {
        let mut cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        cfg.endpoint = "https://litellm.internal/v1/chat/completions".to_string();
        cfg.providers
            .get_mut("openai-compatible")
            .expect("provider")
            .base_url = "https://litellm.internal".to_string();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![
                ChatMessage::User {
                    content: "run tool".to_string(),
                },
                ChatMessage::Assistant {
                    content: None,
                    native_parts: None,
                    reasoning_content: None,
                    tool_calls: vec![codingbuddy_core::LlmToolCall {
                        id: "call_1".to_string(),
                        name: "fs_read".to_string(),
                        arguments: "{}".to_string(),
                    }],
                },
                ChatMessage::Tool {
                    tool_call_id: "call_1".to_string(),
                    content: "ok".to_string(),
                    tool_name: None,
                },
            ],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
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
        };

        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["tools"][0]["function"]["name"], "_noop");
        assert!(payload.get("parallel_tool_calls").is_none());
        assert!(payload.get("tool_choice").is_none());
    }

    #[test]
    fn gemini_payload_sanitizes_tool_schema_for_gateway_compatibility() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gemini-2.0-flash".to_string(),
            messages: vec![ChatMessage::User {
                content: "pick a mode".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "pick_mode".to_string(),
                    description: "Pick a mode".to_string(),
                    strict: None,
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "mode": {
                                "type": "integer",
                                "enum": [1, 2]
                            },
                            "items": {
                                "type": "array",
                                "items": {}
                            }
                        },
                        "required": ["mode", "missing"]
                    }),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::auto(),
            max_tokens: 128,
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
        };

        let payload = client.build_chat_payload(&req).expect("build payload");
        let schema = &payload["tools"][0]["function"]["parameters"];
        assert_eq!(schema["required"], serde_json::json!(["mode"]));
        assert_eq!(schema["properties"]["mode"]["type"], "string");
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            serde_json::json!(["1", "2"])
        );
        assert_eq!(schema["properties"]["items"]["items"]["type"], "string");
    }

    #[test]
    fn openai_reasoning_family_uses_max_completion_tokens_key() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "o3-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
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
        };

        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["max_completion_tokens"], 4096);
        assert!(
            payload.get("max_tokens").is_none(),
            "openai reasoning families should use max_completion_tokens"
        );
    }

    #[test]
    fn openai_payload_enables_parallel_tool_calls_when_supported() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "read a file".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        };

        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["parallel_tool_calls"], true);
        assert!(payload.get("tool_choice").is_some());
    }

    #[test]
    fn capability_override_can_disable_tool_choice_and_parallel_payload_flags() {
        let mut cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        cfg.capability_overrides.models.insert(
            "openai-compatible@gpt-4o-mini".to_string(),
            codingbuddy_core::CapabilityOverride {
                supports_tool_choice: Some(false),
                supports_parallel_tool_calls: Some(false),
                ..codingbuddy_core::CapabilityOverride::default()
            },
        );
        let client = ApiClient::new(cfg).expect("client");

        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "read a file".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        };

        let payload = client.build_chat_payload(&req).expect("build payload");
        assert!(payload.get("parallel_tool_calls").is_none());
        assert!(payload.get("tool_choice").is_none());
        assert!(payload.get("tools").is_some());
    }

    #[test]
    fn ollama_provider_allows_requests_without_api_key() {
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            api_key: None,
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        assert_eq!(client.resolve_request_api_key().expect("no auth"), None);
    }

    #[test]
    fn ollama_payload_downgrades_required_tool_choice() {
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "qwen2.5-coder:7b".to_string(),
            messages: vec![ChatMessage::User {
                content: "read a file".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn ollama_payload_sets_num_predict_option_alias() {
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "qwen2.5-coder:7b".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 512,
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
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["options"]["num_predict"], 512);
    }

    #[test]
    fn missing_api_key_is_rejected() {
        let cfg = LlmConfig {
            providers: std::collections::HashMap::new(),
            api_key: None,
            api_key_env: "CODINGBUDDY_NONEXISTENT_KEY_FOR_TEST".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let err = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hello".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 128,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect_err("missing API key should fail");
        assert!(err.to_string().contains("not set and llm.api_key is empty"));
    }

    #[test]
    fn adds_language_system_instruction_when_not_english() {
        let cfg = LlmConfig {
            language: "es".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let payload = client.build_payload(&LlmRequest {
            unit: codingbuddy_core::LlmUnit::Planner,
            prompt: "hola".to_string(),
            model: "deepseek-chat".to_string(),
            max_tokens: 128,
            non_urgent: false,
            images: vec![],
            provider_options: Default::default(),
        });
        let messages = payload["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("Respond in es")
        );
        // P9: no cache_control annotations — DeepSeek uses server-side prefix caching
        assert!(messages[0].get("cache_control").is_none());
    }

    #[test]
    fn transport_error_text_never_contains_the_request_url() {
        // Port 1 on loopback refuses the connection immediately.
        let error = reqwest::blocking::Client::new()
            .post("http://127.0.0.1:1/v1beta/models/m:generateContent?key=SECRET-KEY-123")
            .send()
            .expect_err("connection must fail");
        assert!(
            error.to_string().contains("SECRET-KEY-123"),
            "precondition: reqwest's own Display includes the URL"
        );
        let logged = format!("{:#}", format_transport_error(&error));
        assert!(!logged.contains("SECRET-KEY-123"), "{logged}");
        assert!(!logged.contains("127.0.0.1:1"), "{logged}");
    }

    #[test]
    fn chat_payload_has_no_cache_annotations() {
        // P9: DeepSeek uses automatic server-side prefix caching.
        // No client-side cache_control annotations should be sent.
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![
                ChatMessage::System {
                    content: "system prompt".to_string(),
                },
                ChatMessage::User {
                    content: "hello".to_string(),
                },
            ],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 64,
            temperature: Some(0.2),
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: None,
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        let messages = payload["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|m| m.get("cache_control").is_none()));
    }

    #[test]
    fn resolve_api_key_uses_config_fallback() {
        let cfg = LlmConfig {
            api_key_env: "DEEPSEEK_API_KEY_TEST_FALLBACK".to_string(),
            api_key: Some("local-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY_TEST_FALLBACK");
        }
        let resolved = client.resolve_api_key().expect("fallback key");
        assert_eq!(resolved, "local-key");
    }

    #[test]
    fn retry_status_classification_matches_codingbuddy_guidance() {
        assert!(should_retry_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(should_retry_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(should_retry_status(StatusCode::BAD_GATEWAY));
        assert!(should_retry_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!should_retry_status(StatusCode::UNAUTHORIZED));
        assert!(!should_retry_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn billing_error_remains_actionable_without_protocol_details() {
        let error = with_compatibility_context(
            format_api_error(StatusCode::PAYMENT_REQUIRED, "{}", 0, 0),
            &AppliedCompatibility {
                provider: "deepseek".into(),
                family: "deepseek".into(),
                transforms: vec!["protocol:openai-chat".into()],
                degraded_inputs: Vec::new(),
            },
        );
        assert!(matches!(
            error.downcast_ref(),
            Some(ConnectionError::BillingRequired)
        ));
        assert!(error.to_string().contains("/connect"));
        assert!(!error.to_string().contains("transforms"));
    }

    #[test]
    fn format_api_error_401_includes_setup_instructions() {
        let err = format_api_error(
            StatusCode::UNAUTHORIZED,
            r#"{"error":"invalid_api_key"}"#,
            0,
            3,
        );
        let msg = err.to_string();
        assert!(
            msg.contains("Invalid or missing API key"),
            "should mention invalid/missing key: {msg}"
        );
        assert!(
            msg.contains("llm.api_key"),
            "should mention config field: {msg}"
        );
        assert!(
            msg.contains("active provider API key"),
            "should mention provider auth: {msg}"
        );
    }

    #[test]
    fn format_api_error_401_does_not_retry() {
        // 401 should not be retryable
        assert!(!should_retry_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn network_retry_base_uses_one_second_delays() {
        // Verify the constant matches the spec: base ~1s, ~2s, ~4s (with jitter)
        assert_eq!(NETWORK_RETRY_BASE_MS, 1000);
        let d0 = retry_delay_ms(NETWORK_RETRY_BASE_MS, 0, None);
        let d1 = retry_delay_ms(NETWORK_RETRY_BASE_MS, 1, None);
        let d2 = retry_delay_ms(NETWORK_RETRY_BASE_MS, 2, None);
        // Jitter range: base * 2^attempt * [0.5, 1.5], capped at 30s
        let ms0 = d0.as_millis();
        let ms1 = d1.as_millis();
        let ms2 = d2.as_millis();
        assert!(
            (500..=1500).contains(&ms0),
            "attempt 0 should be ~1s (got {ms0}ms)"
        );
        assert!(
            (1000..=3000).contains(&ms1),
            "attempt 1 should be ~2s (got {ms1}ms)"
        );
        assert!(
            (2000..=6000).contains(&ms2),
            "attempt 2 should be ~4s (got {ms2}ms)"
        );
    }

    #[test]
    fn retry_after_parses_seconds_and_http_date() {
        let seconds_header = reqwest::header::HeaderValue::from_static("7");
        assert_eq!(parse_retry_after_seconds(Some(&seconds_header)), Some(7));

        let future = Utc::now() + chrono::Duration::seconds(5);
        let http_date = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let date_header = reqwest::header::HeaderValue::from_str(&http_date).expect("header");
        let parsed = parse_retry_after_seconds(Some(&date_header)).expect("parsed");
        assert!(parsed <= 10);
    }

    #[test]
    fn complete_retries_transient_status_then_succeeds() {
        let server = start_mock_retry_server(vec![
            MockHttpResponse {
                status: 503,
                body: r#"{"error":"temporarily_unavailable"}"#.to_string(),
                retry_after: Some("0".to_string()),
            },
            MockHttpResponse {
                status: 200,
                body: r#"{"choices":[{"message":{"content":"ok-after-retry"}}]}"#.to_string(),
                retry_after: None,
            },
        ]);

        let cfg = LlmConfig {
            endpoint: server.endpoint.clone(),
            stream: false,
            providers: std::collections::HashMap::new(),
            api_key: Some("test-key".to_string()),
            max_retries: 3,
            retry_base_ms: 1,
            ..LlmConfig::default()
        };
        let client = local_mock_client(cfg);

        let out = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "retry test".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 64,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect("response should eventually succeed");
        assert_eq!(out.text, "ok-after-retry");
        assert!(server.request_count() >= 2);
    }

    #[test]
    fn complete_stops_after_bounded_retries() {
        let server = start_mock_retry_server(vec![MockHttpResponse {
            status: 429,
            body: r#"{"error":"rate_limited"}"#.to_string(),
            retry_after: Some("0".to_string()),
        }]);

        let cfg = LlmConfig {
            endpoint: server.endpoint.clone(),
            stream: false,
            providers: std::collections::HashMap::new(),
            api_key: Some("test-key".to_string()),
            max_retries: 2,
            retry_base_ms: 1,
            ..LlmConfig::default()
        };
        let client = local_mock_client(cfg);

        let err = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "retry limit test".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 64,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect_err("request should fail after retries are exhausted");
        assert!(err.to_string().contains("Rate limited (HTTP 429)"));
        assert_eq!(server.request_count(), 3);
    }

    #[test]
    fn complete_returns_clear_instructions_on_401() {
        let server = start_mock_retry_server(vec![MockHttpResponse {
            status: 401,
            body: r#"{"error":{"message":"invalid_api_key"}}"#.to_string(),
            retry_after: None,
        }]);

        let cfg = LlmConfig {
            endpoint: server.endpoint.clone(),
            stream: false,
            providers: std::collections::HashMap::new(),
            api_key: Some("bad-key".to_string()),
            max_retries: 2,
            retry_base_ms: 1,
            ..LlmConfig::default()
        };
        let client = local_mock_client(cfg);

        let err = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hello".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 64,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect_err("401 should fail without retrying");

        let msg = err.to_string();
        assert!(
            msg.contains("Invalid or missing API key"),
            "401 error should include clear message: {msg}"
        );
        assert!(
            msg.contains("active provider API key"),
            "401 error should mention provider auth: {msg}"
        );
        // 401 is non-retryable, so only 1 request
        assert_eq!(server.request_count(), 1);
    }

    fn local_mock_client(cfg: LlmConfig) -> ApiClient {
        let mut client = ApiClient::new(cfg).expect("client");
        client.client = Client::builder()
            .no_proxy()
            .timeout(StdDuration::from_secs(5))
            .build()
            .expect("loopback HTTP client");
        client
    }

    #[derive(Clone)]
    struct MockHttpResponse {
        status: u16,
        body: String,
        retry_after: Option<String>,
    }

    struct RetryMockServer {
        endpoint: String,
        request_count: Arc<AtomicUsize>,
        stop_tx: Option<mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl RetryMockServer {
        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }
    }

    impl Drop for RetryMockServer {
        fn drop(&mut self) {
            if let Some(tx) = self.stop_tx.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn start_mock_retry_server(responses: Vec<MockHttpResponse>) -> RetryMockServer {
        let scripted = if responses.is_empty() {
            vec![MockHttpResponse {
                status: 500,
                body: r#"{"error":"empty_script"}"#.to_string(),
                retry_after: None,
            }]
        } else {
            responses
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind retry mock server");
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");
        let addr = listener.local_addr().expect("addr");
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_thread = Arc::clone(&request_count);
        let (tx, rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            loop {
                if rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Accepted sockets can inherit nonblocking mode on Windows.
                        stream
                            .set_nonblocking(false)
                            .expect("blocking mock connection");
                        stream
                            .set_read_timeout(Some(StdDuration::from_secs(5)))
                            .expect("mock request read timeout");
                        consume_http_request(&mut stream).expect("read mock HTTP request");
                        let idx = request_count_thread.fetch_add(1, Ordering::SeqCst);
                        let selected = scripted
                            .get(idx)
                            .cloned()
                            .or_else(|| scripted.last().cloned())
                            .expect("scripted response");
                        let status_text = match selected.status {
                            200 => "OK",
                            429 => "Too Many Requests",
                            500 => "Internal Server Error",
                            503 => "Service Unavailable",
                            _ => "Error",
                        };
                        let mut headers = format!(
                            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                            selected.status,
                            status_text,
                            selected.body.len()
                        );
                        if let Some(retry_after) = selected.retry_after {
                            headers.push_str(&format!("Retry-After: {retry_after}\r\n"));
                        }
                        headers.push_str("\r\n");
                        let response = format!("{headers}{}", selected.body);
                        stream
                            .write_all(response.as_bytes())
                            .expect("write mock HTTP response");
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(StdDuration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        RetryMockServer {
            endpoint: format!("http://{addr}/chat/completions"),
            request_count,
            stop_tx: Some(tx),
            handle: Some(handle),
        }
    }

    fn consume_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut header_end = None;
        while header_end.is_none() {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            buffer.extend_from_slice(&chunk[..read]);
            header_end = find_subsequence(&buffer, b"\r\n\r\n").map(|idx| idx + 4);
            if buffer.len() > 1_048_576 {
                break;
            }
        }
        let header_len = header_end.unwrap_or(buffer.len());
        let content_length = parse_content_length(&buffer[..header_len]);
        let mut body = if header_len <= buffer.len() {
            buffer[header_len..].to_vec()
        } else {
            Vec::new()
        };
        while body.len() < content_length {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            body.extend_from_slice(&chunk[..read]);
        }
        Ok(())
    }

    fn parse_content_length(headers: &[u8]) -> usize {
        let raw = String::from_utf8_lossy(headers);
        for line in raw.lines() {
            let mut parts = line.splitn(2, ':');
            let key = parts.next().unwrap_or_default().trim();
            if key.eq_ignore_ascii_case("content-length")
                && let Some(value) = parts.next()
                && let Ok(parsed) = value.trim().parse::<usize>()
            {
                return parsed;
            }
        }
        0
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn complete_streaming_invokes_callback_per_chunk() {
        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]\n";
        let server = start_mock_retry_server(vec![MockHttpResponse {
            status: 200,
            body: sse_body.to_string(),
            retry_after: None,
        }]);

        let cfg = LlmConfig {
            endpoint: server.endpoint.clone(),
            stream: true,
            providers: std::collections::HashMap::new(),
            api_key: Some("test-key".to_string()),
            max_retries: 0,
            ..LlmConfig::default()
        };
        let client = local_mock_client(cfg);

        let chunks = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let chunks_clone = Arc::clone(&chunks);
        let cb: StreamCallback = Arc::new(move |chunk| match chunk {
            StreamChunk::ContentDelta(text) => {
                chunks_clone.lock().expect("test lock").push(text);
            }
            StreamChunk::Done { .. } => {
                chunks_clone
                    .lock()
                    .expect("test lock")
                    .push("[DONE]".to_string());
            }
            _ => {}
        });

        let resp = client
            .complete_streaming(
                &LlmRequest {
                    unit: codingbuddy_core::LlmUnit::Planner,
                    prompt: "hello".to_string(),
                    model: "deepseek-chat".to_string(),
                    max_tokens: 128,
                    non_urgent: false,
                    images: vec![],
                    provider_options: Default::default(),
                },
                cb,
            )
            .expect("streaming response");

        assert_eq!(resp.text, "hello");
        let collected = chunks.lock().expect("test lock");
        assert_eq!(collected.len(), 3); // "hel", "lo", "[DONE]"
        assert_eq!(collected[0], "hel");
        assert_eq!(collected[1], "lo");
        assert_eq!(collected[2], "[DONE]");
    }

    // ── P0-06: SSE keep-alive test coverage ───────────────────────────

    #[test]
    fn sse_keep_alive_lines_are_ignored() {
        let body = concat!(
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            ": keep-alive\n\n",
            "data: [DONE]"
        );
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "hello");
    }

    #[test]
    fn sse_empty_comment_lines_ignored() {
        let body = concat!(
            ":\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            ":\n\n",
            "data: [DONE]"
        );
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "hi");
    }

    #[test]
    fn sse_blank_lines_between_events() {
        let body = concat!(
            "\n\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
            "\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n",
            "\n",
            "data: [DONE]"
        );
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "onetwo");
    }

    // ── P0-05: Usage parsing tests ────────────────────────────────────

    #[test]
    fn parse_usage_all_fields() {
        let usage_json = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 30,
            "prompt_cache_miss_tokens": 70,
            "completion_tokens_details": {
                "reasoning_tokens": 20
            }
        });
        let usage = parse_usage_object(Some(&usage_json)).expect("should parse");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.prompt_cache_hit_tokens, 30);
        assert_eq!(usage.prompt_cache_miss_tokens, 70);
        assert_eq!(usage.reasoning_tokens, 20);
    }

    #[test]
    fn parse_usage_defaults_missing() {
        let usage_json = serde_json::json!({
            "prompt_tokens": 42,
            "completion_tokens": 10
        });
        let usage = parse_usage_object(Some(&usage_json)).expect("should parse");
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 10);
        assert_eq!(usage.prompt_cache_hit_tokens, 0);
        assert_eq!(usage.prompt_cache_miss_tokens, 42);
        assert_eq!(usage.reasoning_tokens, 0);
    }

    #[test]
    fn parse_usage_none_returns_none() {
        assert!(parse_usage_object(None).is_none());
    }

    // ── P0-07/10: Retry status tests ─────────────────────────────────

    #[test]
    fn http_429_is_retryable() {
        assert!(should_retry_status(StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn http_500_is_retryable() {
        assert!(should_retry_status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[test]
    fn http_503_is_retryable() {
        assert!(should_retry_status(StatusCode::SERVICE_UNAVAILABLE));
    }

    #[test]
    fn http_401_is_not_retryable() {
        assert!(!should_retry_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn http_400_is_not_retryable() {
        assert!(!should_retry_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn http_422_is_not_retryable() {
        assert!(!should_retry_status(StatusCode::UNPROCESSABLE_ENTITY));
    }

    #[test]
    fn retry_includes_502_bad_gateway() {
        assert!(should_retry_status(StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn model_max_output_tokens_reasoner() {
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-reasoner", false),
            65536
        );
        // Reasoner always returns 64K regardless of thinking flag
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-reasoner", true),
            65536
        );
    }

    #[test]
    fn model_max_output_tokens_chat() {
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-chat", false),
            8192
        );
    }

    #[test]
    fn max_output_tokens_thinking_chat_32k() {
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-chat", true),
            32768
        );
    }

    #[test]
    fn max_output_tokens_reasoner_64k() {
        // Reasoner is always 64K regardless of thinking param
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-reasoner", false),
            65536
        );
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-reasoner", true),
            65536
        );
    }

    #[test]
    fn reasoner_caps_max_tokens_to_65536() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        let req = LlmRequest {
            unit: codingbuddy_core::LlmUnit::Planner,
            prompt: "hello".to_string(),
            model: "deepseek-reasoner".to_string(),
            max_tokens: 100_000,
            non_urgent: false,
            images: vec![],
            provider_options: Default::default(),
        };
        let payload = client.build_payload(&req);
        assert_eq!(payload["max_tokens"], 65536);
    }

    #[test]
    fn chat_caps_max_tokens_to_8192() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        let req = LlmRequest {
            unit: codingbuddy_core::LlmUnit::Planner,
            prompt: "hello".to_string(),
            model: "deepseek-chat".to_string(),
            max_tokens: 20_000,
            non_urgent: false,
            images: vec![],
            provider_options: Default::default(),
        };
        let payload = client.build_payload(&req);
        assert_eq!(payload["max_tokens"], 8192);
    }

    // ── P1-18: resolved_endpoint routing tests ──────────────────────────

    #[test]
    fn resolved_endpoint_default_chat() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: String::new(), // empty triggers computed path
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        assert_eq!(
            client.resolved_endpoint(true, false, false),
            "https://api.deepseek.com/chat/completions"
        );
    }

    #[test]
    fn resolved_endpoint_fim_routes_to_beta() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: String::new(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        assert_eq!(
            client.resolved_endpoint(false, true, false),
            "https://api.deepseek.com/beta/completions"
        );
    }

    #[test]
    fn resolved_endpoint_strict_tools_routes_to_beta() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: String::new(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        assert_eq!(
            client.resolved_endpoint(true, false, true),
            "https://api.deepseek.com/beta/completions"
        );
    }

    #[test]
    fn resolved_endpoint_openai_compat_adds_v1_prefix() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            endpoint: String::new(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        assert_eq!(
            client.resolved_endpoint(true, false, false),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn resolved_endpoint_custom_endpoint_overrides() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: "https://custom.example.com/my-api".to_string(),
            openai_compat_prefix: true,
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        // Custom endpoint takes priority over everything
        assert_eq!(
            client.resolved_endpoint(true, false, false),
            "https://custom.example.com/my-api"
        );
    }

    #[test]
    fn set_cancel_token_stores_token() {
        let cfg = LlmConfig::default();
        let mut client = ApiClient::new(cfg).unwrap();
        assert!(client.cancel_token.is_none());

        let token = CancellationToken::new();
        client.set_cancel_token(token.clone());
        assert!(client.cancel_token.is_some());
        assert!(!client.cancel_token.as_ref().unwrap().is_cancelled());

        token.cancel();
        assert!(client.cancel_token.as_ref().unwrap().is_cancelled());
    }

    #[test]
    fn cancellation_token_reset_works() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
        token.reset();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_build_chat_payload_returns_result() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: None,
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let result = client.build_chat_payload(&req);
        assert!(
            result.is_ok(),
            "build_chat_payload should return Ok for valid input"
        );
        let payload = result.unwrap();
        assert_eq!(payload["model"], "deepseek-chat");
        assert_eq!(payload["max_tokens"], 128);
        assert!(payload["messages"].is_array());
    }

    #[test]
    fn test_api_client_caches_api_key() {
        let cfg = LlmConfig {
            api_key_env: "CODINGBUDDY_CACHE_TEST_NONEXISTENT".to_string(),
            api_key: Some("cached-test-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        // The cached key should be resolved eagerly from cfg.api_key
        assert_eq!(
            client.api_key.as_deref(),
            Some("cached-test-key"),
            "api_key should be cached at construction"
        );
        // resolve_api_key should return the cached key
        let resolved = client.resolve_api_key().expect("should resolve cached key");
        assert_eq!(resolved, "cached-test-key");

        // After clearing, resolve_api_key falls back to env/config
        let mut client = client;
        client.clear_api_key();
        assert!(
            client.api_key.is_none(),
            "api_key should be None after clear"
        );
        // Since the env var does not exist and we cleared the cache,
        // resolve_api_key should still fall back to cfg.api_key
        let resolved = client
            .resolve_api_key()
            .expect("should fall back to cfg.api_key");
        assert_eq!(resolved, "cached-test-key");
    }

    #[test]
    fn anthropic_provider_payload_includes_anthropic_version_header() {
        let cfg = LlmConfig {
            provider: "anthropic".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::auto(),
            max_tokens: 1024,
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
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        assert_eq!(payload["model"], "claude-sonnet-4-20250514");
        assert!(payload["messages"].is_array());
    }

    #[test]
    fn google_provider_payload_uses_correct_model() {
        let cfg = LlmConfig {
            provider: "google".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gemini-2.5-flash".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::auto(),
            max_tokens: 1024,
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
        };
        let payload = client.build_chat_payload(&req).expect("build payload");
        // Native Gemini format: uses contents array, not messages
        assert!(
            payload["contents"].is_array(),
            "Google native format should use contents array"
        );
        assert!(
            payload.get("generationConfig").is_some(),
            "Google native format should have generationConfig"
        );
    }
}
