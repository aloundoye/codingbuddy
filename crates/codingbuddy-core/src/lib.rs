use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub mod complexity;
mod config;
pub mod config_watcher;
pub mod cost;
mod events;
pub mod execution;
pub mod heuristics;
mod llm_capabilities;
pub mod logging;
mod model_catalog;
pub mod process_env;
pub mod profiler;
pub mod prompts;
pub mod runtime;
mod stream;
mod tool_metadata;
pub mod workspace_fs;
pub use config::*;
pub use events::*;
pub use llm_capabilities::{
    CapabilityOverride, CapabilityRegistryOverrides, CapabilityResolution, ModelCapabilities,
    ModelFamily, PreferredEditTool, ProviderKind, ThinkingCapability, detect_model_family,
    model_capabilities, model_capabilities_with_registry, normalize_provider_kind,
    resolve_model_capabilities,
};
pub use model_catalog::{
    DEFAULT_MODEL_CATALOG_CACHE_TTL_SECONDS, DEFAULT_MODEL_CATALOG_REFRESH_TIMEOUT_SECONDS,
    DEFAULT_MODELS_DEV_URL, ModelCatalog, ModelCatalogCache, ModelCatalogConfig,
    ModelCatalogSource, ModelCost, ModelInfo, ModelLimits, ModelModality, ModelSelectorItem,
    ModelStatus, ProviderCapability, ProviderStatus,
};
pub use stream::*;
pub use tool_metadata::{
    DynamicToolTrust, InterruptBehavior, RuntimeToolMetadata, ToolAgentRole, ToolMetadata,
    ToolPermissionMatcher, ToolPhaseAccess, ToolResultSizePolicy, ToolTier,
    is_api_tool_name_read_only, is_internal_tool_name_read_only,
};

pub type Result<T> = anyhow::Result<T>;
pub type ProviderOptions = BTreeMap<String, serde_json::Value>;

// DeepSeek API model constants.
pub const CODINGBUDDY_V32_CHAT_MODEL: &str = "deepseek-chat";
pub const CODINGBUDDY_V32_REASONER_MODEL: &str = "deepseek-reasoner";

/// Maximum output tokens for a model, looked up from the capability registry.
/// Falls back to 8192 if no capability data is available.
pub fn max_output_tokens_for_model(
    provider: ProviderKind,
    model: &str,
    thinking_enabled: bool,
) -> u32 {
    let caps = model_capabilities(provider, model);
    match caps.thinking_capability {
        ThinkingCapability::NativeReasoning => 65536,
        ThinkingCapability::ExtendedThinking if thinking_enabled => 32768,
        _ => 8192,
    }
}

pub fn runtime_dir(workspace: &Path) -> PathBuf {
    workspace.join(".codingbuddy")
}

/// Explicit workspace trust comes from the launcher's environment, never project settings.
pub fn workspace_is_trusted(workspace: &Path) -> bool {
    let Some(trusted) = std::env::var_os("CODINGBUDDY_TRUST_WORKSPACE") else {
        return false;
    };
    let trusted = Path::new(&trusted);
    if !trusted.is_absolute() {
        return false;
    }
    trusted
        .canonicalize()
        .ok()
        .zip(workspace.canonicalize().ok())
        .is_some_and(|(trusted, workspace)| trusted == workspace)
}

/// Actions available in the rewind picker menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RewindAction {
    RestoreCodeAndConversation,
    RestoreConversationOnly,
    RestoreCodeOnly,
    Summarize,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Planning,
    ExecutingStep,
    AwaitingApproval,
    Verifying,
    Completed,
    Paused,
    Failed,
}

