use super::*;

#[derive(Debug, Serialize, Deserialize)]
pub struct PendingInvocation {
    pub id: Uuid,
    pub name: String,
    pub provider_call_id: String,
}

#[derive(Serialize, Deserialize)]
pub enum InvocationReceipt {
    File {
        path: PathBuf,
        postimage_sha256: String,
    },
    Patch {
        patch_id: Uuid,
    },
}

impl Store {
    pub(crate) fn init_steps(conn: &Connection) -> Result<()> {
        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS runtime_secrets (
                name TEXT PRIMARY KEY, secret BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_session_routes (
                session_id TEXT PRIMARY KEY, route TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_prompt_steps (
                step_id TEXT PRIMARY KEY, prompt_id TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_native_steps (
                session_id TEXT NOT NULL, step_id TEXT NOT NULL, payload TEXT NOT NULL,
                PRIMARY KEY(session_id,step_id)
            );
            CREATE TABLE IF NOT EXISTS runtime_invocations (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL, step_id TEXT NOT NULL,
                position INTEGER NOT NULL, provider_call_id TEXT NOT NULL, name TEXT NOT NULL,
                state TEXT NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS runtime_invocation_position
                ON runtime_invocations(step_id,position) WHERE state <> 'superseded';
            CREATE TABLE IF NOT EXISTS runtime_invocation_requests (
                invocation_id TEXT PRIMARY KEY, request TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_invocation_retries (
                invocation_id TEXT PRIMARY KEY, source TEXT NOT NULL, root TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_invocation_receipts (
                invocation_id TEXT PRIMARY KEY, payload TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_patch_settlements (
                session_id TEXT NOT NULL, patch_id TEXT NOT NULL,
                PRIMARY KEY(session_id,patch_id)
            );
            CREATE INDEX IF NOT EXISTS runtime_invocation_session ON runtime_invocations(session_id,state);
        ")?;
        Ok(())
    }

    pub fn route_key(&self) -> Result<Vec<u8>> {
        let conn = self.db()?;
        conn.execute("INSERT OR IGNORE INTO runtime_secrets(name,secret) VALUES('session_route',randomblob(32))", [])?;
        Ok(conn.query_row(
            "SELECT secret FROM runtime_secrets WHERE name='session_route'",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn pinned_route(&self, session: Uuid) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT route FROM runtime_session_routes WHERE session_id=?1",
                [session.to_string()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn pin_route(&self, session: Uuid, route: &str) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR IGNORE INTO runtime_session_routes(session_id,route) VALUES(?1,?2)",
            params![session.to_string(), route],
        )?;
        let pinned: String = conn.query_row(
            "SELECT route FROM runtime_session_routes WHERE session_id=?1",
            [session.to_string()],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            pinned == route,
            "Conversation belongs to a different provider, protocol, endpoint, model, or account. Use session portable-fork to preview and transfer history."
        );
        Ok(())
    }

    pub fn provider_history(&self, session: Uuid, route: &str) -> Result<RebuildProjection> {
        self.pin_route(session, route)?;
        let mut projection = self.rebuild_from_events(session)?;
        let conn = self.db()?;
        let mut native = conn.prepare(
            "SELECT payload FROM runtime_native_steps WHERE session_id=?1 AND step_id=?2",
        )?;
        for (step, position) in &projection.model_steps {
            if let Some(payload) = native
                .query_row(params![session.to_string(), step.to_string()], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?
                && let Some(ChatMessage::Assistant { native_parts, .. }) =
                    projection.chat_messages.get_mut(*position)
            {
                *native_parts = Some(serde_json::from_str(&payload)?);
            }
        }
        Ok(projection)
    }

    pub fn model_step(
        &self,
        session: Uuid,
        prompt: Option<Uuid>,
        text: Option<String>,
        calls: &[codingbuddy_core::LlmToolCall],
        ordered: &[codingbuddy_core::TransportPart],
    ) -> Result<Vec<Uuid>> {
        use codingbuddy_core::{ModelPart, TransportPart};
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let step = Uuid::now_v7();
        if let Some(prompt) = prompt {
            let changed = tx.execute("INSERT INTO runtime_prompt_steps(step_id,prompt_id)
                SELECT ?1,id FROM runtime_prompts WHERE id=?2 AND session_id=?3 AND status='running'",
                params![step.to_string(),prompt.to_string(),session.to_string()])?;
            anyhow::ensure!(changed == 1, "model step belongs to an inactive prompt");
        }
        let ids: Vec<_> = calls.iter().map(|_| Uuid::now_v7()).collect();
        let mut call_parts = std::collections::HashMap::new();
        for (position, (call, id)) in calls.iter().zip(&ids).enumerate() {
            let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
                .context("invalid completed tool arguments")?;
            anyhow::ensure!(arguments.is_object(), "tool arguments must be an object");
            let previous = call_parts.insert(
                call.id.as_str(),
                ModelPart::Call {
                    id: *id,
                    provider_call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments,
                },
            );
            anyhow::ensure!(previous.is_none(), "duplicate completed tool-call identity");
            tx.execute("INSERT INTO runtime_invocations(id,session_id,step_id,position,provider_call_id,name,state) VALUES(?1,?2,?3,?4,?5,?6,'prepared')",
                params![id.to_string(),session.to_string(),step.to_string(),position as i64,call.id,call.name])?;
        }
        let mut parts = Vec::new();
        if ordered.is_empty() {
            if let Some(text) = text {
                parts.push(ModelPart::Text { text });
            }
            parts.extend(
                calls
                    .iter()
                    .filter_map(|call| call_parts.remove(call.id.as_str())),
            );
        } else {
            for part in ordered {
                match part {
                    TransportPart::Text(text) => parts.push(ModelPart::Text { text: text.clone() }),
                    TransportPart::Call(id) => parts.push(
                        call_parts
                            .remove(id.as_str())
                            .context("model step refers to an unknown tool call")?,
                    ),
                    TransportPart::Native(payload) => {
                        tx.execute("INSERT INTO runtime_native_steps(session_id,step_id,payload) VALUES(?1,?2,?3)",
                            params![session.to_string(), step.to_string(), serde_json::to_string(payload)?])?;
                    }
                }
            }
            parts.extend(
                calls
                    .iter()
                    .filter_map(|call| call_parts.remove(call.id.as_str())),
            );
        }
        Self::append_event_row(
            &tx,
            &NewEvent::new(
                session,
                EventKind::ModelStep {
                    step_id: step,
                    parts,
                },
            ),
        )?;
        tx.commit()?;
        Ok(ids)
    }

    pub fn record_invocation_receipt(
        &self,
        session: Uuid,
        id: Uuid,
        receipt: &InvocationReceipt,
    ) -> Result<()> {
        let changed = self.db()?.execute(
            "INSERT INTO runtime_invocation_receipts(invocation_id,payload)
             SELECT id,?3 FROM runtime_invocations WHERE session_id=?1 AND id=?2 AND state='running'",
            params![session.to_string(), id.to_string(), serde_json::to_string(receipt)?],
        )?;
        anyhow::ensure!(changed == 1, "invocation not running");
        Ok(())
    }

    pub fn invocation_receipt(&self, id: Uuid) -> Result<Option<InvocationReceipt>> {
        self.db()?
            .query_row(
                "SELECT payload FROM runtime_invocation_receipts WHERE invocation_id=?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    pub fn record_patch_application(
        &self,
        session: Uuid,
        patch: Uuid,
        applied: bool,
        conflicts: Vec<String>,
    ) -> Result<bool> {
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recorded = !applied || tx.execute(
            "INSERT OR IGNORE INTO runtime_patch_settlements(session_id,patch_id) VALUES(?1,?2)",
            params![session.to_string(),patch.to_string()],
        )? == 1;
        if recorded {
            Self::append_event_row(
                &tx,
                &NewEvent::new(
                    session,
                    EventKind::PatchApplied {
                        patch_id: patch,
                        applied,
                        conflicts,
                    },
                ),
            )?;
        }
        tx.commit()?;
        Ok(recorded)
    }

    pub fn dispatch_invocation(&self, session: Uuid, id: Uuid) -> Result<()> {
        self.dispatch_tool_invocation(session, id, None)
    }

    pub fn dispatch_tool_invocation(
        &self,
        session: Uuid,
        id: Uuid,
        call: Option<&codingbuddy_core::ToolCall>,
    ) -> Result<()> {
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute("UPDATE runtime_invocations SET state='running' WHERE id=?1 AND session_id=?2 AND state='prepared'",
            params![id.to_string(),session.to_string()])?;
        anyhow::ensure!(updated == 1, "invocation was already dispatched");
        if let Some(call) = call {
            tx.execute(
                "INSERT INTO runtime_invocation_requests(invocation_id,request) VALUES(?1,?2)",
                params![id.to_string(), serde_json::to_string(call)?],
            )?;
        }
        append_invocation_progress(&tx, id, "tool_dispatch")?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_invocation_request(
        &self,
        session: Uuid,
        id: Uuid,
        call: &codingbuddy_core::ToolCall,
    ) -> Result<()> {
        let written = self.db()?.execute("INSERT INTO runtime_invocation_requests(invocation_id,request)
            SELECT id,?3 FROM runtime_invocations WHERE id=?1 AND session_id=?2 AND state='running'",
            params![id.to_string(),session.to_string(),serde_json::to_string(call)?])?;
        anyhow::ensure!(written == 1, "invocation is not running");
        Ok(())
    }

    pub fn interrupted_tool_request(
        &self,
        session: Uuid,
        id: Uuid,
    ) -> Result<codingbuddy_core::ToolCall> {
        let request = self.db()?.query_row(
            "SELECT request FROM runtime_invocation_requests r JOIN runtime_invocations i ON i.id=r.invocation_id
             WHERE i.id=?1 AND i.session_id=?2 AND i.state='running'",
            params![id.to_string(),session.to_string()], |row| row.get::<_, String>(0)
        ).optional()?.context("No retryable interrupted tool request is available")?;
        Ok(serde_json::from_str(&request)?)
    }

    pub fn retry_invocation(
        &self,
        lease: &SessionLease,
        session: Uuid,
        previous: Uuid,
        call: &codingbuddy_core::ToolCall,
    ) -> Result<PendingInvocation> {
        anyhow::ensure!(
            lease.matches(self, session)?,
            "session lease does not match"
        );
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = Uuid::now_v7();
        let row: (String, i64, String, String) = tx.query_row(
            "SELECT step_id,position,provider_call_id,name FROM runtime_invocations WHERE session_id=?1 AND id=?2 AND state='running'",
            params![session.to_string(), previous.to_string()],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).optional()?.context("invocation is no longer interrupted")?;
        tx.execute(
            "UPDATE runtime_invocations SET state='superseded' WHERE id=?1",
            [previous.to_string()],
        )?;
        tx.execute("INSERT INTO runtime_invocations(id,session_id,step_id,position,provider_call_id,name,state) VALUES(?1,?2,?3,?4,?5,?6,'running')",
            params![id.to_string(),session.to_string(),row.0,row.1,row.2,row.3])?;
        tx.execute(
            "INSERT INTO runtime_invocation_requests(invocation_id,request) VALUES(?1,?2)",
            params![id.to_string(), serde_json::to_string(call)?],
        )?;
        tx.execute("INSERT INTO runtime_invocation_retries(invocation_id,source,root)
            VALUES(?1,?2,COALESCE((SELECT root FROM runtime_invocation_retries WHERE invocation_id=?2),?2))",
            params![id.to_string(),previous.to_string()])?;
        tx.commit()?;
        Ok(PendingInvocation {
            id,
            name: row.3,
            provider_call_id: row.2,
        })
    }

    pub fn settle_invocation(
        &self,
        session: Uuid,
        id: Uuid,
        message: ChatMessage,
        expected: &str,
    ) -> Result<()> {
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute("UPDATE runtime_invocations SET state='settled' WHERE id=?1 AND session_id=?2 AND state=?3",
            params![id.to_string(),session.to_string(),expected])?;
        anyhow::ensure!(updated == 1, "invocation changed before settlement");
        let root: Option<String> = tx
            .query_row(
                "SELECT root FROM runtime_invocation_retries WHERE invocation_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let transcript_id = root
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()?
            .unwrap_or(id);
        append_invocation_progress(&tx, id, "tool_settlement")?;
        Self::append_event_row(
            &tx,
            &NewEvent::new(
                session,
                EventKind::ModelToolResult {
                    invocation_id: transcript_id,
                    message,
                },
            ),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn interrupted_invocations(
        &self,
        lease: &SessionLease,
        session: Uuid,
    ) -> Result<Vec<PendingInvocation>> {
        anyhow::ensure!(
            lease.matches(self, session)?,
            "session lease does not match"
        );
        let conn = self.db()?;
        let mut query = conn.prepare("SELECT id,name,provider_call_id,state FROM runtime_invocations WHERE session_id=?1 AND state IN ('prepared','running') ORDER BY step_id,position")?;
        let pending = query
            .query_map([session.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut ambiguous = Vec::new();
        for (id, name, provider_call_id, state) in pending {
            let id = Uuid::parse_str(&id)?;
            if state == "running" {
                ambiguous.push(PendingInvocation {
                    id,
                    name,
                    provider_call_id,
                });
            } else if ambiguous.is_empty() {
                self.settle_invocation(
                    session,
                    id,
                    ChatMessage::Tool {
                        tool_call_id: provider_call_id,
                        tool_name: Some(name),
                        content: "Tool never dispatched because the previous turn was interrupted."
                            .into(),
                    },
                    "prepared",
                )?;
            }
        }
        Ok(ambiguous)
    }

    pub(crate) fn delete_session_runtime(tx: &Connection, session: &str) -> Result<()> {
        for statement in [
            "DELETE FROM runtime_invocation_receipts WHERE invocation_id IN (SELECT id FROM runtime_invocations WHERE session_id=?1)",
            "DELETE FROM runtime_invocation_requests WHERE invocation_id IN (SELECT id FROM runtime_invocations WHERE session_id=?1)",
            "DELETE FROM runtime_invocation_retries WHERE invocation_id IN (SELECT id FROM runtime_invocations WHERE session_id=?1)",
            "DELETE FROM runtime_invocations WHERE session_id=?1",
            "DELETE FROM runtime_patch_settlements WHERE session_id=?1",
            "DELETE FROM runtime_native_steps WHERE session_id=?1",
            "DELETE FROM runtime_session_routes WHERE session_id=?1",
            "DELETE FROM runtime_prompt_steps WHERE prompt_id IN (SELECT id FROM runtime_prompts WHERE session_id=?1)",
            "DELETE FROM runtime_prompt_events WHERE prompt_id IN (SELECT id FROM runtime_prompts WHERE session_id=?1)",
            "DELETE FROM runtime_prompts WHERE session_id=?1",
        ] {
            tx.execute(statement, [session])?;
        }
        Ok(())
    }

    pub fn resolve_invocation(&self, session: Uuid, id: Uuid) -> Result<()> {
        let lease = self.session_lease(session)?;
        let row = self
            .interrupted_invocations(&lease, session)?
            .into_iter()
            .find(|row| row.id == id)
            .context("unknown interrupted invocation")?;
        self.settle_invocation(session,id,ChatMessage::Tool { tool_call_id:row.provider_call_id,tool_name:Some(row.name),
            content:"Operator acknowledged that the interrupted invocation may have had side effects. Its result is unknown.".into() },"running")
    }
}

fn append_invocation_progress(tx: &Connection, id: Uuid, event: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO runtime_prompt_events(prompt_id,payload)
        SELECT p.id,json_object('type',?2,'invocation_id',i.id,'tool_name',i.name)
        FROM runtime_invocations i JOIN runtime_prompt_steps s ON s.step_id=i.step_id
        JOIN runtime_prompts p ON p.id=s.prompt_id
        WHERE i.id=?1 AND p.status='running'",
        params![id.to_string(), event],
    )?;
    Ok(())
}

#[cfg(test)]
mod delivery_one {
    use super::*;
    use codingbuddy_core::{LlmToolCall, ModelPart, NativeParts, TransportPart};
    use serde_json::json;

    #[test]
    fn delivery_one_explicit_retry_links_new_invocation_to_the_original_call() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::new(directory.path())?;
        let session = Uuid::now_v7();
        let lease = store.session_lease(session)?;
        let call = codingbuddy_core::ToolCall {
            name: "fs.write".into(),
            args: json!({"path":"marker"}),
            requires_approval: true,
        };
        let model = LlmToolCall {
            id: "wire-call".into(),
            name: "fs_write".into(),
            arguments: call.args.to_string(),
        };
        let id = store.model_step(session, None, None, &[model], &[])?[0];
        store.dispatch_tool_invocation(session, id, Some(&call))?;
        let retry = store.retry_invocation(&lease, session, id, &call)?;
        assert_ne!(id, retry.id);
        assert_eq!(retry.provider_call_id, "wire-call");
        assert_eq!(
            store.interrupted_invocations(&lease, session)?[0].id,
            retry.id
        );
        store.settle_invocation(
            session,
            retry.id,
            ChatMessage::Tool {
                tool_call_id: retry.provider_call_id,
                content: "verified".into(),
                tool_name: Some(retry.name),
            },
            "running",
        )?;
        let events = store.session_events(session)?;
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[1].kind, EventKind::ModelToolResult { invocation_id, .. } if *invocation_id == id)
        );
        Ok(())
    }

    #[test]
    fn delivery_one_legacy_tool_events_preserve_pairing_and_redact_private_data() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::new(directory.path())?;
        let session = Uuid::now_v7();
        for message in [
            ChatMessage::Assistant {
                content: Some("visible".into()),
                native_parts: None,
                reasoning_content: Some("secret-reasoning".into()),
                tool_calls: vec![LlmToolCall {
                    id: "gemini:1:secret-signature".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }],
            },
            ChatMessage::Tool {
                tool_call_id: "gemini:1:secret-signature".into(),
                content: "result".into(),
                tool_name: Some("read".into()),
            },
        ] {
            store.append_event(&NewEvent::new(session, EventKind::ChatTurn { message }))?;
        }
        let rebuilt = store.rebuild_from_events(session)?;
        assert!(rebuilt.legacy_tool_history);
        assert!(
            matches!(&rebuilt.chat_messages[0], ChatMessage::Assistant { tool_calls, .. } if tool_calls[0].id == "1")
        );
        assert!(
            matches!(&rebuilt.chat_messages[1], ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "1")
        );
        let public = serde_json::to_string(&store.session_events(session)?)?;
        assert!(!public.contains("secret-"));
        Ok(())
    }

    #[test]
    fn replay_preserves_order_and_scopes_private_provider_parts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::new(directory.path())?;
        let session = Uuid::now_v7();
        let lease = store.session_lease(session)?;
        assert!(
            Store::new(directory.path())?
                .session_lease(session)
                .is_err()
        );
        store.pin_route(session, "route-a")?;
        let prompt =
            store.start_prompt(&lease, session, Uuid::now_v7(), &json!({"prompt":"hi"}))?;
        let calls = [
            LlmToolCall {
                id: "first".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
            LlmToolCall {
                id: "second".into(),
                name: "write".into(),
                arguments: "{\"x\":1}".into(),
            },
        ];
        let invocations = store.model_step(
            session,
            Some(prompt),
            None,
            &calls,
            &[
                TransportPart::Text("before".into()),
                TransportPart::Call("first".into()),
                TransportPart::Text("between".into()),
                TransportPart::Call("second".into()),
                TransportPart::Native(NativeParts {
                    protocol: "gemini".into(),
                    blocks: vec![json!({"signature":"private-fixture"})],
                }),
            ],
        )?;
        store.dispatch_invocation(session, invocations[0])?;
        store.settle_invocation(
            session,
            invocations[0],
            ChatMessage::Tool {
                tool_call_id: "first".into(),
                tool_name: Some("read".into()),
                content: "result".into(),
            },
            "running",
        )?;
        store.dispatch_invocation(session, invocations[1])?;
        drop(lease);

        let reopened = Store::new(directory.path())?;
        let lease = reopened.session_lease(session)?;
        assert_eq!(
            reopened.interrupted_invocations(&lease, session)?[0].id,
            invocations[1]
        );
        let events = reopened.session_events(session)?;
        assert!(
            matches!(&events[0].kind, EventKind::ModelStep { parts, .. } if matches!(
                parts.as_slice(),
                [ModelPart::Text {..}, ModelPart::Call {..}, ModelPart::Text {..}, ModelPart::Call {..}]
            ))
        );
        assert!(
            matches!(&events[1].kind, EventKind::ModelToolResult { invocation_id, .. } if *invocation_id == invocations[0])
        );
        assert!(reopened.provider_history(session, "route-b").is_err());
        let private = reopened.provider_history(session, "route-a")?;
        assert!(matches!(
            &private.chat_messages[0],
            ChatMessage::Assistant {
                native_parts: Some(_),
                ..
            }
        ));
        assert!(!serde_json::to_string(&private.chat_messages)?.contains("private-fixture"));
        assert!(matches!(
            &reopened.rebuild_from_events(session)?.chat_messages[0],
            ChatMessage::Assistant {
                native_parts: None,
                ..
            }
        ));
        Ok(())
    }
}
