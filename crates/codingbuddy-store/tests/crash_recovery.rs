//! E2E test: session survives simulated crash after a durable commit.

use anyhow::Result;
use chrono::Utc;
use codingbuddy_core::{EventKind, Session, SessionBudgets, SessionState};
use codingbuddy_store::Store;
use uuid::Uuid;

#[test]
fn session_survives_crash() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session_id = Uuid::now_v7();

    // Phase 1: Create session, and commit events
    {
        let store = Store::new(dir.path())?;
        store.save_session(&Session {
            session_id,
            workspace_root: dir.path().to_string_lossy().to_string(),
            baseline_commit: None,
            status: SessionState::ExecutingStep,
            budgets: SessionBudgets {
                per_turn_seconds: 30,
                max_think_tokens: 1000,
            },
            active_plan_id: None,
        })?;

        for i in 0..3 {
            store.append_event(&codingbuddy_store::NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: Utc::now(),
                session_id,
                kind: EventKind::TurnAdded {
                    role: "user".to_string(),
                    content: format!("message {i}"),
                },
            })?;
        }
        // Drop store — simulates crash (no graceful shutdown)
    }

    // Phase 2: Reopen and verify
    {
        let store = Store::new(dir.path())?;
        let session = store
            .load_session(session_id)?
            .expect("session should survive");
        assert_eq!(session.session_id, session_id);

        let projection = store.rebuild_from_events(session_id)?;
        assert!(
            projection.transcript.len() >= 3,
            "all 3 events should be recoverable, got {}",
            projection.transcript.len()
        );
    }

    Ok(())
}

fn turn(session_id: Uuid, text: &str) -> codingbuddy_store::NewEvent {
    codingbuddy_store::NewEvent::new(
        session_id,
        EventKind::TurnAdded {
            role: "user".to_owned(),
            content: text.to_owned(),
        },
    )
}

#[test]
fn event_append_is_atomic_and_idempotent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::new(dir.path())?;
    let session = Uuid::now_v7();
    let event = turn(session, "retry");
    store.db()?.execute_batch(
        "CREATE TRIGGER fail_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'insert failure'); END;"
    )?;
    assert!(store.append_event(&event).is_err());
    assert!(store.session_events(session)?.is_empty());
    store.db()?.execute_batch("DROP TRIGGER fail_event")?;
    assert_eq!(store.append_event(&event)?.seq_no, 1);
    assert_eq!(store.append_event(&event)?.seq_no, 1);
    let conflict = codingbuddy_store::NewEvent {
        kind: turn(session, "different").kind,
        ..event
    };
    assert!(store.append_event(&conflict).is_err());

    let approval = codingbuddy_store::NewEvent::new(
        session,
        EventKind::ToolApproved {
            invocation_id: Uuid::now_v7(),
        },
    );
    store.db()?.execute_batch(
        "CREATE TRIGGER fail_projection BEFORE INSERT ON approvals_ledger BEGIN SELECT RAISE(ABORT, 'projection failure'); END;"
    )?;
    assert!(store.append_event(&approval).is_err());
    assert_eq!(store.session_events(session)?.len(), 1);
    store.db()?.execute_batch("DROP TRIGGER fail_projection")?;
    assert_eq!(store.append_event(&approval)?.seq_no, 2);
    assert_eq!(store.append_event(&approval)?.seq_no, 2);
    let count: i64 = store
        .db()?
        .query_row("SELECT count(*) FROM approvals_ledger", [], |row| {
            row.get(0)
        })?;
    assert_eq!(count, 1);
    assert_eq!(store.poll_events(session, 1, 10)?.len(), 1);
    Ok(())
}

#[test]
fn archive_only_upgrade_imports_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join(".codingbuddy");
    std::fs::create_dir(&root)?;
    let session_id = Uuid::now_v7();
    let event = codingbuddy_core::EventEnvelope {
        session_id,
        seq_no: 3,
        at: Utc::now(),
        kind: turn(session_id, "archived").kind,
    };
    let contents = format!("{}\n{{", serde_json::to_string(&event)?);
    let journal = root.join("events.jsonl");
    std::fs::write(&journal, &contents)?;
    for _ in 0..2 {
        let store = Store::new(dir.path())?;
        assert!(store.load_session(session_id)?.is_some());
        assert_eq!(
            store.rebuild_from_events(session_id)?.transcript,
            ["user: archived"]
        );
        assert_eq!(store.session_events(session_id)?[0].seq_no, 3);
    }
    assert_eq!(std::fs::read_to_string(journal)?, contents);
    Ok(())
}

#[test]
fn invalid_complete_archive_record_rolls_back() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join(".codingbuddy");
    std::fs::create_dir(&root)?;
    let journal = root.join("events.jsonl");
    std::fs::write(&journal, "{\n")?;
    let error = Store::new(dir.path())
        .err()
        .expect("invalid complete record");
    assert!(error.to_string().contains("line 1"));
    std::fs::write(&journal, "")?;
    assert!(Store::new(dir.path())?.list_sessions()?.is_empty());
    Ok(())
}

#[test]
fn independent_writers_allocate_unique_sequences() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session = Uuid::now_v7();
    let store = Store::new(dir.path())?;
    std::thread::scope(|scope| {
        let left = scope.spawn(|| -> Result<()> {
            let store = Store::new(dir.path())?;
            for _ in 0..10 {
                store.append_event(&turn(session, "left"))?;
            }
            Ok(())
        });
        let right = scope.spawn(|| -> Result<()> {
            let store = Store::new(dir.path())?;
            for _ in 0..10 {
                store.append_event(&turn(session, "right"))?;
            }
            Ok(())
        });
        left.join().unwrap()?;
        right.join().unwrap()
    })?;
    let sequences: Vec<_> = store
        .session_events(session)?
        .into_iter()
        .map(|event| event.seq_no)
        .collect();
    assert_eq!(sequences, (1..=20).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn legacy_database_takes_precedence_and_repairs_collisions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join(".codingbuddy");
    std::fs::create_dir(&root)?;
    let conn = rusqlite::Connection::open(root.join("store.sqlite"))?;
    conn.execute_batch(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
            seq_no INTEGER NOT NULL, at TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL);",
    )?;
    let session = Uuid::now_v7();
    let at = Utc::now();
    for (seq, content) in [(1, "first"), (1, "first"), (1, "second"), (5, "third")] {
        let payload = serde_json::to_string(&turn(session, content).kind)?;
        conn.execute(
            "INSERT INTO events (session_id, seq_no, at, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![session.to_string(), seq, at.to_rfc3339(), "TurnAdded@v1", payload],
        )?;
    }
    drop(conn);
    let journal = root.join("events.jsonl");
    let archive = serde_json::to_string(&codingbuddy_core::EventEnvelope {
        session_id: session,
        seq_no: 6,
        at,
        kind: turn(session, "uncommitted").kind,
    })?;
    std::fs::write(&journal, &archive)?;
    let store = Store::new(dir.path())?;
    assert_eq!(
        store.rebuild_from_events(session)?.transcript,
        ["user: first", "user: second", "user: third"]
    );
    let rows = store.poll_events(session, 1, 100)?;
    assert_eq!(
        rows.iter().map(|row| row.seq_no).collect::<Vec<_>>(),
        [2, 5]
    );
    assert_eq!(store.append_event(&turn(session, "new"))?.seq_no, 6);
    assert_eq!(std::fs::read_to_string(journal)?, archive);
    Ok(())
}
