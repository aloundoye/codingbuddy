pub mod agent_profiles;
mod analysis;
pub mod apply;
pub mod context;
mod gather_context;
mod intent;
mod repo_map;
mod shared;
pub mod skills;
mod team;
pub mod tool_bridge;
pub mod tool_loop;
mod tool_surface;
mod verify;
pub mod watch;

pub use repo_map::clear_tag_cache;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use codingbuddy_core::{
    AppConfig, ChatMessage, ChatRequest, DynamicToolTrust, EventKind, RuntimeToolMetadata, Session,
    SessionBudgets, SessionState, StreamChunk, TaskPhase, ToolCall, ToolChoice,
    ToolPermissionMatcher, ToolResultSizePolicy, UserQuestionHandler,
};
use codingbuddy_hooks::HookRuntime;
use codingbuddy_llm::{ApiClient, LlmClient};
use codingbuddy_mcp::McpManager;
mod observe;
mod session_prompt;
mod turn;
use codingbuddy_policy::PolicyEngine;
use codingbuddy_store::Store;
use codingbuddy_subagent::SubagentTask;
use codingbuddy_tools::{
    LocalToolHost, detect_signals, plugin_tool_definitions, tiered_tool_definitions,
    tool_definitions, tool_search_definition,
};
use observe::Observer;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tool_surface::shape_tool_surface;
use uuid::Uuid;

/// Handler for spawn_task subagent workers. Retained for API compatibility,
/// but the core loop does not dispatch subagents in this architecture.
pub type SubagentWorkerFn = Arc<dyn Fn(&SubagentTask) -> Result<String> + Send + Sync>;
type ApprovalHandler = Box<dyn FnMut(&ToolCall) -> Result<bool> + Send>;

fn install_mcp_executor(host: &mut LocalToolHost, manager: Arc<McpManager>) {
    host.set_context_mcp_executor(Arc::new(move |alias, args, context| {
        manager.call_tool(alias, args, context)
    }));
}

