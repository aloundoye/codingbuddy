use anyhow::Result;
use codingbuddy_core::CancellationToken;
use std::{future::Future, io, time::Duration};

/// Async HTTP for one model call.
///
/// Requests share the process-wide runtime and connection pool, and every
/// wait races the turn's cancellation token: cancelling drops the request
/// immediately.
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

    /// Await `future`, returning "Turn interrupted" as soon as the
    /// cancellation token fires.
    pub(crate) async fn wait<T>(&self, future: impl Future<Output = T>) -> Result<T> {
        codingbuddy_core::runtime::with_cancellation(self.cancellation.as_ref(), future)
            .await
            .map_err(anyhow::Error::from)
    }

    /// Back off between retries; returns early if the turn is cancelled.
    pub(crate) async fn sleep(&self, delay: Duration) -> Result<()> {
        self.wait(tokio::time::sleep(delay)).await
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
    pub(crate) async fn send_with_retry<F, Fut>(
        &self,
        policy: &RetryPolicy<'_>,
        mut send: F,
        mut http_error: impl FnMut(reqwest::StatusCode, &str, u8) -> anyhow::Error,
        mut transport_error: impl FnMut(&reqwest::Error) -> anyhow::Error,
    ) -> Result<reqwest::Response>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Result<reqwest::Response, reqwest::Error>>>,
    {
        let mut attempt: u8 = 0;
        loop {
            self.check_cancelled()?;
            match send().await? {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let retry_after = crate::parse_retry_after(response.headers());
                    let body = self.text(response).await.unwrap_or_default();
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
                    self.sleep(delay).await?;
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
                    ))
                    .await?;
                }
            }
            attempt = attempt.saturating_add(1);
        }
    }

    pub(crate) async fn wait_with_timeout<T>(
        &self,
        timeout: Duration,
        future: impl Future<Output = T>,
    ) -> Result<Result<T, tokio::time::error::Elapsed>> {
        self.wait(tokio::time::timeout(timeout, future)).await
    }

    pub async fn post(
        &self,
        endpoint: &str,
        headers: reqwest::header::HeaderMap,
        payload: &serde_json::Value,
    ) -> Result<Result<reqwest::Response, reqwest::Error>> {
        // Strip the URL like the sibling methods: query-key providers put
        // the API key in it, and the error may be displayed or logged.
        Ok(self
            .wait(
                self.client
                    .post(endpoint)
                    .headers(headers)
                    .json(payload)
                    .send(),
            )
            .await?
            .map_err(reqwest::Error::without_url))
    }

    pub async fn post_bytes(
        &self,
        endpoint: &str,
        headers: reqwest::header::HeaderMap,
        payload: Vec<u8>,
    ) -> Result<reqwest::Response> {
        Ok(self
            .wait(
                self.client
                    .post(endpoint)
                    .headers(headers)
                    .body(payload)
                    .send(),
            )
            .await?
            .map_err(reqwest::Error::without_url)?)
    }

    pub async fn text(&self, response: reqwest::Response) -> Result<String> {
        Ok(self
            .wait(response.text())
            .await?
            .map_err(reqwest::Error::without_url)?)
    }

    /// Read `response` incrementally (SSE lines, event-stream frames).
    pub fn stream(&self, response: reqwest::Response) -> ByteStream<'_> {
        ByteStream {
            transport: Some(self),
            source: Source::Http(response),
            buffer: Vec::new(),
            position: 0,
        }
    }
}

enum Source {
    Http(reqwest::Response),
    #[cfg(test)]
    Done,
}

/// The longest line a provider stream may send.
const LINE_LIMIT: usize = 32 * 1024 * 1024;

/// A response body read as it arrives. Each read awaits the next network
/// chunk and stops at once when the turn is cancelled.
pub struct ByteStream<'a> {
    transport: Option<&'a ChatTransport>,
    source: Source,
    buffer: Vec<u8>,
    position: usize,
}

impl ByteStream<'static> {
    /// A stream over bytes already in memory (decoder tests).
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            transport: None,
            source: Source::Done,
            buffer: bytes.into(),
            position: 0,
        }
    }
}

