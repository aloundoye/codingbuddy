//! E2E test: session survives simulated crash via flush_sync.

use anyhow::Result;
use chrono::Utc;
use codingbuddy_core::{EventEnvelope, EventKind, Session, SessionBudgets, SessionState};
use codingbuddy_store::Store;
use uuid::Uuid;

#[test]
fn session_survives_crash() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session_id = Uuid::now_v7();

    // Phase 1: Create session, store events, flush
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
            store.append_event(&EventEnvelope {
                seq_no: i + 1,
                at: Utc::now(),
                session_id,
                kind: EventKind::TurnAdded {
                    role: "user".to_string(),
                    content: format!("message {i}"),
                },
            })?;
        }
        store.flush_sync()?;
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
