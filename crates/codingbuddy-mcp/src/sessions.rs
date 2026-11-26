//! A single initialized transport for discovery and invocation. No call replay.
//!
//! Sessions are shared: a stdio server's reader task routes each response to
//! its request by JSON-RPC id, so several calls to one server run at once.
use super::*;
use codingbuddy_core::{
    CancellationToken,
    execution::{AsyncChild, ExecutionContext},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

type Slot = Arc<tokio::sync::Mutex<Option<Arc<Session>>>>;

#[derive(Default)]
pub struct Sessions {
    entries: Mutex<BTreeMap<String, Slot>>,
    pub states: Mutex<BTreeMap<String, String>>,
}
impl Sessions {
    pub fn invalidated(&self, id: &str) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .is_none_or(|slot| match slot.try_lock() {
                Ok(guard) => guard
                    .as_ref()
                    .is_none_or(|session| session.changed.load(Ordering::SeqCst)),
                // Connecting right now.
                Err(_) => false,
            })
    }

    pub fn shutdown(&self) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn set_state(&self, server: &McpServer, state: &str) {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(server.id.clone(), state.into());
    }

    pub async fn request(
        &self,
        server: &McpServer,
        method: &str,
        params: Value,
        context: &ExecutionContext,
    ) -> Result<Value> {
        ensure_server_enabled(server)?;
        context.check()?;
        let slot = self
            .entries
            .lock()
            .map_err(|_| anyhow!("MCP registry unavailable"))?
            .entry(server.id.clone())
            .or_default()
            .clone();
        let session = {
            // Only connecting is serialised; requests share the session.
            let mut guard = bounded(context, slot.lock()).await?;
            let fingerprint = fingerprint(server)?;
            if guard
                .as_ref()
                .is_some_and(|session| session.fingerprint != fingerprint)
            {
                *guard = None;
            }
            if guard.is_none() {
                self.set_state(server, "connecting");
                match Session::connect(server, fingerprint, context).await {
                    Ok(session) => *guard = Some(Arc::new(session)),
                    Err(error) => {
                        self.set_state(server, "failed");
                        return Err(error);
                    }
                }
            }
            guard
                .clone()
                .ok_or_else(|| anyhow!("MCP session is not connected"))?
        };
        if method == "tools/call" {
            anyhow::ensure!(
                session.capabilities.get("tools").is_some(),
                "MCP tools capability unavailable"
            );
            anyhow::ensure!(
                !session.changed.load(Ordering::SeqCst),
                "MCP tool registry changed before dispatch; refresh and approve again"
            );
        }
        let result = session.request(method, params, context).await;
        if result.is_ok() && method == "tools/list" {
            session.changed.store(false, Ordering::SeqCst);
        }
        if result.is_err() {
            // Evict this session (not a newer one another call made); calls
            // still in flight on it finish first, then it shuts down.
            let mut guard = slot.lock().await;
            if guard
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                *guard = None;
            }
            drop(guard);
            self.set_state(
                server,
                if context.cancellation.is_cancelled() {
                    "cancelled"
                } else {
                    "failed"
                },
            );
        } else {
            self.set_state(server, "ready");
        }
        if result.is_err() && method == "tools/call" {
            return Err(codingbuddy_core::execution::UnconfirmedSideEffect.into());
        }
        result
    }
}
pub fn fingerprint(server: &McpServer) -> Result<Vec<u8>> {
    // Kept internal: neither credentials nor a guessable credential digest is exported.
    Ok(Sha256::digest(serde_json::to_vec(&(
        server.command.as_ref(),
        &server.args,
        &server.url,
        &server.headers,
        &server.metadata,
        &server
            .child_env
            .iter()
            .map(|(k, v)| (k.to_string_lossy(), v.to_string_lossy()))
            .collect::<Vec<_>>(),
    ))?)
    .to_vec())
}
struct Session {
    capabilities: Value,
    fingerprint: Vec<u8>,
    transport: Transport,
    next: AtomicU64,
    changed: Arc<AtomicBool>,
}
enum Transport {
    Stdio(Box<StdioPeer>),
    Remote(Box<tokio::sync::Mutex<Remote>>),
}

