use super::*;
use proptest::prelude::*;
use serde_json::json;

fn sample_tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: name.to_string(),
            description: format!("Tool {name}"),
            strict: None,
            parameters: json!({}),
        },
    }
}

fn session_state_strategy() -> impl Strategy<Value = SessionState> {
    prop_oneof![
        Just(SessionState::Idle),
        Just(SessionState::Planning),
        Just(SessionState::ExecutingStep),
        Just(SessionState::AwaitingApproval),
        Just(SessionState::Verifying),
        Just(SessionState::Completed),
        Just(SessionState::Paused),
        Just(SessionState::Failed),
    ]
}

proptest! {
    #[test]
    fn merge_json_value_is_idempotent_for_flat_objects(
        base in prop::collection::btree_map("[a-z]{1,8}", any::<i64>(), 0..12),
        overlay in prop::collection::btree_map("[a-z]{1,8}", any::<i64>(), 0..12),
    ) {
        let mut base_value = json!(base);
        let overlay_value = json!(overlay);
        merge_json_value(&mut base_value, &overlay_value);
        let once = base_value.clone();
        merge_json_value(&mut base_value, &overlay_value);
        prop_assert_eq!(base_value, once);
    }

    #[test]
    fn completed_state_does_not_jump_directly_to_execution(
        to in session_state_strategy()
    ) {
        if matches!(to, SessionState::ExecutingStep | SessionState::AwaitingApproval | SessionState::Verifying) {
            prop_assert!(!is_valid_session_state_transition(&SessionState::Completed, &to));
        }
    }
}

#[test]
fn session_state_transition_allows_expected_recovery_paths() {
    assert!(is_valid_session_state_transition(
        &SessionState::Failed,
        &SessionState::Planning
    ));
    assert!(is_valid_session_state_transition(
        &SessionState::Paused,
        &SessionState::ExecutingStep
    ));
    assert!(!is_valid_session_state_transition(
        &SessionState::Idle,
        &SessionState::Completed
    ));
}

#[test]
fn new_event_types_round_trip_via_serde() {
    let events = vec![
        EventKind::SessionStarted {
            session_id: Uuid::now_v7(),
            workspace: "/tmp/test".to_string(),
        },
        EventKind::SessionResumed {
            session_id: Uuid::now_v7(),
            events_replayed: 42,
        },
        EventKind::ToolDenied {
            invocation_id: Uuid::now_v7(),
            tool_name: "bash.run".to_string(),
            reason: "locked mode".to_string(),
        },
    ];
    for event in events {
        let serialized = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&serialized).expect("deserialize");
        let re_serialized = serde_json::to_string(&deserialized).expect("re-serialize");
        assert_eq!(serialized, re_serialized);
    }
}

#[test]
fn approval_mode_serde_roundtrip() {
    for (mode, expected) in [
        (ApprovalMode::Ask, "\"ask\""),
        (ApprovalMode::Always, "\"always\""),
        (ApprovalMode::Never, "\"never\""),
    ] {
        let serialized = serde_json::to_string(&mode).expect("serialize");
        assert_eq!(serialized, expected);
        let deserialized: ApprovalMode = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(deserialized, mode);
    }
}

#[test]
fn sandbox_mode_serde_roundtrip() {
    for (mode, expected) in [
        (SandboxMode::Allowlist, "\"allowlist\""),
        (SandboxMode::Isolated, "\"isolated\""),
        (SandboxMode::Off, "\"off\""),
        (SandboxMode::ReadOnly, "\"read-only\""),
        (SandboxMode::WorkspaceWrite, "\"workspace-write\""),
    ] {
        let serialized = serde_json::to_string(&mode).expect("serialize");
        assert_eq!(serialized, expected);
        let deserialized: SandboxMode = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(deserialized, mode);
    }
}

#[test]
fn permission_mode_serde_roundtrip() {
    for (mode, expected) in [
        (PermissionMode::Ask, "\"ask\""),
        (PermissionMode::Auto, "\"auto\""),
        (PermissionMode::Locked, "\"locked\""),
    ] {
        let serialized = serde_json::to_string(&mode).expect("serialize");
        assert_eq!(serialized, expected);
        let deserialized: PermissionMode = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(deserialized, mode);
    }
}

