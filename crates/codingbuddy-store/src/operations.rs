//! Durable ownership and cancellation for work that can outlive a client.
use super::*;
use serde_json::Value;

pub type FilePostimages = std::collections::BTreeMap<PathBuf, (Option<String>, Option<u32>)>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: Uuid,
    pub session_id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub kind: String,
    pub state: String,
    pub payload: Value,
    pub cancel_requested: bool,
}
impl Store {
    pub(crate) fn init_operations(db: &Connection) -> Result<()> {
        db.execute_batch("CREATE TABLE IF NOT EXISTS runtime_operations(id TEXT PRIMARY KEY,session_id TEXT,parent_id TEXT,kind TEXT NOT NULL,state TEXT NOT NULL,payload TEXT NOT NULL,cancel_requested INTEGER NOT NULL DEFAULT 0);CREATE INDEX IF NOT EXISTS runtime_operation_parent ON runtime_operations(parent_id);CREATE TABLE IF NOT EXISTS runtime_file_postimages(path TEXT PRIMARY KEY,hash TEXT,recorded_at TEXT NOT NULL);")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS runtime_restore_forks(plan_id TEXT PRIMARY KEY,fork_id TEXT NOT NULL);CREATE TABLE IF NOT EXISTS runtime_restore_requests(plan_id TEXT PRIMARY KEY,mode TEXT NOT NULL);")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS runtime_scoped_postimages(workspace TEXT NOT NULL,path TEXT NOT NULL,hash TEXT,mode INTEGER,recorded_at TEXT NOT NULL,PRIMARY KEY(workspace,path));")?;
        Ok(())
    }
    pub fn begin_operation(
        &self,
        id: Uuid,
        session: Option<Uuid>,
        parent: Option<Uuid>,
        kind: &str,
        payload: &Value,
    ) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(parent) = parent {
            let allowed: bool = tx
                .query_row(
                    "SELECT cancel_requested=0 FROM runtime_operations WHERE id=?1",
                    [parent.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(false);
            anyhow::ensure!(allowed, "Parent operation is unavailable or cancelled");
        }
        tx.execute("INSERT INTO runtime_operations(id,session_id,parent_id,kind,state,payload) VALUES(?1,?2,?3,?4,'running',?5)",params![id.to_string(),session.map(|id|id.to_string()),parent.map(|id|id.to_string()),kind,serde_json::to_string(payload)?])?;
        tx.commit()?;
        Ok(())
    }
    pub fn operation(&self, id: Uuid) -> Result<OperationRecord> {
        let (session,parent,kind,state,payload,cancel):(Option<String>,Option<String>,String,String,String,bool)=self.db()?.query_row("SELECT session_id,parent_id,kind,state,payload,cancel_requested FROM runtime_operations WHERE id=?1",[id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)))?;
        Ok(OperationRecord {
            id,
            session_id: session.map(|id| Uuid::parse_str(&id)).transpose()?,
            parent_id: parent.map(|id| Uuid::parse_str(&id)).transpose()?,
            kind,
            state,
            payload: serde_json::from_str(&payload)?,
            cancel_requested: cancel,
        })
    }
    pub fn transition_operation(
        &self,
        id: Uuid,
        expected: &str,
        next: &str,
        payload: &Value,
    ) -> Result<()> {
        let changed = self.db()?.execute(
            "UPDATE runtime_operations SET state=?1,payload=?2 WHERE id=?3 AND state=?4 AND NOT(cancel_requested=1 AND ?1='running')",
            params![
                next,
                serde_json::to_string(payload)?,
                id.to_string(),
                expected
            ],
        )?;
        anyhow::ensure!(changed == 1, "Operation state changed concurrently");
        Ok(())
    }
    pub fn settle_operation_event(
        &self,
        id: Uuid,
        expected: &str,
        next: &str,
        payload: &Value,
        event: Option<&NewEvent>,
    ) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE runtime_operations SET state=?2,payload=?3 WHERE id=?1 AND state=?4",
            params![
                id.to_string(),
                next,
                serde_json::to_string(payload)?,
                expected
            ],
        )?;
        anyhow::ensure!(changed == 1, "Operation changed concurrently");
        if let Some(event) = event {
            Self::append_event_row(&tx, event)?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn attach_operation_parent(&self, id: Uuid, parent: Uuid) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cycle:bool=tx.query_row("WITH RECURSIVE ancestors(id,parent_id) AS (SELECT id,parent_id FROM runtime_operations WHERE id=?1 UNION ALL SELECT op.id,op.parent_id FROM runtime_operations op JOIN ancestors ON op.id=ancestors.parent_id) SELECT EXISTS(SELECT 1 FROM ancestors WHERE id=?2)",params![parent.to_string(),id.to_string()],|row|row.get(0))?;
        anyhow::ensure!(!cycle, "Operation ownership cycle");
        let changed=tx.execute("UPDATE runtime_operations SET parent_id=?2 WHERE id=?1 AND parent_id IS NULL AND EXISTS(SELECT 1 FROM runtime_operations WHERE id=?2 AND cancel_requested=0)",params![id.to_string(),parent.to_string()])?;
        anyhow::ensure!(changed == 1, "Parent operation unavailable or cancelled");
        tx.commit()?;
        Ok(())
    }
    pub fn cancel_operation(&self, id: Uuid) -> Result<usize> {
        Ok(self.db()?.execute("WITH RECURSIVE owned(id) AS (SELECT id FROM runtime_operations WHERE id=?1 UNION ALL SELECT child.id FROM runtime_operations child JOIN owned ON child.parent_id=owned.id) UPDATE runtime_operations SET cancel_requested=1 WHERE id IN (SELECT id FROM owned)",[id.to_string()])?)
    }
    pub fn record_postimage(&self, path: &Path, hash: Option<&str>) -> Result<()> {
        self.record_postimage_at(
            self.root
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Workspace unavailable"))?,
            path,
            hash,
        )
    }
    pub fn record_postimage_at(
        &self,
        workspace: &Path,
        path: &Path,
        hash: Option<&str>,
    ) -> Result<()> {
        let files = codingbuddy_core::workspace_fs::WorkspaceFs::open(workspace)?;
        let path = files.relative(path)?;
        #[cfg(unix)]
        let mode = if hash.is_some() {
            use std::os::unix::fs::PermissionsExt;
            Some(
                files
                    .open_file(&path)?
                    .into_std()
                    .metadata()?
                    .permissions()
                    .mode()
                    & 0o777,
            )
        } else {
            None
        };
        #[cfg(not(unix))]
        let mode: Option<u32> = None;
        self.db()?.execute("INSERT INTO runtime_scoped_postimages(workspace,path,hash,mode,recorded_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(workspace,path) DO UPDATE SET hash=excluded.hash,mode=excluded.mode,recorded_at=excluded.recorded_at",params![files.root().to_string_lossy(),path.to_string_lossy(),hash,mode,Utc::now().to_rfc3339()])?;
        Ok(())
    }
    pub fn postimages_since(
        &self,
        date: &str,
    ) -> Result<std::collections::BTreeMap<PathBuf, Option<String>>> {
        Ok(self
            .scoped_postimages_since(
                self.root
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("Workspace unavailable"))?,
                date,
            )?
            .into_iter()
            .map(|(path, (hash, _))| (path, hash))
            .collect())
    }
    pub fn scoped_postimages_since(&self, workspace: &Path, date: &str) -> Result<FilePostimages> {
        Ok(self.db()?.prepare("SELECT path,hash,mode FROM runtime_scoped_postimages WHERE workspace=?1 AND recorded_at>=?2")?.query_map(params![workspace.canonicalize()?.to_string_lossy(),date],|row|Ok((PathBuf::from(row.get::<_,String>(0)?),(row.get(1)?,row.get(2)?))))?.collect::<rusqlite::Result<_>>()?)
    }
}