type Pending = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>;

/// A stdio server: one writer task, one reader task that routes responses.
struct StdioPeer {
    // Held for its Drop: killing the server's process group.
    _child: AsyncChild,
    writes: tokio::sync::mpsc::Sender<Vec<u8>>,
    pending: Pending,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
}

impl StdioPeer {
    fn start(server: &McpServer, changed: Arc<AtomicBool>) -> Result<Self> {
        use tokio::io::AsyncWriteExt;
        let mut cmd = tokio::process::Command::from(stdio_command(server)?);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = AsyncChild::spawn(&mut cmd)?;
        let mut input = child.stdin.take().context("Missing MCP input")?;
        let output = child.stdout.take().context("Missing MCP output")?;
        let (writes, mut queued) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let writer = tokio::spawn(async move {
            while let Some(line) = queued.recv().await {
                if input.write_all(&line).await.is_err() || input.flush().await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::default();
        let routes = pending.clone();
        let reader = tokio::spawn(async move {
            let mut output = tokio::io::BufReader::new(output);
            while let Ok(Some(frame)) = read_mcp_frame_async(&mut output).await {
                if frame["method"] == "notifications/tools/list_changed" {
                    changed.store(true, Ordering::SeqCst);
                    continue;
                }
                // Server-initiated requests and notifications are not routed.
                if frame.get("method").is_some() {
                    continue;
                }
                let Some(id) = frame.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let waiter = routes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(frame);
                }
            }
            // The server is gone: fail every waiting request.
            routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        });
        Ok(Self {
            _child: child,
            writes,
            pending,
            reader,
            writer,
        })
    }

    async fn send(&self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.writes
            .send(line)
            .await
            .map_err(|_| anyhow!("MCP connection closed"))
    }

    async fn request(&self, id: u64, value: Value, context: &ExecutionContext) -> Result<Value> {
        let (respond, response) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("MCP response routes unavailable"))?
            .insert(id, respond);
        let exchange = async {
            self.send(&value).await?;
            response
                .await
                .map_err(|_| anyhow!("MCP connection closed before response"))
        };
        let result = bounded(context, exchange).await.and_then(|result| result);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if result.is_err() && value["method"] != "initialize" {
            // Best effort; the server may already be gone.
            let cancelled = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}});
            if let Ok(mut line) = serde_json::to_vec(&cancelled) {
                line.push(b'\n');
                let _ = self.writes.try_send(line);
            }
        }
        result
    }
}