pub fn is_valid_session_state_transition(from: &SessionState, to: &SessionState) -> bool {
    if from == to {
        return true;
    }
    match from {
        SessionState::Idle => matches!(
            to,
            SessionState::Planning | SessionState::Paused | SessionState::Failed
        ),
        SessionState::Planning => matches!(
            to,
            SessionState::ExecutingStep
                | SessionState::Verifying
                | SessionState::Paused
                | SessionState::Failed
        ),
        SessionState::ExecutingStep => matches!(
            to,
            SessionState::AwaitingApproval
                | SessionState::Verifying
                | SessionState::Planning
                | SessionState::Completed
                | SessionState::Paused
                | SessionState::Failed
        ),
        SessionState::AwaitingApproval => matches!(
            to,
            SessionState::ExecutingStep
                | SessionState::Planning
                | SessionState::Paused
                | SessionState::Failed
        ),
        SessionState::Verifying => matches!(
            to,
            SessionState::Planning
                | SessionState::ExecutingStep
                | SessionState::Completed
                | SessionState::Paused
                | SessionState::Failed
        ),
        SessionState::Completed => matches!(to, SessionState::Idle | SessionState::Planning),
        SessionState::Paused => matches!(
            to,
            SessionState::Planning | SessionState::ExecutingStep | SessionState::Failed
        ),
        SessionState::Failed => matches!(to, SessionState::Planning | SessionState::Idle),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBudgets {
    pub per_turn_seconds: u64,
    pub max_think_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_id: Uuid,
    pub workspace_root: String,
    pub baseline_commit: Option<String>,
    pub status: SessionState,
    pub budgets: SessionBudgets,
    pub active_plan_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub plan_id: Uuid,
    pub version: u32,
    pub goal: String,
    pub assumptions: Vec<String>,
    pub steps: Vec<PlanStep>,
    pub verification: Vec<String>,
    pub risk_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub step_id: Uuid,
    pub title: String,
    pub intent: String,
    pub tools: Vec<String>,
    pub files: Vec<String>,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunState {
    Context,
    Planning,
    GatherEvidence,
    Subagents,
    Executing,
    Completed,
    Recover,
    Final,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub status: RunState,
    pub prompt: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LlmUnit {
    Model(String),
    Planner,
    Executor,
}

/// Check whether a tool name refers to an MCP-discovered tool (`mcp__server__name` convention).
#[must_use]
pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with("mcp__")
}

/// Type-safe tool name enum covering all built-in tools.
///
/// Maps between underscored API names (`fs_read`) and dotted internal names (`fs.read`).
/// Plugin and MCP tools are not represented here — use `from_api_name` which returns `None`
/// for unknown tool names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolName {
    FsRead,
    FsWrite,
    FsEdit,
    FsList,
    FsGlob,
    FsGrep,
    BashRun,
    MultiEdit,
    GitStatus,
    GitDiff,
    GitShow,
    WebFetch,
    WebSearch,
    NotebookRead,
    NotebookEdit,
    IndexQuery,
    PatchStage,
    PatchApply,
    PatchDirect,
    DiagnosticsCheck,
    ExtendedThinking,
    ToolSearch,
    UserQuestion,
    TaskCreate,
    TaskUpdate,
    TodoRead,
    TodoWrite,
    TaskGet,
    TaskList,
    SpawnTask,
    SendMessage,
    TaskOutput,
    TaskStop,
    EnterPlanMode,
    ExitPlanMode,
    EnterWorktree,
    ExitWorktree,
    Skill,
    KillShell,
    Batch,
    LspHover,
    LspDefinition,
    LspReferences,
    LspSymbols,
    GithubCreatePr,
    GithubListIssues,
    GithubViewPr,
}

impl ToolName {
    /// Parse from underscored API name (e.g. `"fs_read"`). Returns `None` for
    /// unknown names (plugins, MCP tools).
    #[must_use]
    pub fn from_api_name(s: &str) -> Option<Self> {
        Some(match s {
            "fs_read" => Self::FsRead,
            "fs_write" => Self::FsWrite,
            "fs_edit" => Self::FsEdit,
            "fs_list" => Self::FsList,
            "fs_glob" => Self::FsGlob,
            "fs_grep" => Self::FsGrep,
            "bash_run" => Self::BashRun,
            "multi_edit" => Self::MultiEdit,
            "git_status" => Self::GitStatus,
            "git_diff" => Self::GitDiff,
            "git_show" => Self::GitShow,
            "web_fetch" => Self::WebFetch,
            "web_search" => Self::WebSearch,
            "notebook_read" => Self::NotebookRead,
            "notebook_edit" => Self::NotebookEdit,
            "index_query" => Self::IndexQuery,
            "patch_stage" => Self::PatchStage,
            "patch_apply" => Self::PatchApply,
            "patch_direct" => Self::PatchDirect,
            "diagnostics_check" => Self::DiagnosticsCheck,
            "extended_thinking" | "think_deeply" => Self::ExtendedThinking,
            "tool_search" => Self::ToolSearch,
            "user_question" => Self::UserQuestion,
            "task_create" => Self::TaskCreate,
            "task_update" => Self::TaskUpdate,
            "todo_read" => Self::TodoRead,
            "todo_write" => Self::TodoWrite,
            "task_get" => Self::TaskGet,
            "task_list" => Self::TaskList,
            "spawn_task" => Self::SpawnTask,
            "send_message" => Self::SendMessage,
            "task_output" => Self::TaskOutput,
            "task_stop" => Self::TaskStop,
            "enter_plan_mode" => Self::EnterPlanMode,
            "exit_plan_mode" => Self::ExitPlanMode,
            "enter_worktree" => Self::EnterWorktree,
            "exit_worktree" => Self::ExitWorktree,
            "skill" => Self::Skill,
            "kill_shell" => Self::KillShell,
            "batch" => Self::Batch,
            "lsp_hover" => Self::LspHover,
            "lsp_definition" => Self::LspDefinition,
            "lsp_references" => Self::LspReferences,
            "lsp_symbols" => Self::LspSymbols,
            "github_create_pr" => Self::GithubCreatePr,
            "github_list_issues" => Self::GithubListIssues,
            "github_view_pr" => Self::GithubViewPr,
            _ => return None,
        })
    }

    /// Parse from dotted internal name (e.g. `"fs.read"`). Returns `None` for
    /// unknown names.
    #[must_use]
    pub fn from_internal_name(s: &str) -> Option<Self> {
        Some(match s {
            "fs.read" => Self::FsRead,
            "fs.write" => Self::FsWrite,
            "fs.edit" => Self::FsEdit,
            "fs.list" => Self::FsList,
            "fs.glob" => Self::FsGlob,
            "fs.grep" => Self::FsGrep,
            "bash.run" => Self::BashRun,
            "multi_edit" => Self::MultiEdit,
            "git.status" => Self::GitStatus,
            "git.diff" => Self::GitDiff,
            "git.show" => Self::GitShow,
            "web.fetch" => Self::WebFetch,
            "web.search" => Self::WebSearch,
            "notebook.read" => Self::NotebookRead,
            "notebook.edit" => Self::NotebookEdit,
            "index.query" => Self::IndexQuery,
            "patch.stage" => Self::PatchStage,
            "patch.apply" => Self::PatchApply,
            "patch.direct" => Self::PatchDirect,
            "diagnostics.check" => Self::DiagnosticsCheck,
            "extended_thinking" | "think_deeply" => Self::ExtendedThinking,
            "tool_search" => Self::ToolSearch,
            "user_question" => Self::UserQuestion,
            "task_create" => Self::TaskCreate,
            "task_update" => Self::TaskUpdate,
            "todo_read" => Self::TodoRead,
            "todo_write" => Self::TodoWrite,
            "task_get" => Self::TaskGet,
            "task_list" => Self::TaskList,
            "spawn_task" => Self::SpawnTask,
            "send_message" => Self::SendMessage,
            "task_output" => Self::TaskOutput,
            "task_stop" => Self::TaskStop,
            "enter_plan_mode" => Self::EnterPlanMode,
            "exit_plan_mode" => Self::ExitPlanMode,
            "enter_worktree" => Self::EnterWorktree,
            "exit_worktree" => Self::ExitWorktree,
            "skill" => Self::Skill,
            "kill_shell" => Self::KillShell,
            "batch" => Self::Batch,
            "lsp.hover" | "lsp_hover" => Self::LspHover,
            "lsp.definition" | "lsp_definition" => Self::LspDefinition,
            "lsp.references" | "lsp_references" => Self::LspReferences,
            "lsp.symbols" | "lsp_symbols" => Self::LspSymbols,
            "github.create_pr" | "github_create_pr" => Self::GithubCreatePr,
            "github.list_issues" | "github_list_issues" => Self::GithubListIssues,
            "github.view_pr" | "github_view_pr" => Self::GithubViewPr,
            _ => return None,
        })
    }

    /// Dotted internal name (e.g. `"fs.read"`, `"bash.run"`).
    #[must_use]
    pub fn as_internal(&self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::FsEdit => "fs.edit",
            Self::FsList => "fs.list",
            Self::FsGlob => "fs.glob",
            Self::FsGrep => "fs.grep",
            Self::BashRun => "bash.run",
            Self::MultiEdit => "multi_edit",
            Self::GitStatus => "git.status",
            Self::GitDiff => "git.diff",
            Self::GitShow => "git.show",
            Self::WebFetch => "web.fetch",
            Self::WebSearch => "web.search",
            Self::NotebookRead => "notebook.read",
            Self::NotebookEdit => "notebook.edit",
            Self::IndexQuery => "index.query",
            Self::PatchStage => "patch.stage",
            Self::PatchApply => "patch.apply",
            Self::PatchDirect => "patch.direct",
            Self::DiagnosticsCheck => "diagnostics.check",
            Self::ExtendedThinking => "extended_thinking",
            Self::ToolSearch => "tool_search",
            Self::UserQuestion => "user_question",
            Self::TaskCreate => "task_create",
            Self::TaskUpdate => "task_update",
            Self::TodoRead => "todo_read",
            Self::TodoWrite => "todo_write",
            Self::TaskGet => "task_get",
            Self::TaskList => "task_list",
            Self::SpawnTask => "spawn_task",
            Self::SendMessage => "send_message",
            Self::TaskOutput => "task_output",
            Self::TaskStop => "task_stop",
            Self::EnterPlanMode => "enter_plan_mode",
            Self::ExitPlanMode => "exit_plan_mode",
            Self::EnterWorktree => "enter_worktree",
            Self::ExitWorktree => "exit_worktree",
            Self::Skill => "skill",
            Self::KillShell => "kill_shell",
            Self::Batch => "batch",
            Self::LspHover => "lsp.hover",
            Self::LspDefinition => "lsp.definition",
            Self::LspReferences => "lsp.references",
            Self::LspSymbols => "lsp.symbols",
            Self::GithubCreatePr => "github.create_pr",
            Self::GithubListIssues => "github.list_issues",
            Self::GithubViewPr => "github.view_pr",
        }
    }

    /// Underscored API name (e.g. `"fs_read"`, `"bash_run"`).
    #[must_use]
    pub fn as_api_name(&self) -> &'static str {
        match self {
            Self::FsRead => "fs_read",
            Self::FsWrite => "fs_write",
            Self::FsEdit => "fs_edit",
            Self::FsList => "fs_list",
            Self::FsGlob => "fs_glob",
            Self::FsGrep => "fs_grep",
            Self::BashRun => "bash_run",
            Self::MultiEdit => "multi_edit",
            Self::GitStatus => "git_status",
            Self::GitDiff => "git_diff",
            Self::GitShow => "git_show",
            Self::WebFetch => "web_fetch",
            Self::WebSearch => "web_search",
            Self::NotebookRead => "notebook_read",
            Self::NotebookEdit => "notebook_edit",
            Self::IndexQuery => "index_query",
            Self::PatchStage => "patch_stage",
            Self::PatchApply => "patch_apply",
            Self::PatchDirect => "patch_direct",
            Self::DiagnosticsCheck => "diagnostics_check",
            Self::ExtendedThinking => "extended_thinking",
            Self::ToolSearch => "tool_search",
            Self::UserQuestion => "user_question",
            Self::TaskCreate => "task_create",
            Self::TaskUpdate => "task_update",
            Self::TodoRead => "todo_read",
            Self::TodoWrite => "todo_write",
            Self::TaskGet => "task_get",
            Self::TaskList => "task_list",
            Self::SpawnTask => "spawn_task",
            Self::SendMessage => "send_message",
            Self::TaskOutput => "task_output",
            Self::TaskStop => "task_stop",
            Self::EnterPlanMode => "enter_plan_mode",
            Self::ExitPlanMode => "exit_plan_mode",
            Self::EnterWorktree => "enter_worktree",
            Self::ExitWorktree => "exit_worktree",
            Self::Skill => "skill",
            Self::KillShell => "kill_shell",
            Self::Batch => "batch",
            Self::LspHover => "lsp_hover",
            Self::LspDefinition => "lsp_definition",
            Self::LspReferences => "lsp_references",
            Self::LspSymbols => "lsp_symbols",
            Self::GithubCreatePr => "github_create_pr",
            Self::GithubListIssues => "github_list_issues",
            Self::GithubViewPr => "github_view_pr",
        }
    }

    /// Whether this tool is read-only (allowed in plan/explore mode).
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.metadata().read_only
    }

    /// Whether this tool is handled by AgentEngine, not LocalToolHost.
    #[must_use]
    pub fn is_agent_level(&self) -> bool {
        self.metadata().agent_level
    }

    /// Whether this tool is blocked during review mode.
    #[must_use]
    pub fn is_review_blocked(&self) -> bool {
        self.metadata().review_blocked
    }

    /// All built-in tool name variants.
    pub const ALL: &'static [ToolName] = &[
        Self::FsRead,
        Self::FsWrite,
        Self::FsEdit,
        Self::FsList,
        Self::FsGlob,
        Self::FsGrep,
        Self::BashRun,
        Self::MultiEdit,
        Self::GitStatus,
        Self::GitDiff,
        Self::GitShow,
        Self::WebFetch,
        Self::WebSearch,
        Self::NotebookRead,
        Self::NotebookEdit,
        Self::IndexQuery,
        Self::PatchStage,
        Self::PatchApply,
        Self::PatchDirect,
        Self::DiagnosticsCheck,
        Self::ExtendedThinking,
        Self::ToolSearch,
        Self::UserQuestion,
        Self::TaskCreate,
        Self::TaskUpdate,
        Self::TodoRead,
        Self::TodoWrite,
        Self::TaskGet,
        Self::TaskList,
        Self::SpawnTask,
        Self::SendMessage,
        Self::TaskOutput,
        Self::TaskStop,
        Self::EnterPlanMode,
        Self::ExitPlanMode,
        Self::EnterWorktree,
        Self::ExitWorktree,
        Self::Skill,
        Self::KillShell,
        Self::Batch,
        Self::LspHover,
        Self::LspDefinition,
        Self::LspReferences,
        Self::LspSymbols,
        Self::GithubCreatePr,
        Self::GithubListIssues,
        Self::GithubViewPr,
    ];
}