#[test]
fn tool_name_roundtrip_all_variants() {
    for &tool in ToolName::ALL {
        let api = tool.as_api_name();
        let internal = tool.as_internal();
        assert_eq!(
            ToolName::from_api_name(api),
            Some(tool),
            "from_api_name roundtrip failed for {api}"
        );
        assert_eq!(
            ToolName::from_internal_name(internal),
            Some(tool),
            "from_internal_name roundtrip failed for {internal}"
        );
    }
}

#[test]
fn canonical_tool_api_name_accepts_internal_and_variant_spellings() {
    assert_eq!(canonical_tool_api_name("fs_read"), Some("fs_read"));
    assert_eq!(canonical_tool_api_name("fs.read"), Some("fs_read"));
    assert_eq!(canonical_tool_api_name("FS-READ"), Some("fs_read"));
    assert_eq!(canonical_tool_api_name(" Git.Status "), Some("git_status"));
    assert_eq!(canonical_tool_api_name("plugin__custom__tool"), None);
}

#[test]
fn repair_tool_api_name_repairs_known_and_active_tool_names() {
    let tools = vec![
        sample_tool("fs_read"),
        sample_tool("fs_write"),
        sample_tool("plugin__phase_c_plugin__review"),
    ];

    assert_eq!(
        repair_tool_api_name("fs.read", &tools),
        Some("fs_read".to_string())
    );
    assert_eq!(
        repair_tool_api_name("FS-READ", &tools),
        Some("fs_read".to_string())
    );
    assert_eq!(
        repair_tool_api_name("fs_reaf", &tools),
        Some("fs_read".to_string())
    );
    assert_eq!(
        repair_tool_api_name("plugin__phase-c-plugin__review", &tools),
        Some("plugin__phase_c_plugin__review".to_string())
    );
    assert_eq!(
        repair_tool_api_name("plugin__custom__unknown", &tools),
        None
    );
}

#[test]
fn tool_phase_access_reflects_runtime_contract() {
    for name in &[
        "fs_read",
        "fs_list",
        "fs_glob",
        "fs_grep",
        "git_status",
        "git_diff",
        "git_show",
        "web_fetch",
        "web_search",
        "index_query",
        "notebook_read",
        "diagnostics_check",
        "extended_thinking",
        "tool_search",
        "todo_read",
    ] {
        let tool = ToolName::from_api_name(name).expect("known tool");
        assert!(tool.is_read_only(), "{name} should be read-only");
        assert!(
            tool.is_allowed_in_phase(TaskPhase::Explore),
            "{name} should be available in Explore"
        );
        assert!(
            tool.is_allowed_in_phase(TaskPhase::Plan),
            "{name} should be available in Plan"
        );
    }

    for name in &["fs_write", "fs_edit", "bash_run", "notebook_edit"] {
        let tool = ToolName::from_api_name(name).expect("known tool");
        assert!(!tool.is_read_only(), "{name} should not be read-only");
        assert!(
            !tool.is_allowed_in_phase(TaskPhase::Explore),
            "{name} should be blocked in Explore"
        );
    }

    for name in &[
        "task_create",
        "task_update",
        "todo_write",
        "spawn_task",
        "exit_plan_mode",
    ] {
        let tool = ToolName::from_api_name(name).expect("known tool");
        assert!(
            tool.is_allowed_in_phase(TaskPhase::Plan),
            "{name} should be available in Plan"
        );
    }

    let enter_plan = ToolName::from_api_name("enter_plan_mode").expect("enter_plan_mode");
    assert!(enter_plan.is_allowed_in_phase(TaskPhase::Explore));
    assert!(!enter_plan.is_allowed_in_phase(TaskPhase::Plan));
    let exit_plan = ToolName::from_api_name("exit_plan_mode").expect("exit_plan_mode");
    assert!(!exit_plan.is_allowed_in_phase(TaskPhase::Explore));
    assert!(exit_plan.is_allowed_in_phase(TaskPhase::Plan));
}

