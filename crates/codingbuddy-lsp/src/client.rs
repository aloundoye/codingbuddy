use anyhow::{Result, anyhow};
use codingbuddy_core::execution::{AsyncChild, ExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

type Pending = Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Value>>>>;

/// A single LSP client connected to one server process over stdio.
///
/// A reader task routes each response to the request that is waiting for
/// it (by JSON-RPC id), so several requests can be in flight at once and
/// none of them holds a thread while the server works.
pub struct LspClient {
    process: Mutex<AsyncChild>,
    context: Mutex<ExecutionContext>,
    pending: Pending,
    writes: tokio::sync::mpsc::Sender<Vec<u8>>,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
    next_id: AtomicI64,
    server_command: String,
    workspace_root: PathBuf,
    initialized: tokio::sync::Mutex<bool>,
}

/// Largest JSON-RPC body sent or accepted.
const FRAME_LIMIT: usize = 16 * 1024 * 1024;
/// Longest a single request may take.
const REQUEST_LIMIT: Duration = Duration::from_secs(30);

/// An LSP location (file + line + column).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspLocation {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub end_line: Option<u32>,
    pub end_col: Option<u32>,
}

/// Hover result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoverResult {
    pub contents: String,
}

/// Document symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub col: u32,
    pub children: Vec<DocumentSymbol>,
}

impl LspClient {
    /// Spawn an LSP server process and connect over stdio.
    pub fn spawn(
        server_command: &str,
        server_args: &[&str],
        workspace_root: &Path,
    ) -> Result<Self> {
        let _ = (server_command, server_args, workspace_root);
        anyhow::bail!("Language-server startup requires a policy-prepared owned process")
    }

