//! HTTP error formatting, retry decisions, Retry-After parsing and endpoints.

use super::*;

pub(crate) fn redact_key<'a>(body: &'a str, key: Option<&str>) -> std::borrow::Cow<'a, str> {
    match key.filter(|key| !key.is_empty()) {
        Some(key) if body.contains(key) => body.replace(key, "***").into(),
        _ => body.into(),
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
        anyhow!("Network error. Check the configured endpoint and retry.")
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
    if err.is::<ConnectionError>() {
        err
    } else {
        anyhow!("{err}. {}", compatibility_error_context(compatibility))
    }
}

/// Logged at every retry site so a slow or flaky provider shows up in the log
/// even when the request eventually succeeds.
pub(crate) fn log_retry(
    provider: &str,
    attempt: u8,
    max_attempts: u8,
    reason: &dyn std::fmt::Display,
) {
    tracing::warn!(
        target: "codingbuddy_llm::retry",
        provider,
        attempt = attempt + 1,
        max_attempts,
        reason = %reason,
        "retrying model request"
    );
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

pub(crate) fn resolved_endpoint(
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