fn normalize_tool_name_key(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace(['-', '.'], "_")
}

/// Canonicalize a built-in tool name to its underscored API form.
///
/// Accepts either underscored API names (`fs_read`) or dotted internal names
/// (`fs.read`), plus common casing/hyphen variations. Returns `None` for
/// unknown names such as MCP or plugin tools.
#[must_use]
pub fn canonical_tool_api_name(name: &str) -> Option<&'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(tool) = ToolName::from_api_name(trimmed) {
        return Some(tool.as_api_name());
    }

    if let Some(tool) = ToolName::from_internal_name(trimmed) {
        return Some(tool.as_api_name());
    }

    let normalized = normalize_tool_name_key(trimmed);
    ToolName::from_api_name(&normalized).map(|tool| tool.as_api_name())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
    pub requires_approval: bool,
}

impl ToolCall {
    /// Parse the internal tool name into a typed `ToolName`.
    /// Returns `None` for plugin/MCP tools or unknown names.
    #[must_use]
    pub fn tool_name(&self) -> Option<ToolName> {
        ToolName::from_internal_name(&self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolProposal {
    pub invocation_id: Uuid,
    pub call: ToolCall,
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ToolPermission {
    Deny {
        source: &'static str,
        reason: String,
        target: Option<String>,
    },
    Ask {
        source: &'static str,
        reason: String,
        fresh: bool,
    },
    Allow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovedToolCall {
    pub invocation_id: Uuid,
    pub call: ToolCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub invocation_id: Uuid,
    pub success: bool,
    pub output: serde_json::Value,
}

#[derive(Clone, Copy, Default)]
pub struct ToolExecutionContext<'a> {
    pub prompt_id: Option<Uuid>,
    pub parent_run_id: Option<Uuid>,
    pub deadline: Option<std::time::Instant>,
    pub session_id: Option<Uuid>,
    pub invocation_id: Option<Uuid>,
    pub cancellation: Option<&'a CancellationToken>,
}

impl ToolExecutionContext<'_> {
    pub fn execution(&self) -> execution::ExecutionContext {
        execution::ExecutionContext {
            session_id: self.session_id,
            prompt_id: self.prompt_id,
            invocation_id: self.invocation_id,
            parent_run_id: self.parent_run_id,
            deadline: self.deadline,
            cancellation: self.cancellation.cloned().unwrap_or_default(),
        }
    }
}

/// Executes tool calls. `execute*` and `verify_command` are async: an
/// implementation that blocks (subprocesses, file I/O) must wrap that work
/// in `runtime::blocking_section` or `runtime::spawn_blocking`.
#[async_trait::async_trait]
pub trait ToolHost {
    fn scoped_workspace(
        &self,
        _workspace: &std::path::Path,
    ) -> anyhow::Result<std::sync::Arc<dyn ToolHost + Send + Sync>> {
        anyhow::bail!("This tool transport cannot change execution workspace")
    }
    async fn verify_command(
        &self,
        _command: &str,
        _cwd: &Path,
        _timeout: u64,
        _context: ToolExecutionContext<'_>,
    ) -> serde_json::Value {
        serde_json::json!({"success":false,"status":"unavailable"})
    }

    fn approval_details(&self, _invocation: Uuid) -> Option<String> {
        None
    }

    fn discard_proposal(&self, _invocation: Uuid) {}

    fn propose_with_permission(&self, call: ToolCall) -> (ToolProposal, ToolPermission) {
        let permission = self.permission(&call);
        (self.propose(call), permission)
    }

    fn permission(&self, call: &ToolCall) -> ToolPermission {
        match self.validate(call) {
            Ok(()) => ToolPermission::Allow,
            Err(error) => ToolPermission::Deny {
                source: "tool host",
                reason: error.to_string(),
                target: None,
            },
        }
    }

    /// Validate execution preconditions before cached or live results can be returned.
    fn validate(&self, _call: &ToolCall) -> Result<()> {
        Ok(())
    }

    /// Include host-specific execution context in a read cache key.
    fn cache_key(&self, call: &ToolCall) -> serde_json::Value {
        call.args.clone()
    }

    fn propose(&self, call: ToolCall) -> ToolProposal;
    async fn execute(&self, approved: ApprovedToolCall) -> ToolResult;

    async fn execute_with_context(
        &self,
        approved: ApprovedToolCall,
        _context: ToolExecutionContext<'_>,
    ) -> ToolResult {
        self.execute(approved).await
    }
}

/// Blocking wrappers around [`ToolHost`] for synchronous callers outside a
/// turn (standalone commands and tests). Never call these from async code.
pub trait ToolHostBlockingExt: ToolHost + Sync {
    fn execute_blocking(&self, approved: ApprovedToolCall) -> ToolResult {
        runtime::block_on(self.execute(approved))
    }
    fn execute_with_context_blocking(
        &self,
        approved: ApprovedToolCall,
        context: ToolExecutionContext<'_>,
    ) -> ToolResult {
        runtime::block_on(self.execute_with_context(approved, context))
    }
    fn verify_command_blocking(
        &self,
        command: &str,
        cwd: &Path,
        timeout: u64,
        context: ToolExecutionContext<'_>,
    ) -> serde_json::Value {
        runtime::block_on(self.verify_command(command, cwd, timeout, context))
    }
}

impl<T: ToolHost + Sync + ?Sized> ToolHostBlockingExt for T {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageContent {
    pub mime: String,
    pub base64_data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub unit: LlmUnit,
    pub prompt: String,
    pub model: String,
    pub max_tokens: u32,
    #[serde(default)]
    pub non_urgent: bool,
    #[serde(default)]
    pub images: Vec<ImageContent>,
    #[serde(default)]
    pub provider_options: ProviderOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelPart {
    Text {
        text: String,
    },
    Call {
        id: Uuid,
        provider_call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
}

/// Provider-owned blocks are carried in memory and in the private replay store.
#[derive(Clone, Serialize, Deserialize)]
pub struct NativeParts {
    pub protocol: String,
    pub blocks: Vec<serde_json::Value>,
}

impl std::fmt::Debug for NativeParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeParts")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub enum TransportPart {
    Text(String),
    Call(String),
    Native(NativeParts),
}

fn default_finish_reason() -> String {
    "stop".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    #[serde(skip)]
    pub transport_parts: Vec<TransportPart>,
    pub text: String,
    #[serde(default = "default_finish_reason")]
    pub finish_reason: String,
    #[serde(default)]
    pub reasoning_content: String,
    #[serde(default)]
    pub tool_calls: Vec<LlmToolCall>,
    /// Token usage from the API response.
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    /// Provider compatibility transforms applied to this request/response path.
    #[serde(default)]
    pub compatibility: Option<AppliedCompatibility>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedCompatibility {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub transforms: Vec<String>,
    #[serde(default)]
    pub degraded_inputs: Vec<String>,
}

/// Token usage information from a DeepSeek API response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u64,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u64,
    /// Tokens used for chain-of-thought reasoning (thinking mode).
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// Execution phase for complex tasks.
/// Enforces disciplined Explore->Plan->Execute->Verify workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPhase {
    /// Read-only exploration of the codebase.
    Explore,
    /// Planning phase: model states its approach.
    Plan,
    /// Active implementation: all tools available.
    Execute,
    /// Post-implementation verification (tests, compilation).
    Verify,
}

impl TaskPhase {
    /// Display name for the phase.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskPhase::Explore => "explore",
            TaskPhase::Plan => "plan",
            TaskPhase::Execute => "execute",
            TaskPhase::Verify => "verify",
        }
    }
}

impl std::fmt::Display for TaskPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A question posed by the agent to the user via the `user_question` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestion {
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// Handler invoked when the agent asks the user a question.
/// Returns `Some(answer)` if the user provides an answer, or `None` on cancellation.
pub type UserQuestionHandler = std::sync::Arc<dyn Fn(UserQuestion) -> Option<String> + Send + Sync>;

// ── Chat-with-tools types (DeepSeek function calling) ──────────────────

/// Configuration for thinking mode on `deepseek-chat`.
///
/// When enabled, the model produces chain-of-thought reasoning before responding.
/// This replaces the old `deepseek-reasoner` model selection — `deepseek-chat`
/// with thinking enabled gives us reasoning + function calling simultaneously.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub thinking_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

impl ThinkingConfig {
    /// Enable thinking mode with a token budget for chain-of-thought.
    #[must_use]
    pub fn enabled(budget: u32) -> Self {
        Self {
            thinking_type: "enabled".to_string(),
            budget_tokens: Some(budget),
        }
    }

    /// Disable thinking mode.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            thinking_type: "disabled".to_string(),
            budget_tokens: None,
        }
    }
}

/// A message in a multi-turn conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum ChatMessage {
    #[serde(rename = "system")]
    System { content: String },
    #[serde(rename = "user")]
    User { content: String },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(skip)]
        native_parts: Option<NativeParts>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        /// Chain-of-thought reasoning from thinking mode (DeepSeek V3.2).
        /// Present when the model uses thinking and returns reasoning alongside
        /// tool calls or content. Per V3.2 docs: **keep** reasoning_content
        /// within a tool loop (same user question) so the model retains its
        /// logical thread, but **strip** it from prior conversation turns
        /// (previous user questions) to save bandwidth.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        tool_calls: Vec<LlmToolCall>,
    },
    #[serde(rename = "tool")]
    Tool {
        tool_call_id: String,
        content: String,
        /// Function name that produced this result (used by Gemini's functionResponse).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tool_name: Option<String>,
    },
}

