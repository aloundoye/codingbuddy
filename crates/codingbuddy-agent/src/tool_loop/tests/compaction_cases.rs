use super::*;

// ── Batch 4 (P9): Compaction & Correctness ──

#[test]
fn compaction_preserves_tool_result_pairing() {
    // Build a conversation with tool calls that must stay paired.
    let llm = ScriptedLlm::new(vec![make_text_response("Final answer.")]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));

    let config = ToolLoopConfig {
        // Small context window so compaction budget is tight
        context_window_tokens: 2000,
        ..Default::default()
    };
    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        config,
        "system".to_string(),
        default_tools(),
    );

    // Manually build history to simulate a conversation with tool exchanges
    loop_.messages.push(ChatMessage::User {
        content: "first question".to_string(),
    });
    for i in 0..5 {
        loop_.messages.push(ChatMessage::Assistant {
            content: None,
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![LlmToolCall {
                id: format!("call_{i}"),
                name: "fs_read".to_string(),
                arguments: format!(r#"{{"path":"file{i}.rs"}}"#),
            }],
        });
        loop_.messages.push(ChatMessage::Tool {
            tool_call_id: format!("call_{i}"),
            content: format!("content of file{i}.rs — {}", "x".repeat(400)),
            tool_name: None,
        });
    }
    loop_.messages.push(ChatMessage::User {
        content: "second question".to_string(),
    });
    loop_.messages.push(ChatMessage::Assistant {
        content: Some("Here is what I found.".to_string()),
        native_parts: None,
        reasoning_content: None,
        tool_calls: vec![],
    });

    // Force compaction with a tight budget
    let compacted = codingbuddy_core::runtime::block_on(loop_.compact_messages(200));
    assert!(compacted, "should have compacted");

    // Verify no orphaned Tool messages (every Tool must follow an Assistant with tool_calls)
    let msgs = &loop_.messages;
    for (i, msg) in msgs.iter().enumerate() {
        if matches!(msg, ChatMessage::Tool { .. }) {
            assert!(
                i > 0
                    && matches!(&msgs[i - 1], ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty()),
                "Tool message at index {i} is orphaned (no preceding Assistant with tool_calls)"
            );
        }
    }
}

