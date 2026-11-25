use anyhow::{Context, Result, anyhow};
use codingbuddy_agent::{AgentEngine, ChatMode, ChatOptions};
use codingbuddy_core::{
    AppConfig, ChatMessage, EventKind, Session, SessionBudgets, SessionState, StreamChunk,
    stream_chunk_to_event_json,
};
use codingbuddy_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// Enhanced context management
use codingbuddy_agent::context::ContextManager;

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// Standard JSON-RPC error codes.
pub const ERR_PARSE: i64 = -32700;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL: i64 = -32603;

// Application-level error codes.
pub const ERR_SESSION_NOT_FOUND: i64 = -32000;
pub const ERR_TOOL_DENIED: i64 = -32001;
pub const ERR_PATCH_CONFLICT: i64 = -32002;

impl JsonRpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Trait for handling JSON-RPC method dispatching.
pub trait RpcHandler {
    fn handle(&self, method: &str, params: Value) -> Result<Value>;
}

/// Run a JSON-RPC 2.0 server over stdio (newline-delimited JSON).
pub fn run_stdio_server(handler: &dyn RpcHandler) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = stdin.lock();
    let mut writer = stdout.lock();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(req) => {
                if req.method == "shutdown" {
                    let resp = JsonRpcResponse::success(req.id, serde_json::json!({"ok": true}));
                    let out = serde_json::to_string(&resp)?;
                    writeln!(writer, "{out}")?;
                    writer.flush()?;
                    return Ok(());
                }
                match handler.handle(&req.method, req.params) {
                    Ok(result) => JsonRpcResponse::success(req.id, result),
                    Err(e) => JsonRpcResponse::error(req.id, ERR_INTERNAL, e.to_string()),
                }
            }
            Err(_) => JsonRpcResponse::error(Value::Null, ERR_PARSE, "Parse error"),
        };

        let out = serde_json::to_string(&response)?;
        writeln!(writer, "{out}")?;
        writer.flush()?;
    }

    Ok(())
}

/// A default handler that supports basic methods.
pub struct DefaultRpcHandler;

impl RpcHandler for DefaultRpcHandler {
    fn handle(&self, method: &str, _params: Value) -> Result<Value> {
        match method {
            "initialize" => Ok(serde_json::json!({
                "name": "codingbuddy-cli",
                "version": env!("CARGO_PKG_VERSION"),
                "capabilities": ["chat", "tool/execute", "status"]
            })),
            "status" => Ok(serde_json::json!({"status": "ready"})),
            "cancel" => Ok(serde_json::json!({"cancelled": true})),
            _ => Err(anyhow!("method not found: {method}")),
        }
    }
}

// ---------------------------------------------------------------------------
// IDE-oriented RPC handler with full session, prompt, tool, and patch methods
// ---------------------------------------------------------------------------

/// Tracks pending tool approvals for a session.
#[derive(Debug, Default)]
struct PendingToolApproval {
    /// Maps invocation_id → tool_name for tools awaiting approval.
    pending: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandoffDescriptor {
    schema: String,
    handoff_id: Uuid,
    bundle_path: String,
    token_hash: String,
    created_at: String,
    expires_at: String,
    used_at: Option<String>,
    session_id: Uuid,
}

/// Full-featured IDE handler backed by a `Store`.
pub struct IdeRpcHandler {
    store: Arc<Store>,
    http_workspace: Option<PathBuf>,
    http_sessions: Mutex<HashSet<Uuid>>,
    http_prompts: Mutex<HashSet<Uuid>>,
    exposed_session: Option<Uuid>,
    running: Arc<Mutex<HashMap<Uuid, codingbuddy_core::CancellationToken>>>,
    /// Per-session pending tool approvals.
    approvals: Mutex<HashMap<Uuid, PendingToolApproval>>,
    /// Whether partial-message streaming should be enabled by default.
    include_partial_messages: bool,
    /// Context manager for intelligent file suggestions.
    context_manager: Mutex<Option<ContextManager>>,
}

impl IdeRpcHandler {
    pub fn new(workspace: &Path) -> Result<Self> {
        let store = Store::new(workspace)?;

        // Initialize context manager (may fail if workspace is empty)
        let context_manager = match ContextManager::new(workspace) {
            Ok(mut manager) => {
                // Try to analyze workspace, but don't fail if it doesn't work
                let _ = manager.analyze_workspace();
                Some(manager)
            }
            Err(_) => None,
        };

        Ok(Self {
            store: Arc::new(store),
            http_workspace: Some(workspace.canonicalize()?),
            http_sessions: Mutex::new(HashSet::new()),
            http_prompts: Mutex::new(HashSet::new()),
            exposed_session: None,
            running: Arc::new(Mutex::new(HashMap::new())),
            approvals: Mutex::new(HashMap::new()),
            include_partial_messages: env_flag_enabled("CODINGBUDDY_INCLUDE_PARTIAL_MESSAGES"),
            context_manager: Mutex::new(context_manager),
        })
    }

    pub fn from_store(store: Arc<Store>) -> Self {
        Self {
            store,
            http_workspace: None,
            http_sessions: Mutex::new(HashSet::new()),
            http_prompts: Mutex::new(HashSet::new()),
            exposed_session: None,
            running: Arc::new(Mutex::new(HashMap::new())),
            approvals: Mutex::new(HashMap::new()),
            include_partial_messages: env_flag_enabled("CODINGBUDDY_INCLUDE_PARTIAL_MESSAGES"),
            context_manager: Mutex::new(None),
        }
    }

    pub fn expose_session(&mut self, id: Uuid) -> Result<()> {
        let session = self
            .store
            .load_session(id)?
            .context("session is unavailable")?;
        anyhow::ensure!(
            Some(Path::new(&session.workspace_root).canonicalize()?) == self.http_workspace,
            "session belongs to another workspace"
        );
        self.exposed_session = Some(id);
        self.http_sessions
            .lock()
            .expect("HTTP sessions lock")
            .insert(id);
        Ok(())
    }