/// Strip `reasoning_content` from all assistant messages that belong to
/// **prior** user-question turns, while keeping it for the **current** turn's
/// tool-call loop.
///
/// DeepSeek API lifecycle rules:
/// - Within a single user question's tool-call loop: **keep** reasoning_content
///   so the model retains its logical thread.
/// - When the next user question begins: **clear** reasoning_content from
///   earlier turns to save bandwidth and avoid API 400 errors.
///
/// Strategy: walk backward from the end; every assistant message *after* the
/// last User message belongs to the current turn (keep reasoning). Everything
/// before that last User message is a prior turn (strip reasoning).
pub fn strip_prior_reasoning_content(messages: &mut [ChatMessage]) {
    // Find the index of the last User message.
    let last_user_idx = messages
        .iter()
        .rposition(|m| matches!(m, ChatMessage::User { .. }));

    let boundary = match last_user_idx {
        Some(idx) => idx,
        None => return, // no user messages → nothing to strip
    };

    // Strip reasoning_content from all assistant messages before the boundary.
    for msg in &mut messages[..boundary] {
        if let ChatMessage::Assistant {
            reasoning_content, ..
        } = msg
        {
            *reasoning_content = None;
        }
    }
}

/// Rough token estimate: ~4 chars per token for English (conservative).
/// DeepSeek docs say ~0.3 tokens/char; we use 0.25 as safety margin.
pub fn estimate_message_tokens(messages: &[ChatMessage]) -> u64 {
    let total_chars: u64 = messages
        .iter()
        .map(|m| match m {
            ChatMessage::System { content } => content.len() as u64,
            ChatMessage::User { content } => content.len() as u64,
            ChatMessage::Assistant {
                content,
                reasoning_content,
                native_parts,
                tool_calls,
                ..
            } => {
                native_parts.as_ref().map_or(0, |native| {
                    serde_json::to_vec(&native.blocks).map_or(0, |bytes| bytes.len() as u64)
                }) + content.as_deref().map_or(0, |c| c.len() as u64)
                    + reasoning_content.as_deref().map_or(0, |r| r.len() as u64)
                    + tool_calls
                        .iter()
                        .map(|tc| tc.arguments.len() as u64)
                        .sum::<u64>()
            }
            ChatMessage::Tool { content, .. } => content.len() as u64,
        })
        .sum();
    total_chars / 4
}