#[test]
fn tool_name_unknown_returns_none() {
    assert_eq!(ToolName::from_api_name("nonexistent_tool"), None);
    assert_eq!(ToolName::from_api_name("plugin__foo__bar"), None);
    assert_eq!(ToolName::from_api_name("mcp__server_tool"), None);
    assert_eq!(ToolName::from_internal_name("unknown.tool"), None);
}

#[test]
fn event_kind_all_variants_have_category() {
    // Verify a representative from each category returns a non-empty string
    let events = vec![
        EventKind::SessionStarted {
            session_id: Uuid::nil(),
            workspace: String::new(),
        },
        EventKind::ChatTurn {
            message: ChatMessage::User {
                content: String::new(),
            },
        },
        EventKind::ToolProposed {
            proposal: ToolProposal {
                invocation_id: Uuid::nil(),
                call: ToolCall {
                    name: "fs.read".to_string(),
                    args: serde_json::json!({}),
                    requires_approval: false,
                },
                approved: false,
            },
        },
        EventKind::TaskCreated {
            task_id: Uuid::nil(),
            title: String::new(),
            priority: 0,
        },
        EventKind::PluginEnabled {
            plugin_id: String::new(),
        },
        EventKind::HookExecuted {
            phase: String::new(),
            hook_path: String::new(),
            success: true,
            timed_out: false,
            exit_code: Some(0),
        },
    ];
    for event in &events {
        let cat = event.category();
        assert!(!cat.is_empty(), "category should not be empty");
    }
    // Verify specific categories
    assert!(events[0].is_session_event());
    assert!(!events[0].is_tool_event());
    assert!(events[2].is_tool_event());
    assert!(!events[2].is_session_event());
}

#[test]
fn parse_event_kind_compat_maps_legacy_router_event() {
    let raw = r#"{"type":"RouterDecisionV1","payload":{"decision":"v3","score":0.91}}"#;
    let kind = parse_event_kind_compat(raw).expect("compat parse");
    match kind {
        EventKind::TelemetryEvent { name, properties } => {
            assert_eq!(name, "legacy.router_decision");
            assert_eq!(properties["decision"], "v3");
            assert_eq!(properties["score"], 0.91);
        }
        other => panic!("unexpected mapped kind: {:?}", other),
    }
}

#[test]
fn parse_event_envelope_compat_maps_legacy_router_event() {
    let session_id = Uuid::now_v7();
    let raw = format!(
        r#"{{"seq_no":7,"at":"2026-02-23T00:00:00Z","session_id":"{}","kind":{{"type":"RouterEscalationV1","payload":{{"reason":"repeat failures"}}}}}}"#,
        session_id
    );
    let envelope = parse_event_envelope_compat(&raw).expect("compat envelope parse");
    match envelope.kind {
        EventKind::TelemetryEvent { name, properties } => {
            assert_eq!(name, "legacy.router_escalation");
            assert_eq!(properties["reason"], "repeat failures");
        }
        other => panic!("unexpected mapped kind: {:?}", other),
    }
}

#[test]
fn strip_prior_reasoning_clears_old_turns_keeps_current() {
    let mut messages = vec![
        ChatMessage::System {
            content: "system".to_string(),
        },
        ChatMessage::User {
            content: "first question".to_string(),
        },
        ChatMessage::Assistant {
            content: Some("answer 1".to_string()),
            native_parts: None,
            reasoning_content: Some("thinking about q1".to_string()),
            tool_calls: vec![],
        },
        // New user turn starts here
        ChatMessage::User {
            content: "second question".to_string(),
        },
        ChatMessage::Assistant {
            content: Some("tool call".to_string()),
            native_parts: None,
            reasoning_content: Some("thinking about q2".to_string()),
            tool_calls: vec![],
        },
    ];

    strip_prior_reasoning_content(&mut messages);

    // Prior turn (index 2): reasoning_content should be cleared
    if let ChatMessage::Assistant {
        reasoning_content, ..
    } = &messages[2]
    {
        assert!(
            reasoning_content.is_none(),
            "prior turn reasoning should be stripped"
        );
    } else {
        panic!("expected assistant at index 2");
    }

    // Current turn (index 4, after last User): reasoning_content should be kept
    if let ChatMessage::Assistant {
        reasoning_content, ..
    } = &messages[4]
    {
        assert_eq!(
            reasoning_content.as_deref(),
            Some("thinking about q2"),
            "current turn reasoning should be kept"
        );
    } else {
        panic!("expected assistant at index 4");
    }
}