impl Store {
    pub fn checkpoint_cursor(&self, session: Uuid) -> Result<u64> {
        let mut pending = std::collections::HashSet::new();
        let mut safe = 0;
        for event in self.session_events(session)? {
            match &event.kind {
                EventKind::ModelStep { parts, .. } => {
                    for part in parts {
                        if let codingbuddy_core::ModelPart::Call { id, .. } = part {
                            pending.insert(*id);
                        }
                    }
                }
                EventKind::ModelToolResult { invocation_id, .. } => {
                    pending.remove(invocation_id);
                }
                _ => {}
            }
            if pending.is_empty() {
                safe = event.seq_no;
            }
        }
        Ok(safe)
    }
    pub fn fork_session_at_cursor(&self, source: Uuid, cursor: u64) -> Result<Session> {
        self.fork_cursor(source, cursor, None)
    }
    pub fn fork_for_restore(&self, plan: Uuid, source: Uuid, cursor: u64) -> Result<Session> {
        self.fork_cursor(source, cursor, Some(plan))
    }
    pub fn bind_restore_mode(&self, id: Uuid, mode: &str) -> Result<()> {
        anyhow::ensure!(
            matches!(mode, "files" | "conversation" | "both"),
            "Invalid restore mode"
        );
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO runtime_restore_requests(plan_id,mode) VALUES(?1,?2)",
            params![id.to_string(), mode],
        )?;
        let existing: String = tx.query_row(
            "SELECT mode FROM runtime_restore_requests WHERE plan_id=?1",
            [id.to_string()],
            |row| row.get(0),
        )?;
        anyhow::ensure!(existing == mode, "Restore mode changed; preview a new plan");
        tx.commit()?;
        Ok(())
    }
    fn fork_cursor(&self, source: Uuid, cursor: u64, restore: Option<Uuid>) -> Result<Session> {
        let original = self
            .load_session(source)?
            .ok_or_else(|| anyhow::anyhow!("Source session unavailable"))?;
        let events = self.session_events(source)?;
        anyhow::ensure!(
            cursor <= events.last().map_or(0, |event| event.seq_no),
            "Conversation cursor is beyond recorded history"
        );
        let mut pending = std::collections::HashSet::new();
        for event in events.iter().filter(|event| event.seq_no <= cursor) {
            match &event.kind {
                EventKind::ModelStep { parts, .. } => {
                    for part in parts {
                        if let codingbuddy_core::ModelPart::Call { id, .. } = part {
                            pending.insert(*id);
                        }
                    }
                }
                EventKind::ModelToolResult { invocation_id, .. } => {
                    pending.remove(invocation_id);
                }
                _ => {}
            }
        }
        anyhow::ensure!(
            pending.is_empty(),
            "Conversation cursor contains unsettled tool calls"
        );
        let fork = Session {
            session_id: Uuid::now_v7(),
            status: SessionState::Idle,
            active_plan_id: None,
            ..original
        };
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(plan) = restore {
            if let Some(existing) = tx
                .query_row(
                    "SELECT fork_id FROM runtime_restore_forks WHERE plan_id=?1",
                    [plan.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                drop(tx);
                return self
                    .load_session(Uuid::parse_str(&existing)?)?
                    .ok_or_else(|| anyhow::anyhow!("Restore fork receipt is unavailable"));
            }
            tx.execute(
                "INSERT INTO runtime_restore_forks(plan_id,fork_id) VALUES(?1,?2)",
                params![plan.to_string(), fork.session_id.to_string()],
            )?;
        }
        Self::save_session_row(&tx, &fork)?;
        let linked = EventKind::SessionForked {
            from_session_id: source,
            to_session_id: fork.session_id,
        };
        Self::append_event_row(&tx, &NewEvent::new(source, linked.clone()))?;
        Self::append_event_row(&tx, &NewEvent::new(fork.session_id, linked))?;
        tx.execute("INSERT INTO runtime_session_routes(session_id,route) SELECT ?1,route FROM runtime_session_routes WHERE session_id=?2",params![fork.session_id.to_string(),source.to_string()])?;
        for event in events.into_iter().filter(|event| event.seq_no <= cursor) {
            if matches!(
                &event.kind,
                EventKind::ChatTurn { .. }
                    | EventKind::ModelStep { .. }
                    | EventKind::ModelToolResult { .. }
            ) {
                if let EventKind::ModelStep { step_id, .. } = &event.kind {
                    tx.execute("INSERT INTO runtime_native_steps(session_id,step_id,payload) SELECT ?1,step_id,payload FROM runtime_native_steps WHERE session_id=?2 AND step_id=?3",params![fork.session_id.to_string(),source.to_string(),step_id.to_string()])?;
                }
                Self::append_event_row(&tx, &NewEvent::new(fork.session_id, event.kind))?;
            }
        }
        tx.commit()?;
        Ok(fork)
    }
}

