//! Session handoff between clients: export, import, one-time links and remote resume.

use super::*;

impl IdeRpcHandler {
    pub(crate) fn handle_session_handoff_export(&self, params: Value) -> Result<Value> {
        let session_id = if let Some(session_id) = optional_uuid(&params, "session_id")? {
            session_id
        } else {
            self.store
                .load_latest_session()?
                .map(|s| s.session_id)
                .ok_or_else(|| anyhow!("no sessions available to export"))?
        };
        let session = self
            .store
            .load_session(session_id)?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let projection = self.store.rebuild_from_events(session_id)?;
        let bundle_id = Uuid::now_v7();
        let output_path = params
            .get("output_path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                self.store
                    .root
                    .join("teleport")
                    .join(format!("{bundle_id}.json"))
            });
        if let Some(parent) = output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let payload = serde_json::json!({
            "schema": "deepseek.handoff.v1",
            "bundle_id": bundle_id.to_string(),
            "exported_at": chrono::Utc::now().to_rfc3339(),
            "session": {
                "session_id": session_id.to_string(),
                "workspace_root": session.workspace_root,
                "status": format!("{:?}", session.status),
            },
            "chat_messages": projection.chat_messages,
            "transcript": projection.transcript,
            "step_status": projection.step_status,
        });
        fs::write(&output_path, serde_json::to_vec_pretty(&payload)?)?;
        self.store.append_event(&codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: chrono::Utc::now(),
            session_id,
            kind: EventKind::TeleportBundleCreated {
                bundle_id,
                path: output_path.to_string_lossy().to_string(),
            },
        })?;

        Ok(serde_json::json!({
            "bundle_id": bundle_id.to_string(),
            "session_id": session_id.to_string(),
            "path": output_path.to_string_lossy().to_string(),
            "turn_count": payload["chat_messages"].as_array().map_or(0, std::vec::Vec::len),
        }))
    }

    pub(crate) fn handle_session_handoff_import(&self, params: Value) -> Result<Value> {
        let bundle_path = params
            .get("bundle_path")
            .or_else(|| params.get("handoff_path"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'bundle_path' parameter"))?;
        let raw = fs::read_to_string(bundle_path)?;
        let payload: Value = serde_json::from_str(&raw)?;
        if payload
            .get("schema")
            .and_then(|v| v.as_str())
            .map(|value| value != "deepseek.handoff.v1")
            .unwrap_or(false)
        {
            return Err(anyhow!("unsupported handoff schema"));
        }

        let workspace_root = params
            .get("workspace_root")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .or_else(|| {
                payload
                    .get("session")
                    .and_then(|s| s.get("workspace_root"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| ".".to_string());
        let session_id = Uuid::now_v7();
        let session = Session {
            session_id,
            workspace_root,
            baseline_commit: None,
            status: SessionState::Idle,
            budgets: SessionBudgets {
                per_turn_seconds: 300,
                max_think_tokens: 8192,
            },
            active_plan_id: None,
        };
        self.store.save_session(&session)?;

        let chat_messages = payload
            .get("chat_messages")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<ChatMessage>>(v).ok())
            .unwrap_or_default();
        for message in &chat_messages {
            self.store.append_event(&codingbuddy_store::NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: chrono::Utc::now(),
                session_id,
                kind: EventKind::ChatTurn {
                    message: message.clone(),
                },
            })?;
        }
        let resume_import = params
            .get("resume")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if resume_import {
            self.store.append_event(&codingbuddy_store::NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: chrono::Utc::now(),
                session_id,
                kind: EventKind::SessionResumed {
                    session_id,
                    events_replayed: chat_messages.len() as u64,
                },
            })?;
        }

        Ok(serde_json::json!({
            "session_id": session_id.to_string(),
            "imported": true,
            "bundle_path": bundle_path,
            "turn_count": chat_messages.len(),
            "status": "idle",
            "resumed": resume_import,
        }))
    }

    pub(crate) fn handle_session_handoff_link_create(&self, params: Value) -> Result<Value> {
        let exported = self.handle_session_handoff_export(params.clone())?;
        let bundle_path = exported
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("handoff export did not return bundle path"))?;
        let session_id = exported
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("handoff export did not return session id"))?;
        let session_id = Uuid::parse_str(session_id)?;

        let handoff_id = Uuid::now_v7();
        let token = format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple());
        let token_hash = sha256_hex(&token);
        let created_at = chrono::Utc::now();
        let ttl_minutes = params
            .get("ttl_minutes")
            .and_then(|v| v.as_u64())
            .unwrap_or(30)
            .clamp(1, 24 * 60);
        let expires_at = (created_at + chrono::Duration::minutes(ttl_minutes as i64)).to_rfc3339();

        let descriptor = HandoffDescriptor {
            schema: "deepseek.handoff_link.v1".to_string(),
            handoff_id,
            bundle_path: bundle_path.to_string(),
            token_hash,
            created_at: created_at.to_rfc3339(),
            expires_at: expires_at.clone(),
            used_at: None,
            session_id,
        };
        write_handoff_descriptor(self.store.root.as_path(), &descriptor)?;

        self.store.append_event(&codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: chrono::Utc::now(),
            session_id,
            kind: EventKind::TeleportHandoffLinkCreated {
                handoff_id,
                session_id,
                expires_at: expires_at.clone(),
            },
        })?;

        let base_url = params
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("https://app.deepseek.com/handoff");
        let sep = if base_url.contains('?') { '&' } else { '?' };
        let link_url = format!("{base_url}{sep}handoff_id={handoff_id}&token={token}");

        Ok(serde_json::json!({
            "schema": "deepseek.handoff_link.v1",
            "handoff_id": handoff_id.to_string(),
            "bundle_path": bundle_path,
            "session_id": session_id.to_string(),
            "expires_at": expires_at,
            "token": token,
            "url": link_url,
        }))
    }

    pub(crate) fn handle_session_handoff_link_consume(&self, params: Value) -> Result<Value> {
        let handoff_id = require_uuid(&params, "handoff_id")?;
        let token = params
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'token' parameter"))?;
        let mut descriptor = read_handoff_descriptor(self.store.root.as_path(), handoff_id)?;
        let now = chrono::Utc::now();

        let invalid_reason = if descriptor.used_at.is_some() {
            Some("already_used")
        } else if sha256_hex(token) != descriptor.token_hash {
            Some("token_mismatch")
        } else {
            let expires = chrono::DateTime::parse_from_rfc3339(&descriptor.expires_at)
                .map(|value| value.with_timezone(&chrono::Utc))
                .map_err(|_| anyhow!("invalid descriptor expiry timestamp"))?;
            if now > expires { Some("expired") } else { None }
        };

        if let Some(reason) = invalid_reason {
            self.store.append_event(&codingbuddy_store::NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: chrono::Utc::now(),
                session_id: descriptor.session_id,
                kind: EventKind::TeleportHandoffLinkConsumed {
                    handoff_id,
                    session_id: descriptor.session_id,
                    success: false,
                    reason: reason.to_string(),
                },
            })?;
            return Err(anyhow!("handoff token rejected: {reason}"));
        }

        let imported = self.handle_session_handoff_import(serde_json::json!({
            "bundle_path": descriptor.bundle_path,
            "resume": true,
        }))?;
        descriptor.used_at = Some(now.to_rfc3339());
        write_handoff_descriptor(self.store.root.as_path(), &descriptor)?;

        self.store.append_event(&codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: chrono::Utc::now(),
            session_id: descriptor.session_id,
            kind: EventKind::TeleportHandoffLinkConsumed {
                handoff_id,
                session_id: descriptor.session_id,
                success: true,
                reason: "consumed".to_string(),
            },
        })?;

        Ok(serde_json::json!({
            "schema": "deepseek.handoff_link.v1",
            "handoff_id": handoff_id.to_string(),
            "consumed": true,
            "imported_session_id": imported["session_id"],
            "turn_count": imported["turn_count"],
        }))
    }

    pub(crate) fn handle_session_remote_resume(&self, params: Value) -> Result<Value> {
        if params.get("bundle_path").is_some() || params.get("handoff_path").is_some() {
            let imported = self.handle_session_handoff_import(params)?;
            return Ok(serde_json::json!({
                "remote": true,
                "mode": "handoff_import",
                "session_id": imported["session_id"],
                "status": imported["status"],
                "turn_count": imported["turn_count"],
            }));
        }
        let resumed = self.handle_session_resume(params.clone())?;
        let session_id = resumed
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing resumed session id"))?;
        let parsed = Uuid::parse_str(session_id)?;
        let events_replayed = resumed
            .get("turn_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        self.store.append_event(&codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: chrono::Utc::now(),
            session_id: parsed,
            kind: EventKind::SessionResumed {
                session_id: parsed,
                events_replayed,
            },
        })?;

        Ok(serde_json::json!({
            "remote": true,
            "mode": "session_resume",
            "session_id": session_id,
            "status": resumed["status"],
            "turn_count": resumed["turn_count"],
            "workspace_root": resumed["workspace_root"],
        }))
    }
}
