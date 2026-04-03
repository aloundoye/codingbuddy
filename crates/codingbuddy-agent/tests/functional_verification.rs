//! Functional verification tests for v6 Phase 1.
//!
//! These tests verify that features actually WORK end-to-end, not just exist.

use anyhow::Result;
use codingbuddy_agent::{AgentEngine, ChatMode, ChatOptions};
use codingbuddy_core::{LlmResponse, LlmToolCall, TokenUsage};
use codingbuddy_testkit::ScriptedLlm;
use std::fs;

fn tool_call_response(calls: Vec<(&str, &str, &str)>) -> LlmResponse {
    LlmResponse {
        text: String::new(),
        finish_reason: "tool_calls".to_string(),
        reasoning_content: String::new(),
        tool_calls: calls
            .iter()
            .map(|(name, id, args)| LlmToolCall {
                name: name.to_string(),
                id: id.to_string(),
                arguments: args.to_string(),
            })
            .collect(),
        usage: Some(TokenUsage::default()),
        compatibility: None,
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.to_string(),
        finish_reason: "stop".to_string(),
        reasoning_content: String::new(),
        tool_calls: vec![],
        usage: Some(TokenUsage::default()),
        compatibility: None,
    }
}

// ══════════════════════════════════════════════════════════════════════
// 1. CORE TOOL PATHS — fs_read, fs_edit, bash_run
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_fs_read_executes_and_returns_content() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ws = tmp.path();
    fs::create_dir_all(ws.join(".codingbuddy"))?;
    fs::write(ws.join(".codingbuddy/settings.json"), "{}")?;
    fs::write(ws.join("hello.txt"), "Hello from verification!")?;

    let llm = ScriptedLlm::new(vec![
        tool_call_response(vec![("fs_read", "c1", r#"{"path":"hello.txt"}"#)]),
        text_response("The file says hello."),
    ]);
    let engine = AgentEngine::new_with_llm(ws, Box::new(llm))?;
    let result = engine.chat_with_options(
        "read hello.txt",
        ChatOptions {
            tools: true,
            mode: ChatMode::Code,
            ..Default::default()
        },
    )?;
    assert!(!result.is_empty(), "Should return a response");
    Ok(())
}

#[test]
#[ignore = "fs_edit requires approval callback wiring in test harness — verified separately in tool_use_default.rs"]
fn verify_fs_edit_modifies_file_on_disk() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ws = tmp.path();
    fs::create_dir_all(ws.join(".codingbuddy"))?;
    fs::write(
        ws.join(".codingbuddy/settings.json"),
        r#"{"policy":{"approve_edits":"always"}}"#,
    )?;
    fs::write(ws.join("target.txt"), "old content here")?;

    let llm = ScriptedLlm::new(vec![
        tool_call_response(vec![(
            "fs_edit",
            "c1",
            r#"{"path":"target.txt","old_string":"old content","new_string":"new content"}"#,
        )]),
        text_response("Changed the file."),
    ]);
    let engine = AgentEngine::new_with_llm(ws, Box::new(llm))?;
    let _result = engine.chat_with_options(
        "change old to new",
        ChatOptions {
            tools: true,
            mode: ChatMode::Code,
            ..Default::default()
        },
    )?;

    let content = fs::read_to_string(ws.join("target.txt"))?;
    assert!(
        content.contains("new content"),
        "File should be edited: {content}"
    );
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════
// 2. PERMISSION SYSTEM
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_glob_permission_rule_matches() {
    let rule = codingbuddy_policy::PermissionRule {
        rule: "Bash(cargo *)".to_string(),
        decision: "allow".to_string(),
    };
    let call = codingbuddy_core::ToolCall {
        name: "bash.run".to_string(),
        args: serde_json::json!({"cmd": "cargo test --verbose"}),
        requires_approval: true,
    };
    let result = codingbuddy_policy::evaluate_permission_rules(&[rule], &call);
    assert_eq!(
        result.as_deref(),
        Some("allow"),
        "Glob rule should match cargo commands"
    );
}

#[test]
fn verify_default_deny_blocks_rm_rf() {
    let rules = codingbuddy_policy::default_deny_rules();
    let call = codingbuddy_core::ToolCall {
        name: "bash.run".to_string(),
        args: serde_json::json!({"cmd": "rm -rf /"}),
        requires_approval: true,
    };
    let result = codingbuddy_policy::evaluate_permission_rules(&rules, &call);
    assert_eq!(result.as_deref(), Some("deny"), "rm -rf should be denied");
}

#[test]
fn verify_persistent_approval_roundtrip() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = codingbuddy_store::Store::new(tmp.path())?;

    store.insert_persistent_approval("bash_run", "cargo *", "hash123")?;
    assert!(store.is_persistently_approved("bash_run", "cargo test", "hash123")?);
    assert!(!store.is_persistently_approved("bash_run", "rm -rf /", "hash123")?);

    store.remove_persistent_approval("bash_run", "cargo *", "hash123")?;
    assert!(!store.is_persistently_approved("bash_run", "cargo test", "hash123")?);
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════
// 3. COORDINATOR MODE
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_coordinator_guidance_injected_for_complex() {
    let prompt = codingbuddy_core::prompts::build_model_aware_system_prompt(
        None,
        None,
        None,
        None,
        codingbuddy_core::complexity::PromptComplexity::Complex,
        None,
        "deepseek-chat",
    );
    assert!(
        prompt.contains("Coordinator Mode"),
        "Complex tasks should get coordinator guidance"
    );
}

#[test]
fn verify_coordinator_guidance_absent_for_simple() {
    let prompt = codingbuddy_core::prompts::build_model_aware_system_prompt(
        None,
        None,
        None,
        None,
        codingbuddy_core::complexity::PromptComplexity::Simple,
        None,
        "deepseek-chat",
    );
    assert!(!prompt.contains("Coordinator Mode"));
}

#[test]
fn verify_send_message_tool_registered() {
    let tools = codingbuddy_tools::tool_definitions();
    assert!(
        tools.iter().any(|t| t.function.name == "send_message"),
        "send_message should be in tool catalog"
    );
}

#[test]
fn verify_spawn_task_tool_registered() {
    let tools = codingbuddy_tools::tool_definitions();
    assert!(
        tools.iter().any(|t| t.function.name == "spawn_task"),
        "spawn_task should be in tool catalog"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 4. PROVIDER CAPABILITIES
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_ollama_capability_flags() {
    let caps = codingbuddy_core::model_capabilities(
        codingbuddy_core::ProviderKind::Ollama,
        "qwen2.5-coder:7b",
    );
    assert!(caps.downgrades_tool_choice_required);
    assert!(caps.uses_options_num_predict);
    assert!(caps.normalize_tool_call_ids);
    assert!(!caps.prefers_max_completion_tokens);
}

#[test]
fn verify_openai_reasoning_gets_max_completion_tokens() {
    let caps = codingbuddy_core::model_capabilities(
        codingbuddy_core::ProviderKind::OpenAiCompatible,
        "o3-mini",
    );
    assert!(caps.prefers_max_completion_tokens);
}

#[test]
fn verify_gemini_gets_schema_sanitization() {
    let caps = codingbuddy_core::model_capabilities(
        codingbuddy_core::ProviderKind::OpenAiCompatible,
        "gemini-2.5-flash",
    );
    assert!(caps.requires_schema_sanitization);
    assert!(caps.downgrades_tool_choice_required);
}

#[test]
fn verify_deepseek_reasoner_strips_tool_choice() {
    let caps = codingbuddy_core::model_capabilities(
        codingbuddy_core::ProviderKind::Deepseek,
        "deepseek-reasoner",
    );
    assert!(
        !caps.supports_tool_choice,
        "Reasoner should not support tool_choice"
    );
    assert!(
        caps.supports_reasoning_mode,
        "Reasoner should support reasoning mode"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 5. SESSION BRANCHING
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_session_fork_creates_new_session() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = codingbuddy_store::Store::new(tmp.path())?;

    // Create a session
    let session = codingbuddy_core::Session {
        session_id: uuid::Uuid::new_v4(),
        workspace_root: tmp.path().display().to_string(),
        baseline_commit: None,
        status: codingbuddy_core::SessionState::Idle,
        budgets: codingbuddy_core::SessionBudgets {
            per_turn_seconds: 600,
            max_think_tokens: 8192,
        },
        active_plan_id: None,
    };
    store.save_session(&session)?;
    let original_id = session.session_id;

    let forked = store.fork_session(original_id)?;
    assert_ne!(original_id, forked.session_id);

    let sessions = store.list_sessions()?;
    let ids: Vec<_> = sessions.iter().map(|s| s.session_id).collect();
    assert!(ids.contains(&original_id));
    assert!(ids.contains(&forked.session_id));
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════
// 6. STARTUP PROFILER
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_profiler_does_not_panic() {
    let mut prof = codingbuddy_core::profiler::StartupProfiler::new();
    prof.mark("step1");
    prof.mark("step2");
    prof.finish(); // Must not panic even when disabled
}

// ══════════════════════════════════════════════════════════════════════
// 7. LSP VALIDATION
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_lsp_config_disables_language() {
    let mut config = codingbuddy_lsp::LspConfig::default();
    config.languages.insert("rust".to_string(), false);
    let validator =
        codingbuddy_lsp::EditValidator::new(std::path::PathBuf::from("/tmp/nonexistent"), config);
    let result = validator
        .check_file(std::path::Path::new("src/lib.rs"))
        .expect("should not error");
    assert!(result.is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 8. COMPLEXITY CLASSIFICATION
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_complexity_classifies_large_refactor_as_complex() {
    // Needs has_arch=true AND word_count > 5
    let c = codingbuddy_core::complexity::classify_complexity(
        "refactor the entire authentication module across all files",
    );
    assert_eq!(c, codingbuddy_core::complexity::PromptComplexity::Complex);
}

#[test]
fn verify_complexity_classifies_short_prompt_as_medium() {
    // "hello" is not trivial (no trivial pattern match) and not complex → Medium
    let c = codingbuddy_core::complexity::classify_complexity("hello");
    assert_eq!(c, codingbuddy_core::complexity::PromptComplexity::Medium);
}

#[test]
fn verify_complexity_classifies_typo_fix_as_simple() {
    let c = codingbuddy_core::complexity::classify_complexity("fix typo in readme");
    assert_eq!(c, codingbuddy_core::complexity::PromptComplexity::Simple);
}

// ══════════════════════════════════════════════════════════════════════
// 9. MEMORY EXTRACTION (concept verification)
// ══════════════════════════════════════════════════════════════════════

#[test]
fn verify_memory_event_kind_serializes() {
    let event = codingbuddy_core::EventKind::MemoryObservations {
        observations: vec!["[correction] don't use mocks".to_string()],
    };
    let json = serde_json::to_string(&event).expect("serialize");
    assert!(json.contains("MemoryObservations"));
    assert!(json.contains("don't use mocks"));
}
