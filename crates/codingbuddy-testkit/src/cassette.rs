//! Record real provider HTTP traffic once, replay it in tests forever.
//!
//! Hand-written fixtures describe what a protocol *allows*; cassettes capture
//! what a provider *actually sends*: how it splits text across stream frames,
//! where it puts a function call relative to text, which usage fields it
//! fills. Bugs in that gap (for example streamed text being discarded when a
//! tool call follows it) only show up against real traffic.
//!
//! * [`RecordingProxy`] is a plain-HTTP proxy in front of the real provider.
//!   Point a provider profile's `base_url` at it; it forwards each request
//!   upstream and streams the response back while saving every chunk with
//!   its timing. API keys never reach the cassette: request headers are not
//!   stored and the `key` query parameter is removed.
//! * [`ReplayServer`] serves a cassette's responses in order, one recorded
//!   chunk per HTTP chunk, so the client sees the same frame boundaries.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const CASSETTE_SCHEMA_VERSION: u32 = 1;
/// Replay compresses recorded gaps so a slow provider does not slow CI, but
/// keeps a small pause so chunks still arrive as separate reads.
const MAX_REPLAY_GAP: Duration = Duration::from_millis(15);
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// One recorded scenario: the prompt, the workspace it ran in, every HTTP
/// exchange with the provider, and what the engine produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cassette {
    pub schema_version: u32,
    /// `recorded` for real traffic, `synthetic` for hand-written stand-ins.
    pub source: String,
    /// Provider id as used in settings (`google`, `deepseek`, `anthropic`, …).
    pub provider: String,
    pub model: String,
    pub scenario: String,
    pub recorded_at: String,
    pub prompt: String,
    /// Files created in the temporary workspace before the run.
    #[serde(default)]
    pub workspace: BTreeMap<String, String>,
    /// Whether the client streamed; replay uses the same setting.
    pub stream: bool,
    pub interactions: Vec<Interaction>,
    pub expect: Expectations,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Expectations {
    /// Tool names the engine executed, in order.
    pub tool_calls: Vec<String>,
    /// The engine's final answer.
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interaction {
    pub request: RecordedRequest,
    pub response: RecordedResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub method: String,
    /// Path and query, with secret query parameters removed.
    pub path: String,
    /// The JSON body, kept for debugging a mismatch. Not used for matching.
    #[serde(default)]
    pub body: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    pub content_type: String,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Time since the previous chunk (or since the request was sent).
    pub delay_ms: u64,
    /// Chunk bytes as text. Provider responses are UTF-8 JSON/SSE; a chunk
    /// boundary can still split a multi-byte character, so see `bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64-free fallback for chunks that are not valid UTF-8 on their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
}

impl Chunk {
    fn new(delay: Duration, data: &[u8]) -> Self {
        let delay_ms = delay.as_millis() as u64;
        match std::str::from_utf8(data) {
            Ok(text) => Self {
                delay_ms,
                text: Some(text.to_string()),
                bytes: None,
            },
            Err(_) => Self {
                delay_ms,
                text: None,
                bytes: Some(data.to_vec()),
            },
        }
    }

    pub fn data(&self) -> &[u8] {
        match (&self.text, &self.bytes) {
            (Some(text), _) => text.as_bytes(),
            (None, Some(bytes)) => bytes,
            (None, None) => &[],
        }
    }
}

impl Cassette {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("could not read cassette {}", path.display()))?;
        let cassette: Self = serde_json::from_str(&raw)
            .with_context(|| format!("invalid cassette {}", path.display()))?;
        ensure!(
            cassette.schema_version == CASSETTE_SCHEMA_VERSION,
            "cassette {} has schema {}, expected {CASSETTE_SCHEMA_VERSION}; re-record it",
            path.display(),
            cassette.schema_version
        );
        Ok(cassette)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        std::fs::write(path, json).with_context(|| format!("could not write {}", path.display()))
    }

    /// The whole body of one response, chunks joined.
    pub fn response_body(&self, index: usize) -> Vec<u8> {
        self.interactions
            .get(index)
            .map(|interaction| {
                interaction
                    .response
                    .chunks
                    .iter()
                    .flat_map(|chunk| chunk.data().iter().copied())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Remove query parameters that carry credentials (Gemini uses `?key=`).
pub fn redact_path(path: &str) -> String {
    let Some((base, query)) = path.split_once('?') else {
        return path.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let name = pair
                .split('=')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            !matches!(
                name.as_str(),
                "key" | "api_key" | "apikey" | "access_token" | "token" | "sig" | "signature"
            )
        })
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

struct IncomingRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<IncomingRequest> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    ensure!(
        reader.read_line(&mut request_line)? > 0,
        "connection closed before the request line"
    );
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing method")?.to_string();
    let path = parts.next().context("missing path")?.to_string();

    let mut headers = Vec::new();
    let mut header_bytes = request_line.len();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        ensure!(
            reader.read_line(&mut line)? > 0,
            "truncated request headers"
        );
        header_bytes += line.len();
        ensure!(
            header_bytes <= MAX_HEADER_BYTES,
            "request headers too large"
        );
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_string(), value.trim().to_string());
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().context("bad content-length")?;
            }
            headers.push((name, value));
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    Ok(IncomingRequest {
        method,
        path,
        headers,
        body,
    })
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn write_head(stream: &mut TcpStream, status: u16, content_type: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        reason_phrase(status)
    )?;
    stream.flush()?;
    Ok(())
}

fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(stream, "{:x}\r\n", data.len())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()?;
    Ok(())
}

fn finish_chunks(stream: &mut TcpStream) -> Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()?;
    Ok(())
}

fn body_json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

/// Accept loop shared by both servers. `handle` returns `Ok(false)` to stop.
fn serve(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    mut handle: impl FnMut(TcpStream) -> Result<bool> + Send + 'static,
) -> JoinHandle<Result<()>> {
    std::thread::spawn(move || -> Result<()> {
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
                    if !handle(stream)? {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })
}

/// Serves a cassette's responses in order.
pub struct ReplayServer {
    base: String,
    served: Arc<Mutex<Vec<RecordedRequest>>>,
    errors: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl ReplayServer {
    pub fn start(cassette: &Cassette) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let base = format!("http://{}", listener.local_addr()?);
        let served = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut remaining = cassette.interactions.clone().into_iter();
        let (served_log, error_log) = (served.clone(), errors.clone());
        let worker =
            serve(listener, stop.clone(), move |mut stream| {
                let request = read_request(&mut stream)?;
                let path = redact_path(&request.path);
                let Some(interaction) = remaining.next() else {
                    error_log.lock().map_err(|_| anyhow!("poisoned"))?.push(format!(
                    "unexpected extra request {} {path}: the cassette has no more responses",
                    request.method
                ));
                    write_head(&mut stream, 500, "text/plain")?;
                    write_chunk(&mut stream, b"cassette exhausted")?;
                    finish_chunks(&mut stream)?;
                    return Ok(true);
                };
                if interaction.request.method != request.method || interaction.request.path != path
                {
                    error_log
                        .lock()
                        .map_err(|_| anyhow!("poisoned"))?
                        .push(format!(
                            "request {} {path} does not match recorded {} {}",
                            request.method, interaction.request.method, interaction.request.path
                        ));
                }
                served_log
                    .lock()
                    .map_err(|_| anyhow!("poisoned"))?
                    .push(RecordedRequest {
                        method: request.method,
                        path,
                        body: body_json(&request.body),
                    });
                let response = &interaction.response;
                write_head(&mut stream, response.status, &response.content_type)?;
                for chunk in &response.chunks {
                    std::thread::sleep(Duration::from_millis(chunk.delay_ms).min(MAX_REPLAY_GAP));
                    // The client may hang up early (e.g. cancellation); that is
                    // the client's behaviour under test, not a server error.
                    if write_chunk(&mut stream, chunk.data()).is_err() {
                        return Ok(true);
                    }
                }
                let _ = finish_chunks(&mut stream);
                Ok(true)
            });
        Ok(Self {
            base,
            served,
            errors,
            stop,
            worker: Some(worker),
        })
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Requests served so far, in order.
    pub fn served(&self) -> Vec<RecordedRequest> {
        self.served
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }

    /// Mismatches and extra requests seen so far.
    pub fn errors(&self) -> Vec<String> {
        self.errors
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }
}

impl Drop for ReplayServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Plain-HTTP proxy that forwards to a real provider and records the traffic.
pub struct RecordingProxy {
    base: String,
    interactions: Arc<Mutex<Vec<Interaction>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
}

/// Headers that describe this hop rather than the request itself.
fn is_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "connection"
            | "accept-encoding"
            | "transfer-encoding"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "upgrade"
    )
}

