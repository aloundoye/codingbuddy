//! A single initialized transport for discovery and invocation. No call replay.
use super::*;
use codingbuddy_core::{
    CancellationToken,
    execution::{ExecutionContext, OwnedChild},
};
use serde_json::Value;
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub struct Sessions {
    entries: Mutex<BTreeMap<String, Arc<Mutex<Option<Session>>>>>,
    pub states: Mutex<BTreeMap<String, String>>,
}
impl Sessions {
    pub fn invalidated(&self, id: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(id)
            .is_none_or(|slot| match slot.try_lock() {
                Ok(guard) => guard
                    .as_ref()
                    .is_none_or(|session| session.changed.load(Ordering::SeqCst)),
                Err(std::sync::TryLockError::WouldBlock) => false,
                Err(_) => true,
            })
    }

    pub fn shutdown(&self) {
        self.entries.lock().unwrap().clear();
    }
    pub fn request(
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
        let mut guard = loop {
            context.check()?;
            match slot.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(_) => return Err(anyhow!("MCP session unavailable")),
            }
        };
        let fingerprint = fingerprint(server)?;
        if guard
            .as_ref()
            .is_some_and(|session| session.fingerprint != fingerprint)
        {
            *guard = None;
        }
        if guard.is_none() {
            self.states
                .lock()
                .unwrap()
                .insert(server.id.clone(), "connecting".into());
            match Session::connect(server, fingerprint, context) {
                Ok(session) => *guard = Some(session),
                Err(error) => {
                    self.states
                        .lock()
                        .unwrap()
                        .insert(server.id.clone(), "failed".into());
                    return Err(error);
                }
            }
        }
        if method == "tools/call" {
            anyhow::ensure!(
                guard.as_ref().unwrap().capabilities.get("tools").is_some(),
                "MCP tools capability unavailable"
            );
        }
        if method == "tools/call" {
            anyhow::ensure!(
                !guard.as_ref().unwrap().changed.load(Ordering::SeqCst),
                "MCP tool registry changed before dispatch; refresh and approve again"
            );
        }
        let result = guard.as_mut().unwrap().request(method, params, context);
        if result.is_ok() && method == "tools/list" {
            guard
                .as_ref()
                .unwrap()
                .changed
                .store(false, Ordering::SeqCst);
        }
        if result.is_err() {
            *guard = None;
            self.states.lock().unwrap().insert(
                server.id.clone(),
                if context.cancellation.is_cancelled() {
                    "cancelled"
                } else {
                    "failed"
                }
                .into(),
            );
        } else {
            self.states
                .lock()
                .unwrap()
                .insert(server.id.clone(), "ready".into());
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
    next: u64,
    changed: Arc<AtomicBool>,
}
enum Transport {
    Stdio {
        child: OwnedChild,
        input: std::process::ChildStdin,
        rx: mpsc::Receiver<Value>,
        reader: Option<std::thread::JoinHandle<()>>,
    },
    Remote(Box<Remote>),
}
impl Session {
    fn connect(
        server: &McpServer,
        fingerprint: Vec<u8>,
        context: &ExecutionContext,
    ) -> Result<Self> {
        let changed = Arc::new(AtomicBool::new(false));
        let transport = match server.transport {
            McpTransport::Stdio => {
                let mut cmd = stdio_command(server)?;
                cmd.stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                let mut child = OwnedChild::spawn(&mut cmd)?;
                let input = child.stdin.take().context("Missing MCP input")?;
                let output = child.stdout.take().context("Missing MCP output")?;
                let (tx, rx) = mpsc::sync_channel(64);
                let notifications = changed.clone();
                let reader_thread = thread::spawn(move || {
                    let mut reader = BufReader::new(output);
                    while let Ok(Some(value)) = read_mcp_frame(&mut reader) {
                        if value["method"] == "notifications/tools/list_changed" {
                            notifications.store(true, Ordering::SeqCst);
                            continue;
                        }
                        if tx.try_send(value).is_err() {
                            break;
                        }
                    }
                });
                Transport::Stdio {
                    child,
                    input,
                    rx,
                    reader: Some(reader_thread),
                }
            }
            _ => Transport::Remote(Box::new(Remote::new(server, context, changed.clone())?)),
        };
        let mut session = Self {
            capabilities: json!({}),
            fingerprint,
            transport,
            next: 1,
            changed,
        };
        let initialized=session.request("initialize",json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"codingbuddy","version":env!("CARGO_PKG_VERSION")}}),context)?;
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
            remote.version = version.to_owned();
        }
        session.notify("notifications/initialized", json!({}), context)?;
        Ok(session)
    }
    fn notify(&mut self, method: &str, params: Value, context: &ExecutionContext) -> Result<()> {
        let value = json!({"jsonrpc":"2.0","method":method,"params":params});
        match &mut self.transport {
            Transport::Stdio { input, .. } => {
                serde_json::to_writer(&mut *input, &value)?;
                input.write_all(b"\n")?;
                input.flush()?;
                Ok(())
            }
            Transport::Remote(remote) => remote.notify(value, context),
        }
    }
    fn request(
        &mut self,
        method: &str,
        params: Value,
        context: &ExecutionContext,
    ) -> Result<Value> {
        context.check()?;
        context.check()?;
        if let Some((capability, _)) = method.split_once('/')
            && matches!(capability, "tools" | "resources" | "prompts")
        {
            anyhow::ensure!(
                self.capabilities.get(capability).is_some(),
                "MCP method was not negotiated with this server"
            );
        }
        let id = self.next;
        self.next += 1;
        let value = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let result = match &mut self.transport {
            Transport::Stdio { input, rx, .. } => {
                serde_json::to_writer(&mut *input, &value)?;
                input.write_all(b"\n")?;
                input.flush()?;
                loop {
                    if let Err(error) = context.check() {
                        if method != "initialize" {
                            let _ = writeln!(
                                input,
                                "{}",
                                json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})
                            );
                            let _ = input.flush();
                        }
                        break Err(error);
                    }
                    match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(frame) if frame["id"] == id => break Ok(frame),
                        Ok(frame) => {
                            if frame["method"] == "notifications/tools/list_changed" {
                                self.changed.store(true, Ordering::SeqCst);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(_) => break Err(anyhow!("MCP connection closed before response")),
                    }
                }
            }
            Transport::Remote(remote) => remote.request(value, context),
        }?;
        anyhow::ensure!(result["error"].is_null(), "MCP returned a protocol error");
        anyhow::ensure!(
            result.get("result").is_some(),
            "MCP response missing result"
        );
        Ok(result)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Transport::Stdio { child, reader, .. } = &mut self.transport {
            child.terminate();
            let _ = child.wait();
            if let Some(reader) = reader.take() {
                let _ = reader.join();
            }
        }
    }
}
struct Remote {
    runtime: tokio::runtime::Runtime,
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
fn wait<T>(
    runtime: &tokio::runtime::Runtime,
    context: &ExecutionContext,
    future: impl Future<Output = T>,
) -> Result<T> {
    runtime.block_on(async {
        let mut future = std::pin::pin!(future);
        loop {
            context.check()?;
            if let Ok(value) =
                tokio::time::timeout(Duration::from_millis(20), future.as_mut()).await
            {
                return Ok(value);
            }
        }
    })
}
impl Remote {
    fn new(
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
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
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
            let response = wait(&remote.runtime, context, remote.builder(false).send())??
                .error_for_status()?;
            remote.events = Some(response);
            loop {
                let (event, data) = remote.event(context)?;
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
    fn notify(&mut self, value: Value, context: &ExecutionContext) -> Result<()> {
        wait(
            &self.runtime,
            context,
            self.builder(true).json(&value).send(),
        )??
        .error_for_status()?;
        Ok(())
    }
    fn event(&mut self, context: &ExecutionContext) -> Result<(String, String)> {
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
            let bytes = wait(&self.runtime, context, response.chunk())??
                .context("MCP stream closed before response")?;
            self.buffer.extend_from_slice(&bytes);
            anyhow::ensure!(
                self.buffer.len() <= 16 * 1024 * 1024,
                "MCP frame limit exceeded"
            );
        }
    }
    fn request(&mut self, value: Value, context: &ExecutionContext) -> Result<Value> {
        let id = value["id"].clone();
        let result = (|| {
            let response = wait(
                &self.runtime,
                context,
                self.builder(true).json(&value).send(),
            )??
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
                while let Some(chunk) = wait(&self.runtime, context, response.chunk())?? {
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
                let (_, data) = self.event(context)?;
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
        })();
        if result.is_err() && value["method"] != "initialize" && context.cancellation.is_cancelled()
        {
            let cleanup = ExecutionContext {
                deadline: Some(Instant::now() + Duration::from_millis(250)),
                cancellation: CancellationToken::new(),
                ..Default::default()
            };
            let _=self.notify(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}}),&cleanup);
        }
        if !self.legacy {
            self.events = None;
            self.buffer.clear();
        }
        result
    }
}
