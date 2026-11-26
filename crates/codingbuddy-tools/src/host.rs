use super::utils::*;
use super::*;
use crate::sandbox::ShellLaunch;
use codingbuddy_core::{ToolPermission, workspace_fs::WorkspaceFs};

/// Device and proc paths that could hang or produce infinite/dangerous output.
const BLOCKED_DEVICE_PREFIXES: &[&str] = &[
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/null",
    "/dev/tty",
    "/proc/self/fd/",
    "/proc/self/mem",
];

fn is_blocked_device_path(path: &str) -> bool {
    BLOCKED_DEVICE_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// Validate the built-in batch envelope and resolve its child names.
pub fn batch_children(args: &serde_json::Value) -> Result<Vec<(&str, &serde_json::Value)>> {
    let calls = args
        .get("tool_calls")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow!("tool_calls array missing"))?;
    anyhow::ensure!(
        (1..=25).contains(&calls.len()),
        "batch requires 1 to 25 tool calls"
    );
    let read_only_tools = [
        "fs.read",
        "fs.list",
        "fs.glob",
        "fs.grep",
        "fs.search_rg",
        "git.status",
        "git.diff",
        "git.show",
        "web.fetch",
        "web.search",
        "notebook.read",
        "index.query",
        "diagnostics.check",
        "lsp.hover",
        "lsp_hover",
        "lsp.definition",
        "lsp_definition",
        "lsp.references",
        "lsp_references",
        "lsp.symbols",
        "lsp_symbols",
    ];
    calls
        .iter()
        .map(|call| {
            let tool = call
                .get("tool")
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow!("batch child tool is missing"))?;
            let internal = ToolName::from_api_name(tool).map_or(tool, |name| name.as_internal());
            anyhow::ensure!(
                read_only_tools.contains(&internal),
                "batch only allows individual read-only tools, got '{tool}'"
            );
            let args = call
                .get("parameters")
                .filter(|value| value.is_object())
                .ok_or_else(|| anyhow!("batch child parameters must be an object"))?;
            Ok((tool, args))
        })
        .collect()
}

/// When a file last changed. `mv` keeps the modification time, so on Unix
/// the later of mtime and ctime (which a rename, chmod or write updates)
/// is used.
fn last_changed(metadata: &std::fs::Metadata) -> Option<std::time::SystemTime> {
    let modified = metadata.modified().ok();
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        u64::try_from(metadata.ctime()).ok().map(|secs| {
            std::time::UNIX_EPOCH
                + Duration::from_secs(secs)
                + Duration::from_nanos(u64::try_from(metadata.ctime_nsec()).unwrap_or(0))
        })
    };
    #[cfg(not(unix))]
    let changed = None;
    modified.max(changed)
}

/// GET `url` on the shared runtime, abandoning the request the moment the
/// tool call is cancelled. Returns (status, content type, body).
async fn http_get_cancellable(
    url: &str,
    timeout: Duration,
    cancellation: Option<&codingbuddy_core::CancellationToken>,
) -> Result<(u16, String, String)> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("codingbuddy-cli/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let url = url.to_string();
    let fetched = codingbuddy_core::runtime::with_cancellation(cancellation, async move {
        let response = client.get(url).send().await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();
        let body = response.text().await?;
        Ok::<_, reqwest::Error>((status, content_type, body))
    })
    .await?;
    Ok(fetched?)
}

fn is_review_blocked(tool_name: &str) -> bool {
    codingbuddy_core::ToolName::from_internal_name(tool_name).is_some_and(|t| t.is_review_blocked())
}

/// Extract (path, line, column) from LSP tool arguments.
fn extract_lsp_position(args: &serde_json::Value) -> Result<(&str, u32, u32)> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing required parameter 'path'"))?;
    let line = args
        .get("line")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("missing required parameter 'line'"))? as u32;
    let col = args
        .get("column")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("missing required parameter 'column'"))? as u32;
    Ok((path, line, col))
}

use codingbuddy_core::runtime::BoxFuture;

/// Callback type for executing MCP tool calls.
/// Takes (server_id, tool_name, arguments) and returns the tool result.
pub type McpExecutor = Arc<
    dyn for<'a> Fn(
            &'a str,
            &'a str,
            &'a serde_json::Value,
        ) -> BoxFuture<'a, Result<serde_json::Value>>
        + Send
        + Sync,
>;

pub type ContextMcpExecutor = Arc<
    dyn for<'a> Fn(
            &'a str,
            &'a serde_json::Value,
            codingbuddy_core::ToolExecutionContext<'a>,
        ) -> BoxFuture<'a, Result<serde_json::Value>>
        + Send
        + Sync,
>;

pub struct LocalToolHost {
    launch_config: AppConfig,
    workspace: PathBuf,
    files: WorkspaceFs,
    policy: PolicyEngine,
    trusted_workspace: bool,
    sandbox_mode: String,
    shell_launches: std::sync::Mutex<std::collections::HashMap<Uuid, (ToolCall, ShellLaunch)>>,
    sandbox: codingbuddy_core::SandboxConfig,
    patches: PatchStore,
    index: IndexService,
    store: Store,
    runner: Arc<dyn ShellRunner + Send + Sync>,
    plugins: Option<PluginManager>,
    hooks_enabled: bool,
    visual_verification_enabled: bool,
    lint_after_edit: Option<String>,
    diagnostics_after_edit: bool,
    review_mode: bool,
    mcp_executor: Option<McpExecutor>,
    context_mcp_executor: Option<ContextMcpExecutor>,
    /// Optional progress callback for streaming live bash output.
    progress_callback: std::sync::Mutex<Option<super::shell::ProgressCallback>>,
    /// LSP server manager for code intelligence tools.
    lsp_manager: codingbuddy_lsp::server_manager::LspServerManager,
}

impl LocalToolHost {
    pub fn set_journal_store(&mut self, store: Store) {
        self.store = store;
    }
    pub fn new(workspace: &Path, policy: PolicyEngine) -> Result<Self> {
        Self::with_runner(workspace, policy, Arc::new(PtyShellRunner))
    }

    pub fn with_runner(
        workspace: &Path,
        policy: PolicyEngine,
        runner: Arc<dyn ShellRunner + Send + Sync>,
    ) -> Result<Self> {
        let cfg = AppConfig::load(workspace).unwrap_or_default();
        Self::with_config(workspace, policy, runner, &cfg)
    }

    pub fn with_config(
        workspace: &Path,
        policy: PolicyEngine,
        runner: Arc<dyn ShellRunner + Send + Sync>,
        cfg: &AppConfig,
    ) -> Result<Self> {
        let files = WorkspaceFs::open(workspace)?;
        let trusted_workspace = cfg
            .workspace_trust
            .unwrap_or_else(|| codingbuddy_core::workspace_is_trusted(workspace));
        let managed_hooks_only = codingbuddy_policy::load_managed_settings()
            .is_some_and(|settings| settings.allow_managed_hooks_only);
        let sandbox_mode = policy.sandbox_mode().to_ascii_lowercase();
        let lint_after_edit = cfg
            .policy
            .lint_after_edit
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let mut lsp_manager = codingbuddy_lsp::server_manager::LspServerManager::new(workspace);
        let language_workspace = files.root().to_owned();
        let language_policy = policy.clone();
        let language_config = cfg.policy.sandbox.clone();
        let language_mode = sandbox_mode.clone();
        lsp_manager.set_launcher(Arc::new(move |program, args, context| {
            context.check()?;
            anyhow::ensure!(
                trusted_workspace,
                "Language-server startup denied in untrusted workspace"
            );
            let expression = std::iter::once(program)
                .chain(args.iter().copied())
                .map(shell_quote)
                .collect::<Vec<_>>()
                .join(" ");
            let call = ToolCall {
                name: "bash.run".into(),
                args: json!({"cmd":expression}),
                requires_approval: false,
            };
            anyhow::ensure!(
                language_policy.denial(&call).is_none(),
                "Language-server startup denied by policy"
            );
            let mut launch = ShellLaunch::resolve(
                &language_workspace,
                &expression,
                &language_mode,
                &language_config,
            )?;
            anyhow::ensure!(
                launch.is_isolated()
                    && (!language_policy.requires_approval(&call)
                        || language_config.auto_allow_bash_if_sandboxed
                            && language_policy.permits_sandbox_auto_approval(&call)),
                "Language-server startup requires fresh approval; no approval channel is available"
            );
            launch.set_consumer("language_tools");
            launch.spawn_stdio(context)
        }));
        Ok(Self {
            launch_config: cfg.clone(),
            workspace: files.root().to_path_buf(),
            files,
            patches: PatchStore::new(workspace)?,
            index: IndexService::new(workspace)?,
            store: Store::new(workspace)?,
            policy,
            trusted_workspace,
            sandbox_mode,
            shell_launches: Default::default(),
            sandbox: cfg.policy.sandbox.clone(),
            runner,
            plugins: trusted_workspace
                .then(|| PluginManager::new(workspace).ok())
                .flatten(),
            hooks_enabled: trusted_workspace
                && !managed_hooks_only
                && cfg.plugins.enabled
                && cfg.plugins.enable_hooks,
            visual_verification_enabled: cfg.experiments.visual_verification,
            lint_after_edit: lint_after_edit.filter(|_| trusted_workspace),
            diagnostics_after_edit: trusted_workspace && cfg.tools.diagnostics_after_edit,
            review_mode: false,
            mcp_executor: None,
            context_mcp_executor: None,
            progress_callback: std::sync::Mutex::new(None),
            lsp_manager,
        })
    }

