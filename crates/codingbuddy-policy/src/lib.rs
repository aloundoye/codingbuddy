pub mod bash_classifier;
mod managed;
pub mod output_scanner;
pub mod privacy;
mod rules;
pub mod shell_parse;
pub use managed::*;
pub use rules::*;

use codingbuddy_core::ToolCall;
use glob::Pattern;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    Ask,
    Auto,
    Plan,
    /// Auto-accepts file edits; still prompts for bash commands.
    AcceptEdits,
    /// Auto-denies everything unless pre-approved via allow rules.
    DontAsk,
    Locked,
    /// Skip ALL permission checks — requires both `--dangerously-skip-permissions`
    /// and `--allow-dangerously-skip-permissions` CLI flags.
    BypassPermissions,
}

impl PermissionMode {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => PermissionMode::Auto,
            "plan" => PermissionMode::Plan,
            "acceptedits" | "accept-edits" | "accept_edits" => PermissionMode::AcceptEdits,
            "dontask" | "dont-ask" | "dont_ask" => PermissionMode::DontAsk,
            "locked" => PermissionMode::Locked,
            "bypasspermissions" | "bypass-permissions" | "bypass_permissions" | "bypass" => {
                PermissionMode::BypassPermissions
            }
            _ => PermissionMode::Ask,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask",
            PermissionMode::Auto => "auto",
            PermissionMode::Plan => "plan",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Locked => "locked",
            PermissionMode::BypassPermissions => "bypassPermissions",
        }
    }

    /// Cycle to the next mode: ask → auto → acceptEdits → plan → dontAsk → locked → ask.
    /// BypassPermissions is NOT included in the cycle — it must be set explicitly.
    pub fn cycle(&self) -> Self {
        match self {
            PermissionMode::Ask => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::DontAsk,
            PermissionMode::DontAsk => PermissionMode::Locked,
            PermissionMode::Locked => PermissionMode::Ask,
            PermissionMode::BypassPermissions => PermissionMode::Ask,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub approve_edits: bool,
    pub approve_bash: bool,
    pub allowlist: Vec<String>,
    pub denied_secret_paths: Vec<String>,
    pub denied_command_prefixes: Vec<String>,
    pub redact_patterns: Vec<String>,
    pub sandbox_mode: String,
    pub sandbox_wrapper: Option<String>,
    pub permission_mode: PermissionMode,
    /// Granular permission rules in Tool(specifier) format.
    #[serde(default)]
    pub permission_rules: Vec<PermissionRule>,
    /// Bash command patterns persistently approved for this project (loaded from store).
    #[serde(default)]
    pub persistent_bash_approvals: Vec<String>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            approve_edits: true,
            approve_bash: true,
            allowlist: vec![
                "rg".to_string(),
                "find .".to_string(),
                "head".to_string(),
                "git status".to_string(),
                "git diff".to_string(),
                "git show".to_string(),
                "cargo test".to_string(),
                "cargo fmt --check".to_string(),
                "cargo clippy".to_string(),
            ],
            denied_secret_paths: vec![
                ".env".to_string(),
                ".ssh".to_string(),
                ".aws".to_string(),
                ".gnupg".to_string(),
                "**/id_*".to_string(),
                "**/secret".to_string(),
            ],
            denied_command_prefixes: vec![
                "rm".to_string(),
                "rmdir".to_string(),
                "del".to_string(),
                "rd".to_string(),
                "mkfs".to_string(),
                "dd".to_string(),
                "format".to_string(),
                "shutdown".to_string(),
                "reboot".to_string(),
                "poweroff".to_string(),
            ],
            redact_patterns: vec![
                "(?i)(api[_-]?key|token|secret|password)\\s*[:=]\\s*['\"]?[a-z0-9_\\-]{8,}['\"]?"
                    .to_string(),
                "\\b\\d{3}-\\d{2}-\\d{4}\\b".to_string(),
                "(?i)\\b(mrn|medical_record_number|patient_id)\\s*[:=]\\s*[a-z0-9\\-]{4,}\\b"
                    .to_string(),
            ],
            sandbox_mode: "allowlist".to_string(),
            sandbox_wrapper: None,
            permission_mode: PermissionMode::Ask,
            permission_rules: vec![],
            persistent_bash_approvals: vec![],
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum PolicyError {
    #[error("path traversal denied")]
    PathTraversal,
    #[error("secret path denied")]
    SecretPath,
    #[error("command is not allowlisted")]
    CommandNotAllowed,
    #[error("command contains forbidden shell metacharacters")]
    CommandInjection,
    #[error("command prefix is blocked by policy")]
    DangerousCommand,
}

/// Maximum audit log entries before oldest are discarded.
const MAX_AUDIT_ENTRIES: usize = 1000;

/// Decision recorded in the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditDecision {
    AutoApproved,
    Denied,
}

/// Audit log entry recorded when permissions are auto-approved (e.g. bypass mode).
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub tool_name: String,
    pub mode: PermissionMode,
    pub decision: AuditDecision,
    pub timestamp: std::time::SystemTime,
}

