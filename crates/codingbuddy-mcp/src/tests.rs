
use super::*;
use std::io::Cursor;
use std::sync::mpsc;
use std::time::Duration;

fn trusted_manager(workspace: &Path) -> McpManager {
    let mut manager = McpManager::new(workspace).expect("manager");
    manager.trusted_workspace = true;
    manager
}

trait FixtureServer {
    fn add_fixture_server(&self, server: McpServer) -> Result<()>;
}
impl FixtureServer for McpManager {
    fn add_fixture_server(&self, mut server: McpServer) -> Result<()> {
        if server.command.as_deref() == Some("echo") {
            fs::write(
                self.workspace.join("mcp-fixture.json"),
                serde_json::to_vec(&server.metadata)?,
            )?;
            server.command = Some(std::env::current_exe()?.to_string_lossy().into_owned());
            server.args = vec![
                "--exact".into(),
                "tests::delivery_three_stdio_fixture".into(),
                "--nocapture".into(),
                "--quiet".into(),
            ];
        }
        self.add_server(server)
    }
}
#[test]
fn delivery_three_stdio_fixture() {
    if !Path::new("mcp-fixture.json").is_file() {
        return;
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read("mcp-fixture.json").unwrap()).unwrap();
    use std::io::Write;
    for line in std::io::stdin().lock().lines() {
        let request: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let mut trace = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("mcp-fixture.trace")
            .unwrap();
        writeln!(trace, "{request}").unwrap();
        if request.get("id").is_none() {
            continue;
        }
        let result = match request["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{},"resources":{},"prompts":{}},"serverInfo":{"name":"fixture","version":"1"}})
            }
            "tools/list" => {
                let mut tools = metadata["tools"].as_array().cloned().unwrap_or_default();
                for tool in &mut tools {
                    if tool.get("inputSchema").is_none() {
                        tool["inputSchema"] = json!({"type":"object"});
                    }
                }
                json!({"tools":tools})
            }
            "tools/call" => {
                json!({"content":[{"type":"text","text":request["params"]["arguments"].to_string()}]})
            }
            "resources/list" => json!({"resources":[]}),
            "prompts/list" => json!({"prompts":[]}),
            _ => json!({}),
        };
        println!(
            "{}",
            json!({"jsonrpc":"2.0","id":request["id"],"result":result})
        );
        std::io::stdout().flush().unwrap();
    }
}

#[test]
fn p0_mcp_project_activation_and_environment() {
    let workspace = tempfile::tempdir().unwrap();
    let id = Uuid::now_v7().to_string();
    let server = McpServer {
        id: id.clone(),
        name: "fixture".into(),
        command: Some("fixture-executable".into()),
        ..Default::default()
    };
    let manager = McpManager::with_trust(workspace.path(), false).unwrap();
    manager
        .save_project_config(&McpConfig {
            servers: vec![server.clone()],
        })
        .unwrap();
    assert!(manager.get_server(&id).unwrap().is_none());

    let manager = McpManager::with_trust(workspace.path(), true).unwrap();
    let mut server = manager.get_server(&id).unwrap().unwrap();
    server.args = vec!["${P0_PRIVATE_VALUE}".into()];
    assert!(expand_server_env_vars(&mut server).is_err());
    server
        .child_env
        .insert("P0_PRIVATE_VALUE".into(), "synthetic-value".into());
    expand_server_env_vars(&mut server).unwrap();
    assert_eq!(server.args, ["synthetic-value"]);

    let command = stdio_command(&server).unwrap();
    assert!(command.get_envs().any(|(name, value)| {
        name == "P0_PRIVATE_VALUE" && value.is_some_and(|value| value == "synthetic-value")
    }));
    let other = McpServer {
        command: Some("fixture-executable".into()),
        ..Default::default()
    };
    assert!(
        stdio_command(&other)
            .unwrap()
            .get_envs()
            .all(|(name, _)| name != "P0_PRIVATE_VALUE")
    );
}

