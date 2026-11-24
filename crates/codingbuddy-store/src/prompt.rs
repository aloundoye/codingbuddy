use super::*;
use serde_json::Value;

#[derive(Debug)]
pub struct SessionLease {
    session_id: Uuid,
    store_root: PathBuf,
    _lock: fs::File,
}

impl SessionLease {
    pub fn matches(&self, store: &Store, session_id: Uuid) -> Result<bool> {
        Ok(self.session_id == session_id && self.store_root == store.root.canonicalize()?)
    }
}

#[derive(Debug, Serialize)]
pub struct PromptState {
    pub prompt_id: Uuid,
    pub session_id: Uuid,
    pub status: String,
    pub output: Option<String>,
    pub error: Option<String>,
}

impl Store {
    pub fn session_lease(&self, session_id: Uuid) -> Result<SessionLease> {
        let root = self.root.canonicalize()?;
        let lock = codingbuddy_core::workspace_fs::WorkspaceFs::open(&root)?
            .open_lock_file(Path::new(&format!("session-{session_id}.lock")))?
            .into_std();
        lock.try_lock().map_err(|error| {
            anyhow::anyhow!("session already has a writer or its lock is unavailable: {error}")
        })?;
        Ok(SessionLease {
            session_id,
            store_root: root,
            _lock: lock,
        })
    }