#[derive(Debug)]
pub struct PolicyEngine {
    cfg: PolicyConfig,
    managed_rule_count: usize,
    builtin_rule_count: usize,
    configured_denials: Vec<(PermissionRule, codingbuddy_core::process_env::Source)>,
    secret_regexes: Vec<Regex>,
    permission_mode: PermissionMode,
    /// Workspace root for path canonicalization. Absolute paths under this root
    /// are permitted; symlinks resolving outside it are blocked.
    workspace_root: Option<std::path::PathBuf>,
    /// Audit trail for auto-approved tool calls.
    audit_log: std::sync::Mutex<Vec<AuditEntry>>,
}

impl Clone for PolicyEngine {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            managed_rule_count: self.managed_rule_count,
            builtin_rule_count: self.builtin_rule_count,
            configured_denials: self.configured_denials.clone(),
            secret_regexes: self.secret_regexes.clone(),
            permission_mode: self.permission_mode,
            workspace_root: self.workspace_root.clone(),
            audit_log: std::sync::Mutex::new(
                self.audit_log
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
        }
    }
}

impl PolicyEngine {
    pub fn new(cfg: PolicyConfig) -> Self {
        let mut secret_regexes = cfg
            .redact_patterns
            .iter()
            .filter_map(|pattern| Regex::new(pattern).ok())
            .collect::<Vec<_>>();
        if secret_regexes.is_empty() {
            secret_regexes = PolicyConfig::default()
                .redact_patterns
                .into_iter()
                .filter_map(|pattern| Regex::new(&pattern).ok())
                .collect::<Vec<_>>();
        }
        let permission_mode = cfg.permission_mode;
        // Prepend safety defaults so they are always evaluated first.
        // Because deny > ask > allow in evaluate_permission_rules, user
        // rules cannot downgrade a default "deny" to "allow".
        let mut merged_rules = default_deny_rules();
        let builtin_rule_count = merged_rules.len();
        merged_rules.extend(cfg.permission_rules.clone());
        let cfg = PolicyConfig {
            permission_rules: merged_rules,
            ..cfg
        };
        Self {
            cfg,
            managed_rule_count: 0,
            builtin_rule_count,
            configured_denials: Vec::new(),
            secret_regexes,
            permission_mode,
            workspace_root: None,
            audit_log: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Create a PolicyEngine with default settings but a specific permission mode string.
    pub fn from_mode(mode: &str) -> Self {
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::from_str_lossy(mode),
            ..PolicyConfig::default()
        };
        Self::new(cfg)
    }

    /// Set the workspace root for path canonicalization and symlink checks.
    pub fn set_workspace_root(&mut self, root: std::path::PathBuf) {
        self.workspace_root = Some(root);
    }

    /// Return a snapshot of the audit log entries.
    pub fn audit_entries(&self) -> Vec<AuditEntry> {
        self.audit_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn from_app_config(cfg: &codingbuddy_core::PolicyConfig) -> Self {
        let defaults = PolicyConfig::default();
        let mut mapped = PolicyConfig {
            approve_edits: cfg.approve_edits != codingbuddy_core::ApprovalMode::Never,
            approve_bash: cfg.approve_bash != codingbuddy_core::ApprovalMode::Never,
            allowlist: cfg.allowlist.clone(),
            denied_secret_paths: if cfg.block_paths.is_empty() {
                defaults.denied_secret_paths.clone()
            } else {
                cfg.block_paths.clone()
            },
            denied_command_prefixes: defaults.denied_command_prefixes,
            redact_patterns: if cfg.redact_patterns.is_empty() {
                defaults.redact_patterns
            } else {
                cfg.redact_patterns.clone()
            },
            sandbox_mode: cfg.sandbox_mode.to_string(),
            sandbox_wrapper: cfg
                .sandbox_wrapper
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            permission_mode: PermissionMode::from_str_lossy(&cfg.permission_mode.to_string()),
            permission_rules: cfg
                .permission_rules
                .iter()
                .map(|r| PermissionRule {
                    rule: r.rule.clone(),
                    decision: r.decision.clone(),
                })
                .collect(),
            persistent_bash_approvals: vec![],
        };
        if let Some(team_policy) = load_team_policy_override() {
            mapped = apply_team_policy_override(mapped, &team_policy);
        }
        let mut engine = Self::new(mapped);
        engine.configured_denials = cfg
            .configured_denials
            .iter()
            .map(|(rule, source)| {
                (
                    PermissionRule {
                        rule: rule.clone(),
                        decision: "deny".into(),
                    },
                    *source,
                )
            })
            .collect();
        // Apply managed settings (enterprise overrides).
        if let Some(managed) = load_managed_settings() {
            apply_managed_settings(&mut engine, &managed);
        }
        engine
    }

    pub fn check_path(&self, path: &str) -> Result<(), PolicyError> {
        let candidate = Path::new(path);
        // Treat rooted paths as absolute-like on all platforms.
        // On Windows, "/foo" is rooted (current drive) but `is_absolute()` is false.
        // We still need to constrain it to the workspace root.
        let absolute_like = candidate.is_absolute() || candidate.has_root();

        // Block absolute paths unless workspace root is set and path is under it.
        if absolute_like {
            if let Some(ref root) = self.workspace_root {
                if !candidate.starts_with(root) {
                    return Err(PolicyError::PathTraversal);
                }
            } else {
                return Err(PolicyError::PathTraversal);
            }
        }

        // Block parent directory traversal.
        if candidate
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(PolicyError::PathTraversal);
        }

        if let Some(root) = &self.workspace_root {
            let workspace = codingbuddy_core::workspace_fs::WorkspaceFs::open(root)
                .map_err(|_| PolicyError::PathTraversal)?;
            let resolved = workspace
                .resolve(candidate, true)
                .map_err(|_| PolicyError::PathTraversal)?;
            self.check_path_name(&resolved)?;
        }
        self.check_path_name(candidate)
    }

    /// Check a requested or resolved path name without performing filesystem I/O.
    pub fn check_path_name(&self, path: &Path) -> Result<(), PolicyError> {
        let normalized = path
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        if self.path_is_blocked(&normalized) {
            return Err(PolicyError::SecretPath);
        }
        Ok(())
    }

    pub fn check_command(&self, cmd: &str) -> Result<(), PolicyError> {
        if shell_parse::contains_forbidden_constructs(cmd) {
            return Err(PolicyError::CommandInjection);
        }
        let segments = split_pipeline_segments(cmd)?;
        for segment in segments {
            let cmd_tokens: Vec<&str> = segment.split_whitespace().collect();
            if cmd_tokens.is_empty() {
                return Err(PolicyError::CommandNotAllowed);
            }
            let command_name = cmd_tokens[0].to_ascii_lowercase();
            if self
                .cfg
                .denied_command_prefixes
                .iter()
                .any(|prefix| prefix.eq_ignore_ascii_case(&command_name))
            {
                return Err(PolicyError::DangerousCommand);
            }
            let allowlisted = self
                .cfg
                .allowlist
                .iter()
                .any(|allowed| allow_pattern_matches(allowed, &cmd_tokens));
            if !allowlisted {
                return Err(PolicyError::CommandNotAllowed);
            }
        }
        Ok(())
    }

    pub fn redact(&self, text: &str) -> String {
        self.secret_regexes
            .iter()
            .fold(text.to_string(), |acc, regex| {
                regex.replace_all(&acc, "[REDACTED]").to_string()
            })
    }

    pub fn denies_tool(&self, call: &ToolCall) -> bool {
        self.denial(call).is_some()
    }

    pub fn ensure_allowed(&self, call: &ToolCall) -> anyhow::Result<()> {
        if let Some(codingbuddy_core::ToolPermission::Deny { reason, .. }) = self.denial(call) {
            anyhow::bail!("{reason}");
        }
        Ok(())
    }

    pub fn check_patch_target(
        &self,
        workspace: &codingbuddy_core::workspace_fs::WorkspaceFs,
        path: &Path,
    ) -> anyhow::Result<()> {
        let resolved = workspace.resolve(path, true)?;
        for candidate in [workspace.relative(path)?, resolved] {
            self.check_path_name(&candidate)?;
            let call = ToolCall {
                name: "fs.edit".into(),
                args: serde_json::json!({"path": candidate.to_string_lossy().replace('\\', "/")}),
                requires_approval: true,
            };
            self.ensure_allowed(&call)?;
        }
        Ok(())
    }

    pub fn permits_sandbox_auto_approval(&self, call: &ToolCall) -> bool {
        !matches!(
            self.permission_mode,
            PermissionMode::Plan | PermissionMode::Locked | PermissionMode::DontAsk
        ) && evaluate_permission_rules(&self.cfg.permission_rules, call).as_deref() != Some("ask")
    }

    pub fn denial(&self, call: &ToolCall) -> Option<codingbuddy_core::ToolPermission> {
        use codingbuddy_core::ToolPermission;
        let why = |source, reason| ToolPermission::Deny {
            source,
            reason,
            target: None,
        };
        let managed = load_managed_settings();
        if managed.as_ref().is_some_and(|settings| {
            settings
                .permission_rules
                .iter()
                .any(|rule| rule.matches(call) == Some("deny"))
        }) {
            return Some(why(
                "managed settings",
                "tool denied by a managed permission rule".into(),
            ));
        }
        if let Some((_, source)) = self
            .configured_denials
            .iter()
            .find(|(rule, _)| rule.matches(call).is_some())
        {
            use codingbuddy_core::process_env::Source;
            return Some(why(
                match source {
                    Source::User => "user settings",
                    Source::Project => "project settings",
                    Source::Managed => "managed settings",
                },
                "tool denied by an explicit permission rule".into(),
            ));
        }
        if let Some((index, _)) = self
            .cfg
            .permission_rules
            .iter()
            .enumerate()
            .find(|(_, rule)| rule.matches(call) == Some("deny"))
        {
            let source = if index < self.managed_rule_count {
                "managed settings"
            } else if index < self.managed_rule_count + self.builtin_rule_count {
                "built-in policy"
            } else {
                "configured permission rules"
            };
            return Some(why(
                source,
                "tool denied by an explicit permission rule".into(),
            ));
        }
        if self.permission_mode == PermissionMode::Locked && self.is_blocked_in_locked_mode(call) {
            return Some(why(
                "permission mode",
                "locked mode blocks all non-read operations".into(),
            ));
        }
        let mcp_id = call
            .name
            .strip_prefix("mcp__")
            .and_then(|name| name.split_once("__"));
        let matches_tool = |name: &String| {
            name == "*"
                || name == &call.name
                || codingbuddy_core::ToolName::from_api_name(name)
                    .is_some_and(|tool| tool.as_internal() == call.name)
        };
        if let Some(settings) = managed
            && (settings.blocked_tools.iter().any(matches_tool)
                || mcp_id.is_some_and(|(id, _)| !is_mcp_server_allowed(id, &settings))
                || !settings.allowed_tools.is_empty()
                    && !settings.allowed_tools.iter().any(matches_tool))
        {
            return Some(why(
                "managed settings",
                "tool denied by managed policy".into(),
            ));
        }
        None
    }

    pub fn requires_approval(&self, call: &ToolCall) -> bool {
        if self.denies_tool(call) {
            return true;
        }
        // BypassPermissions waives ordinary prompts after binding denies have been checked.
        if self.permission_mode == PermissionMode::BypassPermissions {
            if let Ok(mut log) = self.audit_log.lock() {
                let len = log.len();
                if len >= MAX_AUDIT_ENTRIES {
                    log.drain(..len / 2);
                }
                log.push(AuditEntry {
                    tool_name: call.name.clone(),
                    mode: PermissionMode::BypassPermissions,
                    decision: AuditDecision::AutoApproved,
                    timestamp: std::time::SystemTime::now(),
                });
            }
            return false;
        }

        if let Some(decision) = evaluate_permission_rules(&self.cfg.permission_rules, call) {
            return decision != "allow";
        }

        // Check persistent bash approvals (project-scoped, "don't ask again").
        if call.name == "bash.run"
            && let Some(cmd) = call.args.get("cmd").and_then(|v| v.as_str())
            && self.cfg.persistent_bash_approvals.iter().any(|p| p == cmd)
        {
            return false;
        }

        // Auto-approve safe bash commands in permissive modes only.
        // Plan and Locked modes require approval for all writes including safe bash.
        if !matches!(
            self.permission_mode,
            PermissionMode::Plan | PermissionMode::Locked
        ) && (call.name == "bash.run" || call.name == "bash_run")
            && let Some(cmd) = call
                .args
                .get("cmd")
                .or_else(|| call.args.get("command"))
                .and_then(|v| v.as_str())
            && bash_classifier::classify_bash_command(cmd) == bash_classifier::BashSafety::Safe
        {
            return false;
        }

        // MCP tools are non-read-only and require approval under most modes.
        let is_mcp = call.name.starts_with("mcp__");

        match self.permission_mode {
            // BypassPermissions is handled by the early return above.
            PermissionMode::BypassPermissions => unreachable!(),
            PermissionMode::Locked => {
                // In locked mode, all non-read tools require approval (and will be denied).
                if is_mcp {
                    return true;
                }
                !is_read_only_tool(&call.name)
            }
            PermissionMode::DontAsk => {
                // DontAsk: auto-denies all non-read tools unless allowlisted.
                if is_read_only_tool(&call.name) {
                    return false;
                }
                if call.name == "bash.run"
                    && let Some(cmd) = call.args.get("cmd").and_then(|v| v.as_str())
                    && self.check_command(cmd).is_ok()
                {
                    return false; // allowlisted command passes
                }
                true // everything else is denied
            }
            PermissionMode::Plan => {
                // In plan mode, reads are allowed; writes need approval.
                !is_read_only_tool(&call.name)
            }
            PermissionMode::AcceptEdits => {
                // AcceptEdits: file edits auto-approve; bash/MCP still needs approval.
                if is_mcp {
                    return true;
                }
                if is_read_only_tool(&call.name) || is_edit_tool(&call.name) {
                    false
                } else if call.name == "bash.run" {
                    // Bash still requires approval (unless allowlisted).
                    if let Some(cmd) = call.args.get("cmd").and_then(|v| v.as_str())
                        && self.check_command(cmd).is_ok()
                    {
                        return false;
                    }
                    true
                } else {
                    false
                }
            }
            PermissionMode::Auto => {
                // In auto mode, allowlisted tools auto-approve; others still need approval.
                if call.name == "bash.run" {
                    if let Some(cmd) = call.args.get("cmd").and_then(|v| v.as_str())
                        && self.check_command(cmd).is_ok()
                    {
                        return false; // allowlisted command, auto-approve
                    }
                    true
                } else if is_read_only_tool(&call.name) {
                    false
                } else {
                    // For edits/writes: auto mode doesn't prompt
                    false
                }
            }
            PermissionMode::Ask => {
                (call.name == "patch.apply" && self.cfg.approve_edits)
                    || (call.name == "bash.run" && self.cfg.approve_bash)
                    || call.requires_approval
            }
        }
    }

    /// Check if a tool call would be blocked in locked mode.
    pub fn is_blocked_in_locked_mode(&self, call: &ToolCall) -> bool {
        !is_read_only_tool(&call.name)
    }

    /// Dry-run evaluator: returns what would happen to a tool call under the current mode.
    pub fn dry_run(&self, call: &ToolCall) -> PermissionDryRunResult {
        if let Some(codingbuddy_core::ToolPermission::Deny { reason, .. }) = self.denial(call) {
            return PermissionDryRunResult::Denied(reason);
        }
        match self.permission_mode {
            PermissionMode::BypassPermissions => PermissionDryRunResult::AutoApproved,
            // Locked-mode denials are handled by denial() above.
            PermissionMode::Locked => PermissionDryRunResult::Allowed,
            PermissionMode::DontAsk => {
                if is_read_only_tool(&call.name) {
                    PermissionDryRunResult::Allowed
                } else if call.name == "bash.run" {
                    if let Some(cmd) = call.args.get("cmd").and_then(|v| v.as_str())
                        && self.check_command(cmd).is_ok()
                    {
                        return PermissionDryRunResult::AutoApproved;
                    }
                    PermissionDryRunResult::Denied(
                        "dontAsk mode denies non-allowlisted operations".to_string(),
                    )
                } else {
                    PermissionDryRunResult::Denied(
                        "dontAsk mode denies non-allowlisted operations".to_string(),
                    )
                }
            }
            PermissionMode::Plan => {
                if is_read_only_tool(&call.name) {
                    PermissionDryRunResult::Allowed
                } else {
                    PermissionDryRunResult::NeedsApproval
                }
            }
            PermissionMode::AcceptEdits => {
                if is_read_only_tool(&call.name) || is_edit_tool(&call.name) {
                    PermissionDryRunResult::AutoApproved
                } else if call.name == "bash.run" {
                    if let Some(cmd) = call.args.get("cmd").and_then(|v| v.as_str())
                        && self.check_command(cmd).is_ok()
                    {
                        return PermissionDryRunResult::AutoApproved;
                    }
                    PermissionDryRunResult::NeedsApproval
                } else {
                    PermissionDryRunResult::AutoApproved
                }
            }
            PermissionMode::Auto => {
                if is_read_only_tool(&call.name) {
                    PermissionDryRunResult::Allowed
                } else if call.name == "bash.run" {
                    if let Some(cmd) = call.args.get("cmd").and_then(|v| v.as_str())
                        && self.check_command(cmd).is_ok()
                    {
                        return PermissionDryRunResult::AutoApproved;
                    }
                    PermissionDryRunResult::NeedsApproval
                } else {
                    PermissionDryRunResult::AutoApproved
                }
            }
            PermissionMode::Ask => {
                if self.requires_approval(call) {
                    PermissionDryRunResult::NeedsApproval
                } else {
                    PermissionDryRunResult::Allowed
                }
            }
        }
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.permission_mode
    }

    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        let managed = load_managed_settings();
        self.permission_mode = managed
            .as_ref()
            .and_then(|settings| settings.permission_mode.as_deref())
            .map(PermissionMode::from_str_lossy)
            .unwrap_or(mode);
        if self.permission_mode == PermissionMode::BypassPermissions
            && managed.is_some_and(|settings| settings.disable_bypass_permissions_mode)
        {
            self.permission_mode = PermissionMode::Ask;
        }
    }