/// A tool (function) definition sent to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

/// The function schema within a tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub strict: Option<bool>,
    pub parameters: serde_json::Value,
}

/// Attempt to repair a tool call name to the active tool surface.
///
/// Built-in tools are canonicalized to their underscored API names even if the
/// provider returns internal dotted names. For active tools, this also applies
/// lowercase/hyphen normalization and a small Levenshtein repair window.
#[must_use]
pub fn repair_tool_api_name(api_name: &str, available_tools: &[ToolDefinition]) -> Option<String> {
    let trimmed = api_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(canonical) = canonical_tool_api_name(trimmed) {
        return Some(canonical.to_string());
    }

    let tool_names: Vec<&str> = available_tools
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect();

    if tool_names.contains(&trimmed) {
        return Some(trimmed.to_string());
    }

    let normalized = normalize_tool_name_key(trimmed);
    if let Some(name) = tool_names
        .iter()
        .find(|name| normalize_tool_name_key(name) == normalized)
    {
        return Some((*name).to_string());
    }

    let mut best_match: Option<(&str, usize)> = None;
    for name in &tool_names {
        let dist = strsim::levenshtein(&normalized, &normalize_tool_name_key(name));
        if dist <= 2 && best_match.as_ref().is_none_or(|(_, best)| dist < *best) {
            best_match = Some((name, dist));
        }
    }

    best_match.map(|(name, _)| name.to_string())
}

