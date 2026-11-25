//! Server configuration

use super::*;

pub(crate) fn encode_cached_tool_metadata(tool: &McpTool) -> String {
    serde_json::to_string(&json!({
        "input_schema": tool.input_schema.clone(),
        "annotations": tool.annotations.clone(),
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

pub(crate) fn decode_cached_tool_metadata(
    schema_json: &str,
) -> (serde_json::Value, serde_json::Value) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(schema_json) else {
        return (serde_json::Value::Null, serde_json::Value::Null);
    };
    let input_schema = value
        .get("input_schema")
        .or_else(|| value.get("inputSchema"))
        .cloned()
        .unwrap_or_else(|| {
            if value.is_object() && value.get("annotations").is_none() {
                value.clone()
            } else {
                serde_json::Value::Null
            }
        });
    let annotations = value
        .get("annotations")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    (input_schema, annotations)
}

pub(crate) fn tool_fingerprint(tools: &[McpTool]) -> Result<String> {
    let serialized = serde_json::to_vec(tools)?;
    Ok(format!("{:x}", Sha256::digest(serialized)))
}

pub(crate) fn load_config_if_exists(path: &Path) -> Result<McpConfig> {
    if !path.exists() {
        return Ok(McpConfig::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

pub(crate) fn resolve_env_var(var_name: &str) -> Option<String> {
    let name = var_name.trim();
    if let Ok(value) = std::env::var(name)
        && !value.is_empty()
    {
        return Some(value);
    }

    // Cross-platform HOME fallback: many configs use ${HOME} even on Windows.
    if name.eq_ignore_ascii_case("HOME") {
        if let Ok(profile) = std::env::var("USERPROFILE")
            && !profile.is_empty()
        {
            return Some(profile);
        }
        let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
        let path = std::env::var("HOMEPATH").unwrap_or_default();
        if !drive.is_empty() && !path.is_empty() {
            return Some(format!("{drive}{path}"));
        }
    }

    // Inverse fallback can help configs that use USERPROFILE on non-Windows hosts.
    if name.eq_ignore_ascii_case("USERPROFILE")
        && let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(home);
    }

    None
}

/// Expand environment variables (`${VAR}` and `${VAR:-default}`) in a string.
pub fn expand_env_vars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_expr = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var_expr.push(c);
            }
            if let Some((var_name, default)) = var_expr.split_once(":-") {
                result.push_str(&resolve_env_var(var_name).unwrap_or_else(|| default.to_string()));
            } else {
                result.push_str(&resolve_env_var(&var_expr).unwrap_or_default());
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Check if a string contains `..` as a path component (i.e. `../`, `/..',
/// or standalone `..`). Avoids false positives on version strings, Rust ranges, etc.
pub(crate) fn contains_path_traversal(s: &str) -> bool {
    s == ".."
        || s.starts_with("../")
        || s.ends_with("/..")
        || s.contains("/../")
        || s.starts_with("..\\")
        || s.ends_with("\\..")
        || s.contains("\\..\\")
        || s.contains("\\../")
        || s.contains("/..\\")
}

/// Expand environment variables in all string fields of an MCP server config.
/// Returns an error if any expanded value contains shell metacharacters.
pub fn expand_server_env_vars(server: &mut McpServer) -> Result<()> {
    use codingbuddy_policy::shell_parse::contains_shell_injection_chars;

    anyhow::ensure!(
        !server.id.contains("${") && !server.name.contains("${"),
        "MCP IDs and names must be literal"
    );
    let baseline;
    let vars = if server.child_env.is_empty() {
        baseline = process_env::variables(Path::new("."), "", Source::User);
        &baseline
    } else {
        &server.child_env
    };
    let expand = |input: &str| -> Result<String> {
        let mut rest = input;
        let mut output = String::new();
        while let Some((before, after)) = rest.split_once("${") {
            let (expression, tail) = after
                .split_once('}')
                .ok_or_else(|| anyhow!("invalid MCP environment reference"))?;
            let (name, fallback) = expression
                .split_once(":-")
                .map_or((expression, None), |(name, fallback)| {
                    (name, Some(fallback))
                });
            let value = process_env::lookup(vars, name).and_then(|value| value.to_str());
            let value = value
                .filter(|value| !value.is_empty())
                .or(fallback)
                .or(value)
                .ok_or_else(|| anyhow!("unauthorized or unavailable MCP environment variable"))?;
            output.push_str(before);
            output.push_str(value);
            rest = tail;
        }
        output.push_str(rest);
        Ok(output)
    };
    let expanded_cmd = server.command.as_deref().map(expand).transpose()?;
    let expanded_args = server
        .args
        .iter()
        .map(|arg| expand(arg))
        .collect::<Result<Vec<_>>>()?;
    let expanded_url = server.url.as_deref().map(expand).transpose()?;
    for (key, value) in &mut server.headers {
        anyhow::ensure!(!key.contains("${"), "MCP header names must be literal");
        *value = expand(value)?;
    }

    // Validate expanded values don't contain path traversal
    if let Some(ref cmd) = expanded_cmd
        && contains_path_traversal(cmd)
    {
        return Err(anyhow!(
            "MCP server '{}': expanded command contains path traversal",
            server.id
        ));
    }
    if let Some(ref url) = expanded_url
        && contains_path_traversal(url)
    {
        return Err(anyhow!(
            "MCP server '{}': expanded URL contains path traversal",
            server.id
        ));
    }
    for (i, arg) in expanded_args.iter().enumerate() {
        if contains_path_traversal(arg) {
            return Err(anyhow!(
                "MCP server '{}': expanded arg[{i}] contains path traversal",
                server.id
            ));
        }
    }

    if let Some(ref cmd) = expanded_cmd
        && contains_shell_injection_chars(cmd)
    {
        return Err(anyhow!(
            "MCP server '{}': expanded command contains shell metacharacters",
            server.id
        ));
    }
    for arg in &expanded_args {
        if contains_shell_injection_chars(arg) {
            return Err(anyhow!(
                "MCP server '{}': expanded argument contains shell metacharacters",
                server.id
            ));
        }
    }

    server.command = expanded_cmd;
    server.args = expanded_args;
    server.url = expanded_url;
    Ok(())
}

/// Import MCP servers from Claude Desktop config file.
/// Reads `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS)
/// or `%APPDATA%/Claude/claude_desktop_config.json` (Windows).
pub fn import_from_claude_desktop() -> Result<Vec<McpServer>> {
    let config_path =
        claude_desktop_config_path().ok_or_else(|| anyhow!("Claude Desktop config not found"))?;
    if !config_path.exists() {
        return Err(anyhow!(
            "Claude Desktop config not found at {}",
            config_path.display()
        ));
    }
    let raw = fs::read_to_string(&config_path)?;
    let config: serde_json::Value = serde_json::from_str(&raw)?;

    let servers_obj = config
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("no mcpServers in Claude Desktop config"))?;

    let mut servers = Vec::new();
    for (id, entry) in servers_obj {
        let command = entry
            .get("command")
            .and_then(|v| v.as_str())
            .map(String::from);
        let args = entry
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let url = entry.get("url").and_then(|v| v.as_str()).map(String::from);
        let transport = if url.is_some() {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        };
        servers.push(McpServer {
            id: id.clone(),
            name: id.clone(),
            transport,
            command,
            args,
            url,
            enabled: true,
            metadata: entry.get("env").cloned().unwrap_or(serde_json::Value::Null),
            headers: Vec::new(),
            child_env: Default::default(),
        });
    }
    Ok(servers)
}

pub(crate) fn claude_desktop_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").ok()?;
        Some(
            PathBuf::from(home)
                .join("Library/Application Support/Claude/claude_desktop_config.json"),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").ok()?;
        Some(PathBuf::from(appdata).join("Claude/claude_desktop_config.json"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home).join(".config/claude/claude_desktop_config.json"))
    }
}

/// System-wide managed MCP config path for enterprise lockdown.
pub(crate) fn managed_mcp_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(
            "/Library/Application Support/CodingBuddyCode/managed-mcp.json",
        ))
    }
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from("/etc/codingbuddy-code/managed-mcp.json"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(PathBuf::from(
            "C:\\Program Files\\CodingBuddyCode\\managed-mcp.json",
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}