    pub fn sandbox_mode(&self) -> &str {
        &self.cfg.sandbox_mode
    }

    /// Inject persistent bash command approvals (loaded from project store).
    pub fn set_persistent_bash_approvals(&mut self, approvals: Vec<String>) {
        self.cfg.persistent_bash_approvals = approvals;
    }

    /// Add a persistent permission rule at runtime (e.g. "always allow" from user choice).
    /// The rule is appended to the active rules and takes effect immediately.
    /// Returns the rule for the caller to persist to settings.json if desired.
    pub fn add_runtime_rule(&mut self, rule: PermissionRule) -> &PermissionRule {
        self.cfg.permission_rules.push(rule);
        self.cfg
            .permission_rules
            .last()
            .expect("the rule was pushed just above")
    }

    /// Get all current permission rules (including defaults and runtime additions).
    pub fn permission_rules(&self) -> &[PermissionRule] {
        &self.cfg.permission_rules
    }

    /// Record a user denial and return whether the denial threshold was reached.
    /// After `DENIAL_AUTO_DENY_THRESHOLD` denials for the same tool pattern within
    /// the window, the engine can auto-deny without prompting.
    pub fn record_denial(&self, tool_name: &str) -> bool {
        if let Ok(mut log) = self.audit_log.lock() {
            let len = log.len();
            if len >= MAX_AUDIT_ENTRIES {
                log.drain(..len / 2);
            }
            log.push(AuditEntry {
                tool_name: tool_name.to_string(),
                mode: self.permission_mode,
                decision: AuditDecision::Denied,
                timestamp: std::time::SystemTime::now(),
            });
            let count = log
                .iter()
                .filter(|e| e.tool_name == tool_name && matches!(e.decision, AuditDecision::Denied))
                .count();
            count >= 3
        } else {
            false
        }
    }