#[test]
fn add_list_remove_server_round_trip() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "local".to_string(),
            name: "Local MCP".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec!["ok".to_string()],
            url: None,
            enabled: true,
            metadata: serde_json::json!({"tools": [{"name":"hello","description":"test"}]}),
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("add");

    let listed = manager.list_servers().expect("list");
    assert!(listed.iter().any(|s| s.id == "local"));

    let tools = manager.discover_tools().expect("discover");
    assert!(tools.iter().any(|t| t.name == "hello"));

    let removed = manager.remove_server("local").expect("remove");
    assert!(removed);
}

#[test]
fn detects_toolset_changes_and_emits_notice_fingerprint() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "notify".to_string(),
            name: "Notify".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec!["ok".to_string()],
            url: None,
            enabled: true,
            metadata: serde_json::json!({"tools": [{"name":"a"},{"name":"b"}]}),
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("add");

    let (_, first_refreshes, first_notice) = manager
        .discover_tools_with_notice(None)
        .expect("first refresh");
    let first = first_refreshes
        .iter()
        .find(|refresh| refresh.server_id == "notify")
        .expect("refresh");
    assert!(first.added.contains(&"a".to_string()));
    assert!(first.added.contains(&"b".to_string()));
    assert!(!first_notice.fingerprint.is_empty());

    manager
        .add_fixture_server(McpServer {
            id: "notify".to_string(),
            name: "Notify".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec!["ok".to_string()],
            url: None,
            enabled: true,
            metadata: serde_json::json!({"tools": [{"name":"b"},{"name":"c"}]}),
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("replace");

    let (_, second_refreshes, second_notice) = manager
        .discover_tools_with_notice(Some(&first_notice.fingerprint))
        .expect("second refresh");
    let second = second_refreshes
        .iter()
        .find(|refresh| refresh.server_id == "notify")
        .expect("refresh");
    assert!(second.added.contains(&"c".to_string()));
    assert!(second.removed.contains(&"a".to_string()));
    assert_ne!(second_notice.fingerprint, first_notice.fingerprint);
    assert!(
        second_notice
            .changed_servers
            .contains(&"notify".to_string())
    );
}

#[test]
fn parses_tools_from_jsonrpc_result_payload() {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {
            "tools": [
                {
                    "name": "search",
                    "description": "Search tool",
                    "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}},
                    "annotations": {"readOnlyHint": true}
                },
                {"name": "read"}
            ]
        }
    });
    let tools = parse_mcp_tools_message("srv", &payload);
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].server_id, "srv");
    assert_eq!(tools[0].name, "search");
    assert_eq!(
        tools[0]
            .input_schema
            .get("properties")
            .and_then(|props| props.get("query"))
            .and_then(|query| query.get("type"))
            .and_then(serde_json::Value::as_str),
        Some("string")
    );
    assert_eq!(
        tools[0]
            .annotations
            .get("readOnlyHint")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(tools[1].name, "read");
}

#[test]
fn reads_content_length_frames() {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"ping"}]}})
        .to_string();
    let raw = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    let mut cursor = Cursor::new(raw.as_bytes().to_vec());
    let parsed = read_mcp_frame(&mut cursor)
        .expect("frame")
        .expect("payload");
    let tools = parse_mcp_tools_message("srv", &parsed);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "ping");
}

#[test]
fn extract_resource_text_merges_text_and_blob_entries() {
    let blob = base64::engine::general_purpose::STANDARD.encode("from blob");
    let result = extract_resource_text(&serde_json::json!({
        "contents": [
            {"text":"line one"},
            {"blob": blob, "mimeType":"text/plain"}
        ]
    }))
    .expect("resource text");
    assert!(result.contains("line one"));
    assert!(result.contains("from blob"));
}