    pub(crate) fn init_prompts(conn: &Connection) -> Result<()> {
        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS runtime_prompts (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL, request_id TEXT NOT NULL,
                request TEXT NOT NULL, status TEXT NOT NULL, output TEXT, error TEXT,
                UNIQUE(session_id, request_id)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS runtime_active_prompt
                ON runtime_prompts(session_id) WHERE status = 'running';
            CREATE TABLE IF NOT EXISTS runtime_prompt_events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, prompt_id TEXT NOT NULL, payload TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS runtime_prompt_event_cursor
                ON runtime_prompt_events(prompt_id, seq);
        ")?;
        Ok(())
    }

    pub fn existing_prompt(
        &self,
        session: Uuid,
        request: Uuid,
        input: &Value,
    ) -> Result<Option<Uuid>> {
        let row: Option<(String, String)> = self
            .db()?
            .query_row(
                "SELECT id, request FROM runtime_prompts WHERE session_id=?1 AND request_id=?2",
                params![session.to_string(), request.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(id, original)| {
            anyhow::ensure!(
                serde_json::from_str::<Value>(&original)? == *input,
                "request ID already used with different input"
            );
            Ok(Uuid::parse_str(&id)?)
        })
        .transpose()
    }

    pub fn start_prompt(
        &self,
        lease: &SessionLease,
        session: Uuid,
        request: Uuid,
        input: &Value,
    ) -> Result<Uuid> {
        anyhow::ensure!(
            lease.matches(self, session)?,
            "session lease does not match"
        );
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE runtime_prompts SET status='interrupted', error='Session writer exited before settling the prompt'
            WHERE session_id=?1 AND status='running'", [session.to_string()])?;
        let id = Uuid::now_v7();
        tx.execute("INSERT INTO runtime_prompts(id,session_id,request_id,request,status) VALUES(?1,?2,?3,?4,'running')",
            params![id.to_string(),session.to_string(),request.to_string(),serde_json::to_string(input)?])?;
        tx.execute("INSERT INTO runtime_operations(id,session_id,kind,state,payload) VALUES(?1,?2,'prompt','running','{}')",params![id.to_string(),session.to_string()])?;
        tx.commit()?;
        Ok(id)
    }

    pub fn prompt_session(&self, id: Uuid) -> Result<Uuid> {
        let session = self
            .db()?
            .query_row(
                "SELECT session_id FROM runtime_prompts WHERE id=?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .context("prompt is unavailable")?;
        Ok(Uuid::parse_str(&session)?)
    }

    pub fn latest_prompt(&self, session: Uuid) -> Result<Option<PromptState>> {
        let id = self
            .db()?
            .query_row(
                "SELECT id FROM runtime_prompts WHERE session_id=?1 ORDER BY id DESC LIMIT 1",
                [session.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        id.map(|id| self.prompt_state(Uuid::parse_str(&id)?))
            .transpose()
    }

    pub fn prompt_state(&self, id: Uuid) -> Result<PromptState> {
        let row: (String, String, Option<String>, Option<String>) = self
            .db()?
            .query_row(
                "SELECT session_id,status,output,error FROM runtime_prompts WHERE id=?1",
                [id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .context("prompt is unavailable")?;
        let session_id = Uuid::parse_str(&row.0)?;
        let mut state = PromptState {
            prompt_id: id,
            session_id,
            status: row.1,
            output: row.2,
            error: row.3,
        };
        if state.status == "running"
            && let Ok(_lease) = self.session_lease(session_id)
        {
            let changed = self.db()?.execute("UPDATE runtime_prompts SET status='interrupted', error='Session writer exited before settling the prompt' WHERE id=?1 AND status='running'", [id.to_string()])?;
            if changed == 0 {
                return self.prompt_state(id);
            }
            state.status = "interrupted".into();
            state.error = Some("Session writer exited before settling the prompt".into());
        }
        Ok(state)
    }

    pub fn append_prompt_event(&self, id: Uuid, row: &Value) -> Result<()> {
        self.db()?.execute("INSERT INTO runtime_prompt_events(prompt_id,payload) SELECT id,?2 FROM runtime_prompts WHERE id=?1 AND status='running'",
            params![id.to_string(),serde_json::to_string(row)?])?;
        Ok(())
    }

    pub fn prompt_provider_error(&self, id: Uuid) -> Result<Option<Value>> {
        let payload = self.db()?.query_row(
            "SELECT payload FROM runtime_prompt_events WHERE prompt_id=?1 AND json_extract(payload,'$.type')='provider_error' ORDER BY seq DESC LIMIT 1",
            [id.to_string()], |row|row.get::<_, String>(0)).optional()?;
        payload
            .map(|payload| {
                serde_json::from_str::<Value>(&payload)
                    .map(|value| value["provider_error"].clone())
                    .map_err(Into::into)
            })
            .transpose()
    }

    pub fn prompt_events(&self, id: Uuid, after: i64, limit: u32) -> Result<Vec<(i64, Value)>> {
        let conn = self.db()?;
        let mut query = conn.prepare("SELECT seq,payload FROM runtime_prompt_events WHERE prompt_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let rows = query.query_map(params![id.to_string(), after, limit.clamp(1, 128)], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (seq, payload) = row?;
            Ok((seq, serde_json::from_str(&payload)?))
        })
        .collect()
    }

    pub fn prompt_events_by_offset(&self, id: Uuid, offset: i64, limit: u32) -> Result<Vec<Value>> {
        let conn = self.db()?;
        let mut query = conn.prepare("SELECT payload FROM runtime_prompt_events WHERE prompt_id=?1 ORDER BY seq LIMIT ?2 OFFSET ?3")?;
        let rows = query.query_map(
            params![id.to_string(), limit.clamp(1, 129), offset],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn finish_prompt(
        &self,
        id: Uuid,
        status: &str,
        output: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(status, "completed" | "failed" | "cancelled"),
            "invalid terminal prompt status"
        );
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute("UPDATE runtime_prompts SET status=?2,output=?3,error=?4 WHERE id=?1 AND status='running'", params![id.to_string(),status,output,error])?;
        anyhow::ensure!(changed == 1, "prompt already settled");
        tx.execute(
            "UPDATE runtime_operations SET state=?2 WHERE id=?1 AND state='running'",
            params![id.to_string(), status],
        )?;
        tx.execute(
            "INSERT INTO runtime_prompt_events(prompt_id,payload) VALUES(?1,?2)",
            params![
                id.to_string(),
                serde_json::json!({"type":"done","status":status}).to_string()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod delivery_one {
    use super::*;
    use serde_json::json;

    #[test]
    fn cursor_replay_and_dedup_survive_writer_loss() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let store = Store::new(workspace.path())?;
        let session = Uuid::now_v7();
        let request = Uuid::now_v7();
        let input = json!({"prompt":"test"});
        let lease = store.session_lease(session)?;
        let id = store.start_prompt(&lease, session, request, &input)?;
        store.append_prompt_event(id, &json!({"text":"one"}))?;
        store.append_prompt_event(id, &json!({"text":"two"}))?;
        assert_eq!(store.prompt_state(id)?.status, "running");
        let first = store.prompt_events(id, 0, 1)?;
        assert_eq!(first[0].1["text"], "one");
        drop(lease);
        let reopened = Store::new(workspace.path())?;
        assert_eq!(
            reopened.existing_prompt(session, request, &input)?,
            Some(id)
        );
        assert!(
            reopened
                .existing_prompt(session, request, &json!({"prompt":"changed"}))
                .is_err()
        );
        assert_eq!(reopened.prompt_state(id)?.status, "interrupted");
        let second = reopened.prompt_events(id, first[0].0, 1)?;
        assert_eq!(second[0].1["text"], "two");
        assert!(reopened.prompt_events(id, second[0].0, 1)?.is_empty());
        Ok(())
    }
}