/// Controls how the model picks tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    /// "none", "auto", or "required"
    Mode(String),
    /// Force a specific function.
    Function {
        #[serde(rename = "type")]
        choice_type: String,
        function: ToolChoiceFunction,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolChoiceFunction {
    pub name: String,
}

impl ToolChoice {
    pub fn auto() -> Self {
        Self::Mode("auto".to_string())
    }
    pub fn none() -> Self {
        Self::Mode("none".to_string())
    }
    /// Force the model to return at least one tool call.
    pub fn required() -> Self {
        Self::Mode("required".to_string())
    }
}

/// Request for the chat-with-tools API.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    /// Request log probabilities. Incompatible with thinking mode (API will error).
    pub logprobs: Option<bool>,
    /// Number of top log probabilities to return. Incompatible with thinking mode.
    pub top_logprobs: Option<u8>,
    /// When set, enables thinking mode on `deepseek-chat`.
    /// The API requires temperature/top_p/presence_penalty/frequency_penalty to be
    /// omitted and logprobs/top_logprobs to not be set when thinking is enabled.
    pub thinking: Option<ThinkingConfig>,
    /// Optional images to include with the user message (multimodal).
    pub images: Vec<ImageContent>,
    pub provider_options: ProviderOptions,
    /// Optional response format, e.g json_object
    pub response_format: Option<serde_json::Value>,
}