#[test]
fn strip_prior_reasoning_no_user_messages_is_noop() {
    let mut messages = vec![
        ChatMessage::System {
            content: "system".to_string(),
        },
        ChatMessage::Assistant {
            content: Some("answer".to_string()),
            native_parts: None,
            reasoning_content: Some("thinking".to_string()),
            tool_calls: vec![],
        },
    ];
    strip_prior_reasoning_content(&mut messages);
    // No user messages → nothing stripped
    if let ChatMessage::Assistant {
        reasoning_content, ..
    } = &messages[1]
    {
        assert_eq!(reasoning_content.as_deref(), Some("thinking"));
    }
}

#[test]
fn strip_prior_reasoning_single_turn_preserves_all() {
    let mut messages = vec![
        ChatMessage::User {
            content: "question".to_string(),
        },
        ChatMessage::Assistant {
            content: Some("answer".to_string()),
            native_parts: None,
            reasoning_content: Some("thinking".to_string()),
            tool_calls: vec![],
        },
    ];
    strip_prior_reasoning_content(&mut messages);
    // Only one user turn → assistant after it is "current", reasoning kept
    if let ChatMessage::Assistant {
        reasoning_content, ..
    } = &messages[1]
    {
        assert_eq!(reasoning_content.as_deref(), Some("thinking"));
    }
}

// ── P0 estimate_message_tokens tests ──────────────────────────────

#[test]
fn estimate_message_tokens_basic() {
    // 400 chars → ~100 tokens
    let messages = vec![ChatMessage::User {
        content: "a".repeat(400),
    }];
    assert_eq!(estimate_message_tokens(&messages), 100);
}

#[test]
fn estimate_message_tokens_empty() {
    assert_eq!(estimate_message_tokens(&[]), 0);
}

#[test]
fn estimate_message_tokens_multi_role() {
    let messages = vec![
        ChatMessage::System {
            content: "x".repeat(100),
        },
        ChatMessage::User {
            content: "y".repeat(200),
        },
        ChatMessage::Assistant {
            content: Some("z".repeat(100)),
            native_parts: None,
            reasoning_content: Some("r".repeat(200)),
            tool_calls: vec![],
        },
    ];
    // (100 + 200 + 100 + 200) / 4 = 150
    assert_eq!(estimate_message_tokens(&messages), 150);
}

// ── P0 strip_reasoning_from_prior_turns tests ─────────────────────

#[test]
fn strip_reasoning_from_prior_turns() {
    let mut messages = vec![
        ChatMessage::User {
            content: "turn 1 question".to_string(),
        },
        ChatMessage::Assistant {
            content: Some("turn 1 answer".to_string()),
            native_parts: None,
            reasoning_content: Some("turn 1 thinking".to_string()),
            tool_calls: vec![],
        },
        ChatMessage::User {
            content: "turn 2 question".to_string(),
        },
        ChatMessage::Assistant {
            content: Some("turn 2 answer".to_string()),
            native_parts: None,
            reasoning_content: Some("turn 2 thinking".to_string()),
            tool_calls: vec![],
        },
    ];
    strip_prior_reasoning_content(&mut messages);
    // Turn 1 assistant reasoning cleared
    if let ChatMessage::Assistant {
        reasoning_content, ..
    } = &messages[1]
    {
        assert!(
            reasoning_content.is_none(),
            "turn 1 reasoning should be stripped"
        );
    }
    // Turn 2 assistant reasoning kept (current turn)
    if let ChatMessage::Assistant {
        reasoning_content, ..
    } = &messages[3]
    {
        assert_eq!(reasoning_content.as_deref(), Some("turn 2 thinking"));
    }
}