    pub fn set_context_mcp_executor(&mut self, executor: ContextMcpExecutor) {
        self.context_mcp_executor = Some(executor);
    }

    /// Set the MCP executor callback for handling `mcp__*` tool calls.
    pub fn set_mcp_executor(&mut self, executor: McpExecutor) {
        self.mcp_executor = Some(executor);
    }

    /// Set a progress callback for streaming live bash output.
    pub fn set_progress_callback(&self, cb: super::shell::ProgressCallback) {
        if let Ok(mut guard) = self.progress_callback.lock() {
            *guard = Some(cb);
        }
    }

    pub fn index(&self) -> &codingbuddy_index::IndexService {
        &self.index
    }

    /// Enable review mode (read-only pipeline).
    pub fn set_review_mode(&mut self, enabled: bool) {
        self.review_mode = enabled;
    }

    pub fn is_review_mode(&self) -> bool {
        self.review_mode
    }

    fn resolve_path(&self, path: &Path, allow_missing: bool) -> Result<PathBuf> {
        self.policy.check_path_name(&self.files.relative(path)?)?;
        let resolved = self.files.resolve(path, allow_missing)?;
        self.policy.check_path_name(&resolved)?;
        Ok(resolved)
    }

    fn resolved_path_requires_approval(&self, call: &ToolCall) -> bool {
        if self.policy.permission_mode() == codingbuddy_policy::PermissionMode::BypassPermissions {
            return false;
        }
        let (key, allow_missing) = match call.name.as_str() {
            "fs.read" | "fs.edit" => ("path", false),
            "fs.write" => ("path", true),
            "fs.grep" | "fs.glob" => ("base", false),
            _ => return false,
        };
        let Some(path) = call.args.get(key).and_then(|value| value.as_str()) else {
            return false;
        };
        let Ok(resolved) = self.resolve_path(Path::new(path), allow_missing) else {
            return true;
        };
        let mut target = call.clone();
        target.args[key] = json!(normalize_rel_path(&resolved));
        codingbuddy_policy::evaluate_permission_rules(self.policy.permission_rules(), &target)
            .is_some_and(|decision| matches!(decision.as_str(), "ask" | "deny"))
    }

    fn check_resolved_rule(&self, call: &ToolCall, key: &str, resolved: &Path) -> Result<()> {
        self.policy.ensure_allowed(call)?;
        let mut target = call.clone();
        if let Some(args) = target.args.as_object_mut() {
            args.insert(key.to_owned(), json!(normalize_rel_path(resolved)));
        }
        self.policy.ensure_allowed(&target)
    }