    fn run_durable_prompt(&self, params: Value, background: bool) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let request = if background {
            require_uuid(&params, "request_id")?
        } else {
            Uuid::now_v7()
        };
        anyhow::ensure!(
            params["prompt"]
                .as_str()
                .is_some_and(|prompt| !prompt.trim().is_empty()),
            "missing prompt"
        );
        anyhow::ensure!(
            self.store.load_session(session_id)?.is_some(),
            "session is unavailable"
        );
        if let Some(id) = self.store.existing_prompt(session_id, request, &params)? {
            return Ok(serde_json::to_value(self.store.prompt_state(id)?)?);
        }
        let lease = match self.store.session_lease(session_id) {
            Ok(lease) => Arc::new(lease),
            Err(error) => {
                if let Some(id) = self.store.existing_prompt(session_id, request, &params)? {
                    return Ok(serde_json::to_value(self.store.prompt_state(id)?)?);
                }
                return Err(error);
            }
        };
        if let Some(id) = self.store.existing_prompt(session_id, request, &params)? {
            drop(lease);
            return Ok(serde_json::to_value(self.store.prompt_state(id)?)?);
        }
        let id = self
            .store
            .start_prompt(&lease, session_id, request, &params)?;
        let token = codingbuddy_core::CancellationToken::new();
        self.running
            .lock()
            .expect("prompt cancellation lock")
            .insert(id, token.clone());
        let run = move |handler: &Self| {
            let result = handler.execute_prompt(params, id, Arc::clone(&lease), token.clone());
            let (status, output, error) = match &result {
                Ok(row) => (
                    row["status"].as_str().unwrap_or("failed"),
                    row["output"].as_str(),
                    row["error"].as_str(),
                ),
                Err(_) => ("failed", None, Some("Prompt execution failed")),
            };
            let status = if token.is_cancelled() && status != "completed" {
                "cancelled"
            } else {
                status
            };
            let finished = handler.store.finish_prompt(id, status, output, error);
            drop(lease);
            handler
                .running
                .lock()
                .expect("prompt cancellation lock")
                .remove(&id);
            finished?;
            result
        };
        if background {
            let mut worker = Self::from_store(Arc::clone(&self.store));
            worker.running = Arc::clone(&self.running);
            std::thread::spawn(move || {
                let _ = run(&worker);
            });
            return Ok(
                serde_json::json!({"prompt_id":id,"session_id":session_id,"status":"running"}),
            );
        }
        run(self)
    }

    fn handle_prompt_events(&self, params: Value) -> Result<Value> {
        let id = require_uuid(&params, "prompt_id")?;
        let cursor = params["cursor"].as_i64().unwrap_or(0).max(0);
        let limit = params["limit"].as_u64().unwrap_or(32).clamp(1, 127) as u32;
        let state = self.store.prompt_state(id)?;
        let writer_active = self
            .running
            .lock()
            .expect("prompt cancellation lock")
            .contains_key(&id);
        let mut rows = self.store.prompt_events(id, cursor, limit + 1)?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next = rows.last().map_or(cursor, |(seq, _)| *seq);
        let events: Vec<_> = rows
            .into_iter()
            .map(|(seq, event)| serde_json::json!({"seq":seq,"event":event}))
            .collect();
        Ok(
            serde_json::json!({"prompt_id":id,"status":state.status,"output":state.output,"error":state.error,"provider_error":self.store.prompt_provider_error(id)?,
            "events":events,"next_cursor":next,"done":state.status != "running" && !more && !writer_active}),
        )
    }

    fn handle_prompt_cancel(&self, params: Value) -> Result<Value> {
        let id = require_uuid(&params, "prompt_id")?;
        let state = self.store.prompt_state(id)?;
        let persisted = self.store.cancel_operation(id)?;
        if let Some(token) = self
            .running
            .lock()
            .expect("prompt cancellation lock")
            .get(&id)
        {
            token.cancel();
            return Ok(serde_json::json!({"prompt_id":id,"status":"cancelling"}));
        }
        if state.status == "running" && persisted > 0 {
            return Ok(serde_json::json!({"prompt_id":id,"status":"cancelling"}));
        }
        anyhow::ensure!(
            state.status != "running",
            "Legacy prompt has no durable cancellation channel"
        );
        Ok(serde_json::to_value(state)?)
    }

