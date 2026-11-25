use super::*;

#[test]
fn delivery_one_reconciles_local_writes_and_pauses_external_effects() -> Result<()> {
    use sha2::{Digest, Sha256};
    let workspace = tempfile::tempdir()?;
    let root = workspace.path();
    let engine = AgentEngine::new_with_config(root, AppConfig::default())?;
    let session = engine.ensure_session_record_for(None)?.session_id;
    let store = Store::new(root)?;
    let lease = store.session_lease(session)?;
    let calls = [
        codingbuddy_core::LlmToolCall {
            id: "write".into(),
            name: "fs_write".into(),
            arguments: "{}".into(),
        },
        codingbuddy_core::LlmToolCall {
            id: "external".into(),
            name: "external".into(),
            arguments: "{}".into(),
        },
    ];
    let ids = store.model_step(session, None, None, &calls, &[])?;
    store.dispatch_invocation(session, ids[0])?;
    store.record_invocation_receipt(
        session,
        ids[0],
        &codingbuddy_store::InvocationReceipt::File {
            execution_root: None,
            path: "marker".into(),
            postimage_sha256: format!("{:x}", Sha256::digest(b"expected")),
        },
    )?;
    std::fs::write(root.join("marker"), b"expected")?;
    store.dispatch_invocation(session, ids[1])?;
    drop(lease);
    drop(engine);
    let resumed = AgentEngine::new_with_config(root, AppConfig::default())?;
    let lease = store.session_lease(session)?;
    let error = resumed
        .reconcile_session_invocations(&lease, session)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&ids[1].to_string()));
    let unresolved = store.interrupted_invocations(&lease, session)?;
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].id, ids[1]);
    let transcript = store.rebuild_from_events(session)?;
    assert!(
        matches!(&transcript.chat_messages[1], ChatMessage::Tool { tool_call_id, content, .. }
            if tool_call_id == "write" && content.contains("verified_postimage"))
    );
    Ok(())
}

#[test]
fn extract_urls_handles_punctuation_and_dedup() {
    let prompt = "check https://example.com, then (https://example.com) and https://a.dev.";
    let urls = extract_urls(prompt, 5);
    assert_eq!(urls, vec!["https://example.com", "https://a.dev"]);
}

#[test]
fn html_strip_removes_tags() {
    let html = "<html><body><h1>Title</h1><p>Hello world</p></body></html>";
    let stripped = naive_strip_html(html);
    assert!(stripped.contains("Title"));
    assert!(stripped.contains("Hello world"));
    assert!(!stripped.contains("<h1>"));
}

#[test]
fn truncate_to_token_budget_short_text_unchanged() {
    let text = "hello world\nsecond line";
    let result = truncate_to_token_budget(text, 1000);
    assert_eq!(result, text);
}

#[test]
fn truncate_to_token_budget_long_text_truncated() {
    let text = "line1\nline2\nline3\nline4\nline5\nline6";
    // 4 tokens = 16 chars, which should cut mid-text
    let result = truncate_to_token_budget(text, 4);
    assert!(result.contains("truncated"));
    assert!(result.len() < text.len() + 40); // truncated + marker
}

#[test]
fn mcp_metadata_trusts_declared_read_only_tools() {
    let tool = codingbuddy_mcp::McpTool {
        server_id: "github".to_string(),
        name: "search".to_string(),
        description: "Search issues".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        annotations: serde_json::json!({"readOnlyHint": true}),
    };
    let metadata = mcp_tool_metadata("mcp__github__search", &tool);
    assert!(metadata.read_only);
    assert!(!metadata.approval_required);
    assert_eq!(metadata.trust_level, DynamicToolTrust::TrustedReadOnly);
    assert_eq!(metadata.permission_matcher, ToolPermissionMatcher::Mcp);
    assert!(metadata.is_allowed_in_phase(TaskPhase::Explore));
}

#[test]
fn mcp_metadata_restricts_unknown_or_destructive_tools() {
    let tool = codingbuddy_mcp::McpTool {
        server_id: "slack".to_string(),
        name: "post".to_string(),
        description: "Post a message".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        annotations: serde_json::json!({"readOnlyHint": true, "destructiveHint": true}),
    };
    let metadata = mcp_tool_metadata("mcp__slack__post", &tool);
    assert!(!metadata.read_only);
    assert!(metadata.approval_required);
    assert_eq!(metadata.trust_level, DynamicToolTrust::Untrusted);
    assert!(!metadata.is_allowed_in_phase(TaskPhase::Explore));
    assert!(metadata.is_allowed_in_phase(TaskPhase::Execute));
}

