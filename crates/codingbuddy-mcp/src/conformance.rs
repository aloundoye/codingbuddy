//! Real initialized HTTP fixtures, including fragmented SSE and cancellation.
use super::*;
use codingbuddy_core::{CancellationToken, ToolExecutionContext};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
struct Fixture {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}
impl Fixture {
    fn start(sse: bool) -> Self {
        Self::start_transport(sse, false)
    }
    fn start_transport(sse: bool, legacy: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let events = Arc::new(Mutex::new(None));
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let observed = observed.clone();
                        let stop = stopping.clone();
                        let events = events.clone();
                        clients.push(thread::spawn(move || {
                            if let Err(error) = serve(
                                stream,
                                sse,
                                &stop,
                                &observed,
                                legacy.then_some(events.as_ref()),
                            ) {
                                eprintln!("MCP fixture connection: {error:#}");
                            }
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
            requests,
        }
    }
    fn count(&self, method: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == method)
            .count()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn serve(
    mut stream: TcpStream,
    sse: bool,
    stop: &AtomicBool,
    requests: &Mutex<Vec<serde_json::Value>>,
    events: Option<&Mutex<Option<TcpStream>>>,
) -> Result<()> {
    // macOS and Windows may inherit the listener's nonblocking mode.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut header = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        anyhow::ensure!(reader.read_line(&mut line)? > 0, "Request closed");
        header.push_str(&line);
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>()?;
        }
    }
    assert!(
        header
            .lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer synthetic-mcp-key")),
        "Authentication was not consistently applied"
    );
    if header.starts_with("GET ") {
        anyhow::ensure!(events.is_some(), "Unexpected event channel");
        let mut channel = events.unwrap().lock().unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: endpoint\r\ndata: /messages\r\n\r\n")?;
        stream.flush()?;
        *channel = Some(stream);
        return Ok(());
    }
    anyhow::ensure!(length <= 1_048_576, "Fixture request exceeded limit");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let request: serde_json::Value = serde_json::from_slice(&bytes)?;
    let method = request["method"].as_str().unwrap().to_owned();
    if method != "initialize" && events.is_none() {
        assert!(
            header
                .lines()
                .any(|line| line.eq_ignore_ascii_case("mcp-session-id: fixture-session")),
            "Session ID was lost"
        );
    }
    requests.lock().unwrap().push(request.clone());
    if request.get("id").is_none() {
        stream.write_all(
            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    if method == "tools/call" && request["params"]["arguments"]["value"] == "stall" {
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n{")?;
        stream.flush()?;
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(10));
        }
        return Ok(());
    }
    let result = match method.as_str() {
        "initialize" => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{},"resources":{},"prompts":{}},"serverInfo":{"name":"fixture","version":"1"}})
        }
        "tools/list" => {
            json!({"tools":[{"name":"café","description":"fixture","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}}]})
        }
        "tools/call" => {
            json!({"content":[{"type":"text","text":request["params"]["arguments"]["value"]}]})
        }
        "resources/list" => json!({"resources":[{"uri":"fixture://resource","name":"resource"}]}),
        "resources/read" => {
            json!({"contents":[{"uri":"fixture://resource","text":"resource-content"}]})
        }
        "prompts/list" => {
            json!({"prompts":[{"name":"fixture-prompt","description":"fixture","arguments":[]}]})
        }
        "prompts/get" => {
            json!({"messages":[{"role":"user","content":{"type":"text","text":"prompt-content"}}]})
        }
        _ => anyhow::bail!("Unexpected fixture method {method}"),
    };
    let response = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
    if let Some(events) = events {
        stream.write_all(
            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        let mut events = events.lock().unwrap();
        let channel = events.as_mut().context("Event channel missing")?;
        let data = format!("event: message\r\ndata: {response}\r\n\r\n");
        for byte in data.as_bytes() {
            channel.write_all(&[*byte])?;
        }
        channel.flush()?;
    } else if sse && method == "tools/list" {
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: fixture-session\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
        let data = format!("event: message\r\ndata: {response}\r\n\r\n");
        for byte in data.as_bytes() {
            write!(stream, "1\r\n")?;
            stream.write_all(&[*byte])?;
            stream.write_all(b"\r\n")?;
            stream.flush()?;
        }
        stream.write_all(b"0\r\n\r\n")?;
    } else {
        let body = response.to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
    }
    Ok(())
}
fn manager(workspace: &Path, fixture: &Fixture) -> Result<McpManager> {
    let manager = McpManager::with_trust(workspace, true)?;
    manager.add_server(McpServer {
        id: "fixture".into(),
        name: "fixture".into(),
        transport: McpTransport::Http,
        url: Some(fixture.url.clone()),
        headers: vec![("Authorization".into(), "Bearer synthetic-mcp-key".into())],
        ..Default::default()
    })?;
    Ok(manager)
}
#[test]
fn delivery_three_http_reuses_initialized_authenticated_session_and_fragmented_sse() -> Result<()> {
    for sse in [false, true] {
        let workspace = tempfile::tempdir()?;
        let fixture = Fixture::start(sse);
        let manager = manager(workspace.path(), &fixture)?;
        let tools = codingbuddy_core::runtime::block_on(manager.discover_tools())?;
        if tools.len() != 1 {
            let server = manager.get_server("fixture")?.unwrap();
            let diagnostic = codingbuddy_core::runtime::block_on(manager.request_server(
                &server,
                "tools/list",
                json!({}),
                None,
            ));
            anyhow::bail!("Discovery failed (SSE={sse}): {diagnostic:?}");
        }
        assert_eq!(tools[0].name, "café");
        let alias = "mcp__fixture__café";
        assert!(
            codingbuddy_core::runtime::block_on(manager.call_tool(
                alias,
                &json!({"value":12}),
                Default::default()
            ))
            .is_err()
        );
        assert_eq!(fixture.count("tools/call"), 0);
        let output = codingbuddy_core::runtime::block_on(manager.call_tool(
            alias,
            &json!({"value":"direct-arguments"}),
            Default::default(),
        ))?;
        assert_eq!(output["content"][0]["text"], "direct-arguments");
        assert_eq!(
            codingbuddy_core::runtime::block_on(manager.list_resources())?.len(),
            1
        );
        assert_eq!(
            codingbuddy_core::runtime::block_on(
                manager.resolve_resource("@fixture:fixture://resource")
            )?,
            "resource-content"
        );
        assert_eq!(
            codingbuddy_core::runtime::block_on(manager.list_prompts())?.len(),
            1
        );
        assert_eq!(
            codingbuddy_core::runtime::block_on(manager.get_prompt(
                "fixture",
                "fixture-prompt",
                &json!({})
            ))?["messages"][0]["content"]["text"],
            "prompt-content"
        );
        assert_eq!(fixture.count("initialize"), 1);
        assert_eq!(fixture.count("notifications/initialized"), 1);
        assert_eq!(fixture.count("tools/call"), 1);
    }
    Ok(())
}
#[test]
fn delivery_three_remote_cancel_evicts_session_and_marks_unconfirmed_effect() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let fixture = Fixture::start(false);
    let manager = manager(workspace.path(), &fixture)?;
    codingbuddy_core::runtime::block_on(manager.discover_tools())?;
    let token = CancellationToken::new();
    let cancellation = token.clone();
    let seen = fixture.requests.clone();
    let cancel = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && !seen
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["method"] == "tools/call")
        {
            thread::sleep(Duration::from_millis(5));
        }
        cancellation.cancel();
    });
    let started = Instant::now();
    let error = codingbuddy_core::runtime::block_on(manager.call_tool(
        "mcp__fixture__café",
        &json!({"value":"stall"}),
        ToolExecutionContext {
            cancellation: Some(&token),
            ..Default::default()
        },
    ))
    .unwrap_err();
    cancel.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        error
            .downcast_ref::<codingbuddy_core::execution::UnconfirmedSideEffect>()
            .is_some()
    );
    assert_eq!(fixture.count("tools/call"), 1);
    assert!(
        codingbuddy_core::runtime::block_on(manager.call_tool(
            "mcp__fixture__café",
            &json!({"value":"again"}),
            Default::default()
        ))
        .is_err()
    );
    assert_eq!(fixture.count("tools/call"), 1);
    Ok(())
}

