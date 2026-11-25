
use super::*;
use codingbuddy_core::{SessionBudgets, SessionState};

#[test]
fn rebuild_projection_from_events_is_deterministic() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");
    let session = Session {
        session_id: Uuid::now_v7(),
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&session).expect("save session");

    let ev = NewEvent {
        event_id: uuid::Uuid::now_v7(),
        at: Utc::now(),
        session_id: session.session_id,
        kind: EventKind::TurnAdded {
            role: "user".to_string(),
            content: "hello".to_string(),
        },
    };
    store.append_event(&ev).expect("append");
    let p1 = store
        .rebuild_from_events(session.session_id)
        .expect("rebuild 1");
    let p2 = store
        .rebuild_from_events(session.session_id)
        .expect("rebuild 2");
    assert_eq!(p1.transcript, p2.transcript);
}

#[test]
fn append_event_increments_seq_no() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");
    let session_id = Uuid::now_v7();
    let session = Session {
        session_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&session).expect("save session");

    for i in 1..=3 {
        let event = store
            .append_event(&NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: Utc::now(),
                session_id,
                kind: EventKind::TurnAdded {
                    role: "user".to_string(),
                    content: format!("msg {i}"),
                },
            })
            .expect("append");
        assert_eq!(event.seq_no, i);
    }
}

#[test]
fn save_and_load_session_roundtrip() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");
    let session = Session {
        session_id: Uuid::now_v7(),
        workspace_root: "/tmp/test".to_string(),
        baseline_commit: Some("abc123".to_string()),
        status: SessionState::Planning,
        budgets: SessionBudgets {
            per_turn_seconds: 60,
            max_think_tokens: 2000,
        },
        active_plan_id: None,
    };
    store.save_session(&session).expect("save");
    let loaded = store
        .load_session(session.session_id)
        .expect("load")
        .expect("should exist");
    assert_eq!(loaded.session_id, session.session_id);
    assert_eq!(loaded.workspace_root, session.workspace_root);
    assert_eq!(loaded.baseline_commit, session.baseline_commit);
}

#[test]
fn load_latest_session_returns_most_recent() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    let s1 = Session {
        session_id: Uuid::now_v7(),
        workspace_root: "/tmp/first".to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&s1).expect("save s1");
    // Small delay to ensure distinct updated_at timestamps
    std::thread::sleep(std::time::Duration::from_millis(10));

    let s2 = Session {
        session_id: Uuid::now_v7(),
        workspace_root: "/tmp/second".to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&s2).expect("save s2");

    let latest = store
        .load_latest_session()
        .expect("load latest")
        .expect("should exist");
    assert_eq!(latest.session_id, s2.session_id);
}

#[test]
fn list_subagent_runs_filters_by_session() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    let s1 = Session {
        session_id: Uuid::now_v7(),
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    let s2 = Session {
        session_id: Uuid::now_v7(),
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&s1).expect("save s1");
    store.save_session(&s2).expect("save s2");

    let run1 = Uuid::now_v7();
    let run2 = Uuid::now_v7();
    store
        .append_event(&NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: s1.session_id,
            kind: EventKind::SubagentSpawned {
                run_id: run1,
                name: "planner".to_string(),
                goal: "plan".to_string(),
            },
        })
        .expect("append s1 subagent");
    store
        .append_event(&NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: s1.session_id,
            kind: EventKind::SubagentCompleted {
                run_id: run1,
                output: "done".to_string(),
            },
        })
        .expect("append s1 complete");
    store
        .append_event(&NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: s2.session_id,
            kind: EventKind::SubagentSpawned {
                run_id: run2,
                name: "executor".to_string(),
                goal: "exec".to_string(),
            },
        })
        .expect("append s2 subagent");

    let s1_runs = store
        .list_subagent_runs(Some(s1.session_id), 20)
        .expect("list s1");
    assert_eq!(s1_runs.len(), 1);
    assert_eq!(s1_runs[0].run_id, run1);
    assert_eq!(s1_runs[0].status, "completed");

    let s2_runs = store
        .list_subagent_runs(Some(s2.session_id), 20)
        .expect("list s2");
    assert_eq!(s2_runs.len(), 1);
    assert_eq!(s2_runs[0].run_id, run2);
}