    /// Browser transport exposes only the operations used by the bundled client.
    pub fn handle_http(&self, method: &str, params: Value) -> Result<Value> {
        let workspace = self
            .http_workspace
            .as_ref()
            .ok_or_else(|| anyhow!("HTTP workspace unavailable"))?;
        let allowed: &[&str] = match method {
            "session/open" => &["workspace_root", "session_id"],
            "prompt/execute" => &["session_id", "prompt", "include_partial_messages"],
            "prompt/start" => &["session_id", "prompt", "request_id"],
            "prompt/events" => &["prompt_id", "cursor", "limit"],
            "prompt/cancel" => &["prompt_id"],
            "prompt/stream_next" => &["prompt_id", "cursor", "max_chunks"],
            _ => return Err(anyhow!("method unavailable over HTTP")),
        };
        anyhow::ensure!(
            params.is_null()
                || params.as_object().is_some_and(|fields| {
                    fields.keys().all(|key| allowed.contains(&key.as_str()))
                }),
            "unsupported HTTP RPC parameters"
        );

        match method {
            "session/open" => {
                if let Some(root) = params.get("workspace_root") {
                    anyhow::ensure!(
                        root.as_str().is_some_and(|root| {
                            root == "."
                                || Path::new(root).is_absolute()
                                    && Path::new(root)
                                        .canonicalize()
                                        .is_ok_and(|path| path == *workspace)
                        }),
                        "workspace does not match the launch workspace"
                    );
                }
                let requested = params["session_id"]
                    .as_str()
                    .and_then(|id| Uuid::parse_str(id).ok());
                let selected = self.exposed_session.or_else(|| {
                    requested.filter(|id| {
                        self.http_sessions
                            .lock()
                            .expect("HTTP sessions lock")
                            .contains(id)
                    })
                });
                if let Some(id) = selected {
                    let history =
                        public_chat_history(self.store.rebuild_from_events(id)?.chat_messages);
                    let latest = self.store.latest_prompt(id)?;
                    return Ok(serde_json::json!({
                        "session_id":id,"status":"resumed","workspace_root":workspace,
                        "history":history,"latest_prompt":latest
                    }));
                }
                let response =
                    self.handle_session_open(serde_json::json!({"workspace_root": workspace}))?;
                self.http_sessions
                    .lock()
                    .expect("HTTP sessions lock")
                    .insert(require_uuid(&response, "session_id")?);
                Ok(response)
            }
            "prompt/execute" | "prompt/start" => {
                let session_id = require_uuid(&params, "session_id")?;
                anyhow::ensure!(
                    self.http_sessions
                        .lock()
                        .expect("HTTP sessions lock")
                        .contains(&session_id),
                    "session was not opened by this HTTP server"
                );
                let session = self
                    .store
                    .load_session(session_id)?
                    .ok_or_else(|| anyhow!("session not found"))?;
                anyhow::ensure!(
                    Path::new(&session.workspace_root) == workspace
                        && workspace.canonicalize()? == *workspace,
                    "workspace changed since session creation"
                );
                let prompt = params
                    .get("prompt")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("missing 'prompt' parameter"))?;
                let result =
                    if method == "prompt/start" {
                        self.run_durable_prompt(params, true)?
                    } else {
                        self.run_durable_prompt(serde_json::json!({
                        "session_id":session_id,"prompt":prompt,"include_partial_messages":true
                    }), false)?
                    };
                self.http_prompts
                    .lock()
                    .expect("HTTP prompts lock")
                    .insert(require_uuid(&result, "prompt_id")?);
                Ok(result)
            }
            "prompt/events" | "prompt/cancel" => {
                let id = require_uuid(&params, "prompt_id")?;
                let session = self.store.prompt_session(id)?;
                anyhow::ensure!(
                    self.http_sessions
                        .lock()
                        .expect("HTTP sessions lock")
                        .contains(&session),
                    "prompt is unavailable"
                );
                if method == "prompt/cancel" {
                    self.handle_prompt_cancel(params)
                } else {
                    self.handle_prompt_events(params)
                }
            }
            _ => {
                let prompt_id = require_uuid(&params, "prompt_id")?;
                let mut prompts = self.http_prompts.lock().expect("HTTP prompts lock");
                anyhow::ensure!(prompts.contains(&prompt_id), "prompt is unavailable");
                let response = self.handle_prompt_stream_next(params)?;
                if response["done"] == true {
                    prompts.remove(&prompt_id);
                }
                Ok(response)
            }
        }
    }

    // -- Session methods --

    fn handle_session_open(&self, params: Value) -> Result<Value> {
        let workspace = params
            .get("workspace_root")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let session_id = Uuid::now_v7();
        let session = Session {
            session_id,
            workspace_root: workspace.to_string(),
            baseline_commit: params
                .get("baseline_commit")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            status: SessionState::Idle,
            budgets: SessionBudgets {
                per_turn_seconds: 300,
                max_think_tokens: 8192,
            },
            active_plan_id: None,
        };
        self.store.save_session(&session)?;
        Ok(serde_json::json!({
            "session_id": session_id.to_string(),
            "status": "idle",
            "workspace_root": workspace,
        }))
    }

    fn handle_session_resume(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let session = self
            .store
            .load_session(session_id)?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;

        // Rebuild conversation from event log.
        let projection = self.store.rebuild_from_events(session_id)?;
        let turn_count = projection.chat_messages.len();

        let history = public_chat_history(projection.chat_messages.clone());
        let latest = self.store.latest_prompt(session_id)?;
        Ok(serde_json::json!({
            "session_id": session_id.to_string(),
            "status": format!("{:?}", session.status),
            "turn_count": turn_count,
            "history": history,
            "latest_prompt": latest,
            "legacy_tool_history": projection.legacy_tool_history,
            "workspace_root": session.workspace_root,
        }))
    }

    fn handle_session_fork(&self, params: Value) -> Result<Value> {
        let from_id = require_uuid(&params, "session_id")?;
        let forked = self.store.fork_session(from_id)?;

        Ok(serde_json::json!({
            "session_id": forked.session_id.to_string(),
            "forked_from": from_id.to_string(),
            "status": "idle",
        }))
    }

    fn handle_session_list(&self, _params: Value) -> Result<Value> {
        // Query all sessions from the database ordered by updated_at desc.
        let conn = self.store.db()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, workspace_root, status, updated_at
             FROM sessions ORDER BY updated_at DESC LIMIT 50",
        )?;
        let mut sessions = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            sessions.push(serde_json::json!({
                "session_id": row.get::<_, String>(0)?,
                "workspace_root": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "updated_at": row.get::<_, String>(3)?,
            }));
        }
        Ok(serde_json::json!({ "sessions": sessions }))
    }

    // -- Prompt methods --

    fn execute_prompt(
        &self,
        params: Value,
        prompt_id: Uuid,
        lease: Arc<codingbuddy_store::SessionLease>,
        cancellation: codingbuddy_core::CancellationToken,
    ) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'prompt' parameter"))?
            .to_string();

        // Verify session exists.
        let session = self
            .store
            .load_session(session_id)?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;

        let include_partial = params
            .get("include_partial_messages")
            .and_then(|v| v.as_bool())
            .unwrap_or(self.include_partial_messages);

        let repo_root = params
            .get("repo_root")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let debug_context = params
            .get("debug_context")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let force_max_think = params
            .get("force_max_think")
            .or_else(|| params.get("thinking_enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let tools = params
            .get("tools")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .map(parse_chat_mode)
            .unwrap_or(ChatMode::Code);
        let teammate_mode = params
            .get("teammate_mode")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        let additional_dirs = params
            .get("additional_dirs")
            .and_then(|v| v.as_array())
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.as_str())
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let workspace_root = PathBuf::from(session.workspace_root.clone());

        let mut provider_error = Value::Null;
        let (status, output, error_message) = if let Some(mock_output) =
            params.get("mock_output").and_then(|v| v.as_str())
        {
            self.store.append_event(&codingbuddy_store::NewEvent {
                event_id: uuid::Uuid::now_v7(),
                at: chrono::Utc::now(),
                session_id,
                kind: EventKind::ChatTurn {
                    message: ChatMessage::User {
                        content: prompt.clone(),
                    },
                },
            })?;
            self.store.append_prompt_event(
                prompt_id,
                &stream_chunk_to_event_json(&StreamChunk::ContentDelta(mock_output.to_owned())),
            )?;
            ("completed".to_string(), mock_output.to_string(), None)
        } else {
            let engine = AgentEngine::new(&workspace_root)?;
            let chat_options = ChatOptions {
                session_id: Some(session_id),
                session_lease: Some(lease),
                prompt_id: Some(prompt_id),
                cancellation: Some(cancellation),
                tools,
                force_max_think,
                additional_dirs,
                repo_root_override: repo_root,
                debug_context,
                mode,
                teammate_mode,
                ..Default::default()
            };

            match engine.chat_with_options(&prompt, chat_options) {
                Ok(output) => ("completed".to_string(), output, None),
                Err(err) => {
                    provider_error =
                        codingbuddy_llm::error::diagnostic(&err).unwrap_or(Value::Null);
                    if !provider_error.is_null() {
                        self.store.append_prompt_event(prompt_id, &serde_json::json!({"type":"provider_error","provider_error":provider_error}))?;
                    }
                    ("failed".into(), String::new(), Some(format!("{err:#}")))
                }
            }
        };

        if !output.is_empty() && params.get("mock_output").and_then(Value::as_str).is_some() {
            self.store.append_event(&codingbuddy_store::NewEvent::new(
                session_id,
                EventKind::ChatTurn {
                    message: ChatMessage::Assistant {
                        content: Some(output.clone()),
                        native_parts: None,
                        reasoning_content: None,
                        tool_calls: vec![],
                    },
                },
            ))?;
        }

        let stream_meta = if include_partial {
            serde_json::json!({
                "method": "prompt/stream_next",
                "prompt_id": prompt_id.to_string(),
                "cursor": 0,
            })
        } else {
            Value::Null
        };

        Ok(serde_json::json!({
            "prompt_id": prompt_id.to_string(),
            "session_id": session_id.to_string(),
            "status": status,
            "prompt": prompt,
            "output": if output.is_empty() { Value::Null } else { serde_json::json!(output) },
            "error": error_message,
            "provider_error": provider_error,
            "partial_messages_enabled": include_partial,
            "stream": stream_meta,
        }))
    }

    fn handle_prompt_stream_next(&self, params: Value) -> Result<Value> {
        let prompt_id = require_uuid(&params, "prompt_id")?;
        let cursor = params["cursor"].as_i64().unwrap_or(0).max(0);
        let limit = params["max_chunks"].as_u64().unwrap_or(8).clamp(1, 128) as u32;
        let Ok(state) = self.store.prompt_state(prompt_id) else {
            return Ok(
                serde_json::json!({"prompt_id":prompt_id,"chunks":[],"next_cursor":cursor,"done":true,"missing":true}),
            );
        };
        let mut rows = self
            .store
            .prompt_events_by_offset(prompt_id, cursor, limit + 1)?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next = cursor + rows.len() as i64;
        let out_chunks = rows
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let mut chunk = serde_json::json!({
                    "cursor": cursor + idx as i64,
                    "event": row,
                });
                if let Some(kind) = row.get("type").and_then(|v| v.as_str()) {
                    chunk["type"] = serde_json::json!(kind);
                }
                if let Some(text) = row.get("text").and_then(|v| v.as_str()) {
                    chunk["delta"] = serde_json::json!(text);
                }
                chunk
            })
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "prompt_id": prompt_id.to_string(),
            "chunks": out_chunks,
            "next_cursor": next,
            "done": state.status != "running" && !more,
        }))
    }

    fn handle_session_handoff_export(&self, params: Value) -> Result<Value> {
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

    fn handle_session_handoff_import(&self, params: Value) -> Result<Value> {
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

    fn handle_session_handoff_link_create(&self, params: Value) -> Result<Value> {
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

    fn handle_session_handoff_link_consume(&self, params: Value) -> Result<Value> {
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

    fn handle_session_remote_resume(&self, params: Value) -> Result<Value> {
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

    fn handle_events_poll(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let after_seq = params
            .get("after_seq")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as i64;
        let limit = limit.clamp(1, 500);

        let mut next_cursor = after_seq;
        let events = self
            .store
            .poll_events(session_id, after_seq, limit as usize)?
            .into_iter()
            .map(|event| {
                next_cursor = event.seq_no;
                serde_json::json!({
                    "seq_no": event.seq_no,
                    "at": event.at,
                    "kind": event.kind,
                    "payload": event.payload,
                })
            })
            .collect::<Vec<_>>();
        let done = events.len() < limit as usize;

        Ok(serde_json::json!({
            "session_id": session_id.to_string(),
            "events": events,
            "next_cursor": next_cursor,
            "done": done,
        }))
    }

    fn handle_session_timeline(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as i64;
        let limit = limit.clamp(1, 500);

        let mut timeline = self
            .store
            .timeline_events(session_id, limit as usize)?
            .into_iter()
            .map(|event| {
                serde_json::json!({
                    "seq_no": event.seq_no,
                    "at": event.at,
                    "kind": event.kind,
                    "category": timeline_category(&event.kind),
                    "summary": timeline_summary(&event.kind, &event.payload),
                    "payload": event.payload,
                })
            })
            .collect::<Vec<_>>();

        timeline.reverse();
        let projection = self.store.rebuild_from_events(session_id)?;
        let tasks = self.store.list_tasks(Some(session_id))?;
        let background_jobs = self.store.list_background_jobs()?;

        Ok(serde_json::json!({
            "session_id": session_id.to_string(),
            "timeline": timeline,
            "patches": {
                "staged": projection.staged_patches.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "applied": projection.applied_patches.iter().map(ToString::to_string).collect::<Vec<_>>(),
            },
            "steps": projection.step_status.iter().map(|(step_id, done, note)| {
                serde_json::json!({
                    "step_id": step_id.to_string(),
                    "done": done,
                    "note": note,
                })
            }).collect::<Vec<_>>(),
            "tasks": tasks.iter().map(|task| {
                serde_json::json!({
                    "task_id": task.task_id.to_string(),
                    "title": &task.title,
                    "status": &task.status,
                    "priority": task.priority,
                    "outcome": &task.outcome,
                })
            }).collect::<Vec<_>>(),
            "background_jobs": background_jobs.iter().take(25).map(|job| {
                serde_json::json!({
                    "job_id": job.job_id.to_string(),
                    "kind": &job.kind,
                    "reference": &job.reference,
                    "status": &job.status,
                    "metadata": serde_json::from_str::<Value>(&job.metadata_json).unwrap_or(Value::Null),
                    "started_at": &job.started_at,
                    "updated_at": &job.updated_at,
                })
            }).collect::<Vec<_>>(),
        }))
    }

    // -- Tool approval methods --

    fn handle_tool_approve(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let invocation_id_str = params
            .get("invocation_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'invocation_id' parameter"))?;
        let invocation_id = Uuid::parse_str(invocation_id_str)?;

        // Remove from pending approvals.
        let mut approvals = self.approvals.lock().expect("approvals lock");
        let tool_name = approvals
            .get_mut(&session_id)
            .and_then(|pa| pa.pending.remove(invocation_id_str))
            .unwrap_or_default();

        // Record approval event.
        self.store.append_event(&codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: chrono::Utc::now(),
            session_id,
            kind: EventKind::ToolApproved { invocation_id },
        })?;

        Ok(serde_json::json!({
            "approved": true,
            "invocation_id": invocation_id_str,
            "tool_name": tool_name,
        }))
    }

    fn handle_tool_deny(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let invocation_id_str = params
            .get("invocation_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'invocation_id' parameter"))?;

        // Remove from pending approvals.
        let mut approvals = self.approvals.lock().expect("approvals lock");
        let tool_name = approvals
            .get_mut(&session_id)
            .and_then(|pa| pa.pending.remove(invocation_id_str))
            .unwrap_or_default();

        Ok(serde_json::json!({
            "denied": true,
            "invocation_id": invocation_id_str,
            "tool_name": tool_name,
        }))
    }

    // -- Patch methods --

    fn handle_patch_preview(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let patch_id = require_uuid(&params, "patch_id")?;

        // Verify the patch is known in this session.
        let projection = self.store.rebuild_from_events(session_id)?;
        if !projection.staged_patches.contains(&patch_id) {
            return Err(anyhow!("patch not found: {patch_id}"));
        }

        let applied = codingbuddy_diff::PatchStore::new(self.workspace_path())?
            .read_patch(patch_id)?
            .applied;
        Ok(serde_json::json!({
            "patch_id": patch_id.to_string(),
            "staged": true,
            "applied": applied,
        }))
    }

    fn handle_patch_apply(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let patch_id = require_uuid(&params, "patch_id")?;

        let _lease = self.store.session_lease(session_id)?;
        let projection = self.store.rebuild_from_events(session_id)?;
        anyhow::ensure!(
            projection.staged_patches.contains(&patch_id),
            "patch not found: {patch_id}"
        );
        let workspace = self.workspace_path().canonicalize()?;
        let session = self
            .store
            .load_session(session_id)?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        anyhow::ensure!(
            Path::new(&session.workspace_root).canonicalize()? == workspace,
            "session belongs to another workspace"
        );

        let files = codingbuddy_core::workspace_fs::WorkspaceFs::open(&workspace)?;
        let config = AppConfig::load(&workspace)?;
        let policy = codingbuddy_policy::PolicyEngine::from_app_config(&config.policy);
        let call = codingbuddy_core::ToolCall {
            name: "patch.apply".to_string(),
            args: serde_json::json!({"patch_id": patch_id}),
            requires_approval: true,
        };
        let record_denial = |error: anyhow::Error| -> Result<()> {
            self.store.append_event(&codingbuddy_store::NewEvent::new(
                session_id,
                EventKind::ToolDenied {
                    invocation_id: Uuid::now_v7(),
                    tool_name: call.name.clone(),
                    reason: error.to_string(),
                },
            ))?;
            Err(error)
        };
        policy.ensure_allowed(&call).or_else(record_denial)?;

        let patches = codingbuddy_diff::PatchStore::new(&workspace)?;
        let patch = patches.read_patch(patch_id)?;
        let check_target = |path: &Path| {
            policy
                .check_patch_target(&files, path)
                .or_else(record_denial)
        };
        for path in codingbuddy_diff::checked_patch_paths(&workspace, &patch.unified_diff)? {
            check_target(Path::new(&path))?;
        }

        let checkpoint =
            codingbuddy_memory::MemoryManager::new(&workspace)?.create_checkpoint("patch_apply")?;
        self.store.append_event(&codingbuddy_store::NewEvent::new(
            session_id,
            EventKind::CheckpointCreated {
                checkpoint_id: checkpoint.checkpoint_id,
                reason: checkpoint.reason,
                files_count: checkpoint.files_count,
                snapshot_path: checkpoint.snapshot_path,
            },
        ))?;
        let (applied, conflicts, recovery_status) =
            patches.apply_for_session(&self.store, session_id, patch_id, check_target)?;
        Ok(serde_json::json!({
            "patch_id": patch_id,
            "applied": applied,
            "conflicts": conflicts,
            "recovery_status": recovery_status
        }))
    }

    // -- Diagnostics methods --

    fn handle_diagnostics_list(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;

        // Rebuild events and extract diagnostic-related entries from transcript.
        let projection = self.store.rebuild_from_events(session_id)?;
        let diagnostics: Vec<Value> = projection
            .transcript
            .iter()
            .filter(|line| {
                let lower = line.to_lowercase();
                lower.contains("diagnostic")
                    || lower.contains("error")
                    || lower.contains("warning")
                    || lower.contains("lint")
            })
            .map(|line| serde_json::json!({"message": line}))
            .collect();

        Ok(serde_json::json!({
            "session_id": session_id.to_string(),
            "diagnostics": diagnostics,
            "tool_invocations": projection.tool_invocations.len(),
        }))
    }

    // -- Context management methods --

    fn handle_context_suggest(&self, params: Value) -> Result<Value> {
        let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        let context_manager = self.context_manager.lock().expect("context_manager lock");

        if let Some(manager) = context_manager.as_ref() {
            let suggestions = manager.suggest_relevant_files(query, limit);
            let suggestions_json: Vec<Value> = suggestions
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "path": s.path.to_string_lossy(),
                        "score": s.score,
                        "reasons": s.reasons,
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "suggestions": suggestions_json,
                "count": suggestions.len(),
                "query": query,
            }))
        } else {
            Ok(serde_json::json!({
                "suggestions": [],
                "count": 0,
                "query": query,
                "note": "Context analysis unavailable",
            }))
        }
    }

    fn handle_context_analyze(&self, _params: Value) -> Result<Value> {
        let mut context_manager = self.context_manager.lock().expect("context_manager lock");

        if let Some(manager) = context_manager.as_mut() {
            match manager.analyze_workspace() {
                Ok(()) => Ok(serde_json::json!({
                    "status": "analyzed",
                    "file_count": manager.file_count(),
                })),
                Err(e) => Ok(serde_json::json!({
                    "status": "error",
                    "error": e.to_string(),
                })),
            }
        } else {
            Ok(serde_json::json!({
                "status": "unavailable",
                "note": "Context manager not initialized",
            }))
        }
    }

    fn handle_context_compress(&self, params: Value) -> Result<Value> {
        let context = params.get("context").and_then(|v| v.as_str()).unwrap_or("");
        let max_tokens = params
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(4000) as usize;

        let context_manager = self.context_manager.lock().expect("context_manager lock");

        if let Some(manager) = context_manager.as_ref() {
            let compressed = manager.compress_context(context, max_tokens);
            let original_lines = context.lines().count();
            let compressed_lines = compressed.lines().count();

            Ok(serde_json::json!({
                "compressed": compressed,
                "original_lines": original_lines,
                "compressed_lines": compressed_lines,
                "compression_ratio": if original_lines > 0 {
                    (compressed_lines as f64) / (original_lines as f64)
                } else {
                    1.0
                },
            }))
        } else {
            // Fallback simple compression
            let lines: Vec<&str> = context.lines().collect();
            let total_lines = lines.len();
            let keep_lines = (max_tokens / 10).min(total_lines);

            let compressed = if total_lines > keep_lines {
                let start = keep_lines / 2;
                let end = keep_lines - start;
                let mut result: Vec<&str> = lines.iter().take(start).copied().collect();
                result.push("// ... context truncated ...");
                result.extend(lines.iter().rev().take(end).rev());
                result.join("\n")
            } else {
                context.to_string()
            };

            Ok(serde_json::json!({
                "compressed": compressed,
                "original_lines": total_lines,
                "compressed_lines": compressed.lines().count(),
                "compression_ratio": if total_lines > 0 {
                    (compressed.lines().count() as f64) / (total_lines as f64)
                } else {
                    1.0
                },
                "note": "Using fallback compression",
            }))
        }
    }

    fn handle_context_related(&self, params: Value) -> Result<Value> {
        let file_path = params
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'file_path' parameter"))?;
        let depth = params.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize;

        let path = Path::new(file_path);
        let context_manager = self.context_manager.lock().expect("context_manager lock");

        if let Some(manager) = context_manager.as_ref() {
            let related = manager.get_related_files(path, depth);
            let related_json: Vec<Value> = related
                .iter()
                .map(|p| serde_json::json!(p.to_string_lossy()))
                .collect();

            Ok(serde_json::json!({
                "file_path": file_path,
                "depth": depth,
                "related_files": related_json,
                "count": related.len(),
            }))
        } else {
            Ok(serde_json::json!({
                "file_path": file_path,
                "depth": depth,
                "related_files": [],
                "count": 0,
                "note": "Context analysis unavailable",
            }))
        }
    }

    fn handle_context_debug(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'prompt' parameter"))?
            .to_string();

        let session = self
            .store
            .load_session(session_id)?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let workspace_root = PathBuf::from(session.workspace_root);

        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .map(parse_chat_mode)
            .unwrap_or(ChatMode::Code);
        let tools = params
            .get("tools")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let repo_root_override = params
            .get("repo_root")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let engine = AgentEngine::new(&workspace_root)?;
        let digest = engine.context_debug_preview(
            &prompt,
            &ChatOptions {
                tools,
                mode,
                repo_root_override,
                debug_context: true,
                ..Default::default()
            },
        );

        Ok(serde_json::json!({
            "schema": "deepseek.context.debug.v1",
            "session_id": session_id.to_string(),
            "digest": digest,
        }))
    }

    // -- Task methods --

    fn handle_task_list(&self, params: Value) -> Result<Value> {
        let session_id = params
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(Uuid::parse_str)
            .transpose()?;
        let tasks = self.store.list_tasks(session_id)?;
        let task_list: Vec<Value> = tasks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "task_id": t.task_id.to_string(),
                    "title": t.title,
                    "priority": t.priority,
                    "status": t.status,
                    "outcome": t.outcome,
                })
            })
            .collect();
        Ok(serde_json::json!({ "tasks": task_list }))
    }

    fn handle_task_update(&self, params: Value) -> Result<Value> {
        let task_id = require_uuid(&params, "task_id")?;
        let status = params
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'status' parameter"))?;
        let outcome = params.get("outcome").and_then(|v| v.as_str());

        self.store.update_task_status(task_id, status, outcome)?;

        Ok(serde_json::json!({
            "task_id": task_id.to_string(),
            "status": status,
            "updated": true,
        }))
    }

    // -- Model methods --

    fn workspace_path(&self) -> &Path {
        self.store.root.parent().unwrap_or_else(|| Path::new("."))
    }

    fn handle_model_list(&self, _params: Value) -> Result<Value> {
        let cfg = AppConfig::load(self.workspace_path()).unwrap_or_default();
        let catalog = cfg.llm.model_catalog();

        Ok(serde_json::json!({
            "models": catalog.selector_items(&cfg.llm),
            "active_model": cfg.llm.active_base_model(),
            "active_provider": cfg.llm.provider,
            "catalog_source": catalog.source,
        }))
    }

    // -- Config methods --

    fn handle_config_get(&self, params: Value) -> Result<Value> {
        let cfg = AppConfig::load(self.workspace_path()).unwrap_or_default();
        let mut full = serde_json::to_value(&cfg)?;
        if let Some(llm) = full.get_mut("llm").and_then(Value::as_object_mut) {
            llm.remove("api_key");
            if let Some(providers) = llm.get_mut("providers").and_then(Value::as_object_mut) {
                for provider in providers.values_mut().filter_map(Value::as_object_mut) {
                    provider.remove("headers");
                    provider.remove("payload_options");
                }
            }
        }

        if let Some(key) = params.get("key").and_then(|v| v.as_str()) {
            // Dot-separated key path: "llm.base_model" -> cfg["llm"]["base_model"]
            let mut current = &full;
            for segment in key.split('.') {
                current = match current.get(segment) {
                    Some(v) => v,
                    None => return Ok(serde_json::json!({ "key": key, "value": null })),
                };
            }
            Ok(serde_json::json!({ "key": key, "value": current }))
        } else {
            Ok(serde_json::json!({ "config": full }))
        }
    }

    fn handle_config_set(&self, params: Value) -> Result<Value> {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'key' parameter"))?;
        let value = params
            .get("value")
            .ok_or_else(|| anyhow!("missing 'value' parameter"))?
            .clone();

        let settings_path = AppConfig::project_local_settings_path(self.workspace_path());

        // Preserve existing settings so we only update the requested key
        let mut settings: serde_json::Map<String, Value> = if settings_path.exists() {
            let raw = fs::read_to_string(&settings_path)?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            serde_json::Map::new()
        };

        // Set nested key via dot-path: "llm.base_model" -> {"llm": {"base_model": value}}
        let segments: Vec<&str> = key.split('.').collect();
        let mut target = &mut settings;
        for &segment in &segments[..segments.len() - 1] {
            target = target
                .entry(segment)
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .ok_or_else(|| anyhow!("config key segment '{segment}' is not an object"))?;
        }
        if let Some(last) = segments.last() {
            target.insert((*last).to_string(), value.clone());
        }

        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

        Ok(serde_json::json!({
            "key": key,
            "value": value,
            "persisted": true,
            "path": settings_path.to_string_lossy(),
        }))
    }

    // -- Tool progress --

    fn handle_tool_progress(&self, params: Value) -> Result<Value> {
        let prompt_id = require_uuid(&params, "prompt_id")?;
        let cursor = params.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let Ok(state) = self.store.prompt_state(prompt_id) else {
            return Ok(
                serde_json::json!({"prompt_id":prompt_id,"events":[],"next_cursor":cursor,"done":true}),
            );
        };
        let mut rows = self
            .store
            .prompt_events_by_offset(prompt_id, cursor as i64, 129)?;
        let more = rows.len() > 128;
        rows.truncate(128);
        let next = cursor + rows.len();
        rows.retain(|row| {
            matches!(
                row["type"].as_str(),
                Some("tool_dispatch" | "tool_settlement")
            )
        });
        Ok(
            serde_json::json!({"prompt_id":prompt_id,"events":rows,"next_cursor":next,"done":state.status != "running" && !more}),
        )
    }

    /// Register a pending tool approval that the IDE can approve/deny.
    pub fn add_pending_approval(&self, session_id: Uuid, invocation_id: &str, tool_name: &str) {
        self.approvals
            .lock()
            .expect("approvals lock")
            .entry(session_id)
            .or_default()
            .pending
            .insert(invocation_id.to_string(), tool_name.to_string());
    }
}

