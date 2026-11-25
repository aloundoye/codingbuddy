pub mod bash_classifier;
pub mod output_scanner;
pub mod privacy;
pub mod shell_parse;

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

/// A permission rule in `Tool(specifier)` format.
/// Evaluation order: deny > ask > allow (first match wins).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    /// The rule string, e.g. "Bash(npm run *)", "Edit(src/**/*.rs)", "WebFetch(domain:example.com)"
    pub rule: String,
    /// The decision: "allow", "deny", or "ask".
    pub decision: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicyTargets {
    pub paths: Vec<String>,
    pub command: Option<String>,
    pub url: Option<String>,
    pub agent: Option<String>,
    pub broad_file_scan: bool,
}

impl ToolPolicyTargets {
    #[must_use]
    pub fn from_call(call: &ToolCall) -> Self {
        let mut targets = Self::default();
        match call.name.as_str() {
            "bash.run" => {
                targets.command = call
                    .args
                    .get("cmd")
                    .or_else(|| call.args.get("command"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            "fs.read" | "fs.edit" | "fs.write" => {
                push_string_arg(&mut targets.paths, &call.args, "path");
                push_string_arg(&mut targets.paths, &call.args, "file_path");
            }
            "fs.glob" => {
                let base = call
                    .args
                    .get("base")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let pattern = call
                    .args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("**/*");
                add_scan_targets(&mut targets, base, pattern);
            }
            "fs.grep" => {
                let base = call
                    .args
                    .get("base")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let glob = call
                    .args
                    .get("glob")
                    .and_then(|v| v.as_str())
                    .unwrap_or("**/*");
                add_scan_targets(&mut targets, base, glob);
            }
            "web.fetch" => {
                targets.url = call
                    .args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            "spawn_task" => {
                targets.agent = call
                    .args
                    .get("subagent_type")
                    .or_else(|| call.args.get("role"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
        targets.paths.sort();
        targets.paths.dedup();
        targets
    }

    fn matches_path_specifier(&self, specifier: &str) -> bool {
        if specifier == "*" {
            return true;
        }
        if self
            .paths
            .iter()
            .any(|target| permission_path_matches(specifier, target))
        {
            return true;
        }
        self.broad_file_scan && is_sensitive_or_specific_path_rule(specifier)
    }
}

fn push_string_arg(paths: &mut Vec<String>, args: &serde_json::Value, key: &str) {
    if let Some(path) = args.get(key).and_then(|v| v.as_str())
        && !path.trim().is_empty()
    {
        paths.push(path.to_string());
    }
}

fn add_scan_targets(targets: &mut ToolPolicyTargets, base: &str, pattern: &str) {
    let base = if base.trim().is_empty() { "." } else { base };
    let pattern = if pattern.trim().is_empty() {
        "**/*"
    } else {
        pattern
    };
    targets.paths.push(base.to_string());
    targets.paths.push(pattern.to_string());
    if base == "." {
        targets.paths.push(pattern.to_string());
    } else {
        targets
            .paths
            .push(format!("{}/{}", base.trim_end_matches('/'), pattern));
    }
    targets.broad_file_scan = matches!(pattern, "*" | "**" | "**/*" | "./**/*");
}

impl PermissionRule {
    /// Parse a rule and check if it matches a tool call.
    /// Returns `Some(decision)` if matched, `None` otherwise.
    pub fn matches(&self, call: &ToolCall) -> Option<&str> {
        let (tool_prefix, specifier) = parse_rule_syntax(&self.rule)?;

        if tool_prefix.eq_ignore_ascii_case("tool") {
            let specifier = codingbuddy_core::ToolName::from_api_name(&specifier)
                .map_or(specifier.as_str(), |tool| tool.as_internal());
            return (specifier == "*" || specifier == call.name).then_some(&self.decision);
        }

        // MCP tool rules: Mcp(server_id) or Mcp(server_id__tool_name)
        if tool_prefix.eq_ignore_ascii_case("mcp") {
            if !call.name.starts_with("mcp__") {
                return None;
            }
            // specifier is either "server_id" (matches all tools) or "server_id__tool_name"
            let call_rest = &call.name["mcp__".len()..];
            if specifier == "*" {
                return Some(&self.decision);
            }
            if specifier.contains("__") {
                // Exact server+tool match
                if call_rest == specifier {
                    return Some(&self.decision);
                }
            } else {
                // Server-only match: "server_id" matches "mcp__server_id__*"
                if call_rest.starts_with(&specifier)
                    && call_rest[specifier.len()..].starts_with("__")
                {
                    return Some(&self.decision);
                }
            }
            return None;
        }

        let tool_name = match tool_prefix.as_str() {
            "Bash" | "bash" => "bash.run",
            "Read" | "read" => "fs.read",
            "Edit" | "edit" => "fs.edit",
            "Write" | "write" => "fs.write",
            "WebFetch" | "webfetch" | "web_fetch" => "web.fetch",
            "Task" | "task" => "spawn_task",
            "Glob" | "glob" => "fs.glob",
            "Grep" | "grep" => "fs.grep",
            _ => return None,
        };

        if call.name != tool_name {
            return None;
        }

        let targets = ToolPolicyTargets::from_call(call);

        // Check specifier match.
        match tool_name {
            "bash.run" => {
                let cmd = targets.command.as_deref().unwrap_or("");
                if glob_command_matches(&specifier, cmd)
                    || self.decision != "allow"
                        && split_pipeline_segments(cmd).is_ok_and(|stages| {
                            stages
                                .iter()
                                .any(|stage| glob_command_matches(&specifier, stage))
                        })
                {
                    Some(&self.decision)
                } else {
                    None
                }
            }
            "fs.read" | "fs.edit" | "fs.write" | "fs.glob" | "fs.grep" => {
                if targets.matches_path_specifier(&specifier) {
                    return Some(&self.decision);
                }
                None
            }
            "web.fetch" => {
                let url = targets.url.as_deref().unwrap_or("");
                if let Some(domain) = specifier.strip_prefix("domain:") {
                    if url.contains(domain) {
                        return Some(&self.decision);
                    }
                } else if url.contains(&specifier) {
                    return Some(&self.decision);
                }
                None
            }
            "spawn_task" => {
                let agent = targets.agent.as_deref().unwrap_or("");
                if agent == specifier || specifier == "*" {
                    Some(&self.decision)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

fn permission_path_matches(specifier: &str, target: &str) -> bool {
    let specifier = normalize_policy_path(specifier);
    let target = normalize_policy_path(target);
    if specifier == "*" || target == specifier {
        return true;
    }
    if let Ok(pattern) = Pattern::new(&specifier)
        && (pattern.matches(&target) || pattern.matches_path(Path::new(&target)))
    {
        return true;
    }
    if let Some(file_name) = Path::new(&target).file_name().and_then(|v| v.to_str())
        && let Ok(pattern) = Pattern::new(&specifier)
        && pattern.matches(file_name)
    {
        return true;
    }
    !is_glob_pattern(&specifier) && target.ends_with(&specifier)
}

fn normalize_policy_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn is_sensitive_or_specific_path_rule(specifier: &str) -> bool {
    let specifier = normalize_policy_path(specifier);
    specifier.contains(".env")
        || specifier.contains("secret")
        || specifier.contains(".ssh")
        || specifier.contains(".aws")
        || specifier.contains("id_*")
}

/// Parse "Tool(specifier)" syntax. Returns (tool, specifier).
fn parse_rule_syntax(rule: &str) -> Option<(String, String)> {
    let rule = rule.trim();
    let paren_pos = rule.find('(')?;
    if !rule.ends_with(')') {
        return None;
    }
    let tool = rule[..paren_pos].trim().to_string();
    let specifier = rule[paren_pos + 1..rule.len() - 1].trim().to_string();
    if tool.is_empty() || specifier.is_empty() {
        return None;
    }
    Some((tool, specifier))
}

/// Check if a glob-like command pattern matches a command string.
fn glob_command_matches(pattern: &str, cmd: &str) -> bool {
    // Simple glob: "npm run *" matches "npm run test", "npm run build", etc.
    let pattern_tokens: Vec<&str> = pattern.split_whitespace().collect();
    let cmd_tokens: Vec<&str> = cmd.split_whitespace().collect();
    if pattern_tokens.is_empty() {
        return false;
    }
    for (i, pt) in pattern_tokens.iter().enumerate() {
        if *pt == "*" {
            return true; // wildcard matches rest
        }
        if i >= cmd_tokens.len() {
            return false;
        }
        if let Some(prefix) = pt.strip_suffix('*') {
            if !cmd_tokens[i].starts_with(prefix) {
                return false;
            }
        } else if !pt.eq_ignore_ascii_case(cmd_tokens[i]) {
            return false;
        }
    }
    cmd_tokens.len() >= pattern_tokens.len()
}

/// Returns a baseline set of safety-oriented permission rules that are always
/// present regardless of user configuration. These protect against catastrophic
/// actions (e.g. `rm -rf *`) and prompt for confirmation on destructive git
/// operations and sensitive file edits.
///
/// Evaluation order (enforced by `evaluate_permission_rules`): deny > ask > allow.
/// User rules are appended *after* these defaults, so a user "allow" cannot
/// override a default "deny", but a user can still add stricter rules.
#[must_use]
pub fn default_deny_rules() -> Vec<PermissionRule> {
    vec![
        // Catastrophic file deletion -- always deny
        PermissionRule {
            rule: "Bash(rm -rf *)".to_string(),
            decision: "deny".to_string(),
        },
        // Force-push -- ask for confirmation
        PermissionRule {
            rule: "Bash(git push --force*)".to_string(),
            decision: "ask".to_string(),
        },
        PermissionRule {
            rule: "Bash(git push -f*)".to_string(),
            decision: "ask".to_string(),
        },
        // Editing secret / environment files -- ask
        PermissionRule {
            rule: "Edit(.env)".to_string(),
            decision: "ask".to_string(),
        },
        PermissionRule {
            rule: "Edit(.env.*)".to_string(),
            decision: "ask".to_string(),
        },
        // Editing inside node_modules -- always deny
        PermissionRule {
            rule: "Edit(node_modules/**)".to_string(),
            decision: "deny".to_string(),
        },
        // Hard reset -- ask for confirmation
        PermissionRule {
            rule: "Bash(git reset --hard*)".to_string(),
            decision: "ask".to_string(),
        },
        // Discarding local changes -- ask for confirmation
        PermissionRule {
            rule: "Bash(git checkout -- *)".to_string(),
            decision: "ask".to_string(),
        },
    ]
}

/// Evaluate permission rules against a tool call.
/// Returns: "allow", "deny", "ask", or None if no rule matched.
/// Evaluation order: deny > ask > allow (strongest match wins).
#[must_use]
pub fn evaluate_permission_rules(rules: &[PermissionRule], call: &ToolCall) -> Option<String> {
    let mut result: Option<&str> = None;
    for rule in rules {
        if let Some(decision) = rule.matches(call) {
            match decision {
                "deny" => return Some("deny".to_string()), // deny wins immediately
                "ask" => {
                    if result.is_none() || result == Some("allow") {
                        result = Some("ask");
                    }
                }
                "allow" => {
                    if result.is_none() {
                        result = Some("allow");
                    }
                }
                _ => {}
            }
        }
    }
    result.map(|s| s.to_string())
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
        self.cfg.permission_rules.last().unwrap()
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

#[derive(Debug, Clone, Deserialize)]
struct TeamPolicyFile {
    #[serde(default)]
    approve_edits: Option<String>,
    #[serde(default)]
    approve_bash: Option<String>,
    #[serde(default)]
    allowlist: Vec<String>,
    #[serde(default)]
    deny_commands: Vec<String>,
    #[serde(default)]
    block_paths: Vec<String>,
    #[serde(default)]
    redact_patterns: Vec<String>,
    #[serde(default)]
    sandbox_mode: Option<String>,
    #[serde(default)]
    sandbox_wrapper: Option<String>,
    #[serde(default)]
    permission_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TeamPolicyLocks {
    pub path: String,
    pub approve_edits_locked: bool,
    pub approve_bash_locked: bool,
    pub allowlist_locked: bool,
    pub sandbox_mode_locked: bool,
    pub permission_mode_locked: bool,
}

impl TeamPolicyLocks {
    pub fn has_permission_locks(&self) -> bool {
        self.approve_edits_locked
            || self.approve_bash_locked
            || self.allowlist_locked
            || self.sandbox_mode_locked
            || self.permission_mode_locked
    }
}

pub fn team_policy_locks() -> Option<TeamPolicyLocks> {
    let (path, team) = load_team_policy_override_with_path()?;
    Some(TeamPolicyLocks {
        path: path.to_string_lossy().to_string(),
        approve_edits_locked: team.approve_edits.is_some(),
        approve_bash_locked: team.approve_bash.is_some(),
        allowlist_locked: !team.allowlist.is_empty(),
        sandbox_mode_locked: team
            .sandbox_mode
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        permission_mode_locked: team
            .permission_mode
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
    })
}

fn load_team_policy_override() -> Option<TeamPolicyFile> {
    load_team_policy_override_with_path().map(|(_, team)| team)
}

fn resolve_team_policy_path() -> Option<std::path::PathBuf> {
    std::env::var("CODINGBUDDY_TEAM_POLICY_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| std::path::PathBuf::from(home).join(".codingbuddy/team-policy.json"))
        })
}

fn load_team_policy_override_with_path() -> Option<(std::path::PathBuf, TeamPolicyFile)> {
    let path = resolve_team_policy_path()?;
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    let team = serde_json::from_str(&raw).ok()?;
    Some((path, team))
}

fn apply_team_policy_override(mut base: PolicyConfig, team: &TeamPolicyFile) -> PolicyConfig {
    if let Some(mode) = team.approve_edits.as_deref() {
        base.approve_edits = parse_approval_mode(mode);
    }
    if let Some(mode) = team.approve_bash.as_deref() {
        base.approve_bash = parse_approval_mode(mode);
    }
    if !team.allowlist.is_empty() {
        // Team-managed allowlist wins over local config.
        base.allowlist = team.allowlist.clone();
    }
    if !team.deny_commands.is_empty() {
        let mut denied = base.denied_command_prefixes;
        denied.extend(team.deny_commands.iter().cloned());
        denied.sort();
        denied.dedup();
        base.denied_command_prefixes = denied;
    }
    if !team.block_paths.is_empty() {
        let mut blocked = base.denied_secret_paths;
        blocked.extend(team.block_paths.iter().cloned());
        blocked.sort();
        blocked.dedup();
        base.denied_secret_paths = blocked;
    }
    if !team.redact_patterns.is_empty() {
        let mut patterns = base.redact_patterns;
        patterns.extend(team.redact_patterns.iter().cloned());
        patterns.sort();
        patterns.dedup();
        base.redact_patterns = patterns;
    }
    if let Some(mode) = team.sandbox_mode.as_deref() {
        let normalized = mode.trim().to_ascii_lowercase();
        if !normalized.is_empty() {
            base.sandbox_mode = normalized;
        }
    }
    if let Some(wrapper) = team.sandbox_wrapper.as_deref() {
        let normalized = wrapper.trim().to_string();
        base.sandbox_wrapper = if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
    }
    if let Some(mode) = team.permission_mode.as_deref() {
        base.permission_mode = PermissionMode::from_str_lossy(mode);
    }
    base
}

// ──────────────────────────────────────────────────────────────────────────────
// Managed Settings (enterprise/team system-wide controls)
// ──────────────────────────────────────────────────────────────────────────────

/// System-wide managed settings for enterprise/team deployments.
/// Loaded from platform-specific paths and enforced as immutable overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedSettings {
    /// Prevent use of bypassPermissions mode entirely.
    pub disable_bypass_permissions_mode: bool,
    /// Only allow permission rules defined in managed settings.
    pub allow_managed_permission_rules_only: bool,
    /// Only allow hooks defined in managed settings.
    pub allow_managed_hooks_only: bool,
    pub hooks: serde_json::Value,
    /// Allowlist of MCP server IDs (empty = all allowed).
    pub allowed_mcp_servers: Vec<String>,
    /// Denylist of MCP server IDs (checked after allowlist).
    pub denied_mcp_servers: Vec<String>,
    /// Managed permission rules (added to or replace user rules).
    #[serde(default)]
    pub permission_rules: Vec<PermissionRule>,
    /// Force a specific permission mode.
    pub permission_mode: Option<String>,
    /// P5-18: Hard limit on allowed tools (empty = all allowed).
    /// When set, only these tools can be used regardless of user config.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// P5-18: Hard limit on tool-use loop turns.
    /// When set, overrides user's max_turns if it exceeds this value.
    pub max_turns: Option<u64>,
    /// P5-18: Maximum USD budget per session.
    /// When set, the session will stop after exceeding this budget.
    pub max_budget_usd: Option<f64>,
    /// P5-18: Blocked tools that cannot be used regardless of other settings.
    #[serde(default)]
    pub blocked_tools: Vec<String>,
}

/// Platform-specific path for managed settings.
pub fn managed_settings_path() -> Option<std::path::PathBuf> {
    codingbuddy_core::process_env::managed_path()
}

/// Load managed settings from the platform-specific path.
pub fn load_managed_settings() -> Option<ManagedSettings> {
    let path = managed_settings_path()?;
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Check if an MCP server is allowed by managed settings.
pub fn is_mcp_server_allowed(server_id: &str, managed: &ManagedSettings) -> bool {
    // If denied list contains this server, block it.
    if managed
        .denied_mcp_servers
        .iter()
        .any(|s| s == server_id || s == "*")
    {
        return false;
    }
    // If allowed list is non-empty, server must be in it.
    if !managed.allowed_mcp_servers.is_empty() {
        return managed
            .allowed_mcp_servers
            .iter()
            .any(|s| s == server_id || s == "*");
    }
    true // default: allow
}

impl ManagedSettings {
    /// Apply managed settings overrides to the full AppConfig.
    ///
    /// Enforces: permission_mode, max_turns cap, disable_bypass_permissions.
    /// Called after loading config so enterprise constraints are immutable.
    pub fn apply_to_config(&self, cfg: &mut codingbuddy_core::AppConfig) {
        // Force permission mode at the config level
        if let Some(ref mode) = self.permission_mode
            && let Ok(parsed) = mode.parse::<codingbuddy_core::PermissionMode>()
        {
            cfg.policy.permission_mode = parsed;
        }

        // Cap max_turns if managed setting is lower
        if let Some(managed_max) = self.max_turns {
            let current = cfg.agent_loop.max_iterations;
            if current > managed_max {
                cfg.agent_loop.max_iterations = managed_max;
            }
        }

        // Prevent bypass permissions mode — force to Ask
        // (codingbuddy-core's PermissionMode doesn't have Bypass, so this catches
        // the codingbuddy-policy level PermissionMode::BypassPermissions)
        if self.disable_bypass_permissions_mode {
            let policy_mode = PermissionMode::from_str_lossy(
                &serde_json::to_string(&cfg.policy.permission_mode).unwrap_or_default(),
            );
            if policy_mode == PermissionMode::BypassPermissions {
                cfg.policy.permission_mode = codingbuddy_core::PermissionMode::Ask;
            }
        }
    }
}

/// Apply managed settings enforcement to a PolicyEngine.
/// This is called after normal construction to apply enterprise overrides.
pub fn apply_managed_settings(engine: &mut PolicyEngine, managed: &ManagedSettings) {
    // Force permission mode if set.
    if let Some(ref mode) = managed.permission_mode {
        engine.permission_mode = PermissionMode::from_str_lossy(mode);
    }
    // Prevent bypass mode if disabled by managed settings.
    if managed.disable_bypass_permissions_mode
        && engine.permission_mode == PermissionMode::BypassPermissions
    {
        engine.permission_mode = PermissionMode::Ask;
    }
    // Apply managed permission rules.
    if managed.allow_managed_permission_rules_only {
        engine.cfg.permission_rules = managed.permission_rules.clone();
        engine.configured_denials.clear();
        engine.builtin_rule_count = 0;
    } else {
        // Merge: managed rules take priority (prepended).
        let mut merged = managed.permission_rules.clone();
        merged.append(&mut engine.cfg.permission_rules);
        engine.cfg.permission_rules = merged;
    }
    engine.managed_rule_count = managed.permission_rules.len();
}

#[cfg(test)]
mod tests;