#[test]
fn session_todos_roundtrip_and_replace() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    let session = Session {
        session_id: Uuid::now_v7(),
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&session).expect("save session");

    let now = Utc::now().to_rfc3339();
    let first = SessionTodoRecord {
        todo_id: Uuid::now_v7(),
        session_id: session.session_id,
        content: "Inspect auth flow".to_string(),
        status: "in_progress".to_string(),
        position: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    let second = SessionTodoRecord {
        todo_id: Uuid::now_v7(),
        session_id: session.session_id,
        content: "Add integration test".to_string(),
        status: "pending".to_string(),
        position: 1,
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    store.upsert_session_todo(&first).expect("insert first");
    store.upsert_session_todo(&second).expect("insert second");

    let listed = store
        .list_session_todos(session.session_id)
        .expect("list todos");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].content, "Inspect auth flow");
    assert_eq!(listed[1].content, "Add integration test");

    let replacement = SessionTodoRecord {
        todo_id: second.todo_id,
        session_id: session.session_id,
        content: "Add integration test".to_string(),
        status: "completed".to_string(),
        position: 0,
        created_at: now.clone(),
        updated_at: now,
    };
    store
        .replace_session_todos(session.session_id, std::slice::from_ref(&replacement))
        .expect("replace todos");

    let after = store
        .list_session_todos(session.session_id)
        .expect("list replaced");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].todo_id, replacement.todo_id);
    assert_eq!(after[0].status, "completed");
}

#[test]
fn concurrent_appends_do_not_corrupt() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = std::sync::Arc::new(Store::new(&workspace).expect("store"));
    let session_id = Uuid::now_v7();
    let session = Session {
        session_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&session).expect("save");

    let mut handles = vec![];
    for t in 0..2 {
        let s = store.clone();
        let sid = session_id;
        handles.push(std::thread::spawn(move || {
            for i in 0..5 {
                let _ = s.append_event(&NewEvent {
                    event_id: uuid::Uuid::now_v7(),
                    at: Utc::now(),
                    session_id: sid,
                    kind: EventKind::TurnAdded {
                        role: "user".to_string(),
                        content: format!("thread {t} msg {i}"),
                    },
                });
            }
        }));
    }
    for h in handles {
        h.join().expect("thread join");
    }

    let rebuilt = store.rebuild_from_events(session_id).expect("rebuild");
    assert_eq!(rebuilt.transcript.len(), 10);
}

#[test]
fn rebuild_from_events_empty_session() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");
    let rebuilt = store
        .rebuild_from_events(Uuid::now_v7())
        .expect("rebuild empty");
    assert!(rebuilt.transcript.is_empty());
    assert!(rebuilt.chat_messages.is_empty());
}

#[test]
fn event_journal_survives_reopen() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let session_id = Uuid::now_v7();

    // First open: append event
    {
        let store = Store::new(&workspace).expect("store");
        let session = Session {
            session_id,
            workspace_root: workspace.to_string_lossy().to_string(),
            baseline_commit: None,
            status: SessionState::Idle,
            budgets: SessionBudgets {
                per_turn_seconds: 30,
                max_think_tokens: 1000,
            },
            active_plan_id: None,
        };
        store.save_session(&session).expect("save");
        store
            .append_event(&NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: Utc::now(),
                session_id,
                kind: EventKind::TurnAdded {
                    role: "user".to_string(),
                    content: "persisted".to_string(),
                },
            })
            .expect("append");
    }

    // Second open: verify
    {
        let store2 = Store::new(&workspace).expect("reopen store");
        let rebuilt = store2
            .rebuild_from_events(session_id)
            .expect("rebuild after reopen");
        assert_eq!(rebuilt.transcript.len(), 1);
        assert!(rebuilt.transcript[0].contains("persisted"));
    }
}

#[test]
fn rebuild_handles_mixed_old_and_new_event_types() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");
    let session = Session {
        session_id: Uuid::now_v7(),
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&session).expect("save session");

    store
        .append_event(&NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: session.session_id,
            kind: EventKind::TurnAdded {
                role: "user".to_string(),
                content: "compat".to_string(),
            },
        })
        .expect("append old");
    store
        .append_event(&NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: session.session_id,
            kind: EventKind::PluginEnabled {
                plugin_id: "demo".to_string(),
            },
        })
        .expect("append new");

    let rebuilt = store
        .rebuild_from_events(session.session_id)
        .expect("rebuild mixed");
    assert_eq!(rebuilt.transcript.len(), 1);
    assert_eq!(rebuilt.plugin_events, vec!["demo".to_string()]);
}

// ── Phase 11: Deterministic event ordering tests ────────────────────

