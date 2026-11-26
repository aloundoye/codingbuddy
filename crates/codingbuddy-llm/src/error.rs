//! Sanitized provider failures, suitable for CLI and RPC diagnostics.
use crate::protocol::ChatProtocol;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Authentication,
    RateLimit,
    UnsupportedFeature,
    MalformedResponse,
    IncompleteStream,
    IncompleteResponse,
    ProviderFailure,
    Cancelled,
    Transport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    pub protocol: String,
    pub category: FailureKind,
    pub retryable: bool,
    pub http_status: Option<u16>,
    pub provider_code: Option<String>,
}

impl ProviderError {
    pub fn new(protocol: ChatProtocol, category: FailureKind) -> Self {
        Self {
            protocol: protocol.as_key().into(),
            category,
            retryable: matches!(category, FailureKind::RateLimit | FailureKind::Transport),
            http_status: None,
            provider_code: None,
        }
    }

    pub fn http(protocol: ChatProtocol, status: u16, body: &str) -> Self {
        let category = match status {
            401 | 403 => FailureKind::Authentication,
            429 => FailureKind::RateLimit,
            _ => FailureKind::ProviderFailure,
        };
        let mut error = Self::new(protocol, category);
        error.http_status = Some(status);
        error.retryable = status == 429 || status >= 500;
        // Provider response text and codes can contain credentials. Retain only known codes.
        let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
        error.provider_code = value
            .pointer("/error/code")
            .and_then(serde_json::Value::as_str)
            .filter(|code| {
                matches!(
                    *code,
                    "invalid_api_key"
                        | "rate_limit_exceeded"
                        | "insufficient_quota"
                        | "context_length_exceeded"
                        | "model_not_found"
                        | "invalid_request_error"
                )
            })
            .map(str::to_owned)
            .or_else(|| {
                // Google reports a gRPC-style status name next to a numeric code.
                value
                    .pointer("/error/status")
                    .and_then(serde_json::Value::as_str)
                    .filter(|status| {
                        matches!(
                            *status,
                            "UNAVAILABLE"
                                | "NOT_FOUND"
                                | "RESOURCE_EXHAUSTED"
                                | "PERMISSION_DENIED"
                                | "UNAUTHENTICATED"
                                | "INVALID_ARGUMENT"
                                | "FAILED_PRECONDITION"
                                | "DEADLINE_EXCEEDED"
                                | "INTERNAL"
                        )
                    })
                    .map(str::to_owned)
            });
        error
    }

    pub(crate) fn classify(protocol: ChatProtocol, error: anyhow::Error) -> anyhow::Error {
        if error.downcast_ref::<Self>().is_some() {
            return error;
        }
        let detail = format!("{error:#}").to_lowercase();
        let category = if detail.contains("interrupted") || detail.contains("cancelled") {
            FailureKind::Cancelled
        } else if detail.contains("terminal")
            || detail.contains("unfinished")
            || detail.contains("incomplete")
            || detail.contains("truncated")
            || detail.contains("unexpected eof")
        {
            FailureKind::IncompleteStream
        } else if detail.contains("not set") || detail.contains("credential") {
            FailureKind::Authentication
        } else if detail.contains("unsupported")
            || detail.contains("does not support")
            || detail.contains("incompatible")
            || detail.contains("requires logprobs")
        {
            FailureKind::UnsupportedFeature
        } else if detail.contains("provider response failed")
            || detail.contains("completion error")
            || detail.contains("error during")
        {
            FailureKind::ProviderFailure
        } else if error.downcast_ref::<reqwest::Error>().is_some()
            || detail.contains("stream read failed")
        {
            FailureKind::Transport
        } else {
            FailureKind::MalformedResponse
        };
        Self::new(protocol, category).into()
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let http_error = self.http_status.filter(|status| *status >= 300);
        let reason = match self.category {
            FailureKind::Authentication => "Provider authentication failed",
            FailureKind::RateLimit => "Provider rate limit exceeded",
            FailureKind::UnsupportedFeature => "Unsupported provider feature",
            FailureKind::MalformedResponse => {
                "Malformed provider stream event or malformed tool arguments"
            }
            FailureKind::IncompleteStream => "Provider stream ended before a terminal response",
            FailureKind::IncompleteResponse => "Provider response incomplete",
            FailureKind::ProviderFailure if matches!(http_error, Some(503 | 529)) => {
                "Model is overloaded right now (the provider is busy, not a key problem)"
            }
            FailureKind::ProviderFailure if http_error == Some(404) => {
                "Model not found or not enabled for this key"
            }
            FailureKind::ProviderFailure if matches!(http_error, Some(500 | 502 | 504)) => {
                "Provider server error"
            }
            FailureKind::ProviderFailure if http_error.is_some() => {
                "Provider reported a request failure"
            }
            FailureKind::ProviderFailure => "Provider sent an error during the response stream",
            FailureKind::Cancelled => "Turn interrupted",
            FailureKind::Transport => "Provider transport failed",
        };
        if let Some(status) = http_error {
            write!(f, "{reason} ({}, HTTP {status})", self.protocol)
        } else {
            write!(f, "{reason} ({})", self.protocol)
        }
    }
}
impl std::error::Error for ProviderError {}

pub fn diagnostic(error: &anyhow::Error) -> Option<serde_json::Value> {
    error
        .downcast_ref::<ProviderError>()
        .and_then(|error| serde_json::to_value(error).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_failures_name_the_likely_cause_and_keep_only_known_codes() {
        let protocol = ChatProtocol::GeminiGenerateContent;
        let busy = ProviderError::http(
            protocol,
            503,
            r#"{"error":{"code":503,"message":"high demand","status":"UNAVAILABLE"}}"#,
        );
        assert!(busy.to_string().contains("overloaded"), "{busy}");
        assert_eq!(busy.provider_code.as_deref(), Some("UNAVAILABLE"));
        assert!(busy.retryable);

        let missing = ProviderError::http(protocol, 404, "{}");
        assert!(missing.to_string().contains("Model not found"), "{missing}");

        // Free text in the status field is never copied.
        let odd = ProviderError::http(protocol, 400, r#"{"error":{"status":"key=AQ.secret"}}"#);
        assert_eq!(odd.provider_code, None);
    }
}