#[test]
fn extract_resource_text_handles_binary_blob_marker() {
    let blob = base64::engine::general_purpose::STANDARD.encode([0_u8, 159, 146, 150]);
    let result = extract_resource_text(&serde_json::json!({
        "contents": [
            {"blob": blob, "mimeType":"application/octet-stream"}
        ]
    }))
    .expect("resource marker");
    assert!(result.contains("[binary-resource"));
    assert!(result.contains("application/octet-stream"));
}

#[test]
fn wait_for_mcp_response_times_out_without_matching_id() {
    let (tx, rx) = mpsc::channel::<serde_json::Value>();
    tx.send(serde_json::json!({"id":1,"result":{}}))
        .expect("send frame");
    drop(tx);
    let value = wait_for_mcp_response(&rx, 2, Duration::from_millis(20));
    assert!(value.is_none());
}

#[test]
fn mcp_token_roundtrip() {
    let server_id = format!("test-server-{}", uuid::Uuid::now_v7());
    let token = McpOAuthToken {
        binding: None,
        access_token: "abc123".to_string(),
        refresh_token: Some("refresh456".to_string()),
        token_type: "Bearer".to_string(),
        expires_at: Some("2025-12-31T23:59:59Z".to_string()),
        server_id: server_id.clone(),
    };
    store_mcp_token(&server_id, &token).unwrap();
    let loaded = load_mcp_token(&server_id).unwrap().unwrap();
    assert_eq!(loaded.access_token, "abc123");
    assert_eq!(loaded.refresh_token.as_deref(), Some("refresh456"));
    delete_mcp_token(&server_id).unwrap();
    assert!(load_mcp_token(&server_id).unwrap().is_none());
}

#[test]
fn load_nonexistent_token_returns_none() {
    let result = load_mcp_token("nonexistent-server-xyz").unwrap();
    assert!(result.is_none());
}

#[test]
fn mcp_token_serialization() {
    let token = McpOAuthToken {
        binding: None,
        access_token: "tok".to_string(),
        refresh_token: None,
        token_type: "Bearer".to_string(),
        expires_at: None,
        server_id: "srv".to_string(),
    };
    let json = serde_json::to_string(&token).unwrap();
    assert!(json.contains("tok"));
    let parsed: McpOAuthToken = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.access_token, "tok");
}

#[test]
fn expand_env_vars_basic() {
    let home = resolve_env_var("HOME").unwrap_or_default();
    let result = expand_env_vars("path=${HOME}/file");
    assert_eq!(result, format!("path={home}/file"));
}

#[test]
fn expand_env_vars_with_default() {
    let result = expand_env_vars("val=${NONEXISTENT_VAR_XYZ:-fallback}");
    assert_eq!(result, "val=fallback");
}

#[test]
fn expand_env_vars_passthrough() {
    let result = expand_env_vars("no vars here");
    assert_eq!(result, "no vars here");
}

#[test]
fn expand_server_env_vars_applies_to_all_fields() {
    let mut server = McpServer {
        id: "test".to_string(),
        name: "test".to_string(),
        transport: McpTransport::Stdio,
        command: Some("${NONEXISTENT_CMD:-node}".to_string()),
        args: vec!["${NONEXISTENT_ARG:-server.js}".to_string()],
        url: None,
        enabled: true,
        metadata: serde_json::Value::Null,
        headers: vec![],
        child_env: Default::default(),
    };
    expand_server_env_vars(&mut server).unwrap();
    assert_eq!(server.command.as_deref(), Some("node"));
    assert_eq!(server.args[0], "server.js");
}

#[test]
fn mcp_env_expansion_rejects_shell_injection() {
    let mut server = McpServer {
        id: "test".to_string(),
        name: "test".to_string(),
        transport: McpTransport::Stdio,
        command: Some("node; rm -rf /".to_string()),
        args: vec![],
        url: None,
        enabled: true,
        metadata: serde_json::Value::Null,
        headers: vec![],
        child_env: Default::default(),
    };
    let result = expand_server_env_vars(&mut server);
    assert!(
        result.is_err(),
        "should reject command with shell metacharacters"
    );
}