    /// Build a permission rule string from a tool call for "always allow" semantics.
    pub fn build_allow_rule_for_call(call: &ToolCall) -> PermissionRule {
        let specifier = if call.name == "bash.run" || call.name == "bash_run" {
            if let Some(cmd) = call.args.get("cmd").and_then(|v| v.as_str()) {
                // Extract the command base (first word) for the glob pattern
                let base = cmd.split_whitespace().next().unwrap_or(cmd);
                format!("Bash({base} *)")
            } else {
                "Bash(*)".to_string()
            }
        } else if call.name.starts_with("fs.") || call.name.starts_with("fs_") {
            if let Some(path) = call.args.get("path").and_then(|v| v.as_str()) {
                let dir = std::path::Path::new(path)
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| ".".to_string());
                format!("Edit({}/**)", dir)
            } else {
                "Edit(*)".to_string()
            }
        } else {
            format!("Tool({})", call.name)
        };
        PermissionRule {
            rule: specifier,
            decision: "allow".to_string(),
        }
    }
}

/// Result of a dry-run evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDryRunResult {
    /// Tool call is allowed (read-only or no gate).
    Allowed,
    /// Tool call would be auto-approved (auto mode).
    AutoApproved,
    /// Tool call needs user approval.
    NeedsApproval,
    /// Tool call would be denied.
    Denied(String),
}