impl RpcHandler for IdeRpcHandler {
    fn handle(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            // Basic lifecycle
            "initialize" => Ok(serde_json::json!({
                "name": "codingbuddy-cli",
                "version": env!("CARGO_PKG_VERSION"),
                "partial_messages_enabled": self.include_partial_messages,
                "capabilities": [
                    "session/open", "session/resume", "session/fork", "session/list", "session/timeline",
                    "session/remote_resume", "session/handoff_export", "session/handoff_import",
                    "session/handoff_link_create", "session/handoff_link_consume",
                    "prompt/execute", "prompt/start", "prompt/events", "prompt/cancel", "prompt.v2", "prompt/stream_next", "stream/chunk",
                    "tool/approve", "tool/deny",
                    "patch/preview", "patch/apply", "checkpoint/preview", "checkpoint/apply", "checkpoint/reconcile", "checkpoint.v2", "execution.v1",
                    "diagnostics/list", "events/poll",
                    "context/suggest", "context/analyze", "context/compress", "context/related", "context/debug",
                    "task/list", "task/update",
                    "model/list", "config/get", "config/set", "tool/progress",
                    "status", "cancel", "shutdown"
                ]
            })),
            "status" => Ok(serde_json::json!({
                "status": "ready",
                "handler": "ide",
                "partial_messages_enabled": self.include_partial_messages,
            })),
            "cancel" => {
                let id = if let Some(id) = params.get("prompt_id").and_then(Value::as_str) {
                    Uuid::parse_str(id)?
                } else {
                    let running = self.running.lock().unwrap();
                    anyhow::ensure!(
                        running.len() <= 1,
                        "Multiple active prompts; supply prompt_id"
                    );
                    if running.is_empty() {
                        return Ok(serde_json::json!({"cancelled":false,"status":"idle"}));
                    }
                    *running.keys().next().unwrap()
                };
                let result = self.handle_prompt_cancel(serde_json::json!({"prompt_id":id}))?;
                Ok(
                    serde_json::json!({"cancelled":matches!(result["status"].as_str(),Some("cancelling"|"cancelled")),"prompt_id":id,"status":result["status"]}),
                )
            }
            "checkpoint/preview" => {
                let memory = codingbuddy_memory::MemoryManager::new(self.workspace_path())?;
                let plan = if params.get("step_id").is_some() {
                    memory.preview_step_restore(require_uuid(&params, "step_id")?)?
                } else {
                    let checkpoint = require_uuid(&params, "checkpoint_id")?;
                    let paths = params
                        .get("paths")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| {
                                    item.as_str()
                                        .map(PathBuf::from)
                                        .ok_or_else(|| anyhow!("Invalid restore path"))
                                })
                                .collect::<Result<Vec<_>>>()
                        })
                        .transpose()?
                        .unwrap_or_default();
                    memory.preview_restore(checkpoint, &paths)?
                };
                Ok(serde_json::to_value(plan)?)
            }
            "checkpoint/apply" => {
                let id = require_uuid(&params, "plan_id")?;
                let mode = params["mode"].as_str().unwrap_or("files");
                let (plan, fork) = codingbuddy_memory::MemoryManager::new(self.workspace_path())?
                    .complete_restore(id, mode, params["approved"] == true)?;
                Ok(
                    serde_json::json!({"plan_id":plan.id,"checkpoint_id":plan.checkpoint_id,"rewound":true,"recovery_status":"verified","preserved":plan.preserved,"session_id":fork}),
                )
            }
            "checkpoint/reconcile" => {
                let id = require_uuid(&params, "plan_id")?;
                let status = codingbuddy_memory::MemoryManager::new(self.workspace_path())?
                    .reconcile_restore(id)?;
                Ok(serde_json::json!({"plan_id":id,"recovery_status":status}))
            }

            // Session management
            "session/open" => self.handle_session_open(params),
            "session/resume" => self.handle_session_resume(params),
            "session/fork" => self.handle_session_fork(params),
            "session/list" => self.handle_session_list(params),
            "session/timeline" => self.handle_session_timeline(params),
            "session/remote_resume" => self.handle_session_remote_resume(params),
            "session/handoff_export" => self.handle_session_handoff_export(params),
            "session/handoff_import" => self.handle_session_handoff_import(params),
            "session/handoff_link_create" => self.handle_session_handoff_link_create(params),
            "session/handoff_link_consume" => self.handle_session_handoff_link_consume(params),

            // Prompt execution
            "prompt/execute" => self.run_durable_prompt(params, false),
            "prompt/start" => self.run_durable_prompt(params, true),
            "prompt/events" => self.handle_prompt_events(params),
            "prompt/cancel" => self.handle_prompt_cancel(params),
            "prompt/stream_next" => self.handle_prompt_stream_next(params),

            // Tool approval/denial
            "tool/approve" => self.handle_tool_approve(params),
            "tool/deny" => self.handle_tool_deny(params),

            // Patch preview/apply
            "patch/preview" => self.handle_patch_preview(params),
            "patch/apply" => self.handle_patch_apply(params),

            // Diagnostics
            "diagnostics/list" => self.handle_diagnostics_list(params),
            "events/poll" => self.handle_events_poll(params),

            // Context management
            "context/suggest" => self.handle_context_suggest(params),
            "context/analyze" => self.handle_context_analyze(params),
            "context/compress" => self.handle_context_compress(params),
            "context/related" => self.handle_context_related(params),
            "context/debug" => self.handle_context_debug(params),

            // Task management
            "task/list" => self.handle_task_list(params),
            "task/update" => self.handle_task_update(params),

            // Model & config
            "model/list" => self.handle_model_list(params),
            "config/get" => self.handle_config_get(params),
            "config/set" => self.handle_config_set(params),

            // Tool progress
            "tool/progress" => self.handle_tool_progress(params),

            _ => Err(anyhow!("method not found: {method}")),
        }
    }
}