impl Drop for StdioPeer {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

impl Session {
    async fn connect(
        server: &McpServer,
        fingerprint: Vec<u8>,
        context: &ExecutionContext,
    ) -> Result<Self> {
        let changed = Arc::new(AtomicBool::new(false));
        let transport = match server.transport {
            McpTransport::Stdio => {
                Transport::Stdio(Box::new(StdioPeer::start(server, changed.clone())?))
            }
            _ => Transport::Remote(Box::new(tokio::sync::Mutex::new(
                Remote::new(server, context, changed.clone()).await?,
            ))),
        };
        let mut session = Self {
            capabilities: json!({}),
            fingerprint,
            transport,
            next: AtomicU64::new(1),
            changed,
        };
        let initialized = session
            .request(
                "initialize",
                json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"codingbuddy","version":env!("CARGO_PKG_VERSION")}}),
                context,
            )
            .await?;
        let version = initialized
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .context("MCP initialization did not negotiate a version")?;
        anyhow::ensure!(
            matches!(version, "2025-06-18" | "2025-03-26" | "2024-11-05"),
            "Unsupported MCP version"
        );
        session.capabilities = initialized
            .pointer("/result/capabilities")
            .filter(|value| value.is_object())
            .cloned()
            .context("MCP initialization lacks server capabilities")?;
        if let Transport::Remote(remote) = &mut session.transport {
            remote.get_mut().version = version.to_owned();
        }
        session
            .notify("notifications/initialized", json!({}), context)
            .await?;
        Ok(session)
    }
    async fn notify(&self, method: &str, params: Value, context: &ExecutionContext) -> Result<()> {
        let value = json!({"jsonrpc":"2.0","method":method,"params":params});
        match &self.transport {
            Transport::Stdio(peer) => bounded(context, peer.send(&value)).await?,
            Transport::Remote(remote) => remote.lock().await.notify(value, context).await,
        }
    }
    async fn request(
        &self,
        method: &str,
        params: Value,
        context: &ExecutionContext,
    ) -> Result<Value> {
        context.check()?;
        if let Some((capability, _)) = method.split_once('/')
            && matches!(capability, "tools" | "resources" | "prompts")
        {
            anyhow::ensure!(
                self.capabilities.get(capability).is_some(),
                "MCP method was not negotiated with this server"
            );
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let value = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let result = match &self.transport {
            Transport::Stdio(peer) => peer.request(id, value, context).await,
            Transport::Remote(remote) => {
                // HTTP sessions keep per-connection state (session id, event
                // stream); their requests take turns.
                let mut remote = bounded(context, remote.lock()).await?;
                remote.request(value, context).await
            }
        }?;
        anyhow::ensure!(result["error"].is_null(), "MCP returned a protocol error");
        anyhow::ensure!(
            result.get("result").is_some(),
            "MCP response missing result"
        );
        Ok(result)
    }
}
struct Remote {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: Option<String>,
    version: String,
    legacy: bool,
    events: Option<reqwest::Response>,
    buffer: Vec<u8>,
    changed: Arc<AtomicBool>,
}
/// Await `future`, stopping the moment the context is cancelled or its
/// deadline passes. The error is whatever `context.check()` reports, so
/// callers see the same messages as before.
async fn bounded<T>(context: &ExecutionContext, future: impl Future<Output = T>) -> Result<T> {
    context.check()?;
    let deadline = async {
        match context.deadline {
            Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = context.cancellation.cancelled() => {}
        () = deadline => {}
        value = future => return Ok(value),
    }
    context.check()?;
    Err(anyhow!("MCP request interrupted"))
}
impl Remote {
    async fn new(
        server: &McpServer,
        context: &ExecutionContext,
        changed: Arc<AtomicBool>,
    ) -> Result<Self> {
        let url = server
            .url
            .as_deref()
            .context("Remote MCP URL unavailable")?;
        let parsed = reqwest::Url::parse(url)?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https")
                && parsed.username().is_empty()
                && parsed.password().is_none(),
            "Invalid MCP endpoint"
        );
        let mut remote = Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            url: url.into(),
            headers: server.headers.clone(),
            session: None,
            version: String::new(),
            legacy: server.transport == McpTransport::Sse,
            events: None,
            buffer: Vec::new(),
            changed,
        };
        if remote.legacy {
            let response = bounded(context, remote.builder(false).send())
                .await??
                .error_for_status()?;
            remote.events = Some(response);
            loop {
                let (event, data) = remote.event(context).await?;
                if event == "endpoint" {
                    let endpoint = parsed.join(&data)?;
                    anyhow::ensure!(
                        endpoint.origin() == parsed.origin(),
                        "Cross-origin MCP endpoint rejected"
                    );
                    remote.url = endpoint.into();
                    break;
                }
            }
        }
        Ok(remote)
    }
    fn builder(&self, post: bool) -> reqwest::RequestBuilder {
        let mut request = if post {
            self.client.post(&self.url)
        } else {
            self.client.get(&self.url)
        };
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(session) = &self.session {
            request = request.header("Mcp-Session-Id", session);
        }
        if !self.version.is_empty() {
            request = request.header("MCP-Protocol-Version", &self.version);
        }
        request.header("Accept", "application/json, text/event-stream")
    }
    async fn notify(&mut self, value: Value, context: &ExecutionContext) -> Result<()> {
        bounded(context, self.builder(true).json(&value).send())
            .await??
            .error_for_status()?;
        Ok(())
    }
    async fn event(&mut self, context: &ExecutionContext) -> Result<(String, String)> {
        loop {
            if let Some((end, separator)) = self
                .buffer
                .windows(2)
                .position(|b| b == b"\n\n")
                .map(|p| (p, 2))
                .or_else(|| {
                    self.buffer
                        .windows(4)
                        .position(|b| b == b"\r\n\r\n")
                        .map(|p| (p, 4))
                })
            {
                let frame =
                    String::from_utf8(self.buffer[..end].to_vec()).context("Invalid MCP UTF-8")?;
                self.buffer.drain(..end + separator);
                let event = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("event:"))
                    .unwrap_or("message")
                    .trim()
                    .to_owned();
                let data = frame
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(|line| line.strip_prefix(' ').unwrap_or(line))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !data.is_empty() {
                    return Ok((event, data));
                }
                continue;
            }
            let response = self.events.as_mut().context("MCP event stream missing")?;
            let bytes = bounded(context, response.chunk())
                .await??
                .context("MCP stream closed before response")?;
            self.buffer.extend_from_slice(&bytes);
            anyhow::ensure!(
                self.buffer.len() <= 16 * 1024 * 1024,
                "MCP frame limit exceeded"
            );
        }
    }
    async fn request(&mut self, value: Value, context: &ExecutionContext) -> Result<Value> {
        let id = value["id"].clone();
        let result = async {
            let response = bounded(context, self.builder(true).json(&value).send())
                .await??
                .error_for_status()?;
            if let Some(session) = response.headers().get("Mcp-Session-Id") {
                self.session = Some(session.to_str()?.to_owned());
            }
            let is_events = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"));
            if !self.legacy && !is_events {
                let mut response = response;
                let mut bytes = Vec::new();
                while let Some(chunk) = bounded(context, response.chunk()).await?? {
                    anyhow::ensure!(
                        bytes.len() + chunk.len() <= 16 * 1024 * 1024,
                        "MCP response limit exceeded"
                    );
                    bytes.extend_from_slice(&chunk);
                }
                anyhow::ensure!(
                    bytes.len() <= 16 * 1024 * 1024,
                    "MCP response limit exceeded"
                );
                let result: Value = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(result["id"] == id, "MCP response ID mismatch");
                return Ok(result);
            }
            if !self.legacy {
                self.events = Some(response);
                self.buffer.clear();
            }
            loop {
                let (_, data) = self.event(context).await?;
                let result: Value = serde_json::from_str(&data)?;
                if result["method"] == "notifications/tools/list_changed" {
                    self.changed.store(true, Ordering::SeqCst);
                    continue;
                }
                if result.get("id").is_some() && result["id"] != id {
                    continue;
                }
                if result["id"] == id {
                    return Ok(result);
                }
            }
        }
        .await;
        if result.is_err() && value["method"] != "initialize" && context.cancellation.is_cancelled()
        {
            let cleanup = ExecutionContext {
                deadline: Some(Instant::now() + Duration::from_millis(250)),
                cancellation: CancellationToken::new(),
                ..Default::default()
            };
            let _ = self
                .notify(
                    json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}}),
                    &cleanup,
                )
                .await;
        }
        if !self.legacy {
            self.events = None;
            self.buffer.clear();
        }
        result
    }
}

