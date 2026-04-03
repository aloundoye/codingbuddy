use anyhow::{Result, anyhow};
use codingbuddy_core::{EventKind, runtime_dir};
use codingbuddy_mcp::{McpManager, McpServer, McpTransport};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::McpCmd;
use crate::context::*;
use crate::output::*;

pub(crate) fn run_mcp(cwd: &Path, cmd: McpCmd, json_mode: bool) -> Result<()> {
    let manager = McpManager::new(cwd)?;
    match cmd {
        McpCmd::Add(args) => {
            let metadata = if let Some(metadata) = args.metadata.as_deref() {
                serde_json::from_str(metadata)?
            } else {
                serde_json::Value::Null
            };
            let server = McpServer {
                id: args.id.clone(),
                name: args.name.unwrap_or_else(|| args.id.clone()),
                transport: args.transport.into_transport(),
                command: args.command,
                args: args.args,
                url: args.url,
                enabled: args.enabled,
                metadata,
                headers: Vec::new(),
            };
            let endpoint = server
                .command
                .clone()
                .or_else(|| server.url.clone())
                .unwrap_or_default();
            let transport = match server.transport {
                McpTransport::Stdio => "stdio",
                McpTransport::Http => "http",
                McpTransport::Sse => "sse",
            };
            manager.add_server(server.clone())?;
            append_control_event(
                cwd,
                EventKind::McpServerAdded {
                    server_id: server.id.clone(),
                    transport: transport.to_string(),
                    endpoint,
                },
            )?;

            let (discovered, refreshes) = manager.refresh_tools()?;
            let discovered_for_server = discovered
                .iter()
                .filter(|tool| tool.server_id == server.id)
                .cloned()
                .collect::<Vec<_>>();
            emit_mcp_discovery_events(cwd, &refreshes)?;

            if json_mode {
                print_json(&json!({
                    "added": server,
                    "discovered_tools": discovered_for_server,
                }))?;
            } else {
                println!("added mcp server {} (transport={})", args.id, transport);
            }
        }
        McpCmd::List => {
            let (_, refreshes, notice) =
                manager.discover_tools_with_notice(read_mcp_fingerprint(cwd)?.as_deref())?;
            write_mcp_fingerprint(cwd, &notice.fingerprint)?;
            emit_mcp_discovery_events(cwd, &refreshes)?;
            let servers = manager.list_servers()?;
            if json_mode {
                print_json(&servers)?;
            } else if servers.is_empty() {
                println!("no mcp servers configured");
            } else {
                for server in servers {
                    println!(
                        "{} {} enabled={} endpoint={}",
                        server.id,
                        match server.transport {
                            McpTransport::Stdio => "stdio",
                            McpTransport::Http => "http",
                            McpTransport::Sse => "sse",
                        },
                        server.enabled,
                        server
                            .command
                            .as_deref()
                            .or(server.url.as_deref())
                            .unwrap_or_default()
                    );
                }
            }
        }
        McpCmd::Get(args) => {
            let server = manager
                .get_server(&args.server_id)?
                .ok_or_else(|| anyhow!("mcp server not found: {}", args.server_id))?;
            if json_mode {
                print_json(&server)?;
            } else {
                println!("{}", serde_json::to_string_pretty(&server)?);
            }
        }
        McpCmd::Init(args) => {
            let dir = cwd.join(&args.name);
            fs::create_dir(&dir).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    anyhow!("directory already exists: {}", dir.display())
                } else {
                    anyhow!(e)
                }
            })?;
            match args.lang.as_str() {
                "node" | "javascript" | "js" | "typescript" | "ts" => {
                    fs::write(
                        dir.join("package.json"),
                        serde_json::to_string_pretty(&json!({
                            "name": args.name,
                            "version": "0.1.0",
                            "type": "module",
                            "dependencies": {
                                "@modelcontextprotocol/sdk": "^1.0.0"
                            }
                        }))?,
                    )?;
                    fs::write(
                        dir.join("index.js"),
                        format!(
                            r#"import {{ Server }} from "@modelcontextprotocol/sdk/server/index.js";
import {{ StdioServerTransport }} from "@modelcontextprotocol/sdk/server/stdio.js";

const server = new Server({{ name: "{name}", version: "0.1.0" }}, {{
  capabilities: {{ tools: {{}} }}
}});

server.setRequestHandler("tools/list", async () => ({{
  tools: [{{
    name: "hello",
    description: "Say hello",
    inputSchema: {{ type: "object", properties: {{ name: {{ type: "string" }} }}, required: ["name"] }}
  }}]
}}));

server.setRequestHandler("tools/call", async (request) => ({{
  content: [{{ type: "text", text: `Hello, ${{request.params.arguments.name}}!` }}]
}}));

const transport = new StdioServerTransport();
await server.connect(transport);
"#,
                            name = args.name
                        ),
                    )?;
                }
                _ => {
                    // Python (default)
                    fs::write(
                        dir.join("server.py"),
                        format!(
                            r#"#!/usr/bin/env python3
"""MCP server: {name}"""
import json, sys

def handle_request(req):
    method = req.get("method", "")
    if method == "tools/list":
        return {{"tools": [{{"name": "hello", "description": "Say hello",
                "inputSchema": {{"type": "object", "properties": {{"name": {{"type": "string"}}}}, "required": ["name"]}}}}]}}
    if method == "tools/call":
        name = req.get("params", {{}}).get("arguments", {{}}).get("name", "world")
        return {{"content": [{{"type": "text", "text": f"Hello, {{name}}!"}}]}}
    return {{"error": {{"code": -32601, "message": "Method not found"}}}}

if __name__ == "__main__":
    for line in sys.stdin:
        req = json.loads(line)
        resp = handle_request(req)
        resp["id"] = req.get("id")
        resp["jsonrpc"] = "2.0"
        print(json.dumps(resp), flush=True)
"#,
                            name = args.name
                        ),
                    )?;
                }
            }
            if json_mode {
                print_json(&json!({ "created": dir.display().to_string(), "name": args.name }))?;
            } else {
                println!("Created MCP server scaffold at {}", dir.display());
                println!(
                    "Add to config: codingbuddy mcp add {} --transport stdio --command \"{}\"",
                    args.name,
                    if args.lang.starts_with("node")
                        || args.lang.starts_with("js")
                        || args.lang.starts_with("ts")
                    {
                        format!("node {}/index.js", dir.display())
                    } else {
                        format!("python3 {}/server.py", dir.display())
                    }
                );
            }
        }
        McpCmd::Remove(args) => {
            let removed = manager.remove_server(&args.server_id)?;
            if removed {
                append_control_event(
                    cwd,
                    EventKind::McpServerRemoved {
                        server_id: args.server_id.clone(),
                    },
                )?;
            }
            if json_mode {
                print_json(&json!({
                    "server_id": args.server_id,
                    "removed": removed,
                }))?;
            } else if removed {
                println!("removed mcp server {}", args.server_id);
            } else {
                println!("mcp server not found: {}", args.server_id);
            }
        }
    }
    Ok(())
}

pub(crate) fn mcp_fingerprint_path(cwd: &Path) -> PathBuf {
    runtime_dir(cwd).join("mcp").join("tools_fingerprint.txt")
}

pub(crate) fn read_mcp_fingerprint(cwd: &Path) -> Result<Option<String>> {
    let path = mcp_fingerprint_path(cwd);
    if !path.exists() {
        return Ok(None);
    }
    let value = fs::read_to_string(path)?;
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value.to_string()))
}

pub(crate) fn write_mcp_fingerprint(cwd: &Path, fingerprint: &str) -> Result<()> {
    let path = mcp_fingerprint_path(cwd);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, fingerprint)?;
    Ok(())
}

pub(crate) fn emit_mcp_discovery_events(
    cwd: &Path,
    refreshes: &[codingbuddy_mcp::McpToolRefresh],
) -> Result<()> {
    for refresh in refreshes {
        for tool_name in &refresh.added {
            append_control_event(
                cwd,
                EventKind::McpToolDiscovered {
                    server_id: refresh.server_id.clone(),
                    tool_name: tool_name.clone(),
                },
            )?;
        }
    }
    Ok(())
}