#[test]
fn compaction_respects_target_tokens() {
    let llm = ScriptedLlm::new(vec![]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));

    let config = ToolLoopConfig {
        context_window_tokens: 128_000,
        ..Default::default()
    };
    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        config,
        "system".to_string(),
        default_tools(),
    );

    // Add many user/assistant exchanges
    for i in 0..20 {
        loop_.messages.push(ChatMessage::User {
            content: format!("Question {i}: {}", "x".repeat(200)),
        });
        loop_.messages.push(ChatMessage::Assistant {
            content: Some(format!("Answer {i}: {}", "y".repeat(200))),
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    let before_len = loop_.messages.len();
    let compacted = codingbuddy_core::runtime::block_on(loop_.compact_messages(2000));
    assert!(compacted, "should compact");
    assert!(
        loop_.messages.len() < before_len,
        "should have fewer messages"
    );
    // System message should always be first
    assert!(matches!(&loop_.messages[0], ChatMessage::System { .. }));
    // Compaction summary should be second
    assert!(
        matches!(&loop_.messages[1], ChatMessage::User { content } if content.contains("compacted"))
    );
}

#[test]
fn compaction_budget_is_provider_and_model_aware() {
    let llm = ScriptedLlm::new(vec![]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));

    let mut deepseek_loop = ToolUseLoop::new(
        &llm,
        tool_host.clone(),
        ToolLoopConfig::default(),
        "system".to_string(),
        default_tools(),
    );
    deepseek_loop.config.provider_kind = codingbuddy_core::ProviderKind::Deepseek;
    deepseek_loop.config.model = "deepseek-chat".to_string();
    deepseek_loop.config.context_window_tokens = 128_000;
    deepseek_loop.config.max_tokens = 32_768;
    deepseek_loop.config.response_budget_tokens = 8_192;
    deepseek_loop.config.reserved_overhead_tokens = 4_000;

    let mut openai_loop = ToolUseLoop::new(
        &llm,
        tool_host,
        ToolLoopConfig::default(),
        "system".to_string(),
        default_tools(),
    );
    openai_loop.config.provider_kind = codingbuddy_core::ProviderKind::OpenAiCompatible;
    openai_loop.config.model = "gpt-4o-mini".to_string();
    openai_loop.config.context_window_tokens = 128_000;
    openai_loop.config.max_tokens = 32_768;
    openai_loop.config.response_budget_tokens = 8_192;
    openai_loop.config.reserved_overhead_tokens = 4_000;

    let (deep_prune, _, _) = deepseek_loop.compaction_budget_for_next_turn(2_000);
    let (openai_prune, _, _) = openai_loop.compaction_budget_for_next_turn(2_000);
    assert!(
        deep_prune < openai_prune,
        "deepseek thinking budget should reserve more output than openai-compatible chat"
    );
}

#[test]
fn compaction_appends_at_most_one_followup_user_prompt() {
    let llm = ScriptedLlm::new(vec![make_text_response(
        "## Goal\nPreserve context\n## Completed\nCompaction summary retained with files and findings.",
    )]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));
    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        ToolLoopConfig::default(),
        "system".to_string(),
        default_tools(),
    );

    loop_.messages.push(ChatMessage::User {
        content: "Please inspect auth, implement the fix, and run tests.".to_string(),
    });
    for i in 0..4 {
        loop_.messages.push(ChatMessage::Assistant {
            content: Some(format!("assistant-{i}: {}", "x".repeat(220))),
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
    loop_.messages.push(ChatMessage::User {
        content: "Keep going with implementation details.".to_string(),
    });
    for i in 4..10 {
        loop_.messages.push(ChatMessage::Assistant {
            content: Some(format!("assistant-{i}: {}", "x".repeat(220))),
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    let compacted = codingbuddy_core::runtime::block_on(loop_.compact_messages(240));
    assert!(compacted);

    let followup_count = loop_
            .messages
            .iter()
            .filter(|msg| matches!(msg, ChatMessage::User { content } if content.starts_with("Context was compacted.")))
            .count();
    assert_eq!(
        followup_count, 1,
        "compaction should emit exactly one followup user prompt"
    );
}

#[test]
fn compaction_followup_replays_last_actionable_user_prompt() {
    let llm = ScriptedLlm::new(vec![make_text_response(
        "## Goal\nContinue task\n## Completed\nHistory compacted with actionable replay anchor.",
    )]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));
    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        ToolLoopConfig::default(),
        "system".to_string(),
        default_tools(),
    );

    loop_.messages.push(ChatMessage::User {
        content: "Implement authentication middleware and run the integration tests.".to_string(),
    });
    for i in 0..4 {
        loop_.messages.push(ChatMessage::Assistant {
            content: Some(format!("step-{i}: {}", "y".repeat(180))),
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
    loop_.messages.push(ChatMessage::User {
        content: "Proceed with the remaining execution steps.".to_string(),
    });
    for i in 4..12 {
        loop_.messages.push(ChatMessage::Assistant {
            content: Some(format!("step-{i}: {}", "y".repeat(180))),
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    let compacted = codingbuddy_core::runtime::block_on(loop_.compact_messages(220));
    assert!(compacted);

    let replay = loop_
        .messages
        .iter()
        .rev()
        .find_map(|msg| match msg {
            ChatMessage::User { content } if content.starts_with("Context was compacted.") => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("expected followup replay user message");
    assert!(
        replay.contains("Implement authentication middleware"),
        "followup should replay the last actionable user request"
    );
}

#[test]
fn compaction_injects_active_work_state_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = Store::new(temp.path()).expect("store");

    let session_id = Uuid::now_v7();
    let plan_id = Uuid::now_v7();
    let session = Session {
        session_id,
        workspace_root: temp.path().display().to_string(),
        baseline_commit: None,
        status: SessionState::ExecutingStep,
        budgets: SessionBudgets {
            per_turn_seconds: 180,
            max_think_tokens: 32_768,
        },
        active_plan_id: Some(plan_id),
    };
    store.save_session(&session).expect("save session");
    store
        .save_plan(
            session_id,
            &Plan {
                plan_id,
                version: 1,
                goal: "Harden authentication middleware and validate with tests".to_string(),
                assumptions: vec![],
                steps: vec![
                    PlanStep {
                        step_id: Uuid::now_v7(),
                        title: "Inspect middleware entrypoints".to_string(),
                        intent: "Find all auth checks".to_string(),
                        tools: vec!["fs_read".to_string()],
                        files: vec!["src/auth/middleware.rs".to_string()],
                        done: true,
                    },
                    PlanStep {
                        step_id: Uuid::now_v7(),
                        title: "Implement missing guard".to_string(),
                        intent: "Enforce token validation".to_string(),
                        tools: vec!["fs_edit".to_string()],
                        files: vec!["src/auth/middleware.rs".to_string()],
                        done: false,
                    },
                ],
                verification: vec!["cargo test".to_string()],
                risk_notes: vec![],
            },
        )
        .expect("save plan");
    store
        .insert_task(&TaskQueueRecord {
            task_id: Uuid::now_v7(),
            session_id,
            title: "Patch auth middleware".to_string(),
            description: Some("Ensure missing token guard is enforced".to_string()),
            priority: 1,
            status: "in_progress".to_string(),
            outcome: None,
            artifact_path: Some("session://child-auth".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        })
        .expect("insert task");

    let llm = ScriptedLlm::new(vec![make_text_response(
        "## Goal\nKeep active plan/task state\n## Completed\nCompaction retained active work snapshot.",
    )]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));
    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        ToolLoopConfig {
            workspace: Some(temp.path().to_path_buf()),
            session_id: Some(session_id),
            phase_loop_enabled: true,
            initial_phase: Some(TaskPhase::Execute),
            ..Default::default()
        },
        "system".to_string(),
        default_tools(),
    );

    loop_.messages.push(ChatMessage::User {
        content: "Continue implementing the auth fix".to_string(),
    });
    for i in 0..4 {
        loop_.messages.push(ChatMessage::Assistant {
            content: Some(format!("work-{i}: {}", "z".repeat(200))),
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
    loop_.messages.push(ChatMessage::User {
        content: "Carry on with the next execution step.".to_string(),
    });
    for i in 4..12 {
        loop_.messages.push(ChatMessage::Assistant {
            content: Some(format!("work-{i}: {}", "z".repeat(200))),
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    let compacted = codingbuddy_core::runtime::block_on(loop_.compact_messages(240));
    assert!(compacted);

    let snapshot = loop_
        .messages
        .iter()
        .find_map(|msg| match msg {
            ChatMessage::System { content } if content.starts_with("ACTIVE_WORK_STATE") => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("expected active work snapshot message");
    assert!(snapshot.contains("Harden authentication middleware"));
    assert!(snapshot.contains("Patch auth middleware"));
}

#[test]
fn context_warning_fires_once_per_crossing() {
    let llm = ScriptedLlm::new(vec![]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));
    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        ToolLoopConfig::default(),
        "system".to_string(),
        default_tools(),
    );
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    loop_.set_stream_callback(Arc::new(move |chunk| {
        if let StreamChunk::ContextWarning { used_pct, .. } = chunk {
            sink.lock().unwrap().push(used_pct);
        }
    }));
    // Usable budget 1000 tokens → compaction threshold 950.
    loop_.maybe_warn_context_filling(500, 950);
    loop_.maybe_warn_context_filling(750, 950);
    loop_.maybe_warn_context_filling(800, 950);
    // Compaction brings usage down; the next crossing warns again.
    loop_.maybe_warn_context_filling(300, 950);
    loop_.maybe_warn_context_filling(720, 950);
    assert_eq!(*seen.lock().unwrap(), vec![75, 72]);
}

// ── Idle pruning and session memory ──

/// Four turn groups, each with one tool output of `len` bytes.
fn push_turns_with_outputs(loop_: &mut ToolUseLoop<'_>, len: usize) {
    for i in 0..4 {
        loop_.messages.push(ChatMessage::User {
            content: format!("task {i}"),
        });
        loop_.messages.push(ChatMessage::Tool {
            tool_call_id: format!("tc{i}"),
            content: "y".repeat(len),
            tool_name: None,
        });
    }
}

fn tool_output_lengths(loop_: &ToolUseLoop<'_>) -> Vec<usize> {
    loop_
        .messages
        .iter()
        .filter_map(|msg| match msg {
            ChatMessage::Tool { content, .. } => Some(content.len()),
            _ => None,
        })
        .collect()
}

#[test]
fn idle_pause_trims_large_old_outputs_at_any_context_size() {
    let llm = ScriptedLlm::new(vec![]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));
    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        ToolLoopConfig::default(),
        "system".to_string(),
        default_tools(),
    );
    push_turns_with_outputs(&mut loop_, 5_000);

    // A recent response: the prompt cache is warm, nothing changes.
    loop_.last_model_response_at = Some(std::time::Instant::now());
    loop_.prune_after_idle();
    assert_eq!(tool_output_lengths(&loop_), vec![5_000; 4]);

    // After a long pause the old outputs are trimmed; the recent turns stay.
    loop_.last_model_response_at =
        std::time::Instant::now().checked_sub(std::time::Duration::from_secs(6 * 60));
    loop_.prune_after_idle();
    let lengths = tool_output_lengths(&loop_);
    assert!(lengths[0] < 600, "oldest output trimmed: {lengths:?}");
    assert_eq!(&lengths[1..], &[5_000, 5_000, 5_000]);
    assert!(
        loop_.last_model_response_at.is_none(),
        "runs once per pause"
    );
}

#[test]
fn idle_pause_leaves_mid_sized_outputs_alone() {
    let llm = ScriptedLlm::new(vec![]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));
    let mut loop_ = ToolUseLoop::new(
        &llm,
        tool_host,
        ToolLoopConfig::default(),
        "system".to_string(),
        default_tools(),
    );
    push_turns_with_outputs(&mut loop_, 1_500);
    loop_.last_model_response_at =
        std::time::Instant::now().checked_sub(std::time::Duration::from_secs(6 * 60));
    loop_.prune_after_idle();
    assert_eq!(tool_output_lengths(&loop_), vec![1_500; 4]);
}

#[test]
fn session_memory_entries_come_from_the_decisions_section() {
    let summary = "## Goal\n- ship the PR\n\n## Key Decisions Made\n- user wants a single PR\n* never use mocks\n1. stay on dev\n- None\n\n## Work Completed\n- edited lib.rs\n";
    assert_eq!(
        compaction::session_memory_entries(summary),
        vec![
            "user wants a single PR".to_string(),
            "never use mocks".to_string(),
            "stay on dev".to_string(),
        ]
    );
}

fn summary_with_decision(decision: &str) -> String {
    format!(
        "## Goal\n- finish the refactor of the parser module\n\n## Key Decisions Made\n- {decision}\n\n## Work Completed\n- edited src/parser.rs"
    )
}

/// Turns of history long enough that `compact_messages(200)` summarises them.
fn push_compactable_history(loop_: &mut ToolUseLoop<'_>) {
    for i in 0..5 {
        loop_.messages.push(ChatMessage::User {
            content: format!("question {i} {}", "z".repeat(300)),
        });
        loop_.messages.push(ChatMessage::Assistant {
            content: Some(format!("answer {i} {}", "w".repeat(300))),
            native_parts: None,
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
}

fn session_memory_message(loop_: &ToolUseLoop<'_>) -> Option<String> {
    loop_.messages.iter().find_map(|msg| match msg {
        ChatMessage::System { content } if content.starts_with("SESSION MEMORY") => {
            Some(content.clone())
        }
        _ => None,
    })
}

#[test]
fn session_memory_accumulates_across_compactions_and_is_saved() {
    let workspace = tempfile::tempdir().unwrap();
    let session = Uuid::now_v7();
    let config = ToolLoopConfig {
        workspace: Some(workspace.path().to_path_buf()),
        session_id: Some(session),
        ..Default::default()
    };
    let llm = ScriptedLlm::new(vec![
        make_text_response(&summary_with_decision("user wants a single PR")),
        make_text_response(&summary_with_decision("never use mocks in tests")),
    ]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));
    let mut loop_ = ToolUseLoop::new(&llm, tool_host, config, "system".into(), default_tools());

    push_compactable_history(&mut loop_);
    assert!(codingbuddy_core::runtime::block_on(
        loop_.compact_messages(200)
    ));
    let first = session_memory_message(&loop_).expect("memory injected");
    assert!(first.contains("user wants a single PR"));

    // The second summary no longer mentions the first decision; memory keeps it.
    push_compactable_history(&mut loop_);
    assert!(codingbuddy_core::runtime::block_on(
        loop_.compact_messages(200)
    ));
    let second = session_memory_message(&loop_).expect("memory injected");
    assert!(second.contains("user wants a single PR"), "{second}");
    assert!(second.contains("never use mocks in tests"), "{second}");

    let saved = std::fs::read_to_string(
        workspace
            .path()
            .join(".codingbuddy/session_memory")
            .join(format!("{session}.md")),
    )
    .unwrap();
    assert!(saved.contains("user wants a single PR") && saved.contains("never use mocks"));

    // A later run of the same session starts from the saved memory.
    let llm = ScriptedLlm::new(vec![make_text_response(&summary_with_decision(
        "keep the public API unchanged",
    ))]);
    let tool_host = Arc::new(MockToolHost::new(vec![], true));
    let config = ToolLoopConfig {
        workspace: Some(workspace.path().to_path_buf()),
        session_id: Some(session),
        ..Default::default()
    };
    let mut resumed = ToolUseLoop::new(&llm, tool_host, config, "system".into(), default_tools());
    push_compactable_history(&mut resumed);
    assert!(codingbuddy_core::runtime::block_on(
        resumed.compact_messages(200)
    ));
    let third = session_memory_message(&resumed).expect("memory injected");
    assert!(third.contains("user wants a single PR"), "{third}");
    assert!(third.contains("keep the public API unchanged"), "{third}");
}
