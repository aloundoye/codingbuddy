//! Typed description of the JSON-RPC methods served by [`crate::IdeRpcHandler`].
//!
//! Handlers still read their parameters from `serde_json::Value`; these
//! structs are the published contract. [`protocol_schema`] turns them into the
//! JSON Schema checked in at `docs/protocol/jsonrpc.schema.json`, and in tests
//! every call that goes through the handler is checked against them (unknown
//! fields and wrong types fail), so the contract cannot drift from the code.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

/// A method that takes no parameters (`params` may be omitted or `{}`).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelParams {
    /// The prompt to cancel. Optional when exactly one prompt is running.
    pub prompt_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionOpenParams {
    /// Workspace the session works in (default `.`).
    pub workspace_root: Option<String>,
    /// Git commit the session starts from.
    pub baseline_commit: Option<String>,
    /// HTTP transport only: reopen this existing session.
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionIdParams {
    pub session_id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionTimelineParams {
    pub session_id: Uuid,
    /// Maximum number of events (default 100).
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandoffExportParams {
    /// Session to export (default: the latest session).
    pub session_id: Option<Uuid>,
    /// Where to write the bundle (default: the store's handoff directory).
    pub output_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandoffImportParams {
    /// Bundle to import. `handoff_path` is accepted as an alias.
    pub bundle_path: Option<String>,
    pub handoff_path: Option<String>,
    /// Workspace to import into (default: the bundle's workspace).
    pub workspace_root: Option<String>,
    /// Resume the imported session immediately.
    pub resume: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteResumeParams {
    /// Resume this session when no bundle is given.
    pub session_id: Option<Uuid>,
    /// Import this bundle and resume it. `handoff_path` is an alias.
    pub bundle_path: Option<String>,
    pub handoff_path: Option<String>,
    pub workspace_root: Option<String>,
    pub resume: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandoffLinkCreateParams {
    pub session_id: Option<Uuid>,
    pub output_path: Option<String>,
    /// Link lifetime in minutes.
    pub ttl_minutes: Option<u64>,
    /// Base URL the link is built on.
    pub base_url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandoffLinkConsumeParams {
    pub handoff_id: Uuid,
    /// The one-time token from the link.
    pub token: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptParams {
    pub session_id: Uuid,
    /// The user's prompt. Must not be blank.
    pub prompt: String,
    /// Idempotency key. Required by `prompt/start`; a retried request with
    /// the same key returns the existing prompt.
    pub request_id: Option<Uuid>,
    /// Stream partial assistant messages as events.
    pub include_partial_messages: Option<bool>,
    /// Repository root, when it differs from the session workspace.
    pub repo_root: Option<String>,
    /// Include the context-assembly digest in the result.
    pub debug_context: Option<bool>,
    /// Use the maximum thinking budget. `thinking_enabled` is an alias.
    pub force_max_think: Option<bool>,
    pub thinking_enabled: Option<bool>,
    /// Allow tool use (default true).
    pub tools: Option<bool>,
    /// `code` (default), `ask`, `context` or `plan`.
    pub mode: Option<String>,
    /// Team-mode setting passed to the agent.
    pub teammate_mode: Option<String>,
    /// Extra directories the agent may read.
    pub additional_dirs: Option<Vec<String>>,
    /// Testing only: skip the model and return this text.
    pub mock_output: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptEventsParams {
    pub prompt_id: Uuid,
    /// Offset of the first event to return (default 0).
    pub cursor: Option<i64>,
    /// Maximum events to return, 1-127 (default 32).
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptIdParams {
    pub prompt_id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamNextParams {
    pub prompt_id: Uuid,
    pub cursor: Option<i64>,
    /// Maximum chunks to return, 1-128 (default 8).
    pub max_chunks: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolDecisionParams {
    pub session_id: Uuid,
    /// The pending tool invocation.
    pub invocation_id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchParams {
    pub session_id: Uuid,
    pub patch_id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointPreviewParams {
    /// Preview undoing one step. Takes precedence over `checkpoint_id`.
    pub step_id: Option<Uuid>,
    /// Preview restoring this checkpoint.
    pub checkpoint_id: Option<Uuid>,
    /// Limit the restore to these paths.
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointApplyParams {
    /// The plan returned by `checkpoint/preview`.
    pub plan_id: Uuid,
    /// `files` (default) or another restore mode.
    pub mode: Option<String>,
    /// Must be `true` to apply a plan that needs approval.
    pub approved: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanIdParams {
    pub plan_id: Uuid,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsPollParams {
    pub session_id: Uuid,
    /// Return events after this sequence number (default 0).
    pub after_seq: Option<u64>,
    /// 1-500 (default 50).
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextSuggestParams {
    pub query: Option<String>,
    /// Default 10.
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextCompressParams {
    pub context: Option<String>,
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextRelatedParams {
    pub file_path: Option<String>,
    /// Import-graph depth (default 1).
    pub depth: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextDebugParams {
    pub session_id: Uuid,
    pub prompt: String,
    pub mode: Option<String>,
    pub tools: Option<bool>,
    pub repo_root: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskListParams {
    /// Only tasks of this session.
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdateParams {
    pub task_id: Uuid,
    pub status: String,
    pub outcome: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetParams {
    /// A dotted key; omit for the whole configuration.
    pub key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigSetParams {
    /// A dotted key, written to `.codingbuddy/settings.local.json`.
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolProgressParams {
    pub prompt_id: Uuid,
    pub cursor: Option<u64>,
}

type Check = fn(&Value) -> Result<(), serde_json::Error>;
type SchemaFn = fn() -> schemars::Schema;

/// One published method: name, summary, parameter type.
struct Method {
    name: &'static str,
    summary: &'static str,
    check: Check,
    schema: SchemaFn,
}

macro_rules! methods {
    ($($name:literal => $ty:ty, $summary:literal;)*) => {
        &[$(Method {
            name: $name,
            summary: $summary,
            check: |params| {
                let params = if params.is_null() { serde_json::json!({}) } else { params.clone() };
                serde_json::from_value::<$ty>(params).map(drop)
            },
            schema: || schemars::schema_for!($ty),
        },)*]
    };
}

const METHODS: &[Method] = methods! {
    "initialize" => NoParams, "Server name, version and capability list.";
    "status" => NoParams, "Whether the server is ready.";
    "cancel" => CancelParams, "Cancel a running prompt.";
    "shutdown" => NoParams, "Stop the server after replying.";
    "session/open" => SessionOpenParams, "Create a session.";
    "session/resume" => SessionIdParams, "Load a session's state.";
    "session/fork" => SessionIdParams, "Copy a session into a new one.";
    "session/list" => NoParams, "List sessions.";
    "session/timeline" => SessionTimelineParams, "A session's event history.";
    "session/remote_resume" => RemoteResumeParams, "Resume a session, importing a bundle first if given.";
    "session/handoff_export" => HandoffExportParams, "Write a portable session bundle.";
    "session/handoff_import" => HandoffImportParams, "Import a session bundle.";
    "session/handoff_link_create" => HandoffLinkCreateParams, "Export a bundle and create a one-time link to it.";
    "session/handoff_link_consume" => HandoffLinkConsumeParams, "Redeem a handoff link.";
    "prompt/execute" => PromptParams, "Run a prompt and wait for the result.";
    "prompt/start" => PromptParams, "Start a prompt in the background (requires request_id).";
    "prompt/events" => PromptEventsParams, "Page through a prompt's durable events.";
    "prompt/cancel" => PromptIdParams, "Cancel a prompt.";
    "prompt/stream_next" => StreamNextParams, "Next stream chunks of a prompt.";
    "tool/approve" => ToolDecisionParams, "Approve a pending tool call.";
    "tool/deny" => ToolDecisionParams, "Deny a pending tool call.";
    "tool/progress" => ToolProgressParams, "Live tool progress events of a prompt.";
    "patch/preview" => PatchParams, "Show a staged patch.";
    "patch/apply" => PatchParams, "Apply a staged patch.";
    "checkpoint/preview" => CheckpointPreviewParams, "Plan a restore and report what it would change.";
    "checkpoint/apply" => CheckpointApplyParams, "Carry out a restore plan.";
    "checkpoint/reconcile" => PlanIdParams, "Re-check an interrupted restore.";
    "diagnostics/list" => SessionIdParams, "Diagnostics recorded for a session.";
    "events/poll" => EventsPollParams, "Session events after a sequence number.";
    "context/suggest" => ContextSuggestParams, "Files relevant to a query.";
    "context/analyze" => NoParams, "Workspace overview.";
    "context/compress" => ContextCompressParams, "Shrink a block of context to a token budget.";
    "context/related" => ContextRelatedParams, "Files related to a file through imports.";
    "context/debug" => ContextDebugParams, "Show the context a prompt would be sent with.";
    "task/list" => TaskListParams, "List tasks.";
    "task/update" => TaskUpdateParams, "Change a task's status.";
    "model/list" => NoParams, "Available models.";
    "config/get" => ConfigGetParams, "Read configuration.";
    "config/set" => ConfigSetParams, "Write a project-local setting.";
};

/// Names of every published method.
pub fn method_names() -> impl Iterator<Item = &'static str> {
    METHODS.iter().map(|method| method.name)
}

/// Check `params` against the published parameters of `method`.
/// Unknown methods are not checked here; the dispatcher rejects them.
pub fn check_params(method: &str, params: &Value) -> Result<(), String> {
    match METHODS.iter().find(|m| m.name == method) {
        Some(m) => (m.check)(params).map_err(|e| format!("invalid params for {method}: {e}")),
        None => Ok(()),
    }
}

/// The JSON Schema of the whole protocol: one entry per method with its
/// summary and parameter schema.
pub fn protocol_schema() -> Value {
    let methods: serde_json::Map<String, Value> = METHODS
        .iter()
        .map(|method| {
            let mut params = serde_json::to_value((method.schema)()).unwrap_or(Value::Null);
            if let Some(object) = params.as_object_mut() {
                object.remove("$schema");
                object.remove("title");
            }
            (
                method.name.to_string(),
                serde_json::json!({
                    "summary": method.summary,
                    "params": params,
                }),
            )
        })
        .collect();
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "CodingBuddy JSON-RPC protocol",
        "description": "JSON-RPC 2.0 over stdio (one JSON object per line) or HTTP. Each entry is a method name with the schema of its `params` object. Results are JSON objects; see docs/protocol/README.md.",
        "methods": methods,
    })
}
