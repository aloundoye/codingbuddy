use anyhow::Result;
use codingbuddy_core::CancellationToken;
use std::{
    future::Future,
    io::{self, BufRead, Read},
    time::Duration,
};

/// Async HTTP for one model call, driven from synchronous code.
///
/// Futures run on the process-wide runtime (`codingbuddy_core::runtime`),
/// so connection pools outlive a single call, and every wait races the
/// turn's cancellation token: cancelling wakes the wait immediately.
pub(crate) struct ChatTransport {
    client: reqwest::Client,
    cancellation: Option<CancellationToken>,
}

/// How [`ChatTransport::send_with_retry`] retries a request.
pub(crate) struct RetryPolicy<'a> {
    pub provider: &'a str,
    pub max_retries: u8,
    pub retry_base_ms: u64,
    /// Receives `RateLimited` progress for HTTP 429 when a UI is streaming.
    pub progress: Option<&'a codingbuddy_core::StreamCallback>,
}

/// The HTTP client every model call shares: no redirects (credentials must
/// not follow a redirect to another host) and the configured timeout.
pub(crate) fn build_http_client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

impl ChatTransport {
    #[cfg(test)]
    pub fn new(timeout: Duration, cancellation: Option<CancellationToken>) -> Result<Self> {
        Ok(Self::with_client(build_http_client(timeout)?, cancellation))
    }

    /// Reuse an existing client (and its connection pool).
    pub fn with_client(client: reqwest::Client, cancellation: Option<CancellationToken>) -> Self {
        Self {
            client,
            cancellation,
        }
    }

    /// Drive `future` on the shared runtime from this thread, returning
    /// "Turn interrupted" as soon as the cancellation token fires.
    pub(crate) fn wait<T>(&self, future: impl Future<Output = T>) -> Result<T> {
        let runtime = codingbuddy_core::runtime::runtime();
        runtime
            .block_on(codingbuddy_core::runtime::with_cancellation(
                self.cancellation.as_ref(),
                future,
            ))
            .map_err(anyhow::Error::from)
    }

    /// Back off between retries; returns early if the turn is cancelled.
    pub(crate) fn sleep(&self, delay: Duration) -> Result<()> {
        // Build the timer inside the runtime: creating it on this thread
        // first would panic with "no reactor running".
        self.wait(async move { tokio::time::sleep(delay).await })
    }