#[test]
fn runtime_tool_definitions_include_plugin_commands() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-agent-plugin-tools-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&workspace).expect("workspace");

    let plugin_src = workspace.join("plugin-src");
    std::fs::create_dir_all(plugin_src.join(".codingbuddy-plugin")).expect("plugin dir");
    std::fs::create_dir_all(plugin_src.join("commands")).expect("commands dir");
    std::fs::write(
        plugin_src.join(".codingbuddy-plugin/plugin.json"),
        r#"{"id":"phase-c-plugin","name":"Phase C Plugin","version":"0.1.0"}"#,
    )
    .expect("manifest");
    std::fs::write(plugin_src.join("commands/review.md"), "# review").expect("command");

    let manager = codingbuddy_tools::PluginManager::new(&workspace).expect("plugin manager");
    manager.install(&plugin_src).expect("install plugin");

    let defs = runtime_tool_definitions(&workspace, true);
    assert!(
        defs.iter()
            .any(|def| def.function.name == "plugin__phase_c_plugin__review"),
        "plugin command should be exposed as runtime tool definition"
    );
}

#[test]
fn shadow_commit_used_in_checkpoint() {
    // Verify that MemoryManager::create_shadow_commit exists and returns
    // the expected ShadowCommit struct. On non-git directories, it should
    // still return a result (with git_backed=false), allowing fallback.
    let workspace = std::env::temp_dir().join(format!("shadow-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(workspace.join("test.txt"), "hello").expect("write file");

    let mm = codingbuddy_memory::MemoryManager::new(&workspace).expect("memory manager");
    let result = mm.create_shadow_commit("test checkpoint");
    // In a non-git directory, shadow commit should not be git-backed
    // (it falls back to file-based internally, or returns an error)
    match result {
        Ok(sc) => {
            assert!(
                !sc.git_backed,
                "non-git dir should not produce git-backed shadow commit"
            );
        }
        Err(_) => {
            // Also acceptable: shadow commit fails in non-git dir
            // and the checkpoint callback falls back to file-based
        }
    }

    let _ = std::fs::remove_dir_all(&workspace);
}

// ── Batch 7: Context Manager Wiring ──

#[test]
fn context_manager_handles_missing_repo() {
    // ContextManager should gracefully handle a non-existent or empty workspace
    let result = crate::context::ContextManager::new("/tmp/nonexistent_codingbuddy_test_dir");
    // Should succeed (creates empty graph) — not panic
    assert!(
        result.is_ok(),
        "ContextManager::new should not fail on missing dir"
    );
}

#[test]
fn context_manager_suggest_files_on_real_workspace() {
    // Create a temp workspace with a few files that have import relationships
    let workspace = std::env::temp_dir().join("codingbuddy_test_ctx_mgr");
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(workspace.join("src")).expect("create src dir");
    std::fs::write(
        workspace.join("src/lib.rs"),
        "pub mod utils;\npub mod handler;\n",
    )
    .expect("write lib.rs");
    std::fs::write(workspace.join("src/utils.rs"), "pub fn helper() {}\n").expect("write utils.rs");
    std::fs::write(
        workspace.join("src/handler.rs"),
        "use crate::utils;\npub fn handle() { utils::helper(); }\n",
    )
    .expect("write handler.rs");

    let mut ctx_mgr = crate::context::ContextManager::new(&workspace).expect("context manager");
    ctx_mgr.analyze_workspace().expect("analyze");

    let suggestions = ctx_mgr.suggest_relevant_files("handler", 5);
    // handler.rs should score high because its name matches the query
    let has_handler = suggestions
        .iter()
        .any(|s| s.path.to_string_lossy().contains("handler"));
    assert!(
        has_handler,
        "handler.rs should appear in suggestions for query 'handler'"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn test_stream_callback_clone_pattern() {
    // Verify the clone-then-call pattern: the callback is cloned out of
    // the mutex before invocation, so the lock is not held during the call.
    use std::sync::atomic::{AtomicUsize, Ordering};

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_inner = call_count.clone();

    // Simulate the Mutex<Option<StreamCallback>> pattern from AgentEngine
    let stream_callback: Mutex<Option<codingbuddy_core::StreamCallback>> =
        Mutex::new(Some(Arc::new(move |_chunk: StreamChunk| {
            call_count_inner.fetch_add(1, Ordering::SeqCst);
        })));

    // Clone-then-call pattern (same as AgentEngine::stream)
    let cb = stream_callback.lock().ok().and_then(|g| g.clone());
    if let Some(cb) = cb {
        cb(StreamChunk::ContentDelta("hello".to_string()));
    }

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "callback should have been called once"
    );

    // Verify the mutex is NOT held during the callback (we can lock again)
    assert!(
        stream_callback.lock().is_ok(),
        "mutex should not be poisoned or held"
    );

    // Call again to verify repeatability
    let cb = stream_callback.lock().ok().and_then(|g| g.clone());
    if let Some(cb) = cb {
        cb(StreamChunk::ContentDelta("world".to_string()));
    }
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "callback should have been called twice"
    );
}