fn runtime_tool_definitions(
    workspace: &Path,
    trusted: bool,
) -> Vec<codingbuddy_core::ToolDefinition> {
    let mut tools = tool_definitions();
    if !trusted {
        return tools;
    }
    tools.extend(plugin_tool_definitions(workspace));
    tools.extend(codingbuddy_tools::custom_tools::custom_tool_definitions(
        workspace,
    ));
    tools
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatMode {
    Ask, // Read-only, tool-use loop (read_only=true)
    #[default]
    Code, // Default: tool-use loop (full capability)
    Context, // Read-only, tool-use loop (read_only=true)
}

#[derive(Debug, Default, Clone)]
pub struct ChatOptions {
    pub tools: bool,
    /// When true in analysis path, uses max_think model.
    pub force_max_think: bool,
    /// Replace default analysis system prompt.
    pub system_prompt_override: Option<String>,
    /// Append to default analysis system prompt.
    pub system_prompt_append: Option<String>,
    /// Additional directories for repository context lookup.
    pub additional_dirs: Vec<PathBuf>,
    /// Optional repository root override for context bootstrap/analysis.
    pub repo_root_override: Option<PathBuf>,
    /// Log expanded context blocks to stderr before sending to LLM.
    pub debug_context: bool,
    pub mode: ChatMode,
    pub teammate_mode: Option<String>,
    pub disable_team_orchestration: bool,
    /// When true, attempts to parse and fetch web URLs from prompt.
    pub detect_urls: bool,
    /// Watch mode hint (CLI handles filesystem watch orchestration).
    pub watch_files: bool,
    /// Images to include with the next LLM request (multimodal paste).
    pub images: Vec<codingbuddy_core::ImageContent>,
    /// Full context history (transcript) of the active session.
    pub chat_history: Vec<ChatMessage>,
    /// Optional explicit session target, used for child-session delegation.
    pub session_id: Option<Uuid>,
    pub prompt_id: Option<Uuid>,
    /// Manual profile override from `/agent <name>`. When set, overrides auto-selection.
    pub profile_override: Option<String>,
    /// Optional `provider/model` selector applied to this turn without mutating persisted config.
    pub model_spec: Option<String>,
    /// Effective connection for this turn, including session-only credentials.
    pub llm_config: Option<codingbuddy_core::LlmConfig>,
    pub cancellation: Option<codingbuddy_core::CancellationToken>,
    pub deadline: Option<std::time::Instant>,
    pub parent_run_id: Option<Uuid>,
    pub session_lease: Option<Arc<codingbuddy_store::SessionLease>>,
}

pub struct AgentEngine {
    pub(crate) workspace: PathBuf,
    store: Store,
    pub(crate) llm: Box<dyn LlmClient + Send + Sync>,
    observer: Observer,
    pub(crate) tool_host: Arc<LocalToolHost>,
    pub(crate) policy: PolicyEngine,
    pub(crate) cfg: AppConfig,
    stream_callback: Mutex<Option<codingbuddy_core::StreamCallback>>,
    max_turns: Option<u64>,
    max_budget_usd: Option<f64>,
    approval_handler: Mutex<Option<Arc<Mutex<ApprovalHandler>>>>,
    user_question_handler: Mutex<Option<UserQuestionHandler>>,
    subagent_worker: Mutex<Option<SubagentWorkerFn>>,
    pub(crate) hooks: HookRuntime,
    mcp: Option<Arc<McpManager>>,
    /// Watches settings files for hot-reload. The background thread keeps the watch alive;
    /// poll via `config_watcher.as_ref().and_then(|w| w.poll_change())`.
    config_watcher: Option<codingbuddy_core::config_watcher::ConfigWatcher>,
    /// Whether the Setup hook has already fired this session (fires once).
    setup_fired: std::sync::atomic::AtomicBool,
}

impl AgentEngine {
    pub fn new(workspace: &Path) -> Result<Self> {
        Self::new_with_config(
            workspace,
            AppConfig::ensure(workspace).context("failed to load config")?,
        )
    }

    pub fn new_with_config(workspace: &Path, cfg: AppConfig) -> Result<Self> {
        let mut prof = codingbuddy_core::profiler::StartupProfiler::new();

        prof.mark("config_load");

        let llm = Box::new(ApiClient::new(cfg.llm.clone()).context("failed to create LLM client")?);
        prof.mark("llm_client");

        let result = Self::new_with_config_and_llm(workspace, cfg, llm);
        prof.mark("engine_init");
        prof.finish();

        result
    }

    pub fn new_with_llm(workspace: &Path, llm: Box<dyn LlmClient + Send + Sync>) -> Result<Self> {
        let cfg = AppConfig::ensure(workspace)?;
        Self::new_with_config_and_llm(workspace, cfg, llm)
    }

    pub fn new_with_config_and_llm(
        workspace: &Path,
        cfg: AppConfig,
        llm: Box<dyn LlmClient + Send + Sync>,
    ) -> Result<Self> {
        Self::new_with_execution_store(workspace, cfg, llm, Store::new(workspace)?)
    }

    /// Execute in a scoped worktree while retaining the parent's durable session journal.
    /// The caller passes the already-resolved parent policy and provider configuration.
    pub fn new_isolated(
        workspace: &Path,
        journal_workspace: &Path,
        cfg: AppConfig,
    ) -> Result<Self> {
        let llm = Box::new(ApiClient::new(cfg.llm.clone())?);
        Self::new_with_execution_store(workspace, cfg, llm, Store::new(journal_workspace)?)
    }

    fn new_with_execution_store(
        workspace: &Path,
        mut cfg: AppConfig,
        llm: Box<dyn LlmClient + Send + Sync>,
        store: Store,
    ) -> Result<Self> {
        let trusted = cfg
            .workspace_trust
            .unwrap_or_else(|| codingbuddy_core::workspace_is_trusted(workspace));
        cfg.workspace_trust = Some(trusted);
        // Apply managed settings overrides (enterprise/team constraints)
        if let Some(managed) = codingbuddy_policy::load_managed_settings() {
            managed.apply_to_config(&mut cfg);
        }

        // Spawn MCP discovery after Store init to avoid SQLite migration races.
        let mcp_workspace = workspace.to_path_buf();
        let mcp_handle =
            std::thread::spawn(move || McpManager::with_trust(&mcp_workspace, trusted).ok());

        let observer = Observer::new(workspace, &cfg.telemetry)?;
        let mut policy = PolicyEngine::from_app_config(&cfg.policy);

        if let Some(bash_approvals) = saved_bash_approvals(&store, workspace) {
            policy.set_persistent_bash_approvals(bash_approvals);
        }

        let mut tool_host = LocalToolHost::with_config(
            workspace,
            policy.clone(),
            Arc::new(codingbuddy_tools::PtyShellRunner),
            &cfg,
        )?;

        tool_host.set_journal_store(store.clone());
        let mcp = mcp_handle.join().ok().flatten().map(Arc::new);
        if let Some(manager) = &mcp {
            install_mcp_executor(&mut tool_host, manager.clone());
        }

        let tool_host = Arc::new(tool_host);

        if cfg.cleanup_period_days > 0
            && let Ok(mem) = codingbuddy_memory::MemoryManager::new(workspace)
        {
            let _ = mem.cleanup_old_checkpoints(cfg.cleanup_period_days);
        }

        let hooks = HookRuntime::from_app_config(workspace, &cfg);

        Ok(Self {
            workspace: workspace.to_path_buf(),
            store,
            llm,
            observer,
            tool_host,
            policy,
            cfg,
            stream_callback: Mutex::new(None),
            max_turns: None,
            max_budget_usd: None,
            approval_handler: Mutex::new(None),
            user_question_handler: Mutex::new(None),
            subagent_worker: Mutex::new(None),
            hooks,
            mcp,
            config_watcher: codingbuddy_core::config_watcher::watch_settings(workspace).ok(),
            setup_fired: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn set_max_turns(&mut self, max: Option<u64>) {
        self.max_turns = max;
    }

    pub fn set_max_budget_usd(&mut self, max: Option<f64>) {
        self.max_budget_usd = max;
    }

    /// Enable lint auto-fix loop with a single command (convenience for --lint-cmd).
    pub fn set_lint_command(&mut self, lang: &str, cmd: &str) {
        self.cfg.agent_loop.lint.enabled = true;
        self.cfg
            .agent_loop
            .lint
            .commands
            .insert(lang.to_string(), cmd.to_string());
    }

    /// Enable lint auto-fix loop (convenience for --auto-lint).
    pub fn enable_lint(&mut self) {
        self.cfg.agent_loop.lint.enabled = true;
    }

    /// Apply a `provider/model-id` spec (e.g. `"anthropic/claude-sonnet-4-6-20250514"`).
    pub fn apply_model_spec(&mut self, spec: &str) {
        self.cfg.llm.apply_model_spec(spec);
        if let Ok(client) = ApiClient::new(self.cfg.llm.clone()) {
            self.llm = Box::new(client);
        }
    }

    fn effective_config(&self, options: &ChatOptions) -> AppConfig {
        let mut cfg = self.cfg.clone();
        if let Some(llm) = &options.llm_config {
            cfg.llm = llm.clone();
        } else if let Some(spec) = options.model_spec.as_deref() {
            cfg.llm.apply_model_spec(spec);
        }
        cfg
    }

    fn routed_llm_for_options(
        &self,
        cfg: &AppConfig,
        options: &ChatOptions,
    ) -> Result<Option<Box<dyn LlmClient + Send + Sync>>> {
        if options.llm_config.is_some() || options.model_spec.is_some() {
            let mut client = ApiClient::new(cfg.llm.clone())?;
            if let Some(token) = &options.cancellation {
                client.set_cancel_token(token.clone());
            }
            Ok(Some(Box::new(client)))
        } else {
            Ok(options
                .cancellation
                .clone()
                .and_then(|token| self.llm.with_cancellation(token)))
        }
    }

    pub fn validate_api_key(&self) -> Result<()> {
        let provider = self.cfg.llm.active_provider();
        let model = self.cfg.llm.active_base_model();
        let test_request = ChatRequest {
            model: model.clone(),
            messages: vec![ChatMessage::User {
                content: "hi".to_string(),
            }],
            tools: vec![],
            tool_choice: ToolChoice::none(),
            max_tokens: 1,
            temperature: Some(0.0),
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: None,
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };

        match self.llm.complete_chat(&test_request) {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string().to_ascii_lowercase();
                if err_str.contains("401")
                    || err_str.contains("unauthorized")
                    || err_str.contains("invalid api key")
                    || err_str.contains("authentication")
                {
                    Err(anyhow!(
                        "Invalid or missing API key.\n\nSet your API key using one of:\n• export {}=your-key-here\n• Add \"api_key\" to ~/.codingbuddy/settings.json under the \"llm\" section",
                        provider.api_key_env
                    ))
                } else {
                    self.observer.warn_log(&format!(
                        "API key validation got non-auth error (startup continues): {e}"
                    ));
                    Ok(())
                }
            }
        }
    }

    pub fn set_permission_mode(&mut self, mode: &str) {
        self.policy
            .set_permission_mode(codingbuddy_policy::PermissionMode::from_str_lossy(mode));
        match LocalToolHost::with_config(
            &self.workspace,
            self.policy.clone(),
            Arc::new(codingbuddy_tools::PtyShellRunner),
            &self.cfg,
        ) {
            Ok(mut host) => {
                host.set_journal_store(self.store.clone());
                if let Some(manager) = &self.mcp {
                    install_mcp_executor(&mut host, manager.clone());
                }
                self.tool_host = Arc::new(host);
            }
            Err(e) => {
                self.observer.warn_log(&format!(
                    "failed to rebuild tool host for permission mode `{mode}`: {e}"
                ));
            }
        }
    }

    pub fn set_verbose(&mut self, verbose: bool) {
        self.observer.set_verbose(verbose);
    }

    /// A callback that re-reads this project's saved "always allow"
    /// approvals into the live policy. Front-ends call it after the user
    /// saves one, so it applies to the next call in the same session.
    pub fn approval_reloader(&self) -> impl Fn() + Send + Sync + 'static {
        let store = self.store.clone();
        let policy = self.policy.clone();
        let workspace = self.workspace.clone();
        move || {
            if let Some(approvals) = saved_bash_approvals(&store, &workspace) {
                policy.replace_bash_approvals(approvals);
            }
        }
    }

    pub fn set_approval_handler(&self, handler: ApprovalHandler) {
        match self.approval_handler.lock() {
            Ok(mut guard) => {
                *guard = Some(Arc::new(Mutex::new(handler)));
            }
            Err(_) => eprintln!("[agent] approval handler mutex poisoned; handler not installed"),
        }
    }

    pub fn set_stream_callback(&self, cb: codingbuddy_core::StreamCallback) {
        match self.stream_callback.lock() {
            Ok(mut guard) => {
                *guard = Some(cb);
            }
            Err(_) => eprintln!("[agent] stream callback mutex poisoned; callback not installed"),
        }
    }

    fn emit_stream_chunk_best_effort(&self, chunk: StreamChunk) {
        match self.stream_callback.lock() {
            Ok(guard) => {
                if let Some(ref cb) = *guard {
                    cb(chunk);
                }
            }
            Err(_) => eprintln!("[agent] stream callback mutex poisoned; chunk not emitted"),
        }
    }

    pub fn set_user_question_handler(&self, handler: UserQuestionHandler) {
        match self.user_question_handler.lock() {
            Ok(mut guard) => {
                *guard = Some(handler);
            }
            Err(_) => {
                eprintln!("[agent] user question handler mutex poisoned; handler not installed")
            }
        }
    }

    pub fn set_subagent_worker(&self, worker: SubagentWorkerFn) {
        match self.subagent_worker.lock() {
            Ok(mut guard) => {
                *guard = Some(worker);
            }
            Err(_) => eprintln!("[agent] subagent worker mutex poisoned; worker not installed"),
        }
    }

    pub fn subagent_worker(&self) -> Option<SubagentWorkerFn> {
        self.subagent_worker.lock().ok().and_then(|g| g.clone())
    }

    /// Discover available models from the active provider's API.
    pub fn discover_models(&self) -> Vec<String> {
        self.llm.list_models()
    }

    fn active_plan_prompt_addendum(&self, session: &Session) -> Result<Option<String>> {
        if !matches!(
            session.status,
            SessionState::ExecutingStep | SessionState::Verifying
        ) {
            return Ok(None);
        }
        let Some(plan_id) = session.active_plan_id else {
            return Ok(None);
        };
        let Some(plan) = self.store.load_plan(plan_id)? else {
            return Ok(None);
        };

        let mut lines = vec![
            "## Active Approved Plan".to_string(),
            format!("Goal: {}", plan.goal),
        ];
        if !plan.steps.is_empty() {
            lines.push("Execution steps:".to_string());
            for (idx, step) in plan.steps.iter().enumerate().take(8) {
                let file_suffix = if step.files.is_empty() {
                    String::new()
                } else {
                    format!(" [files: {}]", step.files.join(", "))
                };
                lines.push(format!(
                    "{}. {} - {}{}",
                    idx + 1,
                    step.title,
                    step.intent,
                    file_suffix
                ));
            }
        }
        if !plan.verification.is_empty() {
            lines.push("Verification goals:".to_string());
            for item in plan.verification.iter().take(6) {
                lines.push(format!("- {item}"));
            }
        }
        if !plan.risk_notes.is_empty() {
            lines.push("Risk notes:".to_string());
            for note in plan.risk_notes.iter().take(4) {
                lines.push(format!("- {note}"));
            }
        }
        lines.push(
            "Treat this approved plan as the execution baseline and verify changes against it."
                .to_string(),
        );
        Ok(Some(lines.join("\n")))
    }

    /// Build a subagent worker closure from the installed worker, if any.
    ///
    /// The CLI installs a worker that creates child sessions and runs delegated
    /// tasks in isolated engine scopes. When no worker is installed, spawn_task
    /// remains unavailable in that context.
    fn build_subagent_worker(&self) -> Option<tool_loop::SubagentWorker> {
        // Check if user provided an explicit worker
        if let Some(worker) = self.subagent_worker() {
            // Adapt the SubagentWorkerFn to the SubagentWorker signature
            return Some(Arc::new(move |req: tool_loop::SubagentRequest| {
                let role = match req.subagent_type.as_str() {
                    "explore" => codingbuddy_subagent::SubagentRole::Explore,
                    "plan" => codingbuddy_subagent::SubagentRole::Plan,
                    "bash" => codingbuddy_subagent::SubagentRole::Bash,
                    _ => codingbuddy_subagent::SubagentRole::Task,
                };
                let task = SubagentTask {
                    execution: req.execution.child(req.run_id),
                    run_id: req.run_id,
                    name: req.task_name.clone(),
                    goal: req.prompt.clone(),
                    role,
                    team: String::new(),
                    task_id: req.task_id,
                    parent_session_id: req.parent_session_id,
                    child_session_id: req.child_session_id,
                    model_override: req.model_override.clone(),
                    max_turns: req.max_turns,
                    run_in_background: req.run_in_background,
                    read_only_fallback: false,
                    custom_agent: None,
                };
                worker(&task)
            }));
        }
        None
    }

    fn ensure_session_record_for(&self, session_id: Option<Uuid>) -> Result<Session> {
        if let Some(session_id) = session_id {
            return self
                .store
                .load_session(session_id)?
                .ok_or_else(|| anyhow!("session not found: {session_id}"));
        }

        if let Some(existing) = self.store.load_latest_session()? {
            return Ok(existing);
        }

        let session = Session {
            session_id: uuid::Uuid::now_v7(),
            workspace_root: self.workspace.to_string_lossy().to_string(),
            baseline_commit: None,
            status: SessionState::Idle,
            budgets: SessionBudgets {
                per_turn_seconds: self.cfg.budgets.max_turn_duration_secs,
                max_think_tokens: self.cfg.budgets.max_reasoner_tokens_per_session as u32,
            },
            active_plan_id: None,
        };
        self.store.save_session(&session)?;
        Ok(session)
    }

    /// Build a skill runner callback that wraps SkillManager.
    fn build_skill_runner(&self) -> Option<tool_loop::SkillRunner> {
        let workspace = self.workspace.clone();
        let skill_paths: Vec<String> = self.cfg.skills.paths.to_vec();

        Some(Arc::new(move |skill_name: &str, args: Option<&str>| {
            let manager = crate::skills::SkillManager::new(&workspace)?;
            let output = manager.run(skill_name, args, &skill_paths);
            match output {
                Ok(run_output) => Ok(Some(tool_loop::SkillInvocationResult {
                    rendered_prompt: run_output.rendered_prompt,
                    forked: run_output.forked,
                    allowed_tools: run_output.allowed_tools,
                    disallowed_tools: run_output.disallowed_tools,
                    disable_model_invocation: run_output.disable_model_invocation,
                    model_override: run_output.model,
                    effort: run_output.effort,
                })),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("not found") {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                }
            }
        }))
    }

    /// Mutable access to the configuration for test overrides.
    pub fn delegation_config(&self) -> AppConfig {
        let mut cfg = self.cfg.clone();
        if matches!(
            self.policy.permission_mode(),
            codingbuddy_policy::PermissionMode::Plan
                | codingbuddy_policy::PermissionMode::Locked
                | codingbuddy_policy::PermissionMode::DontAsk
        ) {
            cfg.policy.permission_mode = codingbuddy_core::PermissionMode::Locked;
        }
        cfg.budgets.max_turns = self
            .max_turns
            .or(cfg.budgets.max_turns)
            .map(|limit| limit.min(cfg.agent_loop.tool_loop_max_turns));
        cfg.budgets.max_budget_usd = self.max_budget_usd.or(cfg.budgets.max_budget_usd);
        cfg
    }
    pub fn cfg(&self) -> &AppConfig {
        &self.cfg
    }

    pub fn cfg_mut(&mut self) -> &mut codingbuddy_core::AppConfig {
        &mut self.cfg
    }

    /// Fire PreToolUse hooks. Returns the aggregate result.
    /// Callers should check `result.blocked` to decide whether to skip tool execution.
    pub fn fire_pre_tool_use(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> codingbuddy_hooks::HookResult {
        let ws = self.workspace.display().to_string();
        let input =
            codingbuddy_hooks::HookInput::new(codingbuddy_hooks::HookEvent::PreToolUse, &ws)
                .with_tool(tool_name, tool_input.clone());
        self.hooks
            .fire(codingbuddy_hooks::HookEvent::PreToolUse, &input)
    }

    /// Fire Stop hooks at end of agent execution.
    pub fn fire_stop(&self) {
        let ws = self.workspace.display().to_string();
        let input = codingbuddy_hooks::HookInput::new(codingbuddy_hooks::HookEvent::Stop, &ws);
        let _ = self.hooks.fire(codingbuddy_hooks::HookEvent::Stop, &input);
    }

    pub(crate) fn stream(&self, chunk: StreamChunk) {
        // Clone-then-call: release the mutex before invoking the callback
        // so that the callback can safely call back into AgentEngine without
        // deadlocking on stream_callback's mutex.
        let cb = self.stream_callback.lock().ok().and_then(|g| g.clone());
        if let Some(cb) = cb {
            cb(chunk);
        }
    }

    pub fn chat(&self, prompt: &str) -> Result<String> {
        self.chat_with_options(
            prompt,
            ChatOptions {
                tools: true,
                ..Default::default()
            },
        )
    }

    pub fn analyze_with_options(&self, prompt: &str, options: ChatOptions) -> Result<String> {
        let result = self.analyze_prompt(prompt, options)?;
        self.stream(StreamChunk::Done { reason: None });
        Ok(result)
    }

    fn analyze_prompt(&self, prompt: &str, options: ChatOptions) -> Result<String> {
        let cfg = self.effective_config(&options);
        let routed_llm = self.routed_llm_for_options(&cfg, &options)?;
        let llm = match routed_llm.as_ref() {
            Some(client) => client.as_ref(),
            None => self.llm.as_ref(),
        };
        // Grab a clone of the stream callback (if any) to pass to analyze().
        // complete_chat_streaming will fire ContentDelta per-token in real-time.
        let stream_cb = self.prompt_stream_callback(options.prompt_id);

        let response = analysis::analyze(
            llm,
            &cfg,
            &self.workspace,
            prompt,
            &options,
            stream_cb.clone(),
        )?;

        if let Some(compatibility) = response.compatibility.as_ref()
            && (!compatibility.transforms.is_empty() || !compatibility.degraded_inputs.is_empty())
        {
            self.append_event_best_effort_for_session(
                options.session_id,
                EventKind::ProviderCompatibilityApplied {
                    model: if options.force_max_think {
                        cfg.llm.active_reasoner_model()
                    } else {
                        cfg.llm.active_base_model()
                    },
                    provider: compatibility.provider.clone(),
                    transforms: compatibility.transforms.clone(),
                    degraded_inputs: compatibility.degraded_inputs.clone(),
                },
            );
        }

        if let (Some(session), Some(prompt)) = (options.session_id, options.prompt_id) {
            self.store.model_step(
                session,
                Some(prompt),
                Some(response.text.clone()),
                &[],
                &response.transport_parts,
            )?;
        }
        Ok(response.text)
    }

    pub fn context_debug_preview(&self, prompt: &str, options: &ChatOptions) -> String {
        let task_intent = intent::classify_intent(&intent::IntentInput {
            prompt,
            mode: options.mode,
            tools: options.tools,
        });
        let intent_label = match task_intent {
            intent::TaskIntent::InspectRepo => "InspectRepo",
            intent::TaskIntent::EditCode => "EditCode",
        };
        let bootstrap = gather_context::gather_for_prompt(
            &self.workspace,
            &self.cfg,
            prompt,
            options.mode,
            &options.additional_dirs,
            options.repo_root_override.as_deref(),
        );
        bootstrap.debug_digest(intent_label, options.mode)
    }

    pub(crate) fn append_event_best_effort(&self, kind: EventKind) {
        self.append_event_best_effort_for_session(None, kind);
    }

    pub(crate) fn append_event_best_effort_for_session(
        &self,
        session_id: Option<Uuid>,
        kind: EventKind,
    ) {
        let Ok(session) = self.ensure_session_record_for(session_id) else {
            return;
        };
        let event = codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: session.session_id,
            kind,
        };
        if let Err(err) = self.store.append_event(&event) {
            eprintln!("[agent] failed to persist event: {err}");
        }
    }

    #[allow(dead_code)]
    pub(crate) fn record_usage(&self, model: &str, usage: &codingbuddy_core::TokenUsage) {
        self.append_event_best_effort(EventKind::UsageUpdated {
            unit: codingbuddy_core::LlmUnit::Model(model.to_string()),
            model: model.to_string(),
            input_tokens: usage.prompt_tokens,
            cache_hit_tokens: usage.prompt_cache_hit_tokens,
            cache_miss_tokens: usage.prompt_cache_miss_tokens,
            output_tokens: usage.completion_tokens,
        });
    }

    pub(crate) fn request_approval(&self, call: &ToolCall) -> Result<bool> {
        if let Ok(guard) = self.approval_handler.lock()
            && let Some(ref handler) = *guard
        {
            let mut h = handler
                .lock()
                .map_err(|_| anyhow!("approval handler mutex poisoned"))?;
            return h(call);
        }
        Ok(false)
    }

    #[deprecated(note = "resume returns a session summary only")]
    pub fn resume(&self) -> Result<String> {
        let session = self
            .store
            .load_latest_session()?
            .ok_or_else(|| anyhow!("no prior session to resume"))?;
        let projection = self.store.rebuild_from_events(session.session_id)?;
        Ok(format!(
            "resumed session={} turns={} steps={}",
            session.session_id,
            projection.transcript.len(),
            projection.step_status.len()
        ))
    }
}

const INTERRUPTED_TURN_MARKER: &str = "[Previous request was interrupted before a final answer. It is abandoned unless the user asks to continue it.]";

/// True when the stored history ends mid-turn: a user request or tool result
/// with no assistant answer after it.
fn previous_turn_unfinished(history: &[ChatMessage]) -> bool {
    matches!(
        history.last(),
        Some(ChatMessage::User { .. } | ChatMessage::Tool { .. })
    )
}

fn prompt_grants_plan_approval(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    [
        "go ahead",
        "proceed",
        "execute",
        "implement",
        "apply the plan",
        "start coding",
        "continue with the plan",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Cap MCP tool descriptions to prevent OpenAPI-bloated descriptions from
/// eating the context window (2048-char cap, inspired by Claude Code).
const MCP_DESCRIPTION_MAX_CHARS: usize = 2048;

fn register_tool_definitions_metadata(
    metadata: &mut HashMap<String, RuntimeToolMetadata>,
    tools: &[codingbuddy_core::ToolDefinition],
) {
    for tool in tools {
        metadata.insert(
            tool.function.name.clone(),
            RuntimeToolMetadata::for_api_name(&tool.function.name),
        );
    }
}

fn mcp_tool_parameters(input_schema: &serde_json::Value) -> serde_json::Value {
    let argument_schema = if input_schema.is_object() {
        input_schema.clone()
    } else {
        serde_json::json!({
            "type": "object",
            "description": "Arguments to pass to the MCP tool"
        })
    };
    serde_json::json!({
        "type": "object",
        "properties": {
            "arguments": argument_schema
        }
    })
}

fn mcp_tool_metadata(tool_name: &str, tool: &codingbuddy_mcp::McpTool) -> RuntimeToolMetadata {
    let display_name = format!("MCP {}: {}", tool.server_id, tool.name);
    let search_text = format!(
        "mcp server {} tool {} {}",
        tool.server_id, tool.name, tool.description
    );
    let mut metadata = if mcp_annotations_read_only(&tool.annotations)
        && !mcp_annotations_destructive(&tool.annotations)
    {
        RuntimeToolMetadata::trusted_read_only_dynamic()
    } else {
        RuntimeToolMetadata::restricted_dynamic()
    }
    .with_display_text(display_name, search_text);
    metadata.permission_matcher = ToolPermissionMatcher::Mcp;
    metadata.max_result_chars = 30_000;
    metadata.result_size_policy = ToolResultSizePolicy::PersistAndTruncate { max_chars: 30_000 };
    if metadata.read_only {
        metadata.trust_level = DynamicToolTrust::TrustedReadOnly;
    }
    metadata.search_text = format!("{} api-name {}", metadata.search_text, tool_name);
    metadata
}

fn mcp_annotations_read_only(annotations: &serde_json::Value) -> bool {
    annotation_bool(annotations, &["readOnlyHint", "read_only", "readOnly"]).unwrap_or(false)
}

fn mcp_annotations_destructive(annotations: &serde_json::Value) -> bool {
    annotation_bool(annotations, &["destructiveHint", "destructive"]).unwrap_or(false)
}

fn annotation_bool(annotations: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| annotations.get(*key).and_then(serde_json::Value::as_bool))
}

fn truncate_mcp_description(desc: &str) -> String {
    if desc.len() <= MCP_DESCRIPTION_MAX_CHARS {
        return desc.to_string();
    }
    let safe_end = desc.floor_char_boundary(MCP_DESCRIPTION_MAX_CHARS);
    format!("{}... [truncated]", &desc[..safe_end])
}

/// Truncate text to fit a token budget (chars/4 heuristic).
/// Finds a clean break at a newline near the limit.
fn truncate_to_token_budget(text: &str, max_tokens: u64) -> String {
    let max_chars = (max_tokens * 4) as usize;
    if text.len() <= max_chars {
        return text.to_string();
    }
    // Use floor_char_boundary to avoid panicking on multi-byte UTF-8
    let safe_end = text.floor_char_boundary(max_chars.min(text.len()));
    let truncated = &text[..safe_end];
    if let Some(last_newline) = truncated.rfind('\n') {
        let remaining = text.len() - last_newline;
        format!(
            "{}...\n[truncated — {} more chars]",
            &text[..last_newline],
            remaining
        )
    } else {
        format!("{}...\n[truncated]", truncated)
    }
}

/// Build a privacy router from config. Returns None if privacy is disabled.
fn build_privacy_router(
    cfg: &codingbuddy_core::AppConfig,
) -> Option<std::sync::Arc<codingbuddy_policy::privacy::PrivacyRouter>> {
    if !cfg.privacy.enabled {
        return None;
    }
    codingbuddy_policy::privacy::PrivacyRouter::from_settings(&cfg.privacy)
        .ok()
        .map(std::sync::Arc::new)
}

fn should_run_team_orchestration(
    engine: &AgentEngine,
    prompt: &str,
    options: &ChatOptions,
) -> bool {
    if engine.cfg.workspace_trust != Some(true) || options.disable_team_orchestration {
        return false;
    }

    let value = options
        .teammate_mode
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .or_else(|| {
            std::env::var("CODINGBUDDY_TEAMMATE_MODE")
                .ok()
                .map(|v| v.trim().to_ascii_lowercase())
        });

    match value.as_deref() {
        Some("") | Some("0") | Some("false") | Some("off") | Some("none") => false,
        Some(_) => true,
        None => {
            if !engine.cfg.agent_loop.team.auto_enabled {
                return false;
            }
            codingbuddy_core::complexity::score_prompt(prompt)
                >= engine.cfg.agent_loop.team.complexity_threshold
        }
    }
}

/// Fetch up to three URLs named in the prompt, concurrently, and append
/// their text. Gives up (returning the prompt unchanged) as soon as the turn
/// is cancelled; failed fetches are skipped.
fn enrich_prompt_with_urls(
    prompt: &str,
    enabled: bool,
    cancellation: Option<&codingbuddy_core::CancellationToken>,
) -> String {
    if !enabled {
        return prompt.to_string();
    }
    let urls = extract_urls(prompt, 3);
    if urls.is_empty() {
        return prompt.to_string();
    }
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("codingbuddy-cli/", env!("CARGO_PKG_VERSION")))
        .build()
    else {
        return prompt.to_string();
    };
    let fetched = codingbuddy_core::runtime::block_on_cancellable(cancellation, async move {
        let tasks: Vec<_> = urls
            .into_iter()
            .map(|url| {
                let client = client.clone();
                tokio::spawn(async move {
                    let response = client.get(&url).send().await.ok()?;
                    let status = response.status().as_u16();
                    let content_type = response
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("unknown")
                        .to_string();
                    let body = response.text().await.ok()?;
                    Some((url, status, content_type, body))
                })
            })
            .collect();
        let mut pages = Vec::new();
        for task in tasks {
            if let Ok(Some(page)) = task.await {
                pages.push(page);
            }
        }
        pages
    });
    let Ok(pages) = fetched else {
        return prompt.to_string();
    };

    let mut rows = Vec::new();
    for (url, status, content_type, body) in pages {
        let max_bytes = 24_000usize;
        let truncated = body.len() > max_bytes;
        let clipped = if truncated {
            body[..body.floor_char_boundary(max_bytes)].to_string()
        } else {
            body
        };
        let extracted = if content_type.contains("html") {
            naive_strip_html(&clipped)
        } else {
            clipped
        };
        if extracted.trim().is_empty() {
            continue;
        }
        rows.push(format!(
            "URL|{}\nSTATUS|{}\nCONTENT_TYPE|{}\nTRUNCATED|{}\nEXTRACT|\n{}",
            url,
            status,
            content_type,
            truncated,
            truncate_text(&extracted, 8_000)
        ));
    }

    if rows.is_empty() {
        return prompt.to_string();
    }

    format!(
        "{prompt}\n\nAUTO_URL_CONTEXT\n{}\nAUTO_URL_CONTEXT_END",
        rows.join("\n---\n")
    )
}

fn extract_urls(prompt: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    for token in prompt.split_whitespace() {
        let trimmed = token
            .trim_matches(|c: char| {
                c == ','
                    || c == ';'
                    || c == ')'
                    || c == '('
                    || c == ']'
                    || c == '['
                    || c == '"'
                    || c == '\''
            })
            .trim_end_matches('.');
        if (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
            && !out.iter().any(|row| row == trimmed)
        {
            out.push(trimmed.to_string());
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn naive_strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push('\n');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    format!("{}...", &text[..text.floor_char_boundary(max_chars)])
}

#[cfg(test)]
mod tests;

/// This project's saved `bash.run` approval patterns, or `None` when the
/// store cannot be read (the current list is then left as it is).
fn saved_bash_approvals(store: &Store, workspace: &Path) -> Option<Vec<String>> {
    let project_hash = codingbuddy_store::project_hash(workspace);
    let approvals = store.list_persistent_approvals(&project_hash).ok()?;
    Some(
        approvals
            .into_iter()
            .filter(|a| a.tool_name == "bash.run")
            .map(|a| a.command_pattern)
            .collect(),
    )
}