    pub(crate) fn check_cancelled(&self) -> Result<()> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(codingbuddy_core::runtime::Cancelled.into());
        }
        Ok(())
    }

    /// Send with the shared retry rules and return the first successful
    /// response. `send` issues one attempt; `http_error` and
    /// `transport_error` build the error reported when an attempt fails
    /// and no retry remains. Retries apply to retryable HTTP statuses
    /// (honouring Retry-After) and to transient network errors, with
    /// cancellable backoff.
    pub(crate) fn send_with_retry(
        &self,
        policy: &RetryPolicy<'_>,
        mut send: impl FnMut() -> Result<Result<reqwest::Response, reqwest::Error>>,
        mut http_error: impl FnMut(reqwest::StatusCode, &str, u8) -> anyhow::Error,
        mut transport_error: impl FnMut(&reqwest::Error) -> anyhow::Error,
    ) -> Result<reqwest::Response> {
        let mut attempt: u8 = 0;
        loop {
            self.check_cancelled()?;
            match send()? {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let retry_after = crate::parse_retry_after(response.headers());
                    let body = self.text(response).unwrap_or_default();
                    let error = http_error(status, &body, attempt);
                    if !(crate::should_retry_status(status) && attempt < policy.max_retries) {
                        return Err(error);
                    }
                    let delay = crate::retry_delay_ms(policy.retry_base_ms, attempt, retry_after);
                    crate::log_retry(
                        policy.provider,
                        attempt,
                        policy.max_retries,
                        &format!("HTTP {status}"),
                    );
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        && let Some(progress) = policy.progress
                    {
                        progress(codingbuddy_core::StreamChunk::RateLimited {
                            wait_seconds: delay.as_secs().max(1),
                            attempt: attempt + 1,
                            max_attempts: policy.max_retries,
                            provider: policy.provider.to_string(),
                        });
                    }
                    self.sleep(delay)?;
                }
                Err(error) => {
                    let failure = transport_error(&error);
                    if !(crate::should_retry_transport_error(&error)
                        && attempt < policy.max_retries)
                    {
                        return Err(failure);
                    }
                    // Never log `error` itself: its Display includes the URL,
                    // which carries the API key for query-key providers.
                    crate::log_retry(
                        policy.provider,
                        attempt,
                        policy.max_retries,
                        &crate::format_transport_error(&error),
                    );
                    self.sleep(crate::retry_delay_ms(
                        crate::NETWORK_RETRY_BASE_MS,
                        attempt,
                        None,
                    ))?;
                }
            }
            attempt = attempt.saturating_add(1);
        }
    }

    pub(crate) fn wait_with_timeout<T>(
        &self,
        timeout: Duration,
        future: impl Future<Output = T>,
    ) -> Result<Result<T, tokio::time::error::Elapsed>> {
        self.wait(async { tokio::time::timeout(timeout, future).await })
    }

    pub fn post(
        &self,
        endpoint: &str,
        headers: reqwest::header::HeaderMap,
        payload: &serde_json::Value,
    ) -> Result<Result<reqwest::Response, reqwest::Error>> {
        // Strip the URL like the sibling methods: query-key providers put
        // the API key in it, and the error may be displayed or logged.
        Ok(self
            .wait(async {
                self.client
                    .post(endpoint)
                    .headers(headers)
                    .json(payload)
                    .send()
                    .await
            })?
            .map_err(reqwest::Error::without_url))
    }

    pub fn post_bytes(
        &self,
        endpoint: &str,
        headers: reqwest::header::HeaderMap,
        payload: Vec<u8>,
    ) -> Result<reqwest::Response> {
        Ok(self
            .wait(async {
                self.client
                    .post(endpoint)
                    .headers(headers)
                    .body(payload)
                    .send()
                    .await
            })?
            .map_err(reqwest::Error::without_url)?)
    }

    pub fn text(&self, response: reqwest::Response) -> Result<String> {
        Ok(self
            .wait(response.text())?
            .map_err(reqwest::Error::without_url)?)
    }

    pub fn reader(&self, response: reqwest::Response) -> impl Read + '_ {
        ResponseReader {
            transport: self,
            response,
            current: io::Cursor::new(Vec::new()),
        }
    }
}

struct ResponseReader<'a> {
    transport: &'a ChatTransport,
    response: reqwest::Response,
    current: io::Cursor<Vec<u8>>,
}

impl Read for ResponseReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let size = self.current.read(output)?;
            if size != 0 {
                return Ok(size);
            }
            let next = self
                .transport
                .wait(self.response.chunk())
                .map_err(io::Error::other)?
                .map_err(|err| io::Error::other(err.without_url()))?;
            match next {
                Some(bytes) => self.current = io::Cursor::new(bytes.to_vec()),
                None => return Ok(0),
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct SseFrame {
    pub event: String,
    pub data: String,
}

pub(crate) fn next_frame(reader: &mut impl BufRead) -> io::Result<Option<SseFrame>> {
    let mut frame = SseFrame::default();
    let mut line = String::new();
    let mut has_data = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            if has_data {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated SSE frame",
                ));
            }
            return Ok(None);
        }
        let content = line.trim_end_matches(['\r', '\n']);
        if content.is_empty() && has_data {
            return Ok(Some(frame));
        }
        if content.is_empty() {
            frame.event.clear();
        }
        if let Some(value) = content.strip_prefix("event:") {
            frame.event = value.strip_prefix(' ').unwrap_or(value).to_owned();
        }
        if let Some(value) = content.strip_prefix("data:") {
            if has_data {
                frame.data.push('\n');
            }
            frame
                .data
                .push_str(value.strip_prefix(' ').unwrap_or(value));
            if frame.data.len() > 16 * 1024 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SSE frame exceeds size limit",
                ));
            }
            has_data = true;
        }
    }
}