#[test]
fn event_ordering_preserved_across_mixed_types() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");
    let sid = Uuid::now_v7();
    let session = Session {
        session_id: sid,
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&session).expect("save session");

    // Insert events of different types in a known order
    let event_specs: Vec<(u64, EventKind)> = vec![
        (
            1,
            EventKind::TurnAdded {
                role: "user".to_string(),
                content: "first".to_string(),
            },
        ),
        (
            2,
            EventKind::TurnAdded {
                role: "assistant".to_string(),
                content: "second".to_string(),
            },
        ),
        (
            3,
            EventKind::PluginEnabled {
                plugin_id: "plug-a".to_string(),
            },
        ),
        (
            4,
            EventKind::TurnAdded {
                role: "user".to_string(),
                content: "third".to_string(),
            },
        ),
    ];

    for (_, kind) in &event_specs {
        store
            .append_event(&NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: Utc::now(),
                session_id: sid,
                kind: kind.clone(),
            })
            .expect("append");
    }

    // Rebuild and verify insertion order is preserved
    let rebuilt = store.rebuild_from_events(sid).expect("rebuild");
    assert_eq!(rebuilt.transcript.len(), 3); // 3 TurnAdded
    assert_eq!(rebuilt.plugin_events.len(), 1); // 1 PluginEnabled
    assert!(rebuilt.transcript[0].contains("first"));
    assert!(rebuilt.transcript[1].contains("second"));
    assert!(rebuilt.transcript[2].contains("third"));

    // Verify seq_no monotonicity
    let events = store.session_events(sid).expect("events");
    assert_eq!(events.last().unwrap().seq_no, 4);
}

#[test]
fn replay_produces_identical_projections() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");
    let sid = Uuid::now_v7();
    let session = Session {
        session_id: sid,
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    };
    store.save_session(&session).expect("save session");

    // Seed events
    for i in 1..=5 {
        store
            .append_event(&NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: Utc::now(),
                session_id: sid,
                kind: EventKind::TurnAdded {
                    role: if i % 2 == 1 { "user" } else { "assistant" }.to_string(),
                    content: format!("msg-{i}"),
                },
            })
            .expect("append");
    }

    // Replay twice from the same event log
    let p1 = store.rebuild_from_events(sid).expect("rebuild 1");
    let p2 = store.rebuild_from_events(sid).expect("rebuild 2");

    // Both replays must produce identical transcripts
    assert_eq!(p1.transcript, p2.transcript);
    assert_eq!(p1.transcript.len(), 5);
    assert_eq!(p1.usage_input_tokens, p2.usage_input_tokens);
    assert_eq!(p1.plugin_events, p2.plugin_events);
}

// ── P1-12: persistent approvals tests ───────────────────────────────

#[test]
fn insert_and_query_persistent_approval() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-store-approval-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    store
        .insert_persistent_approval("bash.run", "npm test", "proj_abc")
        .expect("insert");
    assert!(
        store
            .is_persistently_approved("bash.run", "npm test", "proj_abc")
            .expect("query")
    );
    assert!(
        !store
            .is_persistently_approved("bash.run", "npm test", "proj_other")
            .expect("other project")
    );
}

#[test]
fn persistent_approval_scoped_to_project() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-store-scope-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    store
        .insert_persistent_approval("bash.run", "cargo build", "project_a")
        .expect("insert");
    assert!(
        !store
            .is_persistently_approved("bash.run", "cargo build", "project_b")
            .expect("wrong project")
    );
}

#[test]
fn remove_persistent_approval_works() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-store-remove-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    store
        .insert_persistent_approval("bash.run", "make", "proj_x")
        .expect("insert");
    assert!(
        store
            .is_persistently_approved("bash.run", "make", "proj_x")
            .unwrap()
    );

    let removed = store
        .remove_persistent_approval("bash.run", "make", "proj_x")
        .expect("remove");
    assert!(removed);
    assert!(
        !store
            .is_persistently_approved("bash.run", "make", "proj_x")
            .unwrap()
    );
}

#[test]
fn list_persistent_approvals_returns_all() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-list-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    store
        .insert_persistent_approval("bash.run", "npm test", "proj_1")
        .unwrap();
    store
        .insert_persistent_approval("bash.run", "npm build", "proj_1")
        .unwrap();
    store
        .insert_persistent_approval("bash.run", "npm lint", "proj_1")
        .unwrap();

    let all = store.list_persistent_approvals("proj_1").expect("list");
    assert_eq!(all.len(), 3);
}

