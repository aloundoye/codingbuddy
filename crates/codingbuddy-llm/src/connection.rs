//! Provider connectivity checks, session routes and base URL validation.

use super::*;

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

pub(crate) fn session_route_with_credential(
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
