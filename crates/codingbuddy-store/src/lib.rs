mod operations;
pub use operations::{OperationMonitor, OperationRecord};
mod steps;
pub use steps::{InvocationReceipt, PendingInvocation};
mod aux_tables;
mod migrate;
mod projection;
mod prompt;
mod records;
mod schema;
use anyhow::{Context, Result};
use chrono::Utc;
use codingbuddy_core::{
    ChatMessage, EventEnvelope, EventKind, Plan, Session, SessionState,
    parse_event_envelope_compat, parse_event_kind_compat, runtime_dir,
};
pub use projection::*;
pub use prompt::{PromptState, SessionLease};
pub use records::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use schema::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Convert a deserialization error into a `rusqlite` FromSqlConversionFailure.
fn text_parse_err(
    col: usize,
    e: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e))
}

/// Parse a UUID string, logging a warning and returning `Uuid::nil()` on failure.
/// This prevents silent data corruption by making parse errors observable.
fn parse_uuid_or_nil(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|e| {
        codingbuddy_core::logging::notice(format_args!(
            "codingbuddy-store: warning: corrupt UUID '{}': {e}",
            &s[..s.len().min(36)]
        ));
        Uuid::nil()
    })
}

/// Key for project-scoped data such as saved "always allow" approvals.
///
/// Keep this algorithm: changing it would orphan every approval already
/// saved. (`DefaultHasher` output can change between Rust releases; if that
/// ever happens, migrate the stored keys rather than silently losing them.)
pub fn project_hash(workspace: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace.to_string_lossy().as_bytes().hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
    db_path: PathBuf,
    events_path: PathBuf,
}

impl Store {
    pub fn new(workspace: &Path) -> Result<Self> {
        let root = runtime_dir(workspace);
        fs::create_dir_all(&root)?;
        let db_path = root.join("store.sqlite");
        let events_path = root.join("events.jsonl");
        let store = Self {
            root,
            db_path,
            events_path,
        };
        store.init_db()?;
        let db = store.db()?;
        Self::init_prompts(&db)?;
        Self::init_steps(&db)?;
        Self::init_operations(&db)?;
        Ok(store)
    }

    pub fn db(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(conn)
    }

    pub fn append_event(&self, event: &NewEvent) -> Result<EventEnvelope> {
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let envelope = Self::append_event_row(&tx, event)?;
        tx.commit()?;
        Ok(envelope)
    }