fn timeline_category(kind: &str) -> &'static str {
    if kind.contains("Patch") || kind.contains("Snapshot") {
        "patch"
    } else if kind.contains("Tool") {
        "tool"
    } else if kind.contains("Background") || kind.contains("Subagent") || kind.contains("Run") {
        "background"
    } else if kind.contains("Task") || kind.contains("Step") || kind.contains("Plan") {
        "plan"
    } else if kind.contains("Model") || kind.contains("Provider") {
        "model"
    } else if kind.contains("Session") || kind.contains("Phase") {
        "session"
    } else if kind.contains("Error") || kind.contains("Warning") {
        "diagnostic"
    } else {
        "message"
    }
}

fn timeline_summary(kind: &str, payload: &Value) -> String {
    let kind = kind.split('@').next().unwrap_or(kind);
    let body = payload.get("payload").unwrap_or(payload);
    match kind {
        "ChatTurn" => body
            .get("message")
            .map(chat_turn_summary)
            .unwrap_or_else(|| "chat turn".to_string()),
        "ToolProposed" => body
            .pointer("/proposal/call/name")
            .and_then(Value::as_str)
            .map(|name| format!("tool proposed: {name}"))
            .unwrap_or_else(|| "tool proposed".to_string()),
        "ToolApproved" => body
            .get("invocation_id")
            .and_then(Value::as_str)
            .map(|id| format!("tool approved: {id}"))
            .unwrap_or_else(|| "tool approved".to_string()),
        "ToolResult" => body
            .pointer("/result/name")
            .and_then(Value::as_str)
            .map(|name| format!("tool result: {name}"))
            .unwrap_or_else(|| "tool result".to_string()),
        "PatchStaged" => body
            .get("patch_id")
            .and_then(Value::as_str)
            .map(|id| format!("patch staged: {id}"))
            .unwrap_or_else(|| "patch staged".to_string()),
        "PatchApplied" => {
            let applied = body
                .get("applied")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            body.get("patch_id")
                .and_then(Value::as_str)
                .map(|id| format!("patch applied={applied}: {id}"))
                .unwrap_or_else(|| format!("patch applied={applied}"))
        }
        "ModelChanged" => body
            .get("model")
            .and_then(Value::as_str)
            .map(|model| format!("model changed: {model}"))
            .unwrap_or_else(|| "model changed".to_string()),
        "ProviderSelected" => {
            let provider = body.get("provider").and_then(Value::as_str).unwrap_or("");
            let model = body.get("model").and_then(Value::as_str).unwrap_or("");
            format!("model selected: {provider}/{model}")
        }
        "PhaseTransition" => {
            let from = body.get("from").and_then(Value::as_str).unwrap_or("");
            let to = body.get("to").and_then(Value::as_str).unwrap_or("");
            format!("phase: {from} -> {to}")
        }
        "SessionStateChanged" => {
            let from = body.get("from").and_then(Value::as_str).unwrap_or("");
            let to = body.get("to").and_then(Value::as_str).unwrap_or("");
            format!("session state: {from} -> {to}")
        }
        _ => kind.to_string(),
    }
}

