//! Retry logic with error differentiation.
//!
//! Categorizes HTTP errors and transport failures to apply appropriate retry
//! strategies. Rate limits get long backoff with Retry-After respect, server
//! errors get quick retries, auth failures surface immediately.

use reqwest::StatusCode;
use std::time::Duration;

/// Category of error for retry decision-making.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryCategory {
    /// 429 — exponential backoff, respect Retry-After header.
    RateLimit,
    /// 500, 502, 503, 529 — quick retry, short backoff.
    ServerOverload,
    /// 401, 403 — don't retry, surface auth error to user.
    AuthFailure,
    /// 400 — don't retry, the request is malformed.
    ClientError,
    /// Connection reset, DNS failure, timeout — moderate backoff retry.
    NetworkError,
}

impl RetryCategory {
    /// Classify an HTTP status code.
    pub fn from_status(status: StatusCode) -> Self {
        match status.as_u16() {
            429 => Self::RateLimit,
            500 | 502 | 503 | 529 => Self::ServerOverload,
            401 | 403 => Self::AuthFailure,
            _ if status.is_client_error() => Self::ClientError,
            _ if status.is_server_error() => Self::ServerOverload,
            _ => Self::ClientError,
        }
    }

    /// Whether this category should be retried.
    pub fn should_retry(self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::ServerOverload | Self::NetworkError
        )
    }

    /// Maximum number of retries for this category.
    pub fn max_retries(self, configured: u8) -> u8 {
        match self {
            Self::RateLimit => configured.max(3),
            Self::ServerOverload => configured.min(3),
            Self::NetworkError => configured.min(3),
            Self::AuthFailure | Self::ClientError => 0,
        }
    }

    /// Base delay for this category.
    pub fn base_delay_ms(self) -> u64 {
        match self {
            Self::RateLimit => 1000,
            Self::ServerOverload => 500,
            Self::NetworkError => 1000,
            Self::AuthFailure | Self::ClientError => 0,
        }
    }

    /// Maximum delay cap for this category.
    pub fn max_delay_ms(self) -> u64 {
        match self {
            Self::RateLimit => 60_000,
            Self::ServerOverload => 5_000,
            Self::NetworkError => 10_000,
            Self::AuthFailure | Self::ClientError => 0,
        }
    }
}

/// Classify a transport/connection error.
pub fn classify_transport_error(err: &reqwest::Error) -> RetryCategory {
    if err.is_timeout() || err.is_connect() || err.is_request() {
        RetryCategory::NetworkError
    } else {
        RetryCategory::ClientError
    }
}

/// Compute retry delay with exponential backoff and jitter.
pub fn compute_delay(category: RetryCategory, attempt: u8, retry_after: Option<u64>) -> Duration {
    // Retry-After header takes precedence for rate limits
    if let Some(seconds) = retry_after
        && category == RetryCategory::RateLimit
    {
        return Duration::from_secs(seconds.max(1));
    }

    let base = category.base_delay_ms() as f64;
    let max = category.max_delay_ms() as f64;
    let exponent = u32::from(attempt);
    let raw = base * 2f64.powi(exponent as i32);
    // Deterministic jitter (avoids rand dependency)
    let jitter = 0.5 + ((attempt as f64 * 1.618).fract());
    let delay = (raw * jitter).min(max).max(base);
    Duration::from_millis(delay as u64)
}

/// Format a user-friendly error hint based on the category.
pub fn category_hint(category: RetryCategory) -> &'static str {
    match category {
        RetryCategory::AuthFailure => "Check your API key — it may be invalid or expired.",
        RetryCategory::RateLimit => "Rate limited. Retrying with backoff...",
        RetryCategory::ServerOverload => "Server overloaded. Retrying shortly...",
        RetryCategory::NetworkError => "Network error. Retrying...",
        RetryCategory::ClientError => "Request error — check model name and parameters.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_429_as_rate_limit() {
        assert_eq!(
            RetryCategory::from_status(StatusCode::TOO_MANY_REQUESTS),
            RetryCategory::RateLimit
        );
    }

    #[test]
    fn classify_529_as_server_overload() {
        let status = StatusCode::from_u16(529).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let cat = RetryCategory::from_status(status);
        assert!(cat.should_retry());
    }

    #[test]
    fn classify_401_as_auth_failure() {
        let cat = RetryCategory::from_status(StatusCode::UNAUTHORIZED);
        assert_eq!(cat, RetryCategory::AuthFailure);
        assert!(!cat.should_retry());
        assert_eq!(cat.max_retries(5), 0);
    }

    #[test]
    fn classify_400_as_client_error() {
        let cat = RetryCategory::from_status(StatusCode::BAD_REQUEST);
        assert_eq!(cat, RetryCategory::ClientError);
        assert!(!cat.should_retry());
    }

    #[test]
    fn rate_limit_respects_retry_after() {
        let delay = compute_delay(RetryCategory::RateLimit, 0, Some(5));
        assert!(delay >= Duration::from_secs(5));
    }

    #[test]
    fn server_overload_short_backoff() {
        let delay = compute_delay(RetryCategory::ServerOverload, 0, None);
        assert!(delay <= Duration::from_secs(2));
    }

    #[test]
    fn backoff_grows_with_attempts() {
        let d0 = compute_delay(RetryCategory::NetworkError, 0, None);
        let d2 = compute_delay(RetryCategory::NetworkError, 2, None);
        assert!(d2 > d0);
    }

    #[test]
    fn delay_capped_at_max() {
        let delay = compute_delay(RetryCategory::ServerOverload, 10, None);
        assert!(delay <= Duration::from_millis(5_000));
    }
}