#[test]
fn strip_reasoning_preserves_current_tool_loop() {
    let mut messages = vec![
        ChatMessage::User {
            content: "question".to_string(),
        },
        ChatMessage::Assistant {
            content: Some("thinking step 1".to_string()),
            native_parts: None,
            reasoning_content: Some("reasoning 1".to_string()),
            tool_calls: vec![LlmToolCall {
                id: "c1".to_string(),
                name: "test".to_string(),
                arguments: "{}".to_string(),
            }],
        },
        ChatMessage::Tool {
            tool_call_id: "c1".to_string(),
            content: "result".to_string(),
            tool_name: None,
        },
        ChatMessage::Assistant {
            content: Some("thinking step 2".to_string()),
            native_parts: None,
            reasoning_content: Some("reasoning 2".to_string()),
            tool_calls: vec![],
        },
    ];
    strip_prior_reasoning_content(&mut messages);
    // All assistant messages are after the last User message (idx 0),
    // so they're all in the "current" turn and reasoning is preserved.
    if let ChatMessage::Assistant {
        reasoning_content, ..
    } = &messages[1]
    {
        assert_eq!(reasoning_content.as_deref(), Some("reasoning 1"));
    }
    if let ChatMessage::Assistant {
        reasoning_content, ..
    } = &messages[3]
    {
        assert_eq!(reasoning_content.as_deref(), Some("reasoning 2"));
    }
}

#[test]
fn legacy_privacy_settings_keep_their_effective_state() -> anyhow::Result<()> {
    for (legacy_enabled, override_enabled, expected) in [
        (true, None, true),
        (false, None, false),
        (true, Some(false), false),
    ] {
        let workspace = tempfile::tempdir()?;
        let path = AppConfig::project_local_settings_path(workspace.path());
        fs::create_dir_all(path.parent().unwrap())?;
        let mut settings = serde_json::json!({
            "local_ml": {
                "enabled": legacy_enabled,
                "privacy": {"enabled": true, "sensitive_regex": ["example-secret"]}
            },
            "privacy": {"policy": "block_cloud"}
        });
        if let Some(enabled) = override_enabled {
            settings["privacy"]["enabled"] = serde_json::json!(enabled);
        }
        fs::write(path, serde_json::to_vec(&settings)?)?;
        let (cfg, _) = AppConfig::load_settings(workspace.path(), true)?;
        assert_eq!(cfg.privacy.enabled, expected);
        assert_eq!(cfg.privacy.policy, "block_cloud");
        assert_eq!(cfg.privacy.sensitive_regex, ["example-secret"]);
    }
    Ok(())
}

#[test]
fn default_config_has_bootstrap_enabled() {
    let cfg = AppConfig::default();
    assert!(
        cfg.agent_loop.context_bootstrap_enabled,
        "context bootstrap should be enabled by default"
    );
}

#[test]
fn index_build_event_roundtrip() {
    let event = EventKind::IndexBuild {
        chunks_indexed: 1500,
        files_processed: 120,
        duration_ms: 3456,
    };
    let serialized = serde_json::to_string(&event).expect("serialize");
    let deserialized: EventKind = serde_json::from_str(&serialized).expect("deserialize");
    let re_serialized = serde_json::to_string(&deserialized).expect("re-serialize");
    assert_eq!(serialized, re_serialized);
}

#[test]
fn privacy_event_roundtrip() {
    let events = vec![
        EventKind::PrivacyRedaction {
            path: "/project/.env".to_string(),
            patterns_matched: 3,
            policy: "redact".to_string(),
        },
        EventKind::PrivacyBlock {
            path: "/project/secrets.yaml".to_string(),
            reason: "block_cloud policy".to_string(),
        },
    ];
    for event in events {
        let serialized = serde_json::to_string(&event).expect("serialize");
        let deserialized: EventKind = serde_json::from_str(&serialized).expect("deserialize");
        let re_serialized = serde_json::to_string(&deserialized).expect("re-serialize");
        assert_eq!(serialized, re_serialized);
    }
}