    fn append_event_row(tx: &Connection, event: &NewEvent) -> Result<EventEnvelope> {
        let session_id = event.session_id.to_string();
        let event_id = event.event_id.to_string();
        let at = event.at.to_rfc3339();
        let payload = serde_json::to_string(&event.kind)?;
        let existing = tx
            .query_row(
                "SELECT session_id, seq_no, at, payload FROM events WHERE event_id = ?1",
                [&event_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let mut envelope = EventEnvelope {
            seq_no: 0,
            at: event.at,
            session_id: event.session_id,
            kind: event.kind.clone(),
        };
        if let Some((owner, seq_no, timestamp, old_payload)) = existing {
            anyhow::ensure!(
                owner == session_id && timestamp == at && old_payload == payload,
                "event ID {event_id} already belongs to a different event"
            );
            envelope.seq_no = u64::try_from(seq_no)?;
        } else {
            let latest: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq_no), 0) FROM events WHERE session_id = ?1",
                [&session_id],
                |row| row.get(0),
            )?;
            let next = latest.checked_add(1).context("event sequence exhausted")?;
            envelope.seq_no = u64::try_from(next)?;
            tx.execute(
                "INSERT INTO events (session_id, seq_no, at, kind, payload, event_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![session_id, next, at, event_kind_name(&event.kind), payload, event_id],
            )?;
            Self::project_event(tx, &envelope)?;
        }
        Ok(envelope)
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        Self::save_session_row(&self.db()?, session)
    }

    fn save_session_row(conn: &Connection, session: &Session) -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO sessions (session_id, workspace_root, baseline_commit, status, budgets, active_plan_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.session_id.to_string(),
                session.workspace_root,
                session.baseline_commit,
                serde_json::to_string(&session.status)?,
                serde_json::to_string(&session.budgets)?,
                session.active_plan_id.map(|id| id.to_string()),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn load_latest_session(&self) -> Result<Option<Session>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, workspace_root, baseline_commit, status, budgets, active_plan_id
             FROM sessions ORDER BY updated_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(Session {
                session_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                workspace_root: row.get(1)?,
                baseline_commit: row.get(2)?,
                status: serde_json::from_str(&row.get::<_, String>(3)?)?,
                budgets: serde_json::from_str(&row.get::<_, String>(4)?)?,
                active_plan_id: row
                    .get::<_, Option<String>>(5)?
                    .map(|v| Uuid::parse_str(&v))
                    .transpose()?,
            }));
        }
        Ok(None)
    }

    pub fn load_session(&self, session_id: Uuid) -> Result<Option<Session>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, workspace_root, baseline_commit, status, budgets, active_plan_id
             FROM sessions WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query(params![session_id.to_string()])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(Session {
                session_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                workspace_root: row.get(1)?,
                baseline_commit: row.get(2)?,
                status: serde_json::from_str(&row.get::<_, String>(3)?)?,
                budgets: serde_json::from_str(&row.get::<_, String>(4)?)?,
                active_plan_id: row
                    .get::<_, Option<String>>(5)?
                    .map(|v| Uuid::parse_str(&v))
                    .transpose()?,
            }));
        }
        Ok(None)
    }

    pub fn save_run(&self, run: &codingbuddy_core::RunRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO runs (run_id, session_id, status, prompt, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run.run_id.to_string(),
                run.session_id.to_string(),
                serde_json::to_string(&run.status)?,
                run.prompt,
                run.created_at,
                run.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_latest_run(&self, session_id: Uuid) -> Result<Option<codingbuddy_core::RunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, session_id, status, prompt, created_at, updated_at
             FROM runs WHERE session_id = ?1 ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id.to_string()])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(codingbuddy_core::RunRecord {
                run_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                session_id: Uuid::parse_str(row.get::<_, String>(1)?.as_str())?,
                status: serde_json::from_str(&row.get::<_, String>(2)?)?,
                prompt: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            }));
        }
        Ok(None)
    }

    pub fn load_run(&self, run_id: Uuid) -> Result<Option<codingbuddy_core::RunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, session_id, status, prompt, created_at, updated_at
             FROM runs WHERE run_id = ?1",
        )?;
        let mut rows = stmt.query(params![run_id.to_string()])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(codingbuddy_core::RunRecord {
                run_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                session_id: Uuid::parse_str(row.get::<_, String>(1)?.as_str())?,
                status: serde_json::from_str(&row.get::<_, String>(2)?)?,
                prompt: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            }));
        }
        Ok(None)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, workspace_root, baseline_commit, status, budgets, active_plan_id
             FROM sessions ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Session {
                session_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())
                    .map_err(|e| text_parse_err(0, e))?,
                workspace_root: row.get(1)?,
                baseline_commit: row.get(2)?,
                status: serde_json::from_str(&row.get::<_, String>(3)?)
                    .map_err(|e| text_parse_err(3, e))?,
                budgets: serde_json::from_str(&row.get::<_, String>(4)?)
                    .map_err(|e| text_parse_err(4, e))?,
                active_plan_id: row
                    .get::<_, Option<String>>(5)?
                    .map(|v| Uuid::parse_str(&v).map_err(|e| text_parse_err(5, e)))
                    .transpose()?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(session) => out.push(session),
                Err(e) => codingbuddy_core::logging::notice(format_args!(
                    "[store] skipping corrupt session row: {e}"
                )),
            }
        }
        Ok(out)
    }

    pub fn list_runs(&self, session_id: Uuid) -> Result<Vec<codingbuddy_core::RunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, session_id, status, prompt, created_at, updated_at
             FROM runs WHERE session_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id.to_string()], |row| {
            Ok(codingbuddy_core::RunRecord {
                run_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())
                    .map_err(|e| text_parse_err(0, e))?,
                session_id: Uuid::parse_str(row.get::<_, String>(1)?.as_str())
                    .map_err(|e| text_parse_err(1, e))?,
                status: serde_json::from_str(&row.get::<_, String>(2)?)
                    .map_err(|e| text_parse_err(2, e))?,
                prompt: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(run) => out.push(run),
                Err(e) => codingbuddy_core::logging::notice(format_args!(
                    "[store] skipping corrupt run row: {e}"
                )),
            }
        }
        Ok(out)
    }

    pub fn save_plan(&self, session_id: Uuid, plan: &Plan) -> Result<()> {
        Self::save_plan_row(&self.db()?, session_id, plan)
    }

    fn save_plan_row(conn: &Connection, session_id: Uuid, plan: &Plan) -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO plans (plan_id, session_id, version, goal, payload, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                plan.plan_id.to_string(),
                session_id.to_string(),
                plan.version as i64,
                plan.goal,
                serde_json::to_string(plan)?,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }
}

