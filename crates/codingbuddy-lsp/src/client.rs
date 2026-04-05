use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

/// A single LSP client connected to one server process over stdio.
pub struct LspClient {
    process: Mutex<Child>,
    next_id: AtomicI64,
    server_command: String,
    workspace_root: PathBuf,
    initialized: Mutex<bool>,
}

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
        let child = Command::new(server_command)
            .args(server_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(workspace_root)
            .spawn()
            .map_err(|e| anyhow!("failed to spawn LSP server '{}': {}", server_command, e))?;

        Ok(Self {
            process: Mutex::new(child),
            next_id: AtomicI64::new(1),
            server_command: server_command.to_string(),
            workspace_root: workspace_root.to_path_buf(),
            initialized: Mutex::new(false),
        })
    }

    /// Send the initialize request and initialized notification.
    pub fn initialize(&self) -> Result<Value> {
        let mut guard = self
            .initialized
            .lock()
            .map_err(|_| anyhow!("lock poisoned"))?;
        if *guard {
            return Ok(json!({"already_initialized": true}));
        }

        let root_uri = format!("file://{}", self.workspace_root.display());
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

        let result = self.send_request("initialize", params)?;

        // Send initialized notification (no id, no response expected)
        self.send_notification("initialized", json!({}))?;

        *guard = true;
        Ok(result)
    }

    /// Notify the server that a file was opened.
    pub fn did_open(&self, file_path: &Path, language_id: &str, text: &str) -> Result<()> {
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
    }

    /// Notify the server that a file was changed.
    pub fn did_change(&self, file_path: &Path, version: i32, text: &str) -> Result<()> {
        let uri = path_to_uri(file_path);
        self.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        )
    }

    /// Request hover information at a position.
    pub fn hover(&self, file_path: &Path, line: u32, col: u32) -> Result<Option<HoverResult>> {
        let result =
            self.send_request("textDocument/hover", position_params(file_path, line, col))?;

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
    pub fn definition(&self, file_path: &Path, line: u32, col: u32) -> Result<Vec<LspLocation>> {
        let result = self.send_request(
            "textDocument/definition",
            position_params(file_path, line, col),
        )?;
        Ok(parse_locations(&result))
    }

    /// Request all references at a position.
    pub fn references(&self, file_path: &Path, line: u32, col: u32) -> Result<Vec<LspLocation>> {
        let uri = path_to_uri(file_path);
        let result = self.send_request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line.saturating_sub(1), "character": col.saturating_sub(1) },
                "context": { "includeDeclaration": true },
            }),
        )?;
        Ok(parse_locations(&result))
    }

    /// Request document symbols.
    pub fn document_symbols(&self, file_path: &Path) -> Result<Vec<DocumentSymbol>> {
        let uri = path_to_uri(file_path);
        let result = self.send_request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        Ok(parse_symbols(&result))
    }

    /// Send a shutdown request then exit notification.
    pub fn shutdown(&self) -> Result<()> {
        let _ = self.send_request("shutdown", json!(null));
        let _ = self.send_notification("exit", json!(null));
        if let Ok(mut proc) = self.process.lock() {
            let _ = proc.kill();
            let _ = proc.wait();
        }
        Ok(())
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

    /// Send a request and read the response while holding the process lock,
    /// preventing interleaved writes from concurrent callers.
    fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut proc = self.process.lock().map_err(|_| anyhow!("lock poisoned"))?;

        // Write
        write_to_stdin(&mut proc, &req)?;

        // Read (holding lock so no other thread can interleave)
        read_response_from_stdout(&mut proc, id)
    }

    fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut proc = self.process.lock().map_err(|_| anyhow!("lock poisoned"))?;
        write_to_stdin(&mut proc, &msg)
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

// ── JSON-RPC I/O (free functions operating on locked Child) ──

fn write_to_stdin(proc: &mut Child, msg: &Value) -> Result<()> {
    let body = serde_json::to_string(msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let stdin = proc.stdin.as_mut().ok_or_else(|| anyhow!("no stdin"))?;
    stdin.write_all(header.as_bytes())?;
    stdin.write_all(body.as_bytes())?;
    stdin.flush()?;
    Ok(())
}

fn read_response_from_stdout(proc: &mut Child, expected_id: i64) -> Result<Value> {
    let stdout = proc.stdout.as_mut().ok_or_else(|| anyhow!("no stdout"))?;
    let mut reader = BufReader::new(stdout);

    loop {
        let mut content_length: usize = 0;
        loop {
            let mut header_line = String::new();
            reader.read_line(&mut header_line)?;
            let trimmed = header_line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some(len_str) = trimmed.strip_prefix("Content-Length:") {
                content_length = len_str.trim().parse().unwrap_or(0);
            }
        }

        if content_length == 0 {
            continue;
        }

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let parsed: Value = serde_json::from_slice(&body)?;

        // Skip notifications (no id field)
        if parsed.get("id").is_none() {
            continue;
        }

        if parsed["id"].as_i64() == Some(expected_id) {
            if let Some(error) = parsed.get("error") {
                return Err(anyhow!(
                    "LSP error: {}",
                    error["message"].as_str().unwrap_or("unknown")
                ));
            }
            return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

// ── Helpers ──

fn path_to_uri(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    format!("file://{}", abs.display())
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
            let file = uri.strip_prefix("file://").unwrap_or(uri).to_string();
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
        let uri = path_to_uri(Path::new("/tmp/foo.rs"));
        assert!(uri.starts_with("file:///tmp/foo.rs"));
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
        let result = json!({
            "uri": "file:///tmp/foo.rs",
            "range": {
                "start": {"line": 9, "character": 4},
                "end": {"line": 9, "character": 10}
            }
        });
        let locs = parse_locations(&result);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].file, "/tmp/foo.rs");
        assert_eq!(locs[0].line, 10);
        assert_eq!(locs[0].col, 5);
    }

    #[test]
    fn parse_location_array() {
        let result = json!([
            {"uri": "file:///a.rs", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}},
            {"uri": "file:///b.rs", "range": {"start": {"line": 10, "character": 3}, "end": {"line": 10, "character": 8}}},
        ]);
        let locs = parse_locations(&result);
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[0].file, "/a.rs");
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