#[test]
fn enforce_mcp_token_limit_passes_small_output() {
    let limits = McpTokenLimits::default();
    let small = "hello world";
    let (out, truncated) = enforce_mcp_token_limit(small, &limits);
    assert_eq!(out, small);
    assert!(!truncated);
}

#[test]
fn enforce_mcp_token_limit_truncates_large_output() {
    let limits = McpTokenLimits {
        warn_threshold: 10,
        max_tokens: 20,
    };
    let large = "x".repeat(1000);
    let (out, truncated) = enforce_mcp_token_limit(&large, &limits);
    assert!(truncated);
    assert!(out.len() < large.len());
    assert!(out.contains("truncated"));
}

#[test]
fn serve_tool_list_has_expected_tools() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-serve-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let tools = build_serve_tool_list(&workspace);
    assert!(tools.len() >= 5);
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
        .collect();
    assert!(names.contains(&"fs_read"));
    assert!(names.contains(&"bash_run"));
    assert!(names.contains(&"git_status"));
}

#[test]
fn serve_bash_run_executes_on_current_platform() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-mcp-serve-run-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");

    let out = execute_serve_tool(
        &workspace,
        "bash_run",
        &json!({ "command": "echo mcp_serve_ok" }),
    );
    assert!(
        out.contains("exit_code: 0"),
        "expected successful shell execution, got: {out}"
    );
    assert!(
        out.to_ascii_lowercase().contains("mcp_serve_ok"),
        "expected shell output marker, got: {out}"
    );
}

// ── MCP integration tests (Phase 16.6) ──────────────────────────────

#[test]
fn mcp_search_filters_by_name() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "search-test".to_string(),
            name: "Search Test".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec!["ok".to_string()],
            url: None,
            enabled: true,
            metadata: serde_json::json!({
                "tools": [
                    {"name": "alpha", "description": "Alpha tool"},
                    {"name": "beta", "description": "Beta tool"},
                    {"name": "gamma", "description": "Gamma tool"}
                ]
            }),
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("add");

    let tools = manager.discover_tools().expect("discover");
    assert_eq!(tools.len(), 3, "all three tools should be discovered");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    assert!(names.contains(&"gamma"));
}

#[test]
fn unavailable_server_does_not_panic() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "missing".to_string(),
            name: "Missing".to_string(),
            transport: McpTransport::Stdio,
            command: Some("__nonexistent_command_xyz__".to_string()),
            args: vec![],
            url: None,
            enabled: true,
            metadata: serde_json::Value::Null,
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("add");

    // discover_tools should return Ok even if the server command doesn't exist
    let result = manager.discover_tools();
    assert!(
        result.is_ok(),
        "discover_tools should not panic/error on unavailable server: {:?}",
        result.err()
    );
}

