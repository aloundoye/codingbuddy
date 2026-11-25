//! codingbuddy mcp serve

use super::*;

/// Start an MCP JSON-RPC 2.0 server on stdin/stdout, exposing CodingBuddy tools.
///
/// This allows other MCP-compatible clients to use CodingBuddy as a tool server.
/// Implements the MCP protocol: `initialize`, `tools/list`, `tools/call`.
pub fn run_mcp_serve(workspace: &Path) -> Result<()> {
    eprintln!("codingbuddy-cli MCP server started (stdio transport)");
    let tool_list = build_serve_tool_list(workspace);

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    loop {
        let message = match read_mcp_frame(&mut reader) {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // EOF
            Err(_) => break,
        };

        let id = message.get("id").cloned();
        let method = message.get("method").and_then(|v| v.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "codingbuddy-cli",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_list }
            }),
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or(json!({}));
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                let result_text = execute_serve_tool(workspace, tool_name, &arguments);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": result_text
                        }]
                    }
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {method}")
                }
            }),
        };

        let body = serde_json::to_vec(&response)?;
        write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
        writer.write_all(&body)?;
        writer.flush()?;
    }

    eprintln!("codingbuddy-cli MCP server stopped");
    Ok(())
}

/// Build the tool list for MCP serve mode — exposes a subset of CodingBuddy tools.
pub(crate) fn build_serve_tool_list(_workspace: &Path) -> Vec<serde_json::Value> {
    let tools = vec![
        (
            "fs_read",
            "Read a file",
            json!({"type": "object", "properties": {"path": {"type": "string", "description": "File path to read"}}, "required": ["path"]}),
        ),
        (
            "fs_write",
            "Write content to a file",
            json!({"type": "object", "properties": {"path": {"type": "string", "description": "File path"}, "content": {"type": "string", "description": "Content to write"}}, "required": ["path", "content"]}),
        ),
        (
            "fs_glob",
            "Find files by glob pattern",
            json!({"type": "object", "properties": {"pattern": {"type": "string", "description": "Glob pattern"}}, "required": ["pattern"]}),
        ),
        (
            "fs_grep",
            "Search file contents with regex",
            json!({"type": "object", "properties": {"pattern": {"type": "string", "description": "Regex pattern"}, "path": {"type": "string", "description": "Search path"}}, "required": ["pattern"]}),
        ),
        (
            "bash_run",
            "Execute a shell command",
            json!({"type": "object", "properties": {"command": {"type": "string", "description": "Command to execute"}, "timeout": {"type": "integer", "description": "Timeout in seconds"}}, "required": ["command"]}),
        ),
        (
            "git_status",
            "Show git status",
            json!({"type": "object", "properties": {}}),
        ),
        (
            "git_diff",
            "Show git diff",
            json!({"type": "object", "properties": {"args": {"type": "string", "description": "Additional git diff args"}}}),
        ),
        (
            "web_fetch",
            "Fetch URL content",
            json!({"type": "object", "properties": {"url": {"type": "string", "description": "URL to fetch"}}, "required": ["url"]}),
        ),
    ];
    tools
        .into_iter()
        .map(|(name, desc, schema)| {
            json!({
                "name": name,
                "description": desc,
                "inputSchema": schema,
            })
        })
        .collect()
}

#[cfg(target_os = "windows")]
pub(crate) fn spawn_serve_shell(
    command: &str,
    workspace: &Path,
) -> std::io::Result<std::process::Output> {
    Command::new("cmd")
        .args(["/C", command])
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn spawn_serve_shell(
    command: &str,
    workspace: &Path,
) -> std::io::Result<std::process::Output> {
    Command::new("sh")
        .args(["-c", command])
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Execute a tool in MCP serve mode (simplified execution without full agent).
pub(crate) fn execute_serve_tool(
    workspace: &Path,
    tool_name: &str,
    arguments: &serde_json::Value,
) -> String {
    match tool_name {
        "fs_read" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let full_path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                workspace.join(path)
            };
            match fs::read_to_string(&full_path) {
                Ok(content) => content,
                Err(e) => format!("Error reading {}: {e}", full_path.display()),
            }
        }
        "fs_write" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = arguments
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let full_path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                workspace.join(path)
            };
            if let Some(parent) = full_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&full_path, content) {
                Ok(()) => format!("Written {} bytes to {}", content.len(), full_path.display()),
                Err(e) => format!("Error writing {}: {e}", full_path.display()),
            }
        }
        "fs_glob" => {
            let pattern = arguments
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("*");
            match glob::glob(&workspace.join(pattern).to_string_lossy()) {
                Ok(entries) => {
                    let paths: Vec<String> = entries
                        .filter_map(|e: Result<PathBuf, glob::GlobError>| e.ok())
                        .map(|p: PathBuf| p.to_string_lossy().to_string())
                        .take(200)
                        .collect();
                    paths.join("\n")
                }
                Err(e) => format!("Glob error: {e}"),
            }
        }
        "bash_run" => {
            let cmd = arguments
                .get("command")
                .or_else(|| arguments.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let _timeout_secs = arguments
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(30);
            match spawn_serve_shell(cmd, workspace) {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    format!(
                        "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
                        output.status.code().unwrap_or(-1),
                        stdout,
                        stderr
                    )
                }
                Err(e) => format!("Command spawn error: {e}"),
            }
        }
        "git_status" | "git_diff" => {
            let git_cmd = if tool_name == "git_status" {
                "status"
            } else {
                "diff"
            };
            let extra_args = arguments.get("args").and_then(|v| v.as_str()).unwrap_or("");
            let mut args = vec![git_cmd];
            if !extra_args.is_empty() {
                args.push(extra_args);
            }
            match Command::new("git")
                .args(&args)
                .current_dir(workspace)
                .output()
            {
                Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
                Err(e) => format!("Git error: {e}"),
            }
        }
        "web_fetch" => {
            let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
            match Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .and_then(|c| c.get(url).send())
            {
                Ok(resp) => resp.text().unwrap_or_else(|e| format!("Read error: {e}")),
                Err(e) => format!("Fetch error: {e}"),
            }
        }
        "fs_grep" => {
            let pattern = arguments
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| {
                    if Path::new(p).is_absolute() {
                        PathBuf::from(p)
                    } else {
                        workspace.join(p)
                    }
                })
                .unwrap_or_else(|| workspace.to_path_buf());
            match Command::new("grep")
                .args(["-rn", "--include=*", pattern])
                .arg(&path)
                .output()
            {
                Ok(output) => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    if text.len() > 10000 {
                        format!("{}...\n(truncated)", &text[..10000])
                    } else {
                        text.to_string()
                    }
                }
                Err(e) => format!("Grep error: {e}"),
            }
        }
        _ => format!("Unknown tool: {tool_name}"),
    }
}

pub(crate) fn parse_tools_array(server_id: &str, tools: &[serde_json::Value]) -> Vec<McpTool> {
    tools
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_string();
            let description = entry
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(McpTool {
                server_id: server_id.to_string(),
                name,
                description,
                input_schema: entry
                    .get("inputSchema")
                    .or_else(|| entry.get("input_schema"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                annotations: entry
                    .get("annotations")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect()
}