#[test]
fn autocomplete_event_roundtrip() {
    let event = EventKind::Autocomplete {
        model_id: "qwen2.5-coder-3b".to_string(),
        tokens_generated: 42,
        latency_ms: 150,
        accepted: true,
    };
    let serialized = serde_json::to_string(&event).expect("serialize");
    let deserialized: EventKind = serde_json::from_str(&serialized).expect("deserialize");
    let re_serialized = serde_json::to_string(&deserialized).expect("re-serialize");
    assert_eq!(serialized, re_serialized);
}

#[test]
fn cancellation_token_default_not_cancelled() {
    let token = CancellationToken::default();
    assert!(!token.is_cancelled());
}

#[test]
fn cancellation_token_cancel_and_reset() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());
    token.cancel();
    assert!(token.is_cancelled());
    token.reset();
    assert!(!token.is_cancelled());
}

#[test]
fn cancellation_token_clone_shares_state() {
    let token = CancellationToken::new();
    let clone = token.clone();
    token.cancel();
    assert!(
        clone.is_cancelled(),
        "clone should see cancellation from original"
    );
}

#[test]
fn usage_update_json_serialization() {
    let chunk = StreamChunk::UsageUpdate {
        input_tokens: 1000,
        output_tokens: 500,
        cache_hit_tokens: 200,
        estimated_cost_usd: 0.0042,
    };
    let json = stream_chunk_to_event_json(&chunk);
    assert_eq!(json["type"], "usage_update");
    assert_eq!(json["input_tokens"], 1000);
    assert_eq!(json["output_tokens"], 500);
    assert_eq!(json["cache_hit_tokens"], 200);
    assert!((json["estimated_cost_usd"].as_f64().unwrap() - 0.0042).abs() < 0.0001);
}

#[test]
fn active_provider_returns_deepseek_by_default() {
    let cfg = LlmConfig::default();
    let provider = cfg.active_provider();
    assert_eq!(provider.base_url, "https://api.deepseek.com");
    assert_eq!(provider.api_key_env, "DEEPSEEK_API_KEY");
    assert_eq!(provider.models.chat, "deepseek-chat");
    assert_eq!(
        provider.models.reasoner.as_deref(),
        Some("deepseek-reasoner")
    );
}

#[test]
fn active_provider_falls_back_to_legacy_fields() {
    let cfg = LlmConfig {
        provider: "custom".to_string(),
        providers: std::collections::HashMap::new(), // no entry for "custom"
        base_url: "http://my-llm:8000".to_string(),
        api_key_env: "MY_KEY".to_string(),
        base_model: "my-model".to_string(),
        max_think_model: "my-reasoner".to_string(),
        ..LlmConfig::default()
    };
    let provider = cfg.active_provider();
    assert_eq!(provider.base_url, "http://my-llm:8000");
    assert_eq!(provider.api_key_env, "MY_KEY");
    assert_eq!(provider.models.chat, "my-model");
    assert_eq!(provider.models.reasoner.as_deref(), Some("my-reasoner"));
}

#[test]
fn active_provider_uses_providers_map_when_available() {
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "openai-compat".to_string(),
        ProviderConfig {
            kind: "openai-compatible".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key_env: "OLLAMA_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "llama3".to_string(),
                reasoner: None,
            },
        },
    );
    let cfg = LlmConfig {
        provider: "openai-compat".to_string(),
        providers,
        ..LlmConfig::default()
    };
    let provider = cfg.active_provider();
    assert_eq!(provider.base_url, "http://localhost:11434/v1");
    assert_eq!(provider.api_key_env, "OLLAMA_KEY");
    assert_eq!(provider.models.chat, "llama3");
    assert!(provider.models.reasoner.is_none());
}

#[test]
fn apply_model_spec_updates_active_provider_model() {
    let mut cfg = LlmConfig::default();
    cfg.apply_model_spec("openrouter/anthropic/claude-sonnet-4");

    assert_eq!(cfg.provider, "openrouter");
    assert_eq!(cfg.base_model, "anthropic/claude-sonnet-4");
    assert_eq!(cfg.active_base_model(), "anthropic/claude-sonnet-4");
    assert_eq!(
        cfg.providers["openrouter"].models.chat,
        "anthropic/claude-sonnet-4"
    );
}