fn chat_turn_summary(message: &Value) -> String {
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        return format!("message: {}", truncate_timeline(content));
    }
    if message.get("tool_calls").is_some() {
        return "assistant tool call turn".to_string();
    }
    "chat turn".to_string()
}

fn truncate_timeline(value: &str) -> String {
    const MAX_CHARS: usize = 120;
    if value.chars().count() <= MAX_CHARS {
        return value.replace('\n', " ");
    }
    let mut out: String = value.chars().take(MAX_CHARS - 1).collect();
    out.push('…');
    out.replace('\n', " ")
}

fn handoff_dir(store_root: &Path) -> PathBuf {
    store_root.join("teleport").join("handoffs")
}

fn handoff_descriptor_path(store_root: &Path, handoff_id: Uuid) -> PathBuf {
    handoff_dir(store_root).join(format!("{handoff_id}.json"))
}

fn write_handoff_descriptor(store_root: &Path, descriptor: &HandoffDescriptor) -> Result<()> {
    let path = handoff_descriptor_path(store_root, descriptor.handoff_id);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(descriptor)?)?;
    Ok(())
}

fn read_handoff_descriptor(store_root: &Path, handoff_id: Uuid) -> Result<HandoffDescriptor> {
    let path = handoff_descriptor_path(store_root, handoff_id);
    let raw = fs::read_to_string(&path)?;
    let descriptor = serde_json::from_str::<HandoffDescriptor>(&raw)?;
    Ok(descriptor)
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Extract a UUID from a JSON params object.
fn public_chat_history(messages: Vec<ChatMessage>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            ChatMessage::User { content } => {
                Some(serde_json::json!({"role":"user","content":content}))
            }
            ChatMessage::Assistant {
                content: Some(content),
                ..
            } => Some(serde_json::json!({"role":"assistant","content":content})),
            _ => None,
        })
        .collect()
}

fn require_uuid(params: &Value, field: &str) -> Result<Uuid> {
    let s = params
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing '{field}' parameter"))?;
    Uuid::parse_str(s).map_err(|e| anyhow!("invalid UUID for '{field}': {e}"))
}

fn optional_uuid(params: &Value, field: &str) -> Result<Option<Uuid>> {
    let Some(raw) = params.get(field).and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    Ok(Some(
        Uuid::parse_str(raw).map_err(|e| anyhow!("invalid UUID for '{field}': {e}"))?,
    ))
}

fn env_flag_enabled(key: &str) -> bool {
    match std::env::var(key) {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            !normalized.is_empty() && !matches!(normalized.as_str(), "0" | "false" | "off" | "no")
        }
        Err(_) => false,
    }
}

fn parse_chat_mode(value: &str) -> ChatMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "ask" => ChatMode::Ask,
        "context" => ChatMode::Context,
        _ => ChatMode::Code,
    }
}

#[cfg(test)]
mod tests;