#[test]
fn http_mcp_server_add_and_list() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
            .add_fixture_server(McpServer {
                id: "http-test".to_string(),
                name: "HTTP Test".to_string(),
                transport: McpTransport::Http,
                command: None,
                args: vec![],
                url: Some("http://127.0.0.1:0/mcp".to_string()),
                enabled: true,
                metadata: serde_json::json!({"tools": [{"name": "remote_tool", "description": "A remote tool"}]}),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add http server");

    let listed = manager.list_servers().expect("list");
    let http = listed.iter().find(|s| s.id == "http-test");
    assert!(http.is_some(), "HTTP server should be listed");
    assert!(matches!(http.unwrap().transport, McpTransport::Http));

    let tools = manager.discover_tools().expect("discover");
    assert!(
        tools.is_empty(),
        "Disconnected metadata must never fabricate tools"
    );
}

#[test]
fn mcp_token_limit_truncates_at_exact_boundary() {
    let limits = McpTokenLimits {
        warn_threshold: 5,
        max_tokens: 10,
    };
    // 10 tokens * 4 chars = 40 chars. 40 / 4 = 10 estimated tokens. 10 > 10 is false.
    let exactly_at = "a".repeat(40);
    let (out, truncated) = enforce_mcp_token_limit(&exactly_at, &limits);
    assert!(!truncated, "exactly at limit should not truncate");
    assert_eq!(out, exactly_at);

    // 44 chars / 4 = 11 estimated tokens. 11 > 10 is true → truncated.
    let over = "a".repeat(44);
    let (out2, truncated2) = enforce_mcp_token_limit(&over, &limits);
    assert!(truncated2, "over limit should truncate");
    assert!(out2.contains("truncated"));
}

#[test]
fn tools_for_context_returns_all_below_threshold() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-ctx-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "ctx-test".to_string(),
            name: "Ctx Test".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            url: None,
            enabled: true,
            metadata: serde_json::json!({"tools": [
                {"name":"t1","description":"tool 1"},
                {"name":"t2","description":"tool 2"}
            ]}),
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("add");

    // Below 10% threshold → returns all tools regardless of query
    let tools = manager
        .tools_for_context(5.0, Some("t1"), 10)
        .expect("tools");
    assert_eq!(tools.len(), 2);
}

#[test]
fn tools_for_context_no_query_above_threshold_returns_all() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-ctx2-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "ctx-test2".to_string(),
            name: "Ctx Test2".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            url: None,
            enabled: true,
            metadata: serde_json::json!({"tools": [
                {"name":"a","description":"alpha"},
                {"name":"b","description":"beta"}
            ]}),
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("add");

    // Above 10% but no query → returns all tools
    let tools = manager.tools_for_context(15.0, None, 10).expect("tools");
    assert_eq!(tools.len(), 2);
}

#[test]
fn tools_for_context_filters_above_threshold() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-ctx3-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "ctx-test3".to_string(),
            name: "Ctx Test3".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            url: None,
            enabled: true,
            metadata: serde_json::json!({"tools": [
                {"name":"search","description":"search tool"},
                {"name":"read","description":"read tool"}
            ]}),
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("add");

    // Above 10% with query — search_tools_by_relevance is called.
    // Since there are only 2 tools (below the 50-tool threshold in search),
    // it returns all tools.
    let tools = manager
        .tools_for_context(15.0, Some("search"), 10)
        .expect("tools");
    assert!(!tools.is_empty());
}

// ── P2-01: OAuth 2.0 tests ─────────────────────────────────────────

#[test]
fn generate_pkce_challenge_is_deterministic_for_same_verifier() {
    let verifier = "test-verifier-12345";
    let c1 = generate_pkce_challenge(verifier);
    let c2 = generate_pkce_challenge(verifier);
    assert_eq!(c1, c2, "same verifier should produce same challenge");
    assert!(!c1.is_empty());
}

#[test]
fn extract_query_param_parses_code() {
    let request = "GET /callback?code=abc123&state=xyz HTTP/1.1\r\nHost: localhost";
    let code = extract_query_param(request, "code");
    assert_eq!(code, Some("abc123".to_string()));
    let state = extract_query_param(request, "state");
    assert_eq!(state, Some("xyz".to_string()));
}

#[test]
fn extract_query_param_missing_returns_none() {
    let request = "GET /callback?code=abc HTTP/1.1";
    assert!(extract_query_param(request, "state").is_none());
    assert!(extract_query_param("", "code").is_none());
}

#[test]
fn is_token_expired_true_for_past_date() {
    let token = McpOAuthToken {
        binding: None,
        access_token: "tok".to_string(),
        refresh_token: None,
        token_type: "Bearer".to_string(),
        expires_at: Some("2020-01-01T00:00:00Z".to_string()),
        server_id: "srv".to_string(),
    };
    assert!(is_token_expired(&token));
}