#[test]
fn delivery_three_explicit_legacy_sse_initializes_once_and_calls_on_event_channel() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let fixture = Fixture::start_transport(true, true);
    let manager = McpManager::with_trust(workspace.path(), true)?;
    manager.add_server(McpServer {
        id: "legacy".into(),
        name: "legacy".into(),
        transport: McpTransport::Sse,
        url: Some(fixture.url.clone()),
        headers: vec![("Authorization".into(), "Bearer synthetic-mcp-key".into())],
        ..Default::default()
    })?;
    assert_eq!(
        codingbuddy_core::runtime::block_on(manager.discover_tools())?.len(),
        1
    );
    let result = codingbuddy_core::runtime::block_on(manager.call_tool(
        "mcp__legacy__café",
        &json!({"value":"legacy-result"}),
        Default::default(),
    ))?;
    assert_eq!(result["content"][0]["text"], "legacy-result");
    assert_eq!(fixture.count("initialize"), 1);
    assert_eq!(fixture.count("notifications/initialized"), 1);
    assert_eq!(fixture.count("tools/call"), 1);
    Ok(())
}

#[test]
fn delivery_three_oauth_callback_waits_for_request_on_accepted_socket() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let sender = thread::spawn(move || -> std::io::Result<()> {
        let mut stream = TcpStream::connect(address)?;
        thread::sleep(Duration::from_millis(50));
        stream.write_all(b"GET /callback?code=synthetic-code&state=synthetic-state HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response)?;
        assert!(String::from_utf8_lossy(&response).contains("200 OK"));
        Ok(())
    });
    let code = wait_for_oauth_callback(&listener, "synthetic-state", Duration::from_secs(2))?;
    assert_eq!(code, "synthetic-code");
    sender.join().unwrap()?;
    Ok(())
}