    /// Connect to a server the caller started (with piped stdin/stdout).
    /// Must be called inside the async runtime.
    pub fn from_owned(
        mut child: AsyncChild,
        server_command: &str,
        workspace_root: &Path,
        context: ExecutionContext,
    ) -> Result<Self> {
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Missing language-server stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Missing language-server stdout"))?;
        let (writes, mut queued) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let writer = tokio::spawn(async move {
            while let Some(frame) = queued.recv().await {
                if input.write_all(&frame).await.is_err() || input.flush().await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::default();
        let routes = pending.clone();
        let reader = tokio::spawn(async move {
            let mut output = tokio::io::BufReader::new(output);
            while let Ok(Some(frame)) = read_frame_async(&mut output).await {
                // Responses carry an id and no method; requests and
                // notifications from the server are not answered.
                if frame.get("method").is_some() {
                    continue;
                }
                let Some(id) = frame.get("id").and_then(Value::as_i64) else {
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
            // Wake every waiting request: the server is gone.
            routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        });
        Ok(Self {
            process: Mutex::new(child),
            context: Mutex::new(context),
            pending,
            writes,
            reader,
            writer,
            next_id: AtomicI64::new(1),
            server_command: server_command.into(),
            workspace_root: workspace_root.into(),
            initialized: tokio::sync::Mutex::new(false),
        })
    }

    pub fn set_context(&self, context: &ExecutionContext) -> Result<()> {
        *self
            .context
            .lock()
            .map_err(|_| anyhow!("Language context unavailable"))? = context.clone();
        Ok(())
    }

    /// Send the initialize request and initialized notification.
    pub async fn initialize(&self) -> Result<Value> {
        let mut guard = self.initialized.lock().await;
        if *guard {
            return Ok(json!({"already_initialized": true}));
        }

        let root_uri = path_to_uri(&self.workspace_root);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{
                "name": "workspace",
                "uri": root_uri,
            }],
            "capabilities": {
                "textDocument": {
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": { "linkSupport": false },
                    "references": {},
                    "documentSymbol": {
                        "hierarchicalDocumentSymbolSupport": true,
                    },
                    "publishDiagnostics": { "versionSupport": true },
                    "synchronization": {
                        "didOpen": true,
                        "didChange": true,
                    },
                },
                "window": { "workDoneProgress": true },
                "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                },
            },
        });

        let result = self.send_request("initialize", params).await?;

        // Send initialized notification (no id, no response expected)
        self.send_notification("initialized", json!({})).await?;

        *guard = true;
        Ok(result)
    }

    /// Notify the server that a file was opened.
    pub async fn did_open(&self, file_path: &Path, language_id: &str, text: &str) -> Result<()> {
        let uri = path_to_uri(file_path);
        self.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 0,
                    "text": text,
                }
            }),
        )
        .await
    }

    /// Notify the server that a file was changed.
    pub async fn did_change(&self, file_path: &Path, version: i32, text: &str) -> Result<()> {
        let uri = path_to_uri(file_path);
        self.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        )
        .await
    }

    /// Request hover information at a position.
    pub async fn hover(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
    ) -> Result<Option<HoverResult>> {
        let result = self
            .send_request("textDocument/hover", position_params(file_path, line, col))
            .await?;

        if result.is_null() {
            return Ok(None);
        }

        let contents = extract_hover_contents(&result);
        if contents.is_empty() {
            return Ok(None);
        }
        Ok(Some(HoverResult { contents }))
    }

    /// Request goto-definition at a position.
    pub async fn definition(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
    ) -> Result<Vec<LspLocation>> {
        let result = self
            .send_request(
                "textDocument/definition",
                position_params(file_path, line, col),
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// Request all references at a position.
    pub async fn references(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
    ) -> Result<Vec<LspLocation>> {
        let uri = path_to_uri(file_path);
        let result = self
            .send_request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line.saturating_sub(1), "character": col.saturating_sub(1) },
                    "context": { "includeDeclaration": true },
                }),
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// Request document symbols.
    pub async fn document_symbols(&self, file_path: &Path) -> Result<Vec<DocumentSymbol>> {
        let uri = path_to_uri(file_path);
        let result = self
            .send_request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await?;
        Ok(parse_symbols(&result))
    }

    /// Stop the server process (and everything it started).
    pub fn shutdown(&self) -> Result<()> {
        self.terminate();
        Ok(())
    }

    fn terminate(&self) {
        self.process
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminate();
    }

    /// Check if the server process is still alive.
    pub fn is_alive(&self) -> bool {
        if let Ok(mut proc) = self.process.lock() {
            matches!(proc.try_wait(), Ok(None))
        } else {
            false
        }
    }

    pub fn server_command(&self) -> &str {
        &self.server_command
    }

    // ── Internal JSON-RPC ──

    /// The context for one request: the caller's, capped at
    /// [`REQUEST_LIMIT`].
    fn request_context(&self) -> Result<ExecutionContext> {
        let mut context = self
            .context
            .lock()
            .map_err(|_| anyhow!("Language context unavailable"))?
            .clone();
        let limit = Instant::now() + REQUEST_LIMIT;
        context.deadline = Some(
            context
                .deadline
                .map_or(limit, |deadline| deadline.min(limit)),
        );
        Ok(context)
    }

    /// Wait for `work` unless the request is cancelled or runs past its
    /// deadline, in which case the server is stopped: a server that ignores
    /// a request must not keep holding the language slot.
    async fn bounded<T>(
        &self,
        context: &ExecutionContext,
        work: impl std::future::Future<Output = Result<T>>,
        on_abandon: impl FnOnce(),
    ) -> Result<T> {
        context.check()?;
        let deadline = codingbuddy_core::execution::deadline_for(context, REQUEST_LIMIT);
        let outcome = tokio::select! {
            biased;
            () = context.cancellation.cancelled() => Err(anyhow!("Execution cancelled")),
            () = tokio::time::sleep_until(deadline) => Err(anyhow!("Execution deadline exceeded")),
            result = work => return result,
        };
        on_abandon();
        self.terminate();
        outcome
    }

    async fn write(&self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message)?;
        anyhow::ensure!(body.len() <= FRAME_LIMIT, "Language request limit exceeded");
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        self.writes
            .send(frame)
            .await
            .map_err(|_| anyhow!("Language input closed"))
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let context = self.request_context()?;
        let (respond, response) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("Language response routes unavailable"))?
            .insert(id, respond);
        let exchange = async {
            self.write(&request).await?;
            let frame = response
                .await
                .map_err(|_| anyhow!("Language server closed before response"))?;
            anyhow::ensure!(
                frame.get("error").is_none(),
                "Language server returned an error"
            );
            frame
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("Language result missing"))
        };
        let result = self
            .bounded(&context, exchange, || {
                // Best effort: tell the server before stopping it.
                let cancel = json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}});
                if let Ok(body) = serde_json::to_vec(&cancel) {
                    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
                    frame.extend_from_slice(&body);
                    let _ = self.writes.try_send(frame);
                }
            })
            .await;
        if let Ok(mut routes) = self.pending.lock() {
            routes.remove(&id);
        }
        result
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let context = self.request_context()?;
        self.bounded(&context, self.write(&message), || {}).await
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.terminate();
        self.reader.abort();
        self.writer.abort();
    }
}