/// Request for the Beta Fill-In-the-Middle (FIM) Completion API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FimRequest {
    pub model: String,
    pub prompt: String,
    pub suffix: Option<String>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
}

/// Recognized build systems for workspace detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSystem {
    Cargo,
    NpmYarn,
    Go,
}

impl BuildSystem {
    /// Human-readable name shown in bootstrap context.
    pub fn name(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo",
            Self::NpmYarn => "npm/yarn",
            Self::Go => "Go",
        }
    }

    /// Collective noun for workspace members.
    pub fn member_noun(self) -> &'static str {
        match self {
            Self::Cargo => "crates",
            Self::NpmYarn => "packages",
            Self::Go => "modules",
        }
    }

    /// Manifest file name at the workspace root.
    pub fn manifest_name(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo.toml",
            Self::NpmYarn => "package.json",
            Self::Go => "go.work",
        }
    }
}

impl std::fmt::Display for BuildSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Detected workspace metadata from a project root manifest.
#[derive(Debug, Clone)]
pub struct WorkspaceMetadata {
    /// The detected build system.
    pub build_system: BuildSystem,
    /// List of workspace member paths/globs.
    pub members: Vec<String>,
}

/// Detect workspace metadata from a project root.
///
/// Tries, in order:
/// 1. `Cargo.toml` — `[workspace] members` array
/// 2. `package.json` — `"workspaces"` array
/// 3. `go.work` — `use` directives
///
/// Returns `None` if no workspace is detected.
pub fn detect_workspace(root: &Path) -> Option<WorkspaceMetadata> {
    // Try Cargo.toml
    if let Ok(content) = fs::read_to_string(root.join("Cargo.toml"))
        && let Ok(parsed) = content.parse::<toml::Table>()
        && let Some(members) = parsed
            .get("workspace")
            .and_then(|ws| ws.get("members"))
            .and_then(|m| m.as_array())
    {
        let items: Vec<String> = members
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if !items.is_empty() {
            return Some(WorkspaceMetadata {
                build_system: BuildSystem::Cargo,
                members: items,
            });
        }
    }

    // Try package.json
    if let Ok(content) = fs::read_to_string(root.join("package.json"))
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(workspaces) = parsed.get("workspaces").and_then(|w| w.as_array())
    {
        let items: Vec<String> = workspaces
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if !items.is_empty() {
            return Some(WorkspaceMetadata {
                build_system: BuildSystem::NpmYarn,
                members: items,
            });
        }
    }

    // Try go.work
    if let Ok(content) = fs::read_to_string(root.join("go.work")) {
        let mut modules = Vec::new();
        let mut in_use_block = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("use (") || trimmed == "use (" {
                in_use_block = true;
                continue;
            }
            if in_use_block && trimmed == ")" {
                in_use_block = false;
                continue;
            }
            if in_use_block && !trimmed.is_empty() {
                modules.push(trimmed.to_string());
            } else if trimmed.starts_with("use ") && !trimmed.contains('(') {
                let module = trimmed.trim_start_matches("use ").trim();
                if !module.is_empty() {
                    modules.push(module.to_string());
                }
            }
        }
        if !modules.is_empty() {
            return Some(WorkspaceMetadata {
                build_system: BuildSystem::Go,
                members: modules,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests;