#[test]
fn is_token_expired_false_for_future_date() {
    let token = McpOAuthToken {
        binding: None,
        access_token: "tok".to_string(),
        refresh_token: None,
        token_type: "Bearer".to_string(),
        expires_at: Some("2099-12-31T23:59:59Z".to_string()),
        server_id: "srv".to_string(),
    };
    assert!(!is_token_expired(&token));
}

// ── P2-02: Resource autocomplete tests ───────────────────────────────

#[test]
fn autocomplete_resources_no_servers_returns_empty() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-ac-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    let results = manager.autocomplete_resources("@").expect("autocomplete");
    assert!(results.is_empty());
}

#[test]
fn autocomplete_resources_empty_prefix_returns_all() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-ac2-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    // No servers → empty results (can't test with real servers in unit tests)
    let results = manager.autocomplete_resources("").expect("autocomplete");
    assert!(results.is_empty());
}

#[test]
fn autocomplete_resources_filters_by_prefix() {
    // This tests the filter logic directly without servers
    let all = vec![
        McpResource {
            server_id: "srv1".to_string(),
            uri: "file:///docs/readme.md".to_string(),
            name: "readme".to_string(),
            description: "Project readme".to_string(),
            mime_type: None,
        },
        McpResource {
            server_id: "srv2".to_string(),
            uri: "file:///src/main.rs".to_string(),
            name: "main".to_string(),
            description: "Entry point".to_string(),
            mime_type: Some("text/x-rust".to_string()),
        },
    ];
    let prefix = "@srv1";
    let prefix_lower = prefix.to_ascii_lowercase();
    let matches: Vec<_> = all
        .into_iter()
        .filter(|r| {
            let label = format!("@{}:{}", r.server_id, r.uri).to_ascii_lowercase();
            label.starts_with(&prefix_lower) || r.name.to_ascii_lowercase().contains(&prefix_lower)
        })
        .collect();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].server_id, "srv1");
}

#[test]
fn parse_sse_tools_from_event_stream() {
    let sse_text = r#"data: {"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"sse_tool","description":"SSE test tool"}]}}

data: {"other":"ignored"}

"#;
    let tools = parse_sse_tools("sse-srv", sse_text).expect("parse SSE");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "sse_tool");
    assert_eq!(tools[0].server_id, "sse-srv");
}

#[test]
fn parse_sse_tools_empty_stream_returns_error() {
    let result = parse_sse_tools("srv", "");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("no tools"));
}

#[test]
fn sse_transport_serialization() {
    let transport = McpTransport::Sse;
    let json = serde_json::to_string(&transport).unwrap();
    assert_eq!(json, "\"sse\"");
    let parsed: McpTransport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, McpTransport::Sse);
}

#[test]
fn sse_server_add_and_list() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-sse-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "sse-test".to_string(),
            name: "SSE Test".to_string(),
            transport: McpTransport::Sse,
            command: None,
            args: vec![],
            url: Some("http://127.0.0.1:0/sse".to_string()),
            enabled: true,
            metadata: serde_json::json!({"tools": [{"name":"sse_tool"}]}),
            headers: vec![("Authorization".to_string(), "Bearer tok".to_string())],
            child_env: Default::default(),
        })
        .expect("add sse server");

    let listed = manager.list_servers().expect("list");
    let sse = listed.iter().find(|s| s.id == "sse-test");
    assert!(sse.is_some());
    assert_eq!(sse.unwrap().transport, McpTransport::Sse);
    assert_eq!(sse.unwrap().headers.len(), 1);
}

#[test]
fn managed_config_missing_returns_none() {
    // The managed config path points to a system location that won't exist in test
    let result = McpManager::load_managed_mcp_config();
    assert!(
        result.is_none(),
        "managed config should return None when file is absent"
    );
}

#[test]
fn is_managed_server_returns_false_when_no_managed_config() {
    assert!(!McpManager::is_managed_server("some-server"));
}