/// Simple glob matching for persistent approval patterns.
/// Supports trailing `*` wildcard (e.g., "cargo *" matches "cargo test").
fn glob_matches_command(pattern: &str, command: &str) -> bool {
    if pattern == command {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return command.starts_with(prefix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return command.starts_with(prefix);
    }
    false
}

impl Store {
    pub fn load_latest_session_for_workspace(
        &self,
        workspace_root: &str,
    ) -> Result<Option<Session>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, workspace_root, baseline_commit, status, budgets, active_plan_id
             FROM sessions WHERE workspace_root = ?1 ORDER BY updated_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([workspace_root])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(Session {
                session_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                workspace_root: row.get(1)?,
                baseline_commit: row.get(2)?,
                status: serde_json::from_str(&row.get::<_, String>(3)?)?,
                budgets: serde_json::from_str(&row.get::<_, String>(4)?)?,
                active_plan_id: row
                    .get::<_, Option<String>>(5)?
                    .map(|v| Uuid::parse_str(&v))
                    .transpose()?,
            }));
        }
        Ok(None)
    }

    pub fn rebuild_from_events(&self, session_id: Uuid) -> Result<RebuildProjection> {
        let mut projection = RebuildProjection::default();
        for event in self.session_events(session_id)? {
            apply_projection(&mut projection, &event);
        }
        Ok(projection)
    }

    pub fn session_events(&self, session_id: Uuid) -> Result<Vec<EventEnvelope>> {
        let conn = self.db()?;
        let mut statement = conn.prepare(
            "SELECT seq_no, at, payload FROM events WHERE session_id = ?1 ORDER BY seq_no",
        )?;
        let mut rows = statement.query([session_id.to_string()])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            events.push(EventEnvelope {
                session_id,
                seq_no: u64::try_from(row.get::<_, i64>(0)?)?,
                at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(1)?)?.to_utc(),
                kind: public_event(parse_event_kind_compat(&row.get::<_, String>(2)?)?),
            });
        }
        Ok(events)
    }

    pub fn poll_events(
        &self,
        session_id: Uuid,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let Ok(after_seq) = i64::try_from(after_seq) else {
            return Ok(Vec::new());
        };
        self.read_event_rows(
            "SELECT seq_no, at, kind, payload FROM events WHERE session_id = ?1 AND seq_no > ?2 ORDER BY seq_no LIMIT ?3",
            params![session_id.to_string(), after_seq, i64::try_from(limit)?],
        )
    }

    pub fn timeline_events(&self, session_id: Uuid, limit: usize) -> Result<Vec<StoredEvent>> {
        self.read_event_rows(
            "SELECT seq_no, at, kind, payload FROM events WHERE session_id = ?1 ORDER BY seq_no DESC LIMIT ?2",
            params![session_id.to_string(), i64::try_from(limit)?],
        )
    }

    fn read_event_rows(
        &self,
        sql: &str,
        parameters: impl rusqlite::Params,
    ) -> Result<Vec<StoredEvent>> {
        let conn = self.db()?;
        let mut statement = conn.prepare(sql)?;
        let mut rows = statement.query(parameters)?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            let kind: String = row.get(2)?;
            let body: String = row.get(3)?;
            let payload = if kind.starts_with("ChatTurn") {
                serde_json::to_value(public_event(parse_event_kind_compat(&body)?))?
            } else {
                serde_json::from_str(&body)?
            };
            events.push(StoredEvent {
                seq_no: u64::try_from(row.get::<_, i64>(0)?)?,
                at: row.get(1)?,
                kind,
                payload,
            });
        }
        Ok(events)
    }

    /// Archive (soft-delete) sessions older than `archive_days`.
    /// Returns the number of sessions archived.
    pub fn archive_old_sessions(&self, archive_days: u32) -> Result<u64> {
        let conn = self.db()?;
        let cutoff = (Utc::now() - chrono::Duration::days(i64::from(archive_days))).to_rfc3339();
        let count = conn.execute(
            "UPDATE sessions SET status = '\"Archived\"'
             WHERE updated_at < ?1 AND status != '\"Archived\"'",
            params![cutoff],
        )?;
        Ok(count as u64)
    }

    /// Permanently delete sessions (and their events) older than `delete_days`.
    /// Returns the number of sessions deleted.
    pub fn delete_old_sessions(&self, delete_days: u32) -> Result<u64> {
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cutoff = (Utc::now() - chrono::Duration::days(i64::from(delete_days))).to_rfc3339();
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT session_id FROM sessions WHERE updated_at < ?1")?;
            stmt.query_map([cutoff], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut leases = Vec::new();
        for id in &ids {
            let Ok(lease) = self.session_lease(Uuid::parse_str(id)?) else {
                continue;
            };
            leases.push(lease);
            Self::delete_session_runtime(&tx, id)?;
            tx.execute("DELETE FROM events WHERE session_id = ?1", [id])?;
            tx.execute("DELETE FROM plans WHERE session_id = ?1", [id])?;
            tx.execute("DELETE FROM sessions WHERE session_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(leases.len() as u64)
    }

    /// Return total database file size in bytes.
    pub fn storage_bytes(&self) -> Result<u64> {
        let meta = fs::metadata(&self.db_path)?;
        let mut total = meta.len();
        if self.events_path.exists() {
            total += fs::metadata(&self.events_path)?.len();
        }
        Ok(total)
    }

    /// Run full garbage collection: archive >30 days, delete >90 days.
    /// Returns `(archived, deleted, storage_bytes)`.
    pub fn gc(&self, archive_days: u32, delete_days: u32) -> Result<(u64, u64, u64)> {
        let archived = self.archive_old_sessions(archive_days)?;
        let deleted = self.delete_old_sessions(delete_days)?;
        let size = self.storage_bytes()?;
        Ok((archived, deleted, size))
    }
}

#[cfg(test)]
mod tests;