#[test]
fn stats_session_history_query() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-store-hist-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    let conn = store.db().expect("db");
    let session_a = Uuid::now_v7().to_string();
    let session_b = Uuid::now_v7().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    // Insert usage records for two sessions
    conn.execute(
            "INSERT INTO usage_ledger (session_id, unit, model, input_tokens, cache_hit_tokens, cache_miss_tokens, output_tokens, recorded_at)
             VALUES (?1, 'chat', 'deepseek-chat', 1000, 0, 0, 500, ?2)",
            rusqlite::params![&session_a, &now],
        ).unwrap();
    conn.execute(
            "INSERT INTO usage_ledger (session_id, unit, model, input_tokens, cache_hit_tokens, cache_miss_tokens, output_tokens, recorded_at)
             VALUES (?1, 'chat', 'deepseek-chat', 2000, 0, 0, 800, ?2)",
            rusqlite::params![&session_a, &now],
        ).unwrap();
    conn.execute(
            "INSERT INTO usage_ledger (session_id, unit, model, input_tokens, cache_hit_tokens, cache_miss_tokens, output_tokens, recorded_at)
             VALUES (?1, 'chat', 'deepseek-chat', 500, 0, 0, 200, ?2)",
            rusqlite::params![&session_b, &now],
        ).unwrap();

    let history = store.session_history(7).expect("session_history");
    assert!(
        history.len() >= 2,
        "should have at least 2 sessions, got {}",
        history.len()
    );

    let entry_a = history.iter().find(|e| e.session_id == session_a).unwrap();
    assert_eq!(entry_a.input_tokens, 3000);
    assert_eq!(entry_a.output_tokens, 1300);
    assert_eq!(entry_a.turn_count, 2);

    let entry_b = history.iter().find(|e| e.session_id == session_b).unwrap();
    assert_eq!(entry_b.input_tokens, 500);
    assert_eq!(entry_b.output_tokens, 200);
    assert_eq!(entry_b.turn_count, 1);
}

fn make_session(workspace: &Path) -> Session {
    Session {
        session_id: Uuid::now_v7(),
        workspace_root: workspace.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1000,
        },
        active_plan_id: None,
    }
}

#[test]
fn archive_old_sessions_marks_stale() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-store-gc-archive-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    // Create a session and manually backdate it
    let session = make_session(&workspace);
    store.save_session(&session).expect("save");
    let conn = store.db().expect("db");
    let old_date = (Utc::now() - chrono::Duration::days(45)).to_rfc3339();
    conn.execute(
        "UPDATE sessions SET updated_at = ?1 WHERE session_id = ?2",
        params![old_date, session.session_id.to_string()],
    )
    .unwrap();

    // Archive sessions older than 30 days
    let archived = store.archive_old_sessions(30).expect("archive");
    assert_eq!(archived, 1);

    // Running again should archive 0 (already archived)
    let archived2 = store.archive_old_sessions(30).expect("archive2");
    assert_eq!(archived2, 0);
}

#[test]
fn delete_old_sessions_removes_data() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-store-gc-delete-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    let session = make_session(&workspace);
    store.save_session(&session).expect("save");

    // Add an event for this session
    let ev = NewEvent {
        event_id: uuid::Uuid::now_v7(),
        at: Utc::now(),
        session_id: session.session_id,
        kind: EventKind::TurnAdded {
            role: "user".to_string(),
            content: "test".to_string(),
        },
    };
    store.append_event(&ev).expect("append");

    // Backdate the session to 100 days ago
    let conn = store.db().expect("db");
    let old_date = (Utc::now() - chrono::Duration::days(100)).to_rfc3339();
    conn.execute(
        "UPDATE sessions SET updated_at = ?1 WHERE session_id = ?2",
        params![old_date, session.session_id.to_string()],
    )
    .unwrap();

    let deleted = store.delete_old_sessions(90).expect("delete");
    assert_eq!(deleted, 1);

    // Session should be gone
    let loaded = store.load_session(session.session_id).expect("load");
    assert!(loaded.is_none());
}

#[test]
fn gc_returns_storage_size() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-store-gc-size-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("temp workspace");
    let store = Store::new(&workspace).expect("store");

    let (archived, deleted, size) = store.gc(30, 90).expect("gc");
    assert_eq!(archived, 0);
    assert_eq!(deleted, 0);
    assert!(size > 0, "db file should have non-zero size");
}