/// Returns true if the tool modifies files (write/edit/patch).
fn is_edit_tool(name: &str) -> bool {
    matches!(
        name,
        "fs.write" | "fs.edit" | "multi_edit" | "patch.stage" | "patch.apply" | "notebook.edit"
    )
}

/// Returns true if the tool is read-only (never modifies state).
fn is_read_only_tool(name: &str) -> bool {
    matches!(name, "fs.search_rg" | "git.log")
        || codingbuddy_core::is_internal_tool_name_read_only(name)
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new(PolicyConfig::default())
    }
}

fn parse_approval_mode(mode: &str) -> bool {
    !matches!(mode, "never" | "false" | "off")
}

fn is_glob_pattern(value: &str) -> bool {
    value.contains('*') || value.contains('?') || value.contains('[')
}

impl PolicyEngine {
    fn path_is_blocked(&self, normalized_path: &str) -> bool {
        if self
            .cfg
            .denied_secret_paths
            .iter()
            .filter(|rule| is_glob_pattern(rule))
            .any(|rule| glob_rule_matches(rule, normalized_path))
        {
            return true;
        }
        self.cfg
            .denied_secret_paths
            .iter()
            .filter(|rule| !is_glob_pattern(rule))
            .any(|needle| normalized_path.contains(&needle.to_ascii_lowercase()))
    }
}