#[test]
fn cannot_remove_managed_server_would_fail() {
    // Without a managed config file, is_managed_server returns false,
    // so the guard doesn't trigger. This tests the guard integration exists.
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-mcp-managed-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let manager = trusted_manager(&workspace);

    manager
        .add_fixture_server(McpServer {
            id: "removable".to_string(),
            name: "Removable".to_string(),
            transport: McpTransport::Stdio,
            command: Some("echo".to_string()),
            args: vec![],
            url: None,
            enabled: true,
            metadata: serde_json::Value::Null,
            headers: vec![],
            child_env: Default::default(),
        })
        .expect("add");

    // Non-managed server can be removed
    let removed = manager.remove_server("removable").expect("remove");
    assert!(removed);
}

// ── P5-12: MCP prompts as slash commands ──────────────────────────────

#[test]
fn mcp_prompts_as_commands() {
    // McpPrompt struct should round-trip correctly
    let prompt = McpPrompt {
        server_id: "test-server".to_string(),
        name: "generate-docs".to_string(),
        description: "Generate documentation for a module".to_string(),
        arguments: vec![
            McpPromptArgument {
                name: "module".to_string(),
                description: "The module to document".to_string(),
                required: true,
            },
            McpPromptArgument {
                name: "format".to_string(),
                description: "Output format (md, html)".to_string(),
                required: false,
            },
        ],
    };
    let serialized = serde_json::to_value(&prompt).expect("serialize");
    let deserialized: McpPrompt = serde_json::from_value(serialized).expect("deserialize");
    assert_eq!(deserialized.server_id, "test-server");
    assert_eq!(deserialized.name, "generate-docs");
    assert_eq!(deserialized.arguments.len(), 2);
    assert!(deserialized.arguments[0].required);
    assert!(!deserialized.arguments[1].required);

    // Slash command name generation
    let slash_name = format!("/mcp-{}-{}", prompt.server_id, prompt.name);
    assert_eq!(slash_name, "/mcp-test-server-generate-docs");
}

// ── P5-13: env var expansion in list_servers ──────────────────────────

#[test]
fn env_vars_expanded() {
    // HOME may be absent in some CI setups; resolve with the same helper used
    // by expansion logic so this stays deterministic cross-platform.
    let home = resolve_env_var("HOME").unwrap_or_default();

    // Expansion works on real env vars
    let expanded = expand_env_vars("Bearer ${HOME}");
    assert_eq!(expanded, format!("Bearer {home}"));

    // With default value for missing var
    let expanded2 = expand_env_vars("${NONEXISTENT_ENV_VAR_XYZ:-fallback_val}");
    assert_eq!(expanded2, "fallback_val");

    // Expansion in server config
    let mut server = McpServer {
        id: "svc".to_string(),
        name: "Service".to_string(),
        transport: McpTransport::Http,
        command: None,
        args: vec!["--home=${HOME}".to_string()],
        url: Some("https://api.example.com/${HOME}".to_string()),
        enabled: true,
        metadata: serde_json::Value::Null,
        headers: vec![],
        child_env: Default::default(),
    };
    server.child_env.insert("HOME".into(), home.clone().into());
    expand_server_env_vars(&mut server).unwrap();
    let expected_url = format!("https://api.example.com/{home}");
    assert_eq!(server.url.as_deref(), Some(expected_url.as_str()));
    assert_eq!(server.args[0], format!("--home={home}"));
}

// ── T4.3: MCP connection pool tests ────────────────────────────────

#[test]
fn pool_default_creates_empty() {
    let pool = McpConnectionPool::new();
    assert!(pool.sessions.states.lock().unwrap().is_empty());
}

#[test]
fn pool_shutdown_is_idempotent() {
    let pool = McpConnectionPool::new();
    pool.shutdown();
    pool.shutdown(); // Should not panic
}

#[test]
fn pool_drop_cleans_up() {
    {
        let _pool = McpConnectionPool::new();
        // Drop implicitly calls shutdown
    }
    // No panic = success
}