/// Bridges cancellation across clients/processes without sharing credentials or listeners.
pub struct OperationMonitor {
    task: tokio::task::JoinHandle<()>,
    store: Store,
    id: Uuid,
    token: codingbuddy_core::CancellationToken,
}
impl Drop for OperationMonitor {
    fn drop(&mut self) {
        if self.token.is_cancelled() {
            let _ = self.store.cancel_operation(self.id);
        }
        self.task.abort();
    }
}
impl Store {
    /// Link `token` to the operation's durable cancel flag, in both
    /// directions: cancelling the token records the request at once, and a
    /// cancel requested by another client (checked every 30 ms) fires the
    /// token. Runs as a task, so a running tool holds no extra thread.
    pub fn monitor_operation(
        &self,
        id: Uuid,
        token: codingbuddy_core::CancellationToken,
    ) -> Result<OperationMonitor> {
        self.operation(id)?;
        let store = self.clone();
        let monitored = token.clone();
        let task = codingbuddy_core::runtime::runtime().spawn(async move {
            let mut poll = tokio::time::interval(std::time::Duration::from_millis(30));
            loop {
                tokio::select! {
                    () = token.cancelled() => {
                        let _ = store.cancel_operation(id);
                        return;
                    }
                    _ = poll.tick() => {
                        match store.operation(id) {
                            Ok(operation) if !operation.cancel_requested => {}
                            _ => {
                                token.cancel();
                                return;
                            }
                        }
                    }
                }
            }
        });
        Ok(OperationMonitor {
            task,
            store: self.clone(),
            id,
            token: monitored,
        })
    }
}