#[cfg(test)]
mod stdio_fixture {
    use super::*;
    use std::io::{BufRead, Write};

    /// A stdio MCP server that answers `tools/call` after 400 ms, from its
    /// own thread per request, so replies can arrive out of order.
    #[test]
    fn server() {
        if std::env::var_os("CODINGBUDDY_MCP_FIXTURE").is_none() {
            return;
        }
        let stdout = Arc::new(Mutex::new(std::io::stdout()));
        let reply = |stdout: &Mutex<std::io::Stdout>, value: Value| {
            let mut out = stdout.lock().unwrap();
            writeln!(out, "{value}").unwrap();
            out.flush().unwrap();
        };
        for line in std::io::stdin().lock().lines() {
            let Ok(request) = serde_json::from_str::<Value>(&line.unwrap()) else {
                continue;
            };
            let id = request["id"].clone();
            match request["method"].as_str() {
                Some("initialize") => reply(
                    &stdout,
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}),
                ),
                Some("ping") => reply(&stdout, json!({"jsonrpc":"2.0","id":id,"result":{}})),
                Some("tools/call") => {
                    let stdout = stdout.clone();
                    let text = request["params"]["arguments"]["value"].clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(400));
                        reply(
                            &stdout,
                            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":text}]}}),
                        );
                    });
                }
                _ => {}
            }
        }
    }

    fn fixture_server() -> Result<McpServer> {
        let mut child_env = codingbuddy_core::process_env::variables(
            Path::new("."),
            "",
            codingbuddy_core::process_env::Source::User,
        );
        child_env.insert("CODINGBUDDY_MCP_FIXTURE".into(), "1".into());
        Ok(McpServer {
            id: "fixture".into(),
            name: "fixture".into(),
            transport: McpTransport::Stdio,
            command: Some(std::env::current_exe()?.to_string_lossy().into_owned()),
            args: [
                "--exact",
                "sessions::stdio_fixture::server",
                "--nocapture",
                "--quiet",
            ]
            .map(String::from)
            .to_vec(),
            child_env,
            ..Default::default()
        })
    }

    fn call(value: &str) -> Value {
        json!({"name":"echo","arguments":{"value":value}})
    }

    #[test]
    fn calls_to_one_stdio_server_run_concurrently() -> Result<()> {
        codingbuddy_core::runtime::block_on(async {
            let sessions = Sessions::default();
            let server = fixture_server()?;
            let context = ExecutionContext {
                deadline: Some(Instant::now() + Duration::from_secs(10)),
                ..Default::default()
            };
            // Connect first so the timing below measures only the calls.
            sessions
                .request(&server, "ping", json!({}), &context)
                .await?;
            let started = Instant::now();
            let (a, b) = tokio::join!(
                sessions.request(&server, "tools/call", call("a"), &context),
                sessions.request(&server, "tools/call", call("b"), &context),
            );
            assert_eq!(a?["result"]["content"][0]["text"], "a");
            assert_eq!(b?["result"]["content"][0]["text"], "b");
            // Two 400 ms calls in sequence would take 800 ms.
            assert!(
                started.elapsed() < Duration::from_millis(700),
                "calls were serialised: {:?}",
                started.elapsed()
            );
            Ok(())
        })
    }

    #[test]
    fn cancelling_a_stdio_call_returns_at_once_and_the_next_call_reconnects() -> Result<()> {
        codingbuddy_core::runtime::block_on(async {
            let sessions = Sessions::default();
            let server = fixture_server()?;
            let token = CancellationToken::new();
            let context = ExecutionContext {
                cancellation: token.clone(),
                deadline: Some(Instant::now() + Duration::from_secs(10)),
                ..Default::default()
            };
            let canceller = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                token.cancel();
            });
            let started = Instant::now();
            let error = sessions
                .request(&server, "tools/call", call("slow"), &context)
                .await
                .unwrap_err();
            canceller.await?;
            assert!(started.elapsed() < Duration::from_millis(350));
            assert!(
                error
                    .downcast_ref::<codingbuddy_core::execution::UnconfirmedSideEffect>()
                    .is_some()
            );
            let fresh = ExecutionContext {
                deadline: Some(Instant::now() + Duration::from_secs(10)),
                ..Default::default()
            };
            let result = sessions
                .request(&server, "tools/call", call("again"), &fresh)
                .await?;
            assert_eq!(result["result"]["content"][0]["text"], "again");
            Ok(())
        })
    }
}