impl ByteStream<'_> {
    /// Make sure unread bytes are buffered; `false` at end of stream.
    async fn fill(&mut self) -> io::Result<bool> {
        if self.position < self.buffer.len() {
            return Ok(true);
        }
        let next = match &mut self.source {
            Source::Http(response) => match self.transport {
                Some(transport) => transport
                    .wait(response.chunk())
                    .await
                    .map_err(io::Error::other)?,
                None => response.chunk().await,
            }
            .map_err(|error| io::Error::other(error.without_url()))?,
            #[cfg(test)]
            Source::Done => None,
        };
        match next {
            Some(bytes) => {
                self.buffer = bytes.to_vec();
                self.position = 0;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Append the next line, newline included, to `line`. Returns how many
    /// bytes were read; 0 at end of stream (like `BufRead::read_line`).
    pub(crate) async fn read_line(&mut self, line: &mut String) -> io::Result<usize> {
        let mut bytes = Vec::new();
        while self.fill().await? {
            let unread = &self.buffer[self.position..];
            let (take, done) = match unread.iter().position(|byte| *byte == b'\n') {
                Some(end) => (end + 1, true),
                None => (unread.len(), false),
            };
            bytes.extend_from_slice(&unread[..take]);
            self.position += take;
            if bytes.len() > LINE_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream line exceeds size limit",
                ));
            }
            if done {
                break;
            }
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stream is not UTF-8"))?;
        line.push_str(&text);
        Ok(text.len())
    }

    /// The next line without its line ending (like `BufRead::lines`).
    pub(crate) async fn next_line(&mut self) -> io::Result<Option<String>> {
        let mut line = String::new();
        if self.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(Some(line))
    }

    /// Fill `output` completely. `Ok(false)` if the stream ended before the
    /// first byte; `UnexpectedEof` if it ended part way.
    pub(crate) async fn read_exact(&mut self, output: &mut [u8]) -> io::Result<bool> {
        let mut filled = 0;
        while filled < output.len() {
            if !self.fill().await? {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let unread = &self.buffer[self.position..];
            let take = unread.len().min(output.len() - filled);
            output[filled..filled + take].copy_from_slice(&unread[..take]);
            self.position += take;
            filled += take;
        }
        Ok(true)
    }
}

#[derive(Default)]
pub(crate) struct SseFrame {
    pub event: String,
    pub data: String,
}

pub(crate) async fn next_frame(reader: &mut ByteStream<'_>) -> io::Result<Option<SseFrame>> {
    let mut frame = SseFrame::default();
    let mut line = String::new();
    let mut has_data = false;
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use codingbuddy_core::runtime::block_on;

    #[test]
    fn sse_frames_join_data_lines_and_keep_the_event_name() {
        let mut stream = ByteStream::from_bytes(
            "event: delta\r\ndata: a\ndata: b\n\n: comment\n\ndata: [DONE]\n\n",
        );
        let first = block_on(next_frame(&mut stream)).unwrap().unwrap();
        assert_eq!(
            (first.event.as_str(), first.data.as_str()),
            ("delta", "a\nb")
        );
        let second = block_on(next_frame(&mut stream)).unwrap().unwrap();
        assert_eq!(second.data, "[DONE]");
        assert!(block_on(next_frame(&mut stream)).unwrap().is_none());
    }

    #[test]
    fn a_stream_cut_mid_frame_is_an_error() {
        let mut stream = ByteStream::from_bytes("data: partial\n");
        let error = block_on(next_frame(&mut stream)).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn lines_drop_their_endings() {
        let mut stream = ByteStream::from_bytes("one\r\ntwo\nthree");
        let mut lines = Vec::new();
        while let Some(line) = block_on(stream.next_line()).unwrap() {
            lines.push(line);
        }
        assert_eq!(lines, ["one", "two", "three"]);
    }

    #[test]
    fn exact_reads_distinguish_a_clean_end_from_a_truncated_record() {
        let mut stream = ByteStream::from_bytes(vec![1, 2, 3, 4, 5, 6]);
        let mut four = [0; 4];
        assert!(block_on(stream.read_exact(&mut four)).unwrap());
        assert_eq!(four, [1, 2, 3, 4]);
        let error = block_on(stream.read_exact(&mut four)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(!block_on(stream.read_exact(&mut four)).unwrap());
    }

    #[test]
    fn cancelling_stops_a_stalled_response_body_immediately() {
        // A server that sends headers and then nothing.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut socket, _)) = listener.accept() {
                let mut request = [0; 1024];
                let _ = socket.read(&mut request);
                let _ = socket.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                );
                std::thread::sleep(Duration::from_secs(30));
            }
        });
        let token = CancellationToken::new();
        let transport = ChatTransport::new(Duration::from_secs(60), Some(token.clone())).unwrap();
        let canceller = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let result = block_on(async {
            let response = transport
                .post(
                    &format!("http://{address}/"),
                    Default::default(),
                    &serde_json::json!({}),
                )
                .await??;
            let mut stream = transport.stream(response);
            Ok::<_, anyhow::Error>(stream.next_line().await?)
        });
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
    }
}