/// Read one `Content-Length` framed JSON-RPC message. `Ok(None)` at EOF.
async fn read_frame_async(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<Option<Value>> {
    let mut content_length = None;
    let mut headers = 0usize;
    loop {
        let mut line = String::new();
        let size = (&mut *reader).take(65537).read_line(&mut line).await?;
        if size == 0 {
            return Ok(None);
        }
        headers += size;
        anyhow::ensure!(headers <= 65536, "Language frame headers exceeded limit");
        #[cfg(test)]
        if line.trim() == "running 1 test" || (line.trim().is_empty() && content_length.is_none()) {
            continue;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            anyhow::ensure!(content_length.is_none(), "Duplicate language frame length");
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let size = content_length.ok_or_else(|| anyhow!("Missing language frame length"))?;
    anyhow::ensure!(size <= FRAME_LIMIT, "Language frame limit exceeded");
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

// ── Test fixture I/O ──

#[cfg(test)]
fn read_frame(reader: &mut impl std::io::BufRead) -> Result<Option<Value>> {
    let mut content_length = None;
    let mut headers = 0usize;
    loop {
        let mut line = String::new();
        let size =
            std::io::BufRead::read_line(&mut std::io::Read::take(&mut *reader, 65537), &mut line)?;
        if size == 0 {
            return Ok(None);
        }
        headers += size;
        anyhow::ensure!(headers <= 65536, "Language frame headers exceeded limit");
        #[cfg(test)]
        if line.trim() == "running 1 test" || (line.trim().is_empty() && content_length.is_none()) {
            continue;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            anyhow::ensure!(content_length.is_none(), "Duplicate language frame length");
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let size = content_length.ok_or_else(|| anyhow!("Missing language frame length"))?;
    anyhow::ensure!(size <= 16 * 1024 * 1024, "Language frame limit exceeded");
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

// ── Helpers ──

fn path_to_uri(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    url::Url::from_file_path(&abs)
        .map(String::from)
        .unwrap_or_else(|()| format!("file://{}", abs.display()))
}

fn position_params(file_path: &Path, line: u32, col: u32) -> Value {
    let uri = path_to_uri(file_path);
    json!({
        "textDocument": { "uri": uri },
        "position": {
            "line": line.saturating_sub(1),
            "character": col.saturating_sub(1),
        },
    })
}

fn extract_hover_contents(result: &Value) -> String {
    if let Some(contents) = result.get("contents") {
        if let Some(s) = contents.as_str() {
            return s.to_string();
        }
        if let Some(obj) = contents.as_object()
            && let Some(value) = obj.get("value").and_then(|v| v.as_str())
        {
            return value.to_string();
        }
        if let Some(arr) = contents.as_array() {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|v| {
                    v.as_str().map(|s| s.to_string()).or_else(|| {
                        v.get("value")
                            .and_then(|v2| v2.as_str())
                            .map(|s| s.to_string())
                    })
                })
                .collect();
            return parts.join("\n");
        }
    }
    String::new()
}

fn parse_locations(result: &Value) -> Vec<LspLocation> {
    let items = if result.is_array() {
        result.as_array().cloned().unwrap_or_default()
    } else if result.is_object() {
        vec![result.clone()]
    } else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|loc| {
            let uri = loc.get("uri")?.as_str()?;
            let file = url::Url::parse(uri)
                .ok()
                .and_then(|uri| uri.to_file_path().ok())
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| uri.to_owned());
            let range = loc.get("range")?;
            let start = range.get("start")?;
            let end = range.get("end");
            Some(LspLocation {
                file,
                line: start.get("line")?.as_u64()? as u32 + 1,
                col: start.get("character")?.as_u64()? as u32 + 1,
                end_line: end
                    .and_then(|e| e.get("line"))
                    .and_then(|l| l.as_u64())
                    .map(|l| l as u32 + 1),
                end_col: end
                    .and_then(|e| e.get("character"))
                    .and_then(|c| c.as_u64())
                    .map(|c| c as u32 + 1),
            })
        })
        .collect()
}

fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        22 => "struct",
        23 => "event",
        25 => "type_parameter",
        _ => "unknown",
    }
}

fn parse_symbols(result: &Value) -> Vec<DocumentSymbol> {
    let arr = match result.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    arr.iter().filter_map(parse_single_symbol).collect()
}

fn parse_single_symbol(val: &Value) -> Option<DocumentSymbol> {
    let name = val.get("name")?.as_str()?.to_string();
    let kind_num = val.get("kind")?.as_u64()?;
    let kind = symbol_kind_name(kind_num).to_string();

    // DocumentSymbol format (has range)
    let (line, col) = if let Some(range) = val.get("range") {
        let start = range.get("start")?;
        (
            start.get("line")?.as_u64()? as u32 + 1,
            start.get("character")?.as_u64()? as u32 + 1,
        )
    } else if let Some(loc) = val.get("location") {
        // SymbolInformation format (has location.range)
        let range = loc.get("range")?;
        let start = range.get("start")?;
        (
            start.get("line")?.as_u64()? as u32 + 1,
            start.get("character")?.as_u64()? as u32 + 1,
        )
    } else {
        (0, 0)
    };

    let children = val
        .get("children")
        .and_then(|c| c.as_array())
        .map(|arr| arr.iter().filter_map(parse_single_symbol).collect())
        .unwrap_or_default();

    Some(DocumentSymbol {
        name,
        kind,
        line,
        col,
        children,
    })
}

/// Format an LSP location for LLM consumption.
pub fn format_location(loc: &LspLocation) -> String {
    format!("{}:{}:{}", loc.file, loc.line, loc.col)
}

/// Format a list of locations for LLM consumption.
pub fn format_locations(locs: &[LspLocation]) -> String {
    if locs.is_empty() {
        return "No results found.".to_string();
    }
    locs.iter()
        .map(format_location)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format hover result for LLM consumption.
pub fn format_hover(hover: &Option<HoverResult>) -> String {
    match hover {
        Some(h) => h.contents.clone(),
        None => "No hover information available.".to_string(),
    }
}

/// Format document symbols for LLM consumption.
pub fn format_symbols(symbols: &[DocumentSymbol], indent: usize) -> String {
    let mut lines = Vec::new();
    for sym in symbols {
        let prefix = "  ".repeat(indent);
        lines.push(format!(
            "{}{} {} (line {})",
            prefix, sym.kind, sym.name, sym.line
        ));
        if !sym.children.is_empty() {
            lines.push(format_symbols(&sym.children, indent + 1));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_uri_absolute() {
        let path = std::env::current_dir().unwrap().join("foo bar.rs");
        let uri = path_to_uri(&path);
        assert!(uri.ends_with("/foo%20bar.rs"));
        assert_eq!(url::Url::parse(&uri).unwrap().to_file_path().unwrap(), path);
    }

    #[test]
    fn extract_hover_string() {
        let result = json!({"contents": "fn foo() -> i32"});
        assert_eq!(extract_hover_contents(&result), "fn foo() -> i32");
    }

    #[test]
    fn extract_hover_markdown_object() {
        let result = json!({"contents": {"kind": "markdown", "value": "# Foo\nbar"}});
        assert_eq!(extract_hover_contents(&result), "# Foo\nbar");
    }

    #[test]
    fn parse_single_location() {
        let path = std::env::current_dir().unwrap().join("foo.rs");
        let result = json!({
            "uri": path_to_uri(&path),
            "range": {
                "start": {"line": 9, "character": 4},
                "end": {"line": 9, "character": 10}
            }
        });
        let locs = parse_locations(&result);
        assert_eq!(locs.len(), 1);
        assert_eq!(Path::new(&locs[0].file), path);
        assert_eq!(locs[0].line, 10);
        assert_eq!(locs[0].col, 5);
    }

    #[test]
    fn parse_location_array() {
        let cwd = std::env::current_dir().unwrap();
        let path = cwd.join("a.rs");
        let result = json!([
            {"uri": path_to_uri(&path), "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}},
            {"uri": path_to_uri(&cwd.join("b.rs")), "range": {"start": {"line": 10, "character": 3}, "end": {"line": 10, "character": 8}}},
        ]);
        let locs = parse_locations(&result);
        assert_eq!(locs.len(), 2);
        assert_eq!(Path::new(&locs[0].file), path);
        assert_eq!(locs[1].line, 11);
    }

    #[test]
    fn parse_null_locations() {
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn parse_document_symbols() {
        let result = json!([
            {
                "name": "MyStruct",
                "kind": 22,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 10, "character": 0}},
                "children": [
                    {
                        "name": "my_method",
                        "kind": 6,
                        "range": {"start": {"line": 2, "character": 4}, "end": {"line": 5, "character": 4}},
                        "children": []
                    }
                ]
            }
        ]);
        let syms = parse_symbols(&result);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "MyStruct");
        assert_eq!(syms[0].kind, "struct");
        assert_eq!(syms[0].children.len(), 1);
        assert_eq!(syms[0].children[0].name, "my_method");
        assert_eq!(syms[0].children[0].kind, "method");
    }

    #[test]
    fn symbol_kind_names() {
        assert_eq!(symbol_kind_name(5), "class");
        assert_eq!(symbol_kind_name(12), "function");
        assert_eq!(symbol_kind_name(22), "struct");
        assert_eq!(symbol_kind_name(999), "unknown");
    }
}

#[cfg(test)]
mod delivery_three {
    use super::*;
    use std::io::Write;
    #[test]
    fn server_fixture() {
        if std::env::var_os("CODINGBUDDY_LSP_FIXTURE").is_none() {
            return;
        }
        let mut input = std::io::BufReader::new(std::io::stdin());
        while let Ok(Some(request)) = read_frame(&mut input) {
            if request.get("id").is_none() {
                continue;
            }
            if request["method"] != "initialize" {
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
            let body = json!({"jsonrpc":"2.0","id":request["id"],"result":{"capabilities":{}}})
                .to_string();
            print!("Content-Length: {}\r\n\r\n{body}", body.len());
            std::io::stdout().flush().unwrap();
        }
    }
    #[test]
    fn stalled_language_read_is_cancelled_and_owned_process_is_reaped() -> Result<()> {
        codingbuddy_core::runtime::block_on(stalled_read())
    }
    async fn stalled_read() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let token = codingbuddy_core::CancellationToken::new();
        let context = ExecutionContext {
            cancellation: token.clone(),
            deadline: Some(Instant::now() + Duration::from_secs(3)),
            ..Default::default()
        };
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "client::delivery_three::server_fixture",
                "--nocapture",
                "--quiet",
            ])
            .env("CODINGBUDDY_LSP_FIXTURE", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let child = AsyncChild::spawn(&mut command)?;
        let client = LspClient::from_owned(child, "fixture", workspace.path(), context)?;
        client.initialize().await?;
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            token.cancel();
        });
        let started = Instant::now();
        assert!(
            client
                .hover(&workspace.path().join("file.rs"), 1, 1)
                .await
                .is_err()
        );
        cancel.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        // SIGKILL delivery and reaping are asynchronous.
        let reaped = Instant::now() + Duration::from_secs(2);
        while client.is_alive() && Instant::now() < reaped {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!client.is_alive());
        Ok(())
    }
}
