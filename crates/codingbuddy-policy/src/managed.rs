//! Team policy overrides and enterprise managed settings.

use super::*;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TeamPolicyFile {
    #[serde(default)]
    pub(crate) approve_edits: Option<String>,
    #[serde(default)]
    pub(crate) approve_bash: Option<String>,
    #[serde(default)]
    pub(crate) allowlist: Vec<String>,
    #[serde(default)]
    pub(crate) deny_commands: Vec<String>,
    #[serde(default)]
    pub(crate) block_paths: Vec<String>,
    #[serde(default)]
    pub(crate) redact_patterns: Vec<String>,
    #[serde(default)]
    pub(crate) sandbox_mode: Option<String>,
    #[serde(default)]
    pub(crate) sandbox_wrapper: Option<String>,
    #[serde(default)]
    pub(crate) permission_mode: Option<String>,
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

pub(crate) fn load_team_policy_override() -> Option<TeamPolicyFile> {
    load_team_policy_override_with_path().map(|(_, team)| team)
}

pub(crate) fn resolve_team_policy_path() -> Option<std::path::PathBuf> {
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

pub(crate) fn load_team_policy_override_with_path() -> Option<(std::path::PathBuf, TeamPolicyFile)>
{
    let path = resolve_team_policy_path()?;
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    let team = serde_json::from_str(&raw).ok()?;
    Some((path, team))
}

pub(crate) fn apply_team_policy_override(
    mut base: PolicyConfig,
    team: &TeamPolicyFile,
) -> PolicyConfig {
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