impl Store {
    /// Reconcile only supervisor ownership, never replay a lost worker or its effects.
    pub fn reconcile_background_job(&self, id: Uuid) -> Result<Option<BackgroundJobRecord>> {
        let Some(mut job) = self.load_background_job(id)? else {
            return Ok(None);
        };
        let Ok(mut operation) = self.operation(id) else {
            return Ok(Some(job));
        };
        if operation.kind != "worker" {
            return Ok(Some(job));
        }
        if matches!(operation.state.as_str(), "running" | "prepared")
            && let Ok(_ownership) = self.session_lease(id)
        {
            let expired = operation.payload["created_at"]
                .as_str()
                .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
                .is_some_and(|created| {
                    Utc::now().signed_duration_since(created).num_seconds() > 120
                });
            if operation.state == "running" || expired {
                let next = if operation.state == "prepared" {
                    "handoff_failed"
                } else {
                    "ambiguous"
                };
                operation.payload["recovery_status"] = serde_json::json!(next);
                self.transition_operation(id, &operation.state, next, &operation.payload)?;
                operation.state = next.into();
            }
        }
        job.status = if operation.cancel_requested
            && matches!(operation.state.as_str(), "running" | "prepared")
        {
            "cancelling".into()
        } else {
            operation.state
        };
        job.metadata_json = operation.payload.to_string();
        job.updated_at = Utc::now().to_rfc3339();
        self.upsert_background_job(&job)?;
        Ok(Some(job))
    }
}

#[cfg(test)]
mod delivery_three {
    use super::*;
    #[test]
    fn cancellation_reaches_descendants_after_reopen_and_blocks_new_children() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::new(temp.path())?;
        let root = Uuid::now_v7();
        let child = Uuid::now_v7();
        let sibling = Uuid::now_v7();
        let leaf = Uuid::now_v7();
        store.begin_operation(root, None, None, "prompt", &serde_json::json!({}))?;
        for id in [child, sibling] {
            store.begin_operation(id, None, Some(root), "worker", &serde_json::json!({}))?;
        }
        store.begin_operation(leaf, None, Some(child), "command", &serde_json::json!({}))?;
        let token = codingbuddy_core::CancellationToken::new();
        let _monitor = store.monitor_operation(leaf, token.clone())?;
        let reopened = Store::new(temp.path())?;
        assert_eq!(reopened.cancel_operation(child)?, 2);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !token.is_cancelled() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(token.is_cancelled());
        assert!(!store.operation(root)?.cancel_requested);
        assert!(!store.operation(sibling)?.cancel_requested);
        assert!(
            store
                .begin_operation(
                    Uuid::now_v7(),
                    None,
                    Some(child),
                    "command",
                    &serde_json::json!({})
                )
                .is_err()
        );
        store.transition_operation(root, "running", "completed", &serde_json::json!({}))?;
        assert!(reopened.cancel_operation(root)? > 0);
        assert!(store.operation(sibling)?.cancel_requested);
        Ok(())
    }
    #[test]
    fn ownership_is_scoped_to_execution_workspace() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let child = tempfile::tempdir()?;
        let store = Store::new(parent.path())?;
        std::fs::write(parent.path().join("same"), "parent")?;
        std::fs::write(child.path().join("same"), "child")?;
        store.record_postimage_at(child.path(), Path::new("same"), Some("child-hash"))?;
        assert!(store.scoped_postimages_since(parent.path(), "")?.is_empty());
        assert_eq!(store.scoped_postimages_since(child.path(), "")?.len(), 1);
        Ok(())
    }
}