    async fn run_tool(
        &self,
        call: &ToolCall,
        shell: Option<ShellLaunch>,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> Result<serde_json::Value> {
        // Enforce review mode: block all non-read tools
        if self.review_mode && is_review_blocked(&call.name) {
            return Err(anyhow!(
                "tool '{}' is blocked during review mode (read-only pipeline)",
                call.name
            ));
        }
        match call.name.as_str() {
            "fs.list" => {
                let dir = call.args.get("dir").and_then(|v| v.as_str()).unwrap_or(".");
                let path = self.resolve_path(Path::new(dir), false)?;
                let mut out = Vec::new();
                for entry in self.files.read_dir(&path)? {
                    let e = entry?;
                    if self.resolve_path(&path.join(e.file_name()), false).is_ok() {
                        out.push(e.file_name().to_string_lossy().to_string());
                    }
                }
                Ok(json!({"entries": out}))
            }
            "fs.read" => {
                let path = call
                    .args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("path missing"))?;
                if is_blocked_device_path(path) {
                    anyhow::bail!(
                        "Cannot read device/proc file: {path}. These can hang or produce infinite output."
                    );
                }
                let full = self.resolve_path(Path::new(path), false)?;
                let bytes = self.files.read(&full)?;
                let sha = format!("{:x}", sha2::Sha256::digest(&bytes));
                let mime = guess_mime(&full);
                let max_bytes = call
                    .args
                    .get("max_bytes")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(READ_MAX_BYTES_DEFAULT);
                let start_line = call
                    .args
                    .get("start_line")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let end_line = call
                    .args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);

                let binary = is_binary(&bytes);
                if binary {
                    if self.visual_verification_enabled
                        && (mime.starts_with("image/") || mime == "application/pdf")
                    {
                        self.emit_visual_artifact_event(path, mime);
                    }
                    // Return base64 for images so multimodal models can process them
                    if mime.starts_with("image/") {
                        let encoded = BASE64.encode(&bytes);
                        return Ok(json!({
                            "path": path,
                            "mime": mime,
                            "binary": true,
                            "size_bytes": bytes.len(),
                            "sha256": sha,
                            "base64": encoded
                        }));
                    }
                    // PDF text extraction
                    if mime == "application/pdf" {
                        let pages_arg = call.args.get("pages").and_then(|v| v.as_str());
                        match extract_pdf_text(&bytes, pages_arg) {
                            Ok(text) => {
                                return Ok(json!({
                                    "path": path,
                                    "mime": mime,
                                    "binary": true,
                                    "size_bytes": bytes.len(),
                                    "sha256": sha,
                                    "content": text,
                                    "pages": pages_arg.unwrap_or("all")
                                }));
                            }
                            Err(_) => {
                                // Fall through to generic binary return if extraction fails
                            }
                        }
                    }
                    return Ok(json!({
                        "path": path,
                        "mime": mime,
                        "binary": true,
                        "size_bytes": bytes.len(),
                        "sha256": sha
                    }));
                }

                let truncated = if bytes.len() > max_bytes {
                    // Find the last valid UTF-8 char boundary to avoid splitting
                    // a multi-byte character, which would cause from_utf8 to fail.
                    let mut end = max_bytes;
                    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
                        end -= 1;
                    }
                    bytes[..end].to_vec()
                } else {
                    bytes.clone()
                };
                let content = String::from_utf8(truncated)?;
                let lines = collect_lines(&content, start_line, end_line);
                Ok(json!({
                    "path": path,
                    "mime": mime,
                    "binary": false,
                    "size_bytes": bytes.len(),
                    "truncated": bytes.len() > max_bytes,
                    "sha256": sha,
                    "content": content,
                    "lines": lines
                }))
            }
            "fs.glob" => {
                codingbuddy_core::runtime::blocking_section(|| -> Result<serde_json::Value> {
                    let pattern = call
                        .args
                        .get("pattern")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow!("pattern missing"))?;
                    let limit = call
                        .args
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(200) as usize;
                    let respect_gitignore = call
                        .args
                        .get("respectGitignore")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    let base = call
                        .args
                        .get("base")
                        .and_then(|v| v.as_str())
                        .unwrap_or(".");
                    let base_path = self
                        .workspace
                        .join(self.resolve_path(Path::new(base), false)?);
                    let mut matches = Vec::new();
                    let compiled = glob::Pattern::new(pattern)
                        .map_err(|err| anyhow!("invalid glob pattern '{pattern}': {err}"))?;
                    for path in walk_paths(&base_path, &self.workspace, respect_gitignore) {
                        let rel_path = match path.strip_prefix(&self.workspace) {
                            Ok(rel) => rel,
                            Err(_) => continue,
                        };
                        if should_skip_rel_path(rel_path) {
                            continue;
                        }
                        let rel = normalize_rel_path(rel_path);
                        let Ok(resolved) = self.resolve_path(rel_path, false) else {
                            continue;
                        };
                        let rel_under_base = path
                            .strip_prefix(&base_path)
                            .map(normalize_rel_path)
                            .unwrap_or_else(|_| rel.clone());
                        if compiled.matches(&rel) || compiled.matches(&rel_under_base) {
                            matches.push(json!({
                                "path": rel,
                                "is_dir": self.files.read_dir(&resolved).is_ok()
                            }));
                            if matches.len() >= limit {
                                break;
                            }
                        }
                    }
                    Ok(json!({
                        "pattern": pattern,
                        "matches": matches
                    }))
                })
            }
            "fs.grep" => {
                codingbuddy_core::runtime::blocking_section(|| -> Result<serde_json::Value> {
                    let pattern = call
                        .args
                        .get("pattern")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow!("pattern missing"))?;
                    let limit = call
                        .args
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(200) as usize;
                    let glob_pattern = call
                        .args
                        .get("glob")
                        .and_then(|v| v.as_str())
                        .unwrap_or("**/*");
                    let base = call
                        .args
                        .get("base")
                        .and_then(|v| v.as_str())
                        .unwrap_or(".");
                    let respect_gitignore = call
                        .args
                        .get("respectGitignore")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    let case_sensitive = call
                        .args
                        .get("case_sensitive")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    let compiled_glob = glob::Pattern::new(glob_pattern)
                        .map_err(|err| anyhow!("invalid glob pattern '{glob_pattern}': {err}"))?;
                    let regex = regex::RegexBuilder::new(pattern)
                        .case_insensitive(!case_sensitive)
                        .build()?;
                    let mut matches = Vec::new();
                    let base_path = self
                        .workspace
                        .join(self.resolve_path(Path::new(base), false)?);
                    for path in walk_paths(&base_path, &self.workspace, respect_gitignore) {
                        let rel_path = match path.strip_prefix(&self.workspace) {
                            Ok(rel) => rel,
                            Err(_) => continue,
                        };
                        if should_skip_rel_path(rel_path) {
                            continue;
                        }
                        let rel = normalize_rel_path(rel_path);
                        let Ok(resolved) = self.resolve_path(rel_path, false) else {
                            continue;
                        };
                        let rel_under_base = path
                            .strip_prefix(&base_path)
                            .map(normalize_rel_path)
                            .unwrap_or_else(|_| rel.clone());
                        if !compiled_glob.matches(&rel) && !compiled_glob.matches(&rel_under_base) {
                            continue;
                        }
                        let bytes = match self.files.read(&resolved) {
                            Ok(bytes) => bytes,
                            Err(_) => continue,
                        };
                        if is_binary(&bytes) {
                            continue;
                        }
                        let content = match String::from_utf8(bytes) {
                            Ok(content) => content,
                            Err(_) => continue,
                        };
                        for (idx, line) in content.lines().enumerate() {
                            if regex.is_match(line) {
                                matches.push(json!({
                                    "path": rel,
                                    "line": idx + 1,
                                    "text": line
                                }));
                                if matches.len() >= limit {
                                    return Ok(json!({
                                        "pattern": pattern,
                                        "glob": glob_pattern,
                                        "matches": matches
                                    }));
                                }
                            }
                        }
                    }
                    Ok(json!({
                        "pattern": pattern,
                        "glob": glob_pattern,
                        "matches": matches
                    }))
                })
            }
            "fs.edit" => {
                let path = call
                    .args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("path missing"))?;
                let full = self.resolve_path(Path::new(path), false)?;
                let before = self.files.read_to_string(&full)?;
                let mut after = before.clone();
                let mut replacements = 0usize;
                let mut fuzzy_strategies: Vec<&str> = Vec::new();

                if let Some(edits) = call.args.get("edits").and_then(|v| v.as_array()) {
                    for edit in edits {
                        let result = apply_single_edit(&mut after, edit)?;
                        replacements += result.replacements;
                        if let Some(s) = result.fuzzy_strategy {
                            fuzzy_strategies.push(s);
                        }
                    }
                } else {
                    let result = apply_single_edit(&mut after, &call.args)?;
                    replacements += result.replacements;
                    if let Some(s) = result.fuzzy_strategy {
                        fuzzy_strategies.push(s);
                    }
                }

                if after == before {
                    return Ok(json!({
                        "path": path,
                        "edited": false,
                        "replacements": 0
                    }));
                }

                let diff = generate_unified_diff(path, &before, &after);
                let checkpoint = self.create_checkpoint("fs_edit").await?;
                self.record_file_receipt(context, &full, after.as_bytes())?;
                self.files.write(&full, after.as_bytes(), false)?;
                self.record_actual_postimage(&full)?;
                let before_sha = format!("{:x}", sha2::Sha256::digest(before.as_bytes()));
                let after_sha = format!("{:x}", sha2::Sha256::digest(after.as_bytes()));

                // Auto-lint after edit (lint-fix loop)
                let lint_output = match &self.lint_after_edit {
                    Some(command) => Some(
                        self.run_verification_command(command, &self.workspace, 30, context)
                            .await,
                    ),
                    None => None,
                };

                let mut result = json!({
                    "path": path,
                    "edited": true,
                    "replacements": replacements,
                    "diff": diff,
                    "before_sha256": before_sha,
                    "after_sha256": after_sha,
                    "checkpoint_id": checkpoint.map(|id| id.to_string())
                });
                if !fuzzy_strategies.is_empty() {
                    result["fuzzy_strategies"] = json!(fuzzy_strategies);
                }
                if let Some(lint) = lint_output {
                    result["lint"] = lint;
                }
                // Auto-diagnostics after edit
                if self.diagnostics_after_edit {
                    self.post_edit_checks(std::slice::from_ref(&full), context, &mut result)
                        .await;
                }
                Ok(result)
            }
            "fs.search_rg" => {
                codingbuddy_core::runtime::blocking_section(|| -> Result<serde_json::Value> {
                    let q = call
                        .args
                        .get("query")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow!("query missing"))?;
                    let limit = call
                        .args
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(20) as usize;
                    let mut matches = Vec::new();
                    for path in walk_paths(&self.workspace, &self.workspace, true) {
                        let rel_path = path.strip_prefix(&self.workspace)?;
                        if should_skip_rel_path(rel_path) {
                            continue;
                        }
                        let rel = rel_path.to_string_lossy().to_string();
                        if let Ok(resolved) = self.resolve_path(rel_path, false)
                            && let Ok(content) = self.files.read_to_string(&resolved)
                        {
                            for (idx, line) in content.lines().enumerate() {
                                if line.contains(q) {
                                    matches
                                        .push(json!({"path": rel, "line": idx + 1, "text": line}));
                                    if matches.len() >= limit {
                                        return Ok(json!({"matches": matches}));
                                    }
                                }
                            }
                        }
                    }
                    Ok(json!({"matches": matches}))
                })
            }
            "git.status" => {
                self.run_git_inspection(&["status", "--short", "--ignore-submodules=all"])
                    .await
            }
            "git.diff" => {
                self.run_git_inspection(&[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--ignore-submodules=all",
                ])
                .await
            }
            "git.show" => {
                let spec = call
                    .args
                    .get("spec")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("spec missing"))?;
                // Reject specs containing shell metacharacters to prevent injection.
                if spec.contains([
                    ';', '|', '&', '$', '`', '(', ')', '{', '}', '<', '>', '\'', '"', '\\', '\n',
                ]) {
                    return Err(anyhow!("git.show: spec contains forbidden characters"));
                }
                anyhow::ensure!(
                    !spec.starts_with('-'),
                    "git.show requires a revision or object"
                );
                self.run_git_inspection(&["show", "--no-ext-diff", "--no-textconv", spec, "--"])
                    .await
            }
            "index.query" => {
                codingbuddy_core::runtime::blocking_section(|| -> Result<serde_json::Value> {
                    let q = call
                        .args
                        .get("q")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow!("q missing"))?;
                    let top_k = call
                        .args
                        .get("top_k")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(10) as usize;
                    let scope = call.args.get("scope").and_then(|v| v.as_str());
                    let mut response = self.index.query(q, top_k, scope)?;
                    response
                        .results
                        .retain(|result| self.resolve_path(Path::new(&result.path), false).is_ok());
                    Ok(serde_json::to_value(response)?)
                })
            }
            "patch.stage" => {
                let diff = call
                    .args
                    .get("unified_diff")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("unified_diff missing"))?;
                let base = call
                    .args
                    .get("base")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .as_bytes()
                    .to_vec();
                let patch = self
                    .patches
                    .stage_checked(diff, &base, |path| self.resolve_path(path, true).map(drop))?;
                Ok(
                    json!({"patch_id": patch.patch_id.to_string(), "base_sha256": patch.base_sha256}),
                )
            }
            "patch.apply" => {
                let id = call
                    .args
                    .get("patch_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("patch_id missing"))?;
                let patch_id = Uuid::parse_str(id)?;
                let (applied, conflicts, recovery_status) = self.apply_patch(context, patch_id)?;
                Ok(
                    json!({"patch_id": id, "applied": applied, "conflicts": conflicts, "recovery_status": recovery_status}),
                )
            }
            "patch.direct" => {
                // Single-step: stage + apply in one call. Ideal for DeepSeek-reasoner
                // which naturally produces unified diffs when thinking.
                let diff = call
                    .args
                    .get("unified_diff")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("unified_diff missing"))?;
                let patch = self
                    .patches
                    .stage_checked(diff, &[], |path| self.resolve_path(path, true).map(drop))?;
                let (applied, conflicts, recovery_status) =
                    self.apply_patch(context, patch.patch_id)?;
                let mut result = if applied {
                    json!({
                        "applied": true,
                        "patch_id": patch.patch_id.to_string(),
                        "files": patch.target_files,
                    })
                } else {
                    json!({
                        "applied": false,
                        "patch_id": patch.patch_id.to_string(),
                        "conflicts": conflicts,
                        "files": patch.target_files,
                    })
                };
                if applied {
                    for path in &patch.target_files {
                        self.record_actual_postimage(Path::new(path))?;
                    }
                }
                result["recovery_status"] = json!(recovery_status);
                // Auto-diagnostics after patch application
                if applied && self.diagnostics_after_edit {
                    self.post_edit_checks(
                        &patch
                            .target_files
                            .iter()
                            .map(|path| self.workspace.join(path))
                            .collect::<Vec<_>>(),
                        context,
                        &mut result,
                    )
                    .await;
                }
                Ok(result)
            }
            "fs.write" => {
                let path = call
                    .args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("path missing"))?;
                let full = self.resolve_path(Path::new(path), true)?;
                let content = call
                    .args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("content missing"))?;
                let checkpoint = self.create_checkpoint("fs_write").await?;
                self.record_file_receipt(context, &full, content.as_bytes())?;
                self.files.write(&full, content.as_bytes(), true)?;
                self.record_actual_postimage(&full)?;
                Ok(json!({
                    "written": true,
                    "checkpoint_id": checkpoint.map(|id| id.to_string())
                }))
            }
            "bash.run" => {
                // Same extraction as the policy, so the approved string is
                // the executed string; conflicting `cmd`/`command` are refused.
                let cmd = codingbuddy_policy::approvals::bash_command_arg(&call.args).ok_or_else(
                    || anyhow!("expected one `command` (or `cmd`); conflicting values are refused"),
                )?;

                // Structural command analysis via tree-sitter-bash AST.
                let cmd_analysis = crate::bash_ast::analyze_command(cmd);
                if cmd_analysis.has_dangerous_patterns {
                    return Err(anyhow!(
                        "command blocked: dangerous pattern detected ({}). \
                         Detected commands: [{}]",
                        if cmd_analysis.has_dangerous_patterns {
                            "eval, curl|sh, or rm -rf /"
                        } else {
                            "unknown"
                        },
                        cmd_analysis.commands.join(", ")
                    ));
                }

                let timeout = call
                    .args
                    .get("timeout")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
                // Shell commands can change files the edit tools never see
                // (rm, sed -i, generators, formatters). Checkpoint first so
                // /rewind can undo them; read-only commands skip the cost.
                // Best effort: a failed checkpoint is logged, not fatal.
                let checkpoint = if codingbuddy_policy::bash_classifier::classify_bash_command(cmd)
                    == codingbuddy_policy::bash_classifier::BashSafety::Safe
                {
                    None
                } else {
                    self.create_checkpoint("bash_run").await.unwrap_or_else(|error| {
                        tracing::warn!(error = %format!("{error:#}"), "checkpoint before shell command failed");
                        None
                    })
                };
                let command_started = std::time::SystemTime::now();
                let launched = shell
                    .ok_or_else(|| anyhow!("shell execution requires a prepared launch"))?
                    .run_with_progress(
                        Duration::from_secs(timeout).min(
                            context
                                .deadline
                                .map(|end| end.saturating_duration_since(std::time::Instant::now()))
                                .unwrap_or(Duration::MAX),
                        ),
                        self.progress_callback
                            .lock()
                            .ok()
                            .and_then(|callback| callback.clone()),
                        context.cancellation,
                    )
                    .await?;
                // Record what the command changed so /rewind treats those
                // changes as the agent's (and still protects later edits).
                let command_finished = std::time::SystemTime::now();
                let changed_files = match checkpoint {
                    Some(id) => codingbuddy_core::runtime::blocking_section(|| {
                        self.record_shell_changes(id, command_started, command_finished)
                    })
                    .unwrap_or_else(|error| {
                        tracing::warn!(error = %format!("{error:#}"), "could not record shell command changes");
                        0
                    }),
                    None => 0,
                };
                let mut result = json!({"status": launched.status, "stdout": launched.stdout,
                    "stderr": launched.stderr, "timed_out": launched.timed_out, "cancelled":launched.cancelled,
                    "checkpoint_id": checkpoint.map(|id| id.to_string()),
                    "changed_files": changed_files});

                // Annotate result with AST-derived security metadata.
                if cmd_analysis.has_network_access
                    || cmd_analysis.has_deletions
                    || cmd_analysis.has_permission_changes
                {
                    result["security_flags"] = json!({
                        "network_access": cmd_analysis.has_network_access,
                        "file_deletions": cmd_analysis.has_deletions,
                        "permission_changes": cmd_analysis.has_permission_changes,
                        "commands": cmd_analysis.commands,
                    });
                }
                Ok(result)
            }
            "web.fetch" => {
                let url = call
                    .args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("url missing"))?;
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(anyhow!("url must start with http:// or https://"));
                }
                let max_bytes = call
                    .args
                    .get("max_bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(500_000) as usize;
                let timeout = call
                    .args
                    .get("timeout")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30);
                let (status, content_type, body) =
                    http_get_cancellable(url, Duration::from_secs(timeout), context.cancellation)
                        .await?;
                let truncated = body.len() > max_bytes;
                let text = if truncated {
                    &body[..body.floor_char_boundary(max_bytes)]
                } else {
                    &body
                };
                // Strip HTML tags for readable text extraction
                let content = if content_type.contains("html") {
                    strip_html_tags(text)
                } else {
                    text.to_string()
                };
                Ok(json!({
                    "url": url,
                    "status": status,
                    "content_type": content_type,
                    "content": content,
                    "truncated": truncated,
                    "bytes": body.len()
                }))
            }
            "web.search" => {
                let query = call
                    .args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("query missing"))?;
                let max_results = call
                    .args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10) as usize;

                // Check cache first
                let query_hash = format!("{:x}", sha2::Sha256::digest(query.as_bytes()));
                if let Ok(Some(cached)) = self.store.get_web_search_cache(&query_hash) {
                    return Ok(json!({
                        "query": query,
                        "results": serde_json::from_str::<serde_json::Value>(&cached.results_json).unwrap_or(json!([])),
                        "cached": true,
                        "results_count": cached.results_count,
                        "provenance": {
                            "source": "cache",
                            "cached_at": cached.cached_at,
                        }
                    }));
                }

                // Perform web search via HTML scraping of a search engine
                let search_url = format!(
                    "https://html.duckduckgo.com/html/?q={}",
                    url_encode_query(query)
                );
                let (_, _, body) = http_get_cancellable(
                    &search_url,
                    Duration::from_secs(15),
                    context.cancellation,
                )
                .await?;

                // Parse search results from HTML
                let results = parse_search_results(&body, max_results);
                let results_json = serde_json::to_string(&results)?;
                let results_count = results.len() as u64;

                // Cache the results
                if let Err(e) =
                    self.store
                        .set_web_search_cache(&codingbuddy_store::WebSearchCacheRecord {
                            query_hash: query_hash.clone(),
                            query: query.to_string(),
                            results_json: results_json.clone(),
                            results_count,
                            cached_at: chrono::Utc::now().to_rfc3339(),
                            ttl_seconds: 900, // 15 minutes
                        })
                {
                    codingbuddy_core::logging::notice(format_args!(
                        "[codingbuddy] web_search: failed to cache results: {e}"
                    ));
                }

                // Emit event
                if let Err(e) = self.store.append_event(&codingbuddy_store::NewEvent {
                    event_id: uuid::Uuid::now_v7(),
                    at: chrono::Utc::now(),
                    session_id: uuid::Uuid::nil(),
                    kind: codingbuddy_core::EventKind::WebSearchExecuted {
                        query: query.to_string(),
                        results_count,
                        cached: false,
                    },
                }) {
                    codingbuddy_core::logging::notice(format_args!(
                        "[codingbuddy] web_search: failed to emit event: {e}"
                    ));
                }

                Ok(json!({
                    "query": query,
                    "results": results,
                    "cached": false,
                    "results_count": results_count,
                    "provenance": {
                        "source": "duckduckgo",
                        "searched_at": chrono::Utc::now().to_rfc3339(),
                    }
                }))
            }
            "notebook.read" => {
                let path = call
                    .args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("path missing"))?;
                let full = self.resolve_path(Path::new(path), false)?;
                let content = self.files.read_to_string(&full)?;
                let nb: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| anyhow!("invalid notebook JSON: {e}"))?;
                let cells = nb
                    .get("cells")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("notebook has no cells array"))?;
                let cell_summaries: Vec<serde_json::Value> = cells
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| {
                        let cell_type = cell
                            .get("cell_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let source = cell
                            .get("source")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .or_else(|| {
                                cell.get("source")
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                            })
                            .unwrap_or_default();
                        let preview: String = source.chars().take(200).collect();
                        json!({
                            "index": i,
                            "cell_type": cell_type,
                            "source_preview": preview,
                            "source_length": source.len()
                        })
                    })
                    .collect();
                Ok(json!({
                    "path": path,
                    "cells_count": cells.len(),
                    "cells": cell_summaries
                }))
            }
            "notebook.edit" => {
                let path = call
                    .args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("path missing"))?;
                let full = self.resolve_path(Path::new(path), false)?;
                let content = self.files.read_to_string(&full)?;
                let mut nb: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| anyhow!("invalid notebook JSON: {e}"))?;
                let operation = call
                    .args
                    .get("operation")
                    .and_then(|v| v.as_str())
                    .unwrap_or("replace");
                let cell_index =
                    call.args
                        .get("cell_index")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| anyhow!("cell_index missing"))? as usize;
                let cells = nb
                    .get_mut("cells")
                    .and_then(|v| v.as_array_mut())
                    .ok_or_else(|| anyhow!("notebook has no cells array"))?;
                let checkpoint = self.create_checkpoint("notebook_edit").await?;
                match operation {
                    "replace" => {
                        if cell_index >= cells.len() {
                            return Err(anyhow!(
                                "cell_index {cell_index} out of range ({} cells)",
                                cells.len()
                            ));
                        }
                        let new_source = call
                            .args
                            .get("new_source")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| anyhow!("new_source missing for replace"))?;
                        let source_lines: Vec<serde_json::Value> = new_source
                            .lines()
                            .map(|l| json!(format!("{l}\n")))
                            .collect();
                        cells[cell_index]["source"] = json!(source_lines);
                        if let Some(ct) = call.args.get("cell_type").and_then(|v| v.as_str()) {
                            cells[cell_index]["cell_type"] = json!(ct);
                        }
                    }
                    "insert" => {
                        if cell_index > cells.len() {
                            return Err(anyhow!(
                                "cell_index {cell_index} out of range for insert ({} cells)",
                                cells.len()
                            ));
                        }
                        let new_source = call
                            .args
                            .get("new_source")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let cell_type = call
                            .args
                            .get("cell_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("code");
                        let source_lines: Vec<serde_json::Value> = new_source
                            .lines()
                            .map(|l| json!(format!("{l}\n")))
                            .collect();
                        let new_cell = json!({
                            "cell_type": cell_type,
                            "source": source_lines,
                            "metadata": {},
                            "outputs": [],
                            "execution_count": null
                        });
                        cells.insert(cell_index, new_cell);
                    }
                    "delete" => {
                        if cell_index >= cells.len() {
                            return Err(anyhow!(
                                "cell_index {cell_index} out of range ({} cells)",
                                cells.len()
                            ));
                        }
                        cells.remove(cell_index);
                    }
                    _ => return Err(anyhow!("unknown notebook operation: {operation}")),
                }
                let updated = serde_json::to_string_pretty(&nb)?;
                self.files.write(&full, updated.as_bytes(), false)?;
                self.record_actual_postimage(&full)?;
                Ok(json!({
                    "path": path,
                    "operation": operation,
                    "cell_index": cell_index,
                    "cells_count": nb.get("cells").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                    "checkpoint_id": checkpoint.map(|id| id.to_string())
                }))
            }
            "multi_edit" => {
                let files = call
                    .args
                    .get("files")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("files array missing"))?;
                let targets = files
                    .iter()
                    .map(|entry| {
                        let path = entry
                            .get("path")
                            .and_then(|value| value.as_str())
                            .ok_or_else(|| anyhow!("path missing in file entry"))?;
                        let resolved = self.resolve_path(Path::new(path), false)?;
                        self.files.open_file(&resolved)?;
                        Ok((path, resolved))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let checkpoint = self.create_checkpoint("multi_edit").await?;
                let mut results = Vec::new();
                let mut total_replacements = 0usize;
                let mut all_succeeded = true;

                for (file_entry, (path, full)) in files.iter().zip(targets) {
                    let before = self.files.read_to_string(&full)?;
                    let mut after = before.clone();
                    let mut file_replacements = 0usize;

                    if let Some(edits) = file_entry.get("edits").and_then(|v| v.as_array()) {
                        for edit in edits {
                            match apply_single_edit(&mut after, edit) {
                                Ok(r) => file_replacements += r.replacements,
                                Err(e) => {
                                    all_succeeded = false;
                                    results.push(json!({
                                        "path": path,
                                        "edited": false,
                                        "error": e.to_string()
                                    }));
                                    continue;
                                }
                            }
                        }
                    }

                    if after == before {
                        results.push(json!({"path": path, "edited": false, "replacements": 0}));
                        continue;
                    }

                    let diff = generate_unified_diff(path, &before, &after);
                    self.files.write(&full, after.as_bytes(), false)?;
                    self.record_actual_postimage(&full)?;
                    let before_sha = format!("{:x}", sha2::Sha256::digest(before.as_bytes()));
                    let after_sha = format!("{:x}", sha2::Sha256::digest(after.as_bytes()));
                    total_replacements += file_replacements;

                    results.push(json!({
                        "path": path,
                        "edited": true,
                        "replacements": file_replacements,
                        "diff": diff,
                        "before_sha256": before_sha,
                        "after_sha256": after_sha,
                    }));
                }

                // Run lint once after all edits
                let lint_output = match &self.lint_after_edit {
                    Some(command) => Some(
                        self.run_verification_command(command, &self.workspace, 30, context)
                            .await,
                    ),
                    None => None,
                };

                let mut result = json!({
                    "results": results,
                    "total_files": files.len(),
                    "total_replacements": total_replacements,
                    "all_succeeded": all_succeeded,
                    "checkpoint_id": checkpoint.map(|id| id.to_string()),
                });
                if let Some(lint) = lint_output {
                    result["lint"] = lint;
                }
                // Auto-diagnostics after multi_edit
                if self.diagnostics_after_edit {
                    let paths = results
                        .iter()
                        .filter(|item| item["edited"] == true)
                        .filter_map(|item| item["path"].as_str().map(PathBuf::from))
                        .collect::<Vec<_>>();
                    self.post_edit_checks(&paths, context, &mut result).await;
                }
                Ok(result)
            }
            "diagnostics.check" => {
                let target = call
                    .args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|path| self.resolve_path(Path::new(path), false))
                    .transpose()?;
                let target = target
                    .as_deref()
                    .map(|path| {
                        path.to_str()
                            .ok_or_else(|| anyhow!("diagnostic path is not UTF-8"))
                    })
                    .transpose()?;
                let (cmd, source) = detect_diagnostics_command(&self.workspace, target)?;
                let result = self
                    .run_verification_command(&cmd, &self.workspace, 60, context)
                    .await;
                let diagnostics = parse_diagnostics(
                    result["stdout"].as_str().unwrap_or(""),
                    result["stderr"].as_str().unwrap_or(""),
                    source,
                );
                Ok(json!({
                    "diagnostics": diagnostics,
                    "command": cmd,
                    "success": result["success"]==true,
                    "verification": result,
                    "source": source,
                }))
            }
            "lsp.hover" | "lsp_hover" => {
                let (path, line, col) = extract_lsp_position(&call.args)?;
                let result = self
                    .lsp_manager
                    .hover_with_context(
                        &self
                            .workspace
                            .join(self.resolve_path(Path::new(path), false)?),
                        line,
                        col,
                        &context.execution(),
                    )
                    .await?;
                Ok(json!({ "hover": result }))
            }
            "lsp.definition" | "lsp_definition" => {
                let (path, line, col) = extract_lsp_position(&call.args)?;
                let result = self
                    .lsp_manager
                    .definition_with_context(
                        &self
                            .workspace
                            .join(self.resolve_path(Path::new(path), false)?),
                        line,
                        col,
                        &context.execution(),
                    )
                    .await?;
                Ok(json!({ "definition": result }))
            }
            "lsp.references" | "lsp_references" => {
                let (path, line, col) = extract_lsp_position(&call.args)?;
                let result = self
                    .lsp_manager
                    .references_with_context(
                        &self
                            .workspace
                            .join(self.resolve_path(Path::new(path), false)?),
                        line,
                        col,
                        &context.execution(),
                    )
                    .await?;
                Ok(json!({ "references": result }))
            }
            "lsp.symbols" | "lsp_symbols" => {
                let path = call
                    .args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing required parameter 'path'"))?;
                let result = self
                    .lsp_manager
                    .symbols_with_context(
                        &self
                            .workspace
                            .join(self.resolve_path(Path::new(path), false)?),
                        &context.execution(),
                    )
                    .await?;
                Ok(json!({ "symbols": result }))
            }
            "github.create_pr" | "github_create_pr" => {
                let title = call
                    .args
                    .get("title")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing required parameter 'title'"))?;
                let body = call.args.get("body").and_then(|v| v.as_str()).unwrap_or("");
                let base = call.args.get("base").and_then(|v| v.as_str());
                let draft = call
                    .args
                    .get("draft")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mut cmd = format!(
                    "gh pr create --title {} --body {}",
                    shell_quote(title),
                    shell_quote(body),
                );
                if let Some(b) = base {
                    cmd.push_str(&format!(" --base {}", shell_quote(b)));
                }
                if draft {
                    cmd.push_str(" --draft");
                }
                self.run_cmd(&cmd, 30, context).await
            }
            "github.list_issues" | "github_list_issues" => {
                let label = call.args.get("label").and_then(|v| v.as_str());
                let state = call
                    .args
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("open");
                let limit = call
                    .args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20);
                let mut cmd = format!(
                    "gh issue list --state {} --limit {limit} --json number,title,labels,assignees",
                    shell_quote(state),
                );
                if let Some(l) = label {
                    cmd.push_str(&format!(" --label {}", shell_quote(l)));
                }
                self.run_cmd(&cmd, 15, context).await
            }
            "github.view_pr" | "github_view_pr" => {
                let pr = call
                    .args
                    .get("pr")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing required parameter 'pr'"))?;
                self.run_cmd(
                    &format!(
                        "gh pr view {} --json title,body,state,reviews,statusCheckRollup,additions,deletions,changedFiles",
                        shell_quote(pr),
                    ),
                    15,context,
                ).await
            }
            name if name.starts_with("plugin__") => self.run_plugin_tool(name, &call.args),
            name if name.starts_with("mcp__") => {
                if let Some(executor) = &self.context_mcp_executor {
                    return executor(name, &call.args, context).await;
                }
                let rest = &name[5..]; // skip "mcp__"
                let (server_id, tool_name) = rest
                    .split_once("__")
                    .ok_or_else(|| anyhow!("invalid MCP tool name: {name}"))?;
                let args = call.args.get("arguments").cloned().unwrap_or(json!({}));
                match &self.mcp_executor {
                    Some(executor) => executor(server_id, tool_name, &args).await,
                    None => Err(anyhow!(
                        "MCP tool '{name}' called but no MCP executor configured"
                    )),
                }
            }
            "batch" => {
                let children = batch_children(&call.args)?;
                let mut results = Vec::with_capacity(children.len());
                let mut successes = 0;
                for (tool_name, params) in children {
                    let internal = ToolName::from_api_name(tool_name)
                        .map_or(tool_name, |name| name.as_internal());
                    let sub_call = ToolCall {
                        name: internal.to_owned(),
                        args: params.clone(),
                        requires_approval: false,
                    };
                    let proposal = self.propose(sub_call);
                    let sub_result = if proposal.approved {
                        Box::pin(self.run_approved(
                            ApprovedToolCall {
                                invocation_id: proposal.invocation_id,
                                call: proposal.call,
                            },
                            codingbuddy_core::ToolExecutionContext {
                                invocation_id: None,
                                ..context
                            },
                        ))
                        .await
                    } else {
                        self.discard_proposal(proposal.invocation_id);
                        ToolResult {
                            invocation_id: proposal.invocation_id,
                            success: false,
                            output: json!({"error": "batch child requires independent approval"}),
                        }
                    };
                    if sub_result.success {
                        successes += 1;
                    }
                    results.push(json!({
                        "tool": tool_name,
                        "success": sub_result.success,
                        "output": sub_result.output,
                    }));
                }
                let total = results.len();
                Ok(json!({
                    "results": results,
                    "total": total,
                    "succeeded": successes,
                    "failed": total - successes,
                }))
            }
            _ => Err(anyhow!("unknown tool: {}", call.name)),
        }
    }

    fn record_actual_postimage(&self, path: &Path) -> Result<()> {
        use sha2::Digest;
        let path = self.files.relative(path)?;
        let hash = match self.files.read(&path) {
            Ok(bytes) => Some(format!("{:x}", sha2::Sha256::digest(bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        self.store
            .record_postimage_at(&self.workspace, &path, hash.as_deref())
    }

    fn record_file_receipt(
        &self,
        context: codingbuddy_core::ToolExecutionContext<'_>,
        path: &Path,
        content: &[u8],
    ) -> Result<()> {
        if let (Some(session), Some(id)) = (context.session_id, context.invocation_id) {
            self.store.record_invocation_receipt(
                session,
                id,
                &codingbuddy_store::InvocationReceipt::File {
                    execution_root: Some(self.workspace.clone()),
                    path: self.files.relative(path)?.to_owned(),
                    postimage_sha256: format!("{:x}", sha2::Sha256::digest(content)),
                },
            )?;
        }
        Ok(())
    }

    fn apply_patch(
        &self,
        context: codingbuddy_core::ToolExecutionContext<'_>,
        patch_id: Uuid,
    ) -> Result<(bool, Vec<String>, Option<String>)> {
        let check = |path: &Path| self.policy.check_patch_target(&self.files, path);
        if let Some(session) = context.session_id {
            if let Some(id) = context.invocation_id {
                self.store.record_invocation_receipt(
                    session,
                    id,
                    &codingbuddy_store::InvocationReceipt::Patch { patch_id },
                )?;
            }
            return self
                .patches
                .apply_for_session(&self.store, session, patch_id, check);
        }
        let (applied, conflicts) = self
            .patches
            .apply_checked(&self.workspace, patch_id, check)?;
        Ok((
            applied,
            conflicts,
            self.patches.read_patch(patch_id)?.recovery_status,
        ))
    }

    async fn run_git_inspection(&self, args: &[&str]) -> Result<serde_json::Value> {
        let command = codingbuddy_core::process_env::git_inspection(&self.workspace);
        let result = super::shell::run_command(
            command,
            args,
            &self.workspace,
            Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
        )
        .await?;
        Ok(
            json!({"status": result.status, "stdout": result.stdout, "stderr": result.stderr, "timed_out": result.timed_out}),
        )
    }

    async fn run_cmd(
        &self,
        cmd: &str,
        timeout_secs: u64,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> Result<serde_json::Value> {
        let call = ToolCall {
            name: "bash.run".into(),
            args: json!({"cmd":cmd}),
            requires_approval: false,
        };
        anyhow::ensure!(
            self.policy.denial(&call).is_none(),
            "Derived command denied by policy"
        );
        let launch = self.shell_launch(&call)?;
        anyhow::ensure!(
            launch.is_isolated(),
            "Derived command requires isolated execution; approve the exact command locally instead"
        );
        let timeout = Duration::from_secs(timeout_secs).min(
            context
                .deadline
                .map(|end| end.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or(Duration::MAX),
        );
        context.execution().check()?;
        let result = launch
            .run_with_progress(timeout, None, context.cancellation)
            .await?;
        Ok(
            json!({"status":result.status,"stdout":result.stdout,"stderr":result.stderr,"timed_out":result.timed_out,"cancelled":result.cancelled}),
        )
    }

    fn shell_launch(&self, call: &ToolCall) -> Result<ShellLaunch> {
        let expression = call
            .args
            .get("cmd")
            .or_else(|| call.args.get("command"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("shell command missing"))?;
        self.policy.check_command(expression)?;
        let mut launch = ShellLaunch::resolve(
            &self.workspace,
            expression,
            &self.sandbox_mode,
            &self.sandbox,
        )?;
        if let Some(cwd) = call.args.get("cwd").and_then(serde_json::Value::as_str) {
            launch.set_cwd(
                &self
                    .workspace
                    .join(self.files.resolve(Path::new(cwd), false)?),
            )?;
        }
        Ok(launch)
    }

    fn run_plugin_tool(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let manager = self
            .plugins
            .as_ref()
            .ok_or_else(|| anyhow!("plugin runtime unavailable"))?;
        let (plugin_id, command_name) = self.resolve_plugin_command(tool_name)?;

        let input = args.get("arguments").or_else(|| args.get("input"));
        let input = match input {
            Some(serde_json::Value::String(text)) => Some(text.clone()),
            Some(value) if !value.is_null() => Some(serde_json::to_string(value)?),
            _ => None,
        };

        let rendered =
            manager.render_command_prompt(&plugin_id, &command_name, input.as_deref())?;
        Ok(json!({
            "plugin_id": rendered.plugin_id,
            "command_name": rendered.command_name,
            "source_path": rendered.source_path,
            "prompt": rendered.prompt,
        }))
    }

    fn resolve_plugin_command(&self, tool_name: &str) -> Result<(String, String)> {
        let manager = self
            .plugins
            .as_ref()
            .ok_or_else(|| anyhow!("plugin runtime unavailable"))?;
        let plugins = manager.list()?;
        for plugin in plugins.into_iter().filter(|p| p.enabled) {
            for cmd_path in &plugin.commands {
                let command_name = plugin_command_lookup_name(&plugin.root, cmd_path);
                let candidate = plugin_tool_api_name(&plugin.manifest.id, &command_name);
                if candidate == tool_name {
                    return Ok((plugin.manifest.id.clone(), command_name));
                }
            }
        }
        Err(anyhow!("unknown plugin tool: {tool_name}"))
    }
}

#[async_trait::async_trait]
impl ToolHost for LocalToolHost {
    fn scoped_workspace(&self, workspace: &Path) -> Result<Arc<dyn ToolHost + Send + Sync>> {
        let mut host = Self::with_config(
            workspace,
            self.policy.clone(),
            self.runner.clone(),
            &self.launch_config,
        )?;
        host.store = self.store.clone();
        host.context_mcp_executor = self.context_mcp_executor.clone();
        host.mcp_executor = self.mcp_executor.clone();
        Ok(Arc::new(host))
    }

    fn permission(&self, call: &ToolCall) -> ToolPermission {
        self.assess(call).0
    }

    fn approval_details(&self, invocation: Uuid) -> Option<String> {
        self.shell_launches
            .lock()
            .ok()?
            .get(&invocation)
            .map(|(_, plan)| plan.restrictions())
    }

    fn discard_proposal(&self, invocation: Uuid) {
        if let Ok(mut launches) = self.shell_launches.lock() {
            launches.remove(&invocation);
        }
    }

    fn validate(&self, call: &ToolCall) -> Result<()> {
        self.policy.ensure_allowed(call)?;
        anyhow::ensure!(
            self.trusted_workspace
                || !(call.name.starts_with("plugin__")
                    || call.name.starts_with("custom__")
                    || call.name.starts_with("lsp.")
                    || call.name.starts_with("lsp_")
                    || call.name == "diagnostics.check"),
            "tool requires workspace trust"
        );
        let (key, default, allow_missing) = match call.name.as_str() {
            "fs.read" | "fs.edit" | "notebook.read" | "notebook.edit" | "lsp.hover"
            | "lsp.definition" | "lsp.references" | "lsp.symbols" => ("path", None, false),
            "fs.write" => ("path", None, true),
            "fs.list" => ("dir", Some("."), false),
            "fs.glob" | "fs.grep" => ("base", Some("."), false),
            "diagnostics.check" if call.args.get("path").is_some() => ("path", None, false),
            "patch.apply" | "patch.direct" => {
                let diff = if call.name == "patch.apply" {
                    let id = call.args["patch_id"]
                        .as_str()
                        .ok_or_else(|| anyhow!("patch_id missing"))?;
                    self.patches.read_patch(Uuid::parse_str(id)?)?.unified_diff
                } else {
                    self.policy.ensure_allowed(&ToolCall {
                        name: "patch.apply".into(),
                        ..call.clone()
                    })?;
                    call.args["unified_diff"]
                        .as_str()
                        .ok_or_else(|| anyhow!("unified_diff missing"))?
                        .to_owned()
                };
                for target in codingbuddy_diff::checked_patch_paths(&self.workspace, &diff)? {
                    self.policy
                        .check_patch_target(&self.files, Path::new(&target))?;
                }
                return Ok(());
            }
            "multi_edit" => {
                let files = call
                    .args
                    .get("files")
                    .and_then(|value| value.as_array())
                    .ok_or_else(|| anyhow!("files array missing"))?;
                for entry in files {
                    let path = entry
                        .get("path")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| anyhow!("path missing in file entry"))?;
                    let resolved = self.resolve_path(Path::new(path), false)?;
                    let file = ToolCall {
                        name: "fs.edit".to_owned(),
                        args: json!({"path": path}),
                        requires_approval: true,
                    };
                    self.check_resolved_rule(&file, "path", &resolved)?;
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        let path = call
            .args
            .get(key)
            .and_then(|value| value.as_str())
            .or(default)
            .ok_or_else(|| anyhow!("{key} missing"))?;
        let resolved = self.resolve_path(Path::new(path), allow_missing)?;
        self.check_resolved_rule(call, key, &resolved)
    }

    fn cache_key(&self, call: &ToolCall) -> serde_json::Value {
        let (field, default) = match call.name.as_str() {
            "fs.read" => ("path", ""),
            "fs.list" => ("dir", "."),
            "fs.glob" | "fs.grep" => ("base", "."),
            _ => return call.args.clone(),
        };
        let path = call
            .args
            .get(field)
            .and_then(|value| value.as_str())
            .unwrap_or(default);
        let resolved = self.resolve_path(Path::new(path), false).ok();
        json!({"arguments": call.args, "resolved": resolved})
    }

    fn propose(&self, call: ToolCall) -> ToolProposal {
        self.propose_with_permission(call).0
    }

    fn propose_with_permission(&self, call: ToolCall) -> (ToolProposal, ToolPermission) {
        let invocation_id = Uuid::now_v7();
        let (mut permission, launch) = self.assess(&call);
        if let Some(launch) = launch {
            match self.shell_launches.lock() {
                Ok(mut launches) if launches.len() < 256 => {
                    launches.insert(invocation_id, (call.clone(), launch));
                }
                _ => {
                    permission = ToolPermission::Deny {
                        source: "shell policy",
                        reason: "could not reserve a shell launch".into(),
                        target: None,
                    }
                }
            }
        }
        let proposal = ToolProposal {
            invocation_id,
            approved: permission == ToolPermission::Allow,
            call,
        };
        (proposal, permission)
    }

    async fn execute(&self, approved: ApprovedToolCall) -> ToolResult {
        self.execute_with_context(approved, Default::default())
            .await
    }

    async fn verify_command(
        &self,
        command: &str,
        cwd: &Path,
        timeout: u64,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> serde_json::Value {
        self.run_verification_command(command, cwd, timeout, context)
            .await
    }

    async fn execute_with_context(
        &self,
        approved: ApprovedToolCall,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> ToolResult {
        self.run_approved(approved, context).await
    }
}

impl LocalToolHost {
    /// Run an approved call to completion on the current thread.
    pub async fn run_approved(
        &self,
        approved: ApprovedToolCall,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> ToolResult {
        let execution_id = approved.invocation_id;
        let cancellation = context
            .cancellation
            .map(codingbuddy_core::CancellationToken::child_token)
            .unwrap_or_default();
        let mut context = context;
        context.cancellation = Some(&cancellation);
        let payload = json!({"tool":approved.call.name,"workspace":self.workspace});
        let tracked = context.session_id.is_some();
        let _ownership = if tracked {
            if let Err(error) = self.store.begin_operation(
                execution_id,
                context.session_id,
                context.parent_run_id.or(context.prompt_id),
                "tool",
                &payload,
            ) {
                return ToolResult {
                    invocation_id: execution_id,
                    success: false,
                    output: json!({"error":error.to_string(),"status":"persistence_failed"}),
                };
            }
            match self
                .store
                .monitor_operation(execution_id, cancellation.clone())
            {
                Ok(monitor) => Some(monitor),
                Err(error) => {
                    return ToolResult {
                        invocation_id: execution_id,
                        success: false,
                        output: json!({"error":error.to_string(),"status":"persistence_failed"}),
                    };
                }
            }
        } else {
            None
        };
        context.parent_run_id = tracked.then_some(execution_id).or(context.parent_run_id);
        let call = approved.call;
        let shell = self
            .shell_launches
            .lock()
            .ok()
            .and_then(|mut plans| plans.remove(&approved.invocation_id));
        let check_plan = |prepared: Option<&(ToolCall, ShellLaunch)>| -> Result<()> {
            if call.name == "bash.run" {
                let (original, launch) = prepared
                    .ok_or_else(|| anyhow!("shell execution requires a prepared launch"))?;
                anyhow::ensure!(
                    original.name == call.name
                        && original.args == call.args
                        && original.requires_approval == call.requires_approval
                        && launch == &self.shell_launch(&call)?,
                    "shell launch changed since approval"
                );
            }
            Ok(())
        };
        let validate = || {
            self.validate(&call)
                .and_then(|()| check_plan(shell.as_ref()))
        };
        let mut precondition = context.execution().check().and_then(|()| validate());
        if precondition.is_ok() {
            self.execute_hooks_with_context("pretooluse", Some(&call), None, context)
                .await;
            precondition = validate();
        }
        if let Err(error) = precondition {
            if tracked {
                let _ =
                    self.store
                        .transition_operation(execution_id, "running", "denied", &payload);
            }
            return ToolResult {
                invocation_id: approved.invocation_id,
                success: false,
                output: json!({"error": error.to_string()}),
            };
        }
        let execution = self
            .run_tool(&call, shell.map(|(_, launch)| launch), context)
            .await;
        let (mut success, mut output) = match execution {
            Ok(output) => (
                output["isError"] != true
                    && output["cancelled"] != true
                    && output["timed_out"] != true
                    && (call.name != "bash.run" || output["status"] == 0),
                output,
            ),
            Err(err) => {
                let message = err.to_string();
                let payload = if err
                    .downcast_ref::<codingbuddy_core::execution::UnconfirmedSideEffect>()
                    .is_some()
                {
                    json!({"error":message,"recovery_status":"ambiguous"})
                } else {
                    json!({"error": message})
                };
                (false, payload)
            }
        };
        if tracked {
            let status = if cancellation.is_cancelled() || output["recovery_status"] == "ambiguous"
            {
                "ambiguous"
            } else if success {
                "completed"
            } else {
                "failed"
            };
            let settlement = json!({"tool":call.name,"workspace":self.workspace,"success":success,"status":status});
            if self
                .store
                .transition_operation(execution_id, "running", status, &settlement)
                .is_err()
            {
                success = false;
                output["recovery_status"] = json!("ambiguous");
            }
        }
        if !context
            .cancellation
            .is_some_and(codingbuddy_core::CancellationToken::is_cancelled)
        {
            if success {
                self.execute_hooks_with_context("posttooluse", Some(&call), Some(&output), context)
                    .await;
            } else {
                self.execute_hooks_with_context(
                    "posttooluse_failure",
                    Some(&call),
                    Some(&output),
                    context,
                )
                .await;
            }
        }
        ToolResult {
            invocation_id: approved.invocation_id,
            success,
            output,
        }
    }
}

impl LocalToolHost {
    fn assess(&self, call: &ToolCall) -> (ToolPermission, Option<ShellLaunch>) {
        if let Some(deny) = self.policy.denial(call) {
            return (deny, None);
        }
        if let Err(error) = self.validate(call) {
            return (
                ToolPermission::Deny {
                    source: "workspace policy",
                    reason: error.to_string(),
                    target: call
                        .args
                        .get("path")
                        .or_else(|| call.args.get("dir"))
                        .or_else(|| call.args.get("base"))
                        .and_then(serde_json::Value::as_str)
                        .and_then(|path| self.resolve_path(Path::new(path), true).ok())
                        .as_deref()
                        .map(normalize_rel_path),
                },
                None,
            );
        }
        let shell = if call.name == "bash.run" {
            match self.shell_launch(call) {
                Ok(launch) => Some(launch),
                Err(error) => {
                    return (
                        ToolPermission::Deny {
                            source: "shell policy",
                            reason: error.to_string(),
                            target: None,
                        },
                        None,
                    );
                }
            }
        } else {
            None
        };
        (self.permission_with_launch(call, shell.as_ref()), shell)
    }

    fn permission_with_launch(
        &self,
        call: &ToolCall,
        shell: Option<&ShellLaunch>,
    ) -> ToolPermission {
        if let Some(denial) = self.policy.denial(call) {
            return denial;
        }
        let fresh = shell.is_some_and(|plan| !plan.is_isolated() || !self.trusted_workspace)
            || !self.trusted_workspace
                && (call.name.starts_with("github.") || call.name.starts_with("github_"));
        let policy_ask =
            self.policy.requires_approval(call) || self.resolved_path_requires_approval(call);
        if fresh
            || policy_ask
                && !shell.is_some_and(|plan| {
                    plan.is_isolated()
                        && self.sandbox.auto_allow_bash_if_sandboxed
                        && self.policy.permits_sandbox_auto_approval(call)
                })
        {
            return ToolPermission::Ask {
                source: if fresh {
                    "execution restrictions"
                } else {
                    "permission policy"
                },
                reason: shell.map_or_else(
                    || "tool requires approval".into(),
                    ShellLaunch::restrictions,
                ),
                fresh,
            };
        }
        ToolPermission::Allow
    }

    /// Fire "stop" hooks when the agent run completes.
    pub async fn fire_stop_hooks(&self) {
        self.execute_hooks("stop", None, None).await;
    }

    /// Fire hooks for a specific lifecycle phase (sessionstart, notification, etc.).
    pub async fn fire_session_hooks(&self, phase: &str) {
        self.execute_hooks(phase, None, None).await;
    }

    async fn execute_hooks(
        &self,
        phase: &str,
        call: Option<&ToolCall>,
        result: Option<&serde_json::Value>,
    ) {
        self.execute_hooks_with_context(phase, call, result, Default::default())
            .await;
    }
    async fn execute_hooks_with_context(
        &self,
        phase: &str,
        call: Option<&ToolCall>,
        result: Option<&serde_json::Value>,
        execution_context: codingbuddy_core::ToolExecutionContext<'_>,
    ) {
        if execution_context.execution().check().is_err() {
            return;
        }
        if !self.hooks_enabled
            || codingbuddy_policy::load_managed_settings()
                .is_some_and(|settings| settings.allow_managed_hooks_only)
        {
            return;
        }
        let Some(manager) = &self.plugins else {
            return;
        };
        let Ok(hooks) = manager.hook_paths_for(phase) else {
            return;
        };
        let context = HookContext {
            phase: phase.to_string(),
            workspace: self.workspace.clone(),
            tool_name: call.map(|call| call.name.clone()),
            tool_args_json: call.map(|call| call.args.to_string()),
            tool_result_json: result.map(ToString::to_string),
        };
        match HookRuntime::run_legacy_with_context(
            &hooks,
            &context,
            Duration::from_secs(30),
            &execution_context.execution(),
        )
        .await
        {
            Ok(runs) => {
                for run in runs {
                    self.emit_hook_event(
                        phase,
                        Path::new(&run.handler_description),
                        run.success,
                        run.timed_out,
                        run.exit_code,
                    );
                }
            }
            Err(_) => {
                for hook in hooks {
                    self.emit_hook_event(phase, &hook, false, false, None);
                }
            }
        }
    }

    fn emit_hook_event(
        &self,
        phase: &str,
        hook_path: &Path,
        success: bool,
        timed_out: bool,
        exit_code: Option<i32>,
    ) {
        let Ok(Some(session)) = self.store.load_latest_session() else {
            return;
        };
        let event = codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: session.session_id,
            kind: EventKind::HookExecuted {
                phase: phase.to_string(),
                hook_path: hook_path.to_string_lossy().to_string(),
                success,
                timed_out,
                exit_code,
            },
        };
        if let Err(e) = self.store.append_event(&event) {
            codingbuddy_core::logging::notice(format_args!(
                "[codingbuddy] hook: failed to emit event: {e}"
            ));
        }
    }

    fn emit_visual_artifact_event(&self, path: &str, mime: &str) {
        let Ok(Some(session)) = self.store.load_latest_session() else {
            return;
        };
        let event = codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: session.session_id,
            kind: EventKind::VisualArtifactCaptured {
                artifact_id: Uuid::now_v7(),
                path: path.to_string(),
                mime: mime.to_string(),
            },
        };
        if let Err(e) = self.store.append_event(&event) {
            codingbuddy_core::logging::notice(format_args!(
                "[codingbuddy] visual_artifact: failed to emit event: {e}"
            ));
        }
    }

    /// Record post-images for the files a shell command changed since
    /// `checkpoint`, so `/rewind` can undo them. Returns how many were
    /// attributed.
    ///
    /// Only changes the command could have made are attributed: a file
    /// that still exists must have been modified inside the command's run
    /// window. Anything else (an editor autosave, the user's own edit) stays
    /// unattributed, and restore keeps protecting it. Deleted files have no
    /// timestamp and are attributed; restoring one recreates it.
    fn record_shell_changes(
        &self,
        checkpoint: Uuid,
        started: std::time::SystemTime,
        finished: std::time::SystemTime,
    ) -> Result<usize> {
        // Coarse filesystem timestamps (1-2 s on some volumes) need slack.
        let earliest = started - Duration::from_secs(2);
        let latest = finished + Duration::from_secs(1);
        let changed = MemoryManager::new(&self.workspace)?.changed_since_checkpoint(checkpoint)?;
        let mut attributed = 0;
        for path in &changed {
            let full = self.workspace.join(path);
            let in_window = match std::fs::symlink_metadata(&full) {
                Ok(metadata) => last_changed(&metadata)
                    .is_some_and(|changed| changed >= earliest && changed <= latest),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false,
            };
            if in_window {
                self.record_actual_postimage(&full)?;
                attributed += 1;
            }
        }
        Ok(attributed)
    }

    async fn create_checkpoint(&self, reason: &str) -> Result<Option<Uuid>> {
        let manager = MemoryManager::new(&self.workspace)?;
        // Walks and hashes the workspace: hand this worker's tasks off.
        let checkpoint =
            codingbuddy_core::runtime::blocking_section(|| manager.create_checkpoint(reason))?;
        let Ok(Some(session)) = self.store.load_latest_session() else {
            return Ok(Some(checkpoint.checkpoint_id));
        };
        let event = codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: session.session_id,
            kind: EventKind::CheckpointCreated {
                checkpoint_id: checkpoint.checkpoint_id,
                reason: checkpoint.reason,
                files_count: checkpoint.files_count,
                snapshot_path: checkpoint.snapshot_path,
            },
        };
        self.store.append_event(&event)?;
        self.fire_session_hooks("checkpointcreated").await;
        Ok(Some(checkpoint.checkpoint_id))
    }
}

pub type VerificationApproval<'a> = Option<&'a mut (dyn FnMut(&ToolCall) -> Result<bool> + Send)>;

impl LocalToolHost {
    pub async fn run_verification_command(
        &self,
        command: &str,
        cwd: &Path,
        timeout: u64,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> serde_json::Value {
        self.run_verification_with_approval(command, cwd, timeout, context, None)
            .await
    }

    pub async fn run_verification_with_approval(
        &self,
        command: &str,
        cwd: &Path,
        timeout: u64,
        context: codingbuddy_core::ToolExecutionContext<'_>,
        mut approval: VerificationApproval<'_>,
    ) -> serde_json::Value {
        let id = Uuid::now_v7();
        let started = std::time::Instant::now();
        let mut result = json!({"invocation_id":id,"command":command,"success":false,"status":"skipped","stdout":"","stderr":""});
        if self
            .store
            .begin_operation(
                id,
                context.session_id,
                context.parent_run_id.or(context.prompt_id),
                "verification",
                &result,
            )
            .is_err()
        {
            result["status"] = json!("persistence_failed");
            return result;
        }
        if !self.trusted_workspace && approval.is_none() {
            result["status"] = json!("denied");
        } else if context
            .cancellation
            .is_some_and(codingbuddy_core::CancellationToken::is_cancelled)
        {
            result["status"] = json!("cancelled");
        } else {
            let call = ToolCall {
                name: "bash.run".into(),
                args: json!({"cmd":command,"cwd":cwd,"timeout":timeout}),
                requires_approval: false,
            };
            let (proposal, permission) = self.propose_with_permission(call);
            let accepted = if matches!(permission, ToolPermission::Deny { .. }) {
                false
            } else if proposal.approved {
                true
            } else if let Some(callback) = approval.as_mut() {
                let mut display = proposal.call.clone();
                display.args["fresh_approval"] = json!(matches!(
                    permission,
                    ToolPermission::Ask { fresh: true, .. }
                ));
                if let Some(detail) = self.approval_details(proposal.invocation_id) {
                    display.args["effective_isolation"] = json!(detail);
                }
                callback(&display).unwrap_or(false)
            } else {
                false
            };
            if !accepted {
                self.discard_proposal(proposal.invocation_id);
                result["status"] = json!("denied");
            } else {
                // Boxed: run_approved -> run_tool -> verification recurses.
                let output = Box::pin(self.run_approved(
                    ApprovedToolCall {
                        invocation_id: proposal.invocation_id,
                        call: proposal.call,
                    },
                    codingbuddy_core::ToolExecutionContext {
                        parent_run_id: Some(id),
                        ..context
                    },
                ))
                .await;
                result["stdout"] = output.output["stdout"].clone();
                result["stderr"] = output.output["stderr"].clone();
                result["status"] = json!(if output.output["cancelled"] == true {
                    "cancelled"
                } else if output.output["timed_out"] == true {
                    "timed_out"
                } else if output.success {
                    "passed"
                } else if matches!(output.output["status"].as_i64(), Some(127 | 9009)) {
                    "unavailable"
                } else {
                    "failed"
                });
                result["success"] = json!(output.success && result["status"] == "passed");
            }
        }
        result["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
        if self
            .store
            .transition_operation(
                id,
                "running",
                result["status"].as_str().unwrap_or("failed"),
                &result,
            )
            .is_err()
        {
            result["success"] = json!(false);
            result["status"] = json!("ambiguous");
        }
        if let Some(session) = context.session_id
            && self
                .store
                .append_event(&codingbuddy_store::NewEvent::new(
                    session,
                    codingbuddy_core::EventKind::VerificationRun {
                        command: command.into(),
                        success: result["success"] == true,
                        output: result.to_string(),
                    },
                ))
                .is_err()
        {
            result["success"] = json!(false);
            result["status"] = json!("persistence_failed");
        }
        result
    }
    async fn post_edit_checks(
        &self,
        paths: &[PathBuf],
        context: codingbuddy_core::ToolExecutionContext<'_>,
        result: &mut serde_json::Value,
    ) {
        let validator =
            codingbuddy_lsp::EditValidator::new(self.workspace.clone(), Default::default());
        let mut checks = Vec::new();
        for plan in validator.command_plans(paths) {
            checks.push(
                self.run_verification_command(
                    &plan.command,
                    &plan.cwd,
                    plan.timeout_seconds,
                    context,
                )
                .await,
            );
        }
        result["verification"] = json!(checks);
    }
}

#[cfg(test)]
mod web_cancellation_tests {
    use super::*;

    #[test]
    fn web_requests_stop_as_soon_as_the_tool_call_is_cancelled() {
        // A server that accepts the connection and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/slow", listener.local_addr().expect("addr"));
        let _server = std::thread::spawn(move || {
            let _held = listener.accept();
            std::thread::sleep(Duration::from_secs(30));
        });
        let token = codingbuddy_core::CancellationToken::new();
        let canceller = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            canceller.cancel();
        });

        let started = std::time::Instant::now();
        let error = codingbuddy_core::runtime::block_on(http_get_cancellable(
            &url,
            Duration::from_secs(30),
            Some(&token),
        ))
        .expect_err("cancelled fetch must fail");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "fetch kept running for {:?}",
            started.elapsed()
        );
        assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
    }
}