#[test]
fn default_providers_includes_deepseek() {
    let providers = default_providers();
    assert!(providers.contains_key("deepseek"));
    assert!(providers.contains_key("openai-compatible"));
    assert!(providers.contains_key("ollama"));
    let ds = &providers["deepseek"];
    assert_eq!(ds.base_url, "https://api.deepseek.com");
    assert_eq!(ds.models.chat, "deepseek-chat");
}

#[test]
fn provider_kind_aliases_resolve() {
    assert_eq!(
        normalize_provider_kind("openai-compat"),
        Some(ProviderKind::OpenAiCompatible)
    );
    assert_eq!(
        normalize_provider_kind("custom"),
        Some(ProviderKind::OpenAiCompatible)
    );
    assert_eq!(
        normalize_provider_kind("ollama"),
        Some(ProviderKind::Ollama)
    );
    assert_eq!(
        normalize_provider_kind("deepseek"),
        Some(ProviderKind::Deepseek)
    );
    assert_eq!(normalize_provider_kind("made-up"), None);
}

#[test]
fn active_models_follow_provider_map() {
    let cfg = LlmConfig {
        provider: "ollama".to_string(),
        ..LlmConfig::default()
    };
    assert_eq!(cfg.active_base_model(), "qwen2.5-coder:7b");
    assert_eq!(cfg.active_reasoner_model(), "qwen2.5-coder:7b");
    assert_eq!(cfg.active_provider_kind(), Some(ProviderKind::Ollama));
}

#[test]
fn capabilities_reflect_provider_and_model() {
    let cfg = LlmConfig::default();
    let deepseek = cfg
        .capabilities_for_model("deepseek-reasoner")
        .expect("deepseek caps");
    assert!(deepseek.supports_reasoning_mode);
    assert!(!deepseek.supports_tool_choice);
    let ollama_cfg = LlmConfig {
        provider: "ollama".to_string(),
        ..LlmConfig::default()
    };
    let ollama = ollama_cfg
        .capabilities_for_model("qwen2.5-coder:7b")
        .expect("ollama caps");
    assert!(!ollama.supports_thinking_config);
    assert_eq!(ollama.family, ModelFamily::Qwen);
}

#[test]
fn capabilities_allow_config_driven_model_override() {
    let mut cfg = LlmConfig {
        provider: "ollama".to_string(),
        ..LlmConfig::default()
    };
    cfg.capability_overrides.models.insert(
        "ollama@qwen2.5-coder:*".to_string(),
        CapabilityOverride {
            supports_tool_choice: Some(false),
            max_safe_tool_count: Some(6),
            supports_image_input: Some(true),
            strict_empty_content_filtering: Some(false),
            normalize_tool_call_ids: Some(false),
            ..CapabilityOverride::default()
        },
    );

    let caps = cfg
        .capabilities_for_model("qwen2.5-coder:7b")
        .expect("caps");
    assert!(!caps.supports_tool_choice);
    assert_eq!(caps.max_safe_tool_count, 6);
    assert!(caps.supports_image_input);
    assert!(!caps.strict_empty_content_filtering);
    assert!(!caps.normalize_tool_call_ids);
}

#[test]
fn capability_resolution_tracks_applied_rules() {
    let mut cfg = LlmConfig {
        provider: "ollama".to_string(),
        ..LlmConfig::default()
    };
    cfg.capability_overrides.families.insert(
        "ollama@qwen".to_string(),
        CapabilityOverride {
            max_safe_tool_count: Some(5),
            ..CapabilityOverride::default()
        },
    );
    let resolution = cfg
        .capability_resolution_for_model("qwen2.5-coder:3b")
        .expect("resolution");
    assert_eq!(resolution.capabilities.max_safe_tool_count, 5);
    assert!(
        resolution
            .applied_rules
            .iter()
            .any(|rule| rule.contains("config_family:ollama@qwen")),
        "expected config family override in applied rules"
    );
}

// ── Phase 3 EventKind variant tests ──

