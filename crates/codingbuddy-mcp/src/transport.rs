//! Stdio transport

use super::*;

/// Per-MCP output token limits (warn at threshold, error at max).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTokenLimits {
    /// Warning threshold (tokens). Default 10_000.
    pub warn_threshold: u64,
    /// Hard maximum (tokens). Default 25_000.
    pub max_tokens: u64,
}

impl Default for McpTokenLimits {
    fn default() -> Self {
        Self {
            warn_threshold: 10_000,
            max_tokens: 25_000,
        }
    }
}

/// Check an MCP tool output against token limits.
/// Returns `(output, was_truncated)`.
pub fn enforce_mcp_token_limit(output: &str, limits: &McpTokenLimits) -> (String, bool) {
    // Rough estimate: ~4 chars per token
    let estimated_tokens = output.len() as u64 / 4;
    if estimated_tokens > limits.max_tokens {
        let max_chars = (limits.max_tokens * 4) as usize;
        let truncated = if output.len() > max_chars {
            format!(
                "{}\n\n... (output truncated: ~{} tokens exceeded limit of {})",
                &output[..max_chars],
                estimated_tokens,
                limits.max_tokens
            )
        } else {
            output.to_string()
        };
        (truncated, true)
    } else {
        (output.to_string(), false)
    }
}

pub(crate) fn server_allowed(server: &McpServer) -> bool {
    codingbuddy_policy::load_managed_settings()
        .is_none_or(|settings| codingbuddy_policy::is_mcp_server_allowed(&server.id, &settings))
}

pub(crate) fn ensure_server_enabled(server: &McpServer) -> Result<()> {
    anyhow::ensure!(
        server.enabled && server_allowed(server),
        "MCP server disabled or denied by policy"
    );
    Ok(())
}

pub(crate) fn stdio_command(server: &McpServer) -> Result<Command> {
    ensure_server_enabled(server)?;
    let executable = server
        .command
        .as_deref()
        .ok_or_else(|| anyhow!("stdio transport requires command"))?;
    let mut command = Command::new(executable);
    let baseline;
    let vars = if server.child_env.is_empty() {
        baseline = process_env::variables(Path::new("."), "", Source::User);
        &baseline
    } else {
        &server.child_env
    };
    process_env::apply(&mut command, vars);
    command.args(&server.args);
    if let Some(workspace) = server
        .metadata
        .get("_codingbuddy_workspace")
        .and_then(|value| value.as_str())
    {
        command.current_dir(workspace);
    }
    Ok(command)
}

#[cfg(test)]
pub(crate) fn parse_sse_tools(server_id: &str, sse_text: &str) -> Result<Vec<McpTool>> {
    let mut tools = Vec::new();
    for chunk in sse_text.split("\n\n") {
        let data = chunk
            .strip_prefix("data: ")
            .or_else(|| chunk.lines().find_map(|l| l.strip_prefix("data: ")));
        if let Some(json_str) = data
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str)
        {
            tools.extend(parse_mcp_tools_message(server_id, &value));
        }
    }
    if tools.is_empty() {
        return Err(anyhow!("no tools discovered from SSE MCP server"));
    }
    Ok(tools)
}

/// Compatibility facade over the initialized transport service.
#[derive(Default)]
pub struct McpConnectionPool {
    pub(crate) sessions: sessions::Sessions,
}
impl McpConnectionPool {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn execute(
        &self,
        server: &McpServer,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        self.sessions.request(
            server,
            method,
            params,
            &codingbuddy_core::execution::ExecutionContext {
                deadline: Some(Instant::now() + timeout),
                ..Default::default()
            },
        )
    }
    pub fn shutdown(&self) {
        self.sessions.shutdown();
    }
}
pub fn execute_mcp_stdio_request(
    server: &McpServer,
    method: &str,
    params: serde_json::Value,
    _request_id: u64,
    timeout: Duration,
) -> Result<serde_json::Value> {
    McpConnectionPool::new().execute(server, method, params, timeout)
}

#[cfg(test)]
pub(crate) fn wait_for_mcp_response(
    rx: &mpsc::Receiver<serde_json::Value>,
    request_id: u64,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .unwrap_or_else(|| Duration::from_millis(1));
        let wait = remaining.min(Duration::from_millis(150));
        match rx.recv_timeout(wait) {
            Ok(message) => {
                if message.get("id").and_then(|v| v.as_u64()) == Some(request_id) {
                    return Some(message);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    None
}

pub(crate) fn extract_resource_text(result: &serde_json::Value) -> Result<String> {
    let contents = result
        .get("contents")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("MCP resources/read missing contents array"))?;
    if contents.is_empty() {
        return Err(anyhow!("MCP resources/read returned no contents"));
    }

    let mut merged = Vec::new();
    for item in contents {
        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            merged.push(text.to_string());
            continue;
        }
        if let Some(blob) = item.get("blob").and_then(|v| v.as_str()) {
            let mime = item
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream");
            match base64::engine::general_purpose::STANDARD.decode(blob) {
                Ok(bytes) => match String::from_utf8(bytes.clone()) {
                    Ok(text) => merged.push(text),
                    Err(_) => merged.push(format!(
                        "[binary-resource mime={} size={} bytes]",
                        mime,
                        bytes.len()
                    )),
                },
                Err(_) => merged.push(format!(
                    "[invalid-base64-resource mime={} chars={}]",
                    mime,
                    blob.len()
                )),
            }
        }
    }

    if merged.is_empty() {
        return Err(anyhow!(
            "MCP resources/read contents had no text/blob payloads"
        ));
    }
    let joined = merged.join("\n");
    let (bounded, _truncated) = enforce_mcp_token_limit(&joined, &McpTokenLimits::default());
    Ok(bounded)
}

pub(crate) fn read_mcp_frame<R: BufRead>(reader: &mut R) -> Result<Option<serde_json::Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = std::io::Read::take(&mut *reader, 16 * 1024 * 1024 + 1).read_line(&mut line)?;
        anyhow::ensure!(n <= 16 * 1024 * 1024, "MCP frame limit exceeded");
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with('{') {
            return Ok(Some(serde_json::from_str(trimmed)?));
        }
        if trimmed.is_empty() {
            if content_length.is_some() {
                break;
            } else {
                continue;
            }
        }
        #[cfg(test)]
        if trimmed == "running 1 test" {
            continue;
        }
        let (header, value) = trimmed
            .split_once(':')
            .ok_or_else(|| anyhow!("Malformed MCP frame"))?;
        if header.eq_ignore_ascii_case("Content-Length") {
            anyhow::ensure!(content_length.is_none(), "Duplicate MCP frame length");
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid Content-Length")?,
            );
        }
    }

    let length = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    anyhow::ensure!(length <= 16 * 1024 * 1024, "MCP frame limit exceeded");
    let mut buf = vec![0_u8; length];
    reader.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

#[cfg(test)]
pub(crate) fn parse_mcp_tools_message(
    server_id: &str,
    message: &serde_json::Value,
) -> Vec<McpTool> {
    let mut out = Vec::new();
    if let Some(tools) = message
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(|tools| tools.as_array())
    {
        out.extend(parse_tools_array(server_id, tools));
    }
    if let Some(tools) = message.get("tools").and_then(|tools| tools.as_array()) {
        out.extend(parse_tools_array(server_id, tools));
    }
    out
}