impl RecordingProxy {
    /// `upstream` is the provider origin, e.g. `https://api.deepseek.com`.
    /// The request path is appended unchanged.
    pub fn start(upstream: &str) -> Result<Self> {
        let upstream = upstream.trim_end_matches('/').to_string();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let base = format!("http://{}", listener.local_addr()?);
        let interactions = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let log = interactions.clone();
        let worker = serve(listener, stop.clone(), move |mut stream| {
            let request = read_request(&mut stream)?;
            let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
            let mut builder = client
                .request(method, format!("{upstream}{}", request.path))
                // Record readable bytes, not a compressed stream.
                .header("accept-encoding", "identity")
                .body(request.body.clone());
            for (name, value) in &request.headers {
                if !is_hop_header(name) {
                    builder = builder.header(name, value);
                }
            }
            let mut last = Instant::now();
            let mut upstream_response = builder.send().context("upstream request failed")?;
            let status = upstream_response.status().as_u16();
            let content_type = upstream_response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            write_head(&mut stream, status, &content_type)?;
            let mut chunks = Vec::new();
            let mut buffer = vec![0u8; 16 * 1024];
            loop {
                let read = upstream_response.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                chunks.push(Chunk::new(last.elapsed(), &buffer[..read]));
                last = Instant::now();
                write_chunk(&mut stream, &buffer[..read])?;
            }
            finish_chunks(&mut stream)?;
            log.lock()
                .map_err(|_| anyhow!("poisoned"))?
                .push(Interaction {
                    request: RecordedRequest {
                        method: request.method,
                        path: redact_path(&request.path),
                        body: body_json(&request.body),
                    },
                    response: RecordedResponse {
                        status,
                        content_type,
                        chunks,
                    },
                });
            Ok(true)
        });
        Ok(Self {
            base,
            interactions,
            stop,
            worker: Some(worker),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Stop the proxy and return everything it recorded.
    pub fn finish(mut self) -> Result<Vec<Interaction>> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow!("recording proxy panicked"))??;
        }
        let interactions = self
            .interactions
            .lock()
            .map_err(|_| anyhow!("poisoned"))?
            .clone();
        if interactions.is_empty() {
            bail!("no requests went through the recording proxy");
        }
        Ok(interactions)
    }
}

impl Drop for RecordingProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_get_chunks(base: &str, path: &str) -> Result<(u16, Vec<u8>)> {
        let response = reqwest::blocking::Client::new()
            .post(format!("{base}{path}"))
            .body("{}")
            .send()?;
        let status = response.status().as_u16();
        Ok((status, response.bytes()?.to_vec()))
    }

    fn cassette(chunks: &[&str]) -> Cassette {
        Cassette {
            schema_version: CASSETTE_SCHEMA_VERSION,
            source: "synthetic".into(),
            provider: "test".into(),
            model: "m".into(),
            scenario: "s".into(),
            recorded_at: "2026-01-01T00:00:00Z".into(),
            prompt: "p".into(),
            workspace: BTreeMap::new(),
            stream: true,
            interactions: vec![Interaction {
                request: RecordedRequest {
                    method: "POST".into(),
                    path: "/v1/x".into(),
                    body: Value::Null,
                },
                response: RecordedResponse {
                    status: 200,
                    content_type: "text/event-stream".into(),
                    chunks: chunks
                        .iter()
                        .map(|text| Chunk::new(Duration::from_millis(1), text.as_bytes()))
                        .collect(),
                },
            }],
            expect: Expectations::default(),
        }
    }

    #[test]
    fn redact_path_strips_credentials_only() {
        assert_eq!(
            redact_path("/v1/m:gen?alt=sse&key=SECRET"),
            "/v1/m:gen?alt=sse"
        );
        assert_eq!(redact_path("/v1/m?key=SECRET"), "/v1/m");
        assert_eq!(redact_path("/v1/m"), "/v1/m");
    }

    #[test]
    fn replay_serves_chunks_in_order_then_reports_extra_requests() -> Result<()> {
        let cassette = cassette(&["data: one\n\n", "data: two\n\n"]);
        let server = ReplayServer::start(&cassette)?;
        let (status, body) = http_get_chunks(server.base(), "/v1/x?key=abc")?;
        assert_eq!(status, 200);
        assert_eq!(body, cassette.response_body(0));
        assert_eq!(server.served()[0].path, "/v1/x");
        assert!(server.errors().is_empty());

        let (status, _) = http_get_chunks(server.base(), "/v1/x")?;
        assert_eq!(status, 500);
        assert!(server.errors()[0].contains("unexpected extra request"));
        Ok(())
    }

    #[test]
    fn recording_proxy_captures_upstream_chunks_without_credentials() -> Result<()> {
        // A replay server stands in for the real provider.
        let upstream = ReplayServer::start(&cassette(&["data: a\n\n", "data: b\n\n"]))?;
        let proxy = RecordingProxy::start(upstream.base())?;
        let (status, body) = http_get_chunks(proxy.base(), "/v1/x?key=SECRET")?;
        assert_eq!(status, 200);
        assert_eq!(body, b"data: a\n\ndata: b\n\n");
        let recorded = proxy.finish()?;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].request.path, "/v1/x");
        let joined: Vec<u8> = recorded[0]
            .response
            .chunks
            .iter()
            .flat_map(|chunk| chunk.data().iter().copied())
            .collect();
        assert_eq!(joined, b"data: a\n\ndata: b\n\n");
        assert!(!serde_json::to_string(&recorded)?.contains("SECRET"));
        Ok(())
    }

    #[test]
    fn chunks_that_split_utf8_round_trip_as_bytes() -> Result<()> {
        let text = "é";
        let chunk = Chunk::new(Duration::ZERO, &text.as_bytes()[..1]);
        assert!(chunk.text.is_none());
        let json = serde_json::to_string(&chunk)?;
        let back: Chunk = serde_json::from_str(&json)?;
        assert_eq!(back.data(), &text.as_bytes()[..1]);
        Ok(())
    }
}