fn glob_rule_matches(rule: &str, normalized_path: &str) -> bool {
    if let Ok(pattern) = Pattern::new(rule)
        && (pattern.matches(normalized_path) || pattern.matches_path(Path::new(normalized_path)))
    {
        return true;
    }
    let relaxed = rule
        .replace("**/", "")
        .replace(['*', '?'], "")
        .to_ascii_lowercase();
    !relaxed.is_empty() && normalized_path.contains(&relaxed)
}

fn allow_pattern_matches(pattern: &str, cmd_tokens: &[&str]) -> bool {
    let allowed_tokens: Vec<&str> = pattern.split_whitespace().collect();
    if allowed_tokens.is_empty() {
        return false;
    }
    if cmd_tokens.len() < allowed_tokens.len() {
        return false;
    }

    for (idx, token) in allowed_tokens.iter().enumerate() {
        if *token == "*" {
            return true;
        }
        let cmd_token = cmd_tokens[idx];
        if let Some(prefix) = token.strip_suffix('*') {
            if !cmd_token.starts_with(prefix) {
                return false;
            }
            continue;
        }
        if !token.eq_ignore_ascii_case(cmd_token) {
            return false;
        }
    }
    true
}

fn split_pipeline_segments(cmd: &str) -> Result<Vec<&str>, PolicyError> {
    // Split on `|` only when not inside single or double quotes.
    let mut segments = Vec::new();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    let bytes = cmd.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'|' if !in_single && !in_double => {
                let seg = cmd[start..i].trim();
                if seg.is_empty() {
                    return Err(PolicyError::CommandInjection);
                }
                segments.push(seg);
                start = i + 1;
            }
            _ => {}
        }
    }
    // Final segment
    let seg = cmd[start..].trim();
    if seg.is_empty() {
        return Err(PolicyError::CommandInjection);
    }
    segments.push(seg);
    Ok(segments)
}

#[cfg(test)]
mod tests;