#[test]
fn tool_loop_event_kinds_serialize_roundtrip() {
    let events: Vec<EventKind> = vec![
        EventKind::CompactionTriggered {
            phase: "full".to_string(),
            pre_tokens: 100_000,
            post_tokens: 50_000,
            messages_before: 40,
            messages_after: 10,
        },
        EventKind::DoomLoopDetected {
            tool_name: "fs_read".to_string(),
            repeat_count: 3,
        },
        EventKind::CircuitBreakerTripped {
            tool_name: "shell_exec".to_string(),
            consecutive_failures: 3,
            cooldown_turns: 2,
        },
        EventKind::HallucinationNudgeFired {
            nudge_count: 1,
            trigger: "long_response".to_string(),
        },
        EventKind::ToolCacheHit {
            tool_name: "fs_read".to_string(),
        },
        EventKind::ErrorRecoveryTriggered {
            level: "stuck".to_string(),
            repeated_error_count: 3,
        },
        EventKind::ModelRoutingChanged {
            from_model: "deepseek-chat".to_string(),
            to_model: "deepseek-reasoner".to_string(),
            reason: "escalation".to_string(),
        },
    ];

    for event in &events {
        let json = serde_json::to_string(event).expect("serialize");
        let restored: EventKind = serde_json::from_str(&json).expect("deserialize");
        // Verify roundtrip by re-serializing
        let json2 = serde_json::to_string(&restored).expect("re-serialize");
        assert_eq!(json, json2, "roundtrip mismatch for {:?}", event);
    }
}

#[test]
fn tool_loop_event_kinds_have_correct_category() {
    assert_eq!(
        EventKind::CompactionTriggered {
            phase: "full".into(),
            pre_tokens: 0,
            post_tokens: 0,
            messages_before: 0,
            messages_after: 0,
        }
        .category(),
        "tool_loop"
    );
    assert_eq!(
        EventKind::DoomLoopDetected {
            tool_name: "x".into(),
            repeat_count: 3,
        }
        .category(),
        "tool_loop"
    );
    assert_eq!(
        EventKind::CircuitBreakerTripped {
            tool_name: "x".into(),
            consecutive_failures: 3,
            cooldown_turns: 2,
        }
        .category(),
        "tool_loop"
    );
    assert_eq!(
        EventKind::HallucinationNudgeFired {
            nudge_count: 1,
            trigger: "x".into(),
        }
        .category(),
        "tool_loop"
    );
    assert_eq!(
        EventKind::ToolCacheHit {
            tool_name: "x".into(),
        }
        .category(),
        "tool_loop"
    );
    assert_eq!(
        EventKind::ErrorRecoveryTriggered {
            level: "recovery".into(),
            repeated_error_count: 1,
        }
        .category(),
        "tool_loop"
    );
    assert_eq!(
        EventKind::ModelRoutingChanged {
            from_model: "a".into(),
            to_model: "b".into(),
            reason: "escalation".into(),
        }
        .category(),
        "tool_loop"
    );
}

#[test]
fn tool_loop_events_are_not_tool_events() {
    // tool_loop events should NOT be classified as tool events
    let event = EventKind::DoomLoopDetected {
        tool_name: "fs_read".into(),
        repeat_count: 3,
    };
    assert!(!event.is_tool_event());
}

#[test]
fn config_load_skips_malformed_json() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let settings_dir = ws.join(".codingbuddy");
    std::fs::create_dir_all(&settings_dir).unwrap();
    // Write malformed JSON to the project settings file
    std::fs::write(settings_dir.join("settings.json"), "{ invalid json !!!").unwrap();
    // Should succeed (skip the bad file) rather than propagate an error
    let result = AppConfig::load(ws);
    assert!(result.is_ok(), "expected Ok, got: {result:?}");
}

#[test]
fn stream_chunk_error_serializes() {
    let chunk = StreamChunk::Error {
        message: "test error".into(),
        recoverable: true,
    };
    let json = stream_chunk_to_event_json(&chunk);
    assert_eq!(json["type"], "error");
    assert_eq!(json["message"], "test error");
    assert_eq!(json["recoverable"], true);
}
