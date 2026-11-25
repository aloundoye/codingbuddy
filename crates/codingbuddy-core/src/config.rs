//! Application configuration

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    #[serde(skip)]
    pub workspace_trust: Option<bool>,
    #[serde(skip)]
    pub hook_sources: std::collections::BTreeMap<String, process_env::Source>,
    pub llm: LlmConfig,
    pub agent_loop: AgentLoopConfig,
    pub git: GitWorkflowConfig,
    pub policy: PolicyConfig,
    pub tools: ToolsConfig,
    pub plugins: PluginsConfig,
    pub skills: SkillsConfig,
    pub usage: UsageConfig,
    pub context: ContextConfig,
    pub autopilot: AutopilotConfig,
    pub scheduling: SchedulingConfig,
    pub replay: ReplayConfig,
    pub ui: UiConfig,
    pub experiments: ExperimentsConfig,
    pub telemetry: TelemetryConfig,
    pub index: IndexConfig,
    pub budgets: BudgetsConfig,
    pub theme: ThemeConfig,
    /// Post-edit LSP validation config (which languages to check after edits).
    #[serde(default)]
    pub lsp: LspValidationConfig,
    pub privacy: PrivacyConfig,
    /// Hooks configuration (maps event names to hook definitions).
    /// Stored as raw JSON, parsed by codingbuddy-hooks at runtime.
    #[serde(default)]
    pub hooks: serde_json::Value,
    /// Directory for storing plan files (default: .codingbuddy/plans).
    #[serde(default = "default_plans_directory")]
    pub plans_directory: String,
    /// Output style: adjusts system prompt tone ("concise", "verbose", "normal").
    #[serde(default = "default_output_style")]
    pub output_style: String,
    /// Preferred response language (e.g. "en", "zh", "ja").
    #[serde(default)]
    pub language: String,
    /// Git commit/PR attribution text.
    #[serde(default = "default_attribution")]
    pub attribution: String,
    /// Restrict available models to this list. Empty = all models available.
    #[serde(default)]
    pub available_models: Vec<String>,
    /// Session cleanup period in days (default: 30).
    #[serde(default = "default_cleanup_period_days")]
    pub cleanup_period_days: u32,
    /// Custom status line script (shell command, stdout replaces status line).
    #[serde(default)]
    pub status_line: String,
    /// Custom `@` file autocomplete script.
    #[serde(default)]
    pub file_suggestion: String,
    /// Custom spinner action verbs.
    #[serde(default)]
    pub spinner_verbs: Vec<String>,
    /// Whether to respect .gitignore when listing/searching files (default: true).
    #[serde(default = "default_respect_gitignore")]
    pub respect_gitignore: bool,
}

fn default_plans_directory() -> String {
    ".codingbuddy/plans".to_string()
}
fn default_output_style() -> String {
    "normal".to_string()
}
fn default_attribution() -> String {
    "CodingBuddy".to_string()
}
fn default_cleanup_period_days() -> u32 {
    30
}
fn default_respect_gitignore() -> bool {
    true
}
fn default_tool_loop_max_turns() -> u64 {
    50
}
fn default_agent_loop_max_iterations() -> u64 {
    6
}
fn default_agent_loop_parse_retries() -> u64 {
    2
}
fn default_agent_loop_max_files_per_iteration() -> u64 {
    12
}
fn default_agent_loop_max_file_bytes() -> u64 {
    200_000
}
fn default_agent_loop_max_diff_bytes() -> u64 {
    400_000
}
fn default_agent_loop_verify_timeout_seconds() -> u64 {
    60
}
fn default_agent_loop_max_context_requests_per_iteration() -> u64 {
    3
}
fn default_agent_loop_max_context_range_lines() -> u64 {
    400
}
fn default_agent_loop_context_bootstrap_enabled() -> bool {
    true
}
fn default_agent_loop_context_bootstrap_max_tree_entries() -> u64 {
    120
}
fn default_agent_loop_context_bootstrap_max_readme_bytes() -> u64 {
    24_000
}
fn default_agent_loop_context_bootstrap_max_manifest_bytes() -> u64 {
    16_000
}
fn default_agent_loop_context_bootstrap_max_repo_map_lines() -> u64 {
    80
}
fn default_agent_loop_context_bootstrap_max_audit_findings() -> u64 {
    20
}
fn default_failure_classifier_repeat_threshold() -> u64 {
    2
}
fn default_failure_classifier_similarity_threshold() -> f32 {
    0.8
}
fn default_failure_classifier_fingerprint_lines() -> u64 {
    40
}
fn default_safety_gate_max_files_without_approval() -> u64 {
    8
}
fn default_safety_gate_max_loc_without_approval() -> u64 {
    600
}
fn default_team_auto_enabled() -> bool {
    true
}
fn default_team_complexity_threshold() -> u64 {
    60
}
fn default_team_max_lanes() -> u64 {
    4
}
fn default_team_max_concurrency() -> u64 {
    2
}
fn default_git_auto_commit_on_verify_pass() -> bool {
    false
}
fn default_git_commit_message_template() -> String {
    "codingbuddy: {goal}".to_string()
}
fn default_git_require_signing() -> bool {
    false
}
fn default_ui_thinking_visibility() -> String {
    "concise".to_string()
}
fn default_ui_phase_heartbeat_ms() -> u64 {
    5000
}
fn default_ui_mission_control_max_events() -> u64 {
    400
}

impl AppConfig {
    pub fn user_settings_path() -> Option<PathBuf> {
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())?;
        Some(Path::new(&home).join(".codingbuddy/settings.json"))
    }

    pub fn project_settings_path(workspace: &Path) -> PathBuf {
        runtime_dir(workspace).join("settings.json")
    }

    pub fn project_local_settings_path(workspace: &Path) -> PathBuf {
        runtime_dir(workspace).join("settings.local.json")
    }

    pub fn keybindings_path() -> Option<PathBuf> {
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())?;
        Some(Path::new(&home).join(".codingbuddy/keybindings.json"))
    }

    pub fn config_path(workspace: &Path) -> PathBuf {
        Self::project_settings_path(workspace)
    }

    pub fn legacy_toml_path(workspace: &Path) -> PathBuf {
        runtime_dir(workspace).join("config.toml")
    }

    pub fn load(workspace: &Path) -> Result<Self> {
        let (mut cfg, explicit) = Self::load_with_provider_origin(workspace)?;
        if !explicit {
            cfg.llm.auto_detect_provider();
        }
        Ok(cfg)
    }

    /// Load settings with the provenance needed by interactive provider selection.
    pub fn load_with_provider_origin(workspace: &Path) -> Result<(Self, bool)> {
        Self::load_settings(workspace, workspace_is_trusted(workspace))
    }

    pub(crate) fn load_settings(workspace: &Path, trusted: bool) -> Result<(Self, bool)> {
        let mut explicit = false;
        let mut hook_sources = std::collections::BTreeMap::new();
        let mut configured_denials = std::collections::BTreeMap::new();
        let mut merged = serde_json::to_value(Self::default())?;
        if let Some(root) = merged.as_object_mut() {
            root.remove("privacy");
        }

        let legacy = Self::legacy_toml_path(workspace);
        if !trusted && legacy.is_file() {
            eprintln!("[trust] Ignored legacy project runtime settings");
        }
        if trusted && legacy.exists() {
            let raw = fs::read_to_string(legacy)?;
            let legacy_cfg: toml::Value = toml::from_str(&raw)?;
            let value = serde_json::to_value(legacy_cfg)?;
            explicit |= value.pointer("/llm/provider").is_some();
            record_hook_sources(&mut hook_sources, &value, process_env::Source::Project);
            record_configured_denials(
                &mut configured_denials,
                &value,
                process_env::Source::Project,
            );
            merge_llm_settings(&mut merged, &value);
        }

        let mut paths = Vec::new();
        if let Some(user) = Self::user_settings_path() {
            paths.push((user, true, process_env::Source::User));
        }
        paths.push((
            Self::project_settings_path(workspace),
            trusted,
            process_env::Source::Project,
        ));
        paths.push((
            Self::project_local_settings_path(workspace),
            trusted,
            process_env::Source::Project,
        ));

        for (path, trusted_source, source) in paths {
            if !path.exists() {
                continue;
            }
            let raw = match fs::read_to_string(&path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!(
                        "[config] warning: could not read {}: {e}; skipping",
                        path.display()
                    );
                    continue;
                }
            };
            let mut value: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "[config] warning: malformed JSON in {}: {e}; skipping",
                        path.display()
                    );
                    continue;
                }
            };
            if !trusted_source && retain_safe_project_settings(&mut value) {
                eprintln!("[trust] Ignored runtime settings from {}", path.display());
            }
            record_hook_sources(&mut hook_sources, &value, source);
            record_configured_denials(&mut configured_denials, &value, source);
            explicit |= value.pointer("/llm/provider").is_some();
            merge_llm_settings(&mut merged, &value);
        }

        migrate_privacy_settings(&mut merged);
        let mut cfg: Self = serde_json::from_value(merged)?;
        cfg.workspace_trust = Some(trusted);
        cfg.hook_sources = hook_sources;
        cfg.policy.configured_denials = configured_denials;
        // Older generated settings wrote the implicit DeepSeek URL for every provider.
        if cfg.llm.provider != "deepseek"
            && cfg.llm.endpoint == "https://api.deepseek.com/chat/completions"
            && cfg.llm.active_provider().base_url != "https://api.deepseek.com"
        {
            cfg.llm.endpoint.clear();
        }
        Ok((cfg, explicit))
    }

    pub fn ensure(workspace: &Path) -> Result<Self> {
        Self::load(workspace)
    }

    pub fn save(&self, workspace: &Path) -> Result<()> {
        let path = Self::project_settings_path(workspace);
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("invalid config path"))?,
        )?;
        fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

fn migrate_privacy_settings(settings: &mut serde_json::Value) {
    let Some(root) = settings.as_object_mut() else {
        return;
    };
    let Some(legacy) = root.remove("local_ml") else {
        return;
    };
    let Some(mut privacy) = legacy
        .get("privacy")
        .filter(|value| value.is_object())
        .cloned()
    else {
        return;
    };
    privacy["enabled"] = serde_json::Value::Bool(
        legacy["enabled"].as_bool() == Some(true) && privacy["enabled"].as_bool() == Some(true),
    );
    if let Some(current) = root.remove("privacy") {
        merge_json_value(&mut privacy, &current);
    }
    root.insert("privacy".to_owned(), privacy);
}

// Legacy endpoint and key fields describe the provider selected by their settings layer.
fn retain_safe_project_settings(value: &mut serde_json::Value) -> bool {
    if let Some(settings) = value.as_object_mut() {
        let before = settings.len();
        settings.retain(|key, _| {
            matches!(
                key.as_str(),
                "theme" | "language" | "output_style" | "respect_gitignore"
            )
        });
        return before != settings.len();
    }
    false
}

fn record_hook_sources(
    sources: &mut std::collections::BTreeMap<String, process_env::Source>,
    settings: &serde_json::Value,
    source: process_env::Source,
) {
    if let Some(hooks) = settings.get("hooks").and_then(serde_json::Value::as_object) {
        for event in hooks.keys() {
            sources.insert(event.clone(), source);
        }
    }
}

fn record_configured_denials(
    denials: &mut std::collections::BTreeMap<String, process_env::Source>,
    settings: &serde_json::Value,
    source: process_env::Source,
) {
    if let Some(rules) = settings
        .pointer("/policy/permission_rules")
        .and_then(serde_json::Value::as_array)
    {
        for rule in rules {
            if rule["decision"] == "deny"
                && let Some(pattern) = rule["rule"].as_str()
            {
                denials.entry(pattern.to_owned()).or_insert(source);
            }
        }
    }
}

fn merge_llm_settings(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    let provider = base
        .pointer("/llm/provider")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let profile = format!(
        "/llm/providers/{}",
        provider.replace('~', "~0").replace('/', "~1")
    );
    let changed = overlay
        .pointer("/llm/provider")
        .is_some_and(|v| v.as_str() != Some(provider.as_str()))
        || [
            "/base_url",
            "/api_key_env",
            "/kind",
            "/chat_protocol",
            "/auth_strategy",
            "/cloud",
            "/gateway",
            "/headers",
            "/account_id",
        ]
        .iter()
        .any(|field| {
            let path = format!("{profile}{field}");
            overlay
                .pointer(&path)
                .is_some_and(|v| Some(v) != base.pointer(&path))
        })
        || ["/llm/base_url", "/llm/endpoint", "/llm/api_key_env"]
            .iter()
            .any(|path| {
                overlay
                    .pointer(path)
                    .is_some_and(|v| Some(v) != base.pointer(path))
            });
    if changed && let Some(llm) = base.get_mut("llm").and_then(|v| v.as_object_mut()) {
        llm.remove("api_key");
        llm.insert("endpoint".into(), serde_json::Value::String(String::new()));
    }
    let selected = overlay
        .pointer("/llm/provider")
        .and_then(|v| v.as_str())
        .unwrap_or(&provider)
        .to_owned();
    merge_json_value(base, overlay);
    let profile = format!(
        "/llm/providers/{}",
        selected.replace('~', "~0").replace('/', "~1")
    );
    for (old, new) in [
        ("base_url", "base_url"),
        ("api_key_env", "api_key_env"),
        ("base_model", "models/chat"),
    ] {
        if let Some(value) = overlay.pointer(&format!("/llm/{old}"))
            && overlay.pointer(&format!("{profile}/{new}")).is_none()
            && let Some(slot) = base.pointer_mut(&format!("{profile}/{new}"))
        {
            *slot = value.clone();
        }
    }
}

pub(crate) fn merge_json_value(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base_obj), serde_json::Value::Object(overlay_obj)) => {
            for (key, overlay_value) in overlay_obj {
                if let Some(base_value) = base_obj.get_mut(key) {
                    merge_json_value(base_value, overlay_value);
                } else {
                    base_obj.insert(key.clone(), overlay_value.clone());
                }
            }
        }
        (base_slot, overlay_value) => {
            *base_slot = overlay_value.clone();
        }
    }
}

/// Prompt caching strategy — deprecated, kept only for config deserialization compat.
///
/// DeepSeek uses automatic server-side prefix caching. Client-side `cache_control`
/// annotations cause 400 errors. All variants are now no-ops.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheStrategy {
    #[default]
    Auto,
    Aggressive,
    Off,
}

/// Configuration for a single provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: String,
    pub base_url: String,
    pub api_key_env: String,
    #[serde(default)]
    pub openai_compat_prefix: bool,
    /// Explicit chat protocol override. Kept outside payload_options so model
    /// selection, doctor/status, and adapter dispatch can inspect it without
    /// knowing provider-specific payload knobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_protocol: Option<String>,
    /// Explicit auth strategy override (`bearer`, `x-api-key`, `query-api-key`,
    /// `none`, `aws-sigv4`). Defaults are derived from provider kind/protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_strategy: Option<String>,
    /// Stable provider account, organization, or project ID when one is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudDeployment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayProfile>,
    /// Extra request headers to attach to provider calls after built-in headers.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub discovery: ProviderDiscoveryConfig,
    #[serde(default)]
    pub payload_options: serde_json::Value,
    pub models: ProviderModels,
}

/// Wire options for OpenAI-compatible gateways.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayProfile {
    pub chat_path: String,
    #[serde(default)]
    pub omit_stream_usage_option: bool,
}

/// Selects an explicit cloud data plane, separately from the model profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CloudDeployment {
    Azure {
        resource: String,
        deployment: String,
        #[serde(default)]
        api_version: AzureApiVersion,
        #[serde(default)]
        credential: AzureCredential,
    },
    Vertex {
        project: String,
        location: String,
    },
    Bedrock {
        region: String,
        #[serde(default)]
        profile: Option<String>,
    },
}

impl CloudDeployment {
    pub fn protocol_key(&self) -> &'static str {
        match self {
            Self::Azure { .. } => "openai-chat",
            Self::Vertex { .. } => "gemini-generate-content",
            Self::Bedrock { .. } => "bedrock-converse",
        }
    }

    pub fn auth_key(&self) -> &'static str {
        match self {
            Self::Azure {
                credential: AzureCredential::ApiKey,
                ..
            } => "azure-api-key",
            Self::Azure {
                credential: AzureCredential::Entra,
                ..
            } => "azure-entra",
            Self::Vertex { .. } => "google-adc",
            Self::Bedrock { .. } => "aws-sigv4",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum AzureApiVersion {
    #[default]
    #[serde(rename = "v1")]
    V1,
    #[serde(rename = "2024-10-21")]
    Dated20241021,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AzureCredential {
    #[default]
    ApiKey,
    Entra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderDiscoveryConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    pub refresh_on_startup: bool,
}

impl Default for ProviderDiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            models_endpoint: None,
            strategy: None,
            refresh_on_startup: false,
        }
    }
}

/// Models available for a given provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModels {
    pub chat: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub base_model: String,
    pub max_think_model: String,
    pub provider: String,
    #[serde(default = "default_providers")]
    pub providers: std::collections::HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub capability_overrides: CapabilityRegistryOverrides,
    #[serde(default)]
    pub model_catalog: ModelCatalogConfig,
    pub profile: String,
    pub context_window_tokens: u64,
    pub temperature: f32,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub api_key_env: String,
    pub fast_mode: bool,
    pub language: String,
    pub prompt_cache_enabled: bool,
    pub cache_strategy: CacheStrategy,
    pub timeout_seconds: u64,
    pub max_retries: u8,
    pub retry_base_ms: u64,
    pub stream: bool,
    /// When true, add `/v1` prefix to API paths for OpenAI SDK compatibility.
    #[serde(default)]
    pub openai_compat_prefix: bool,
}

/// Known provider names and their corresponding API key environment variables.
/// Used for auto-detection and provider enumeration.
pub const KNOWN_PROVIDER_ENV_VARS: &[(&str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai-compatible", "OPENAI_API_KEY"),
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("google", "GOOGLE_API_KEY"),
    ("groq", "GROQ_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
    ("ollama", "OLLAMA_HOST"),
    ("azure", "AZURE_OPENAI_API_KEY"),
    ("bedrock", "AWS_ACCESS_KEY_ID"),
    ("vertex", "GOOGLE_APPLICATION_CREDENTIALS"),
    ("mistral", "MISTRAL_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("together", "TOGETHER_API_KEY"),
    ("copilot", "GITHUB_TOKEN"),
];

impl LlmConfig {
    /// Returns the active provider config. Falls back to constructing one from
    /// the top-level fields if the provider key is not in the map.
    pub fn active_provider(&self) -> ProviderConfig {
        if let Some(p) = self.providers.get(&self.provider) {
            return p.clone();
        }
        // Fallback: build from legacy top-level fields
        ProviderConfig {
            kind: self.provider.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            openai_compat_prefix: self.openai_compat_prefix,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: self.base_model.clone(),
                reasoner: Some(self.max_think_model.clone()),
            },
        }
    }

    /// Auto-detect the best provider from available API key env vars.
    ///
    /// Checks env vars in priority order (best model quality first) and
    /// switches the active provider if the current one has no key available.
    /// Called once at config load time — explicit config always wins.
    pub fn auto_detect_provider(&mut self) {
        // If the current provider already has a key, keep it
        let current = self.active_provider();
        if current.cloud.is_some()
            || !current.api_key_env.is_empty() && std::env::var(&current.api_key_env).is_ok()
        {
            return;
        }
        // Also accept explicit api_key in config
        if self.api_key.is_some() {
            return;
        }

        // Check providers in priority order (best model quality first)
        for &(provider_name, env_var) in KNOWN_PROVIDER_ENV_VARS {
            if std::env::var(env_var).is_ok_and(|key| !key.trim().is_empty())
                && self.providers.contains_key(provider_name)
                && !matches!(provider_name, "ollama" | "bedrock" | "vertex")
                && self
                    .providers
                    .get(provider_name)
                    .is_some_and(|p| !p.base_url.is_empty() || p.cloud.is_some())
            {
                self.select_provider(provider_name);
                let p = self.active_provider();
                self.base_url = p.base_url.clone();
                self.api_key_env = p.api_key_env.clone();
                self.base_model = p.models.chat.clone();
                self.max_think_model = p.models.reasoner.unwrap_or_else(|| p.models.chat.clone());
                return;
            }
        }
    }

    #[must_use]
    pub fn active_provider_kind(&self) -> Option<ProviderKind> {
        let provider = self.active_provider();
        let kind = if provider.kind.trim().is_empty() {
            self.provider.as_str()
        } else {
            provider.kind.as_str()
        };
        normalize_provider_kind(kind)
    }

    #[must_use]
    pub fn active_base_model(&self) -> String {
        self.active_provider().models.chat
    }

    #[must_use]
    pub fn active_reasoner_model(&self) -> String {
        let provider = self.active_provider();
        if matches!(provider.cloud, Some(CloudDeployment::Azure { .. })) {
            provider.models.chat
        } else {
            provider.models.reasoner.unwrap_or(provider.models.chat)
        }
    }

    #[must_use]
    pub fn capability_resolution_for_model(&self, model: &str) -> Option<CapabilityResolution> {
        // Fall back to OpenAI-compatible for unknown providers so custom
        // endpoints and new providers don't cause hard failures.
        let provider = self
            .active_provider_kind()
            .unwrap_or(ProviderKind::OpenAiCompatible);
        Some(resolve_model_capabilities(
            provider,
            model,
            Some(&self.capability_overrides),
        ))
    }

    #[must_use]
    pub fn capabilities_for_model(&self, model: &str) -> Option<ModelCapabilities> {
        self.capability_resolution_for_model(model)
            .map(|resolution| resolution.capabilities)
    }

    #[must_use]
    pub fn model_catalog(&self) -> ModelCatalog {
        ModelCatalog::from_config(self)
    }

    #[must_use]
    pub fn model_catalog_for_runtime(&self, runtime_dir: &Path) -> ModelCatalog {
        ModelCatalog::from_runtime_cache(self, runtime_dir)
    }

    /// Parse a `provider/model-id` spec and apply it to this config.
    ///
    /// Accepts:
    /// - `"anthropic/claude-sonnet-4-6-20250514"` → sets provider + model
    /// - `"deepseek-chat"` → sets model only (keeps current provider)
    /// - `"ollama/qwen2.5-coder:7b"` → sets provider + model
    pub fn apply_model_spec(&mut self, spec: &str) {
        let spec = spec.trim();
        if spec.is_empty() {
            return;
        }
        // Check for provider/model syntax — split on first `/`
        // but only if the part before `/` looks like a provider name
        if let Some((maybe_provider, model)) = spec.split_once('/')
            && !model.trim().is_empty()
            && (self.providers.contains_key(maybe_provider) || maybe_provider == self.provider)
        {
            self.select_provider(maybe_provider);
            if let Some(provider) = self.providers.get_mut(maybe_provider) {
                provider.models.chat = model.to_string();
            }
            let p = self.active_provider();
            self.base_url = p.base_url.clone();
            self.api_key_env = p.api_key_env.clone();
            self.base_model = model.to_string();
            return;
        }
        // No provider prefix — just set the model
        self.base_model = spec.to_string();
        if let Some(provider) = self.providers.get_mut(&self.provider) {
            provider.models.chat = spec.to_string();
        }
    }

    /// Stable destination fingerprint for session history and credential scoping.
    pub fn connection_id(&self) -> String {
        use sha2::{Digest, Sha256};
        let provider = self.active_provider();
        let mut digest = Sha256::new();
        for value in [
            self.provider.as_str(),
            provider.base_url.trim_end_matches('/'),
            self.endpoint.as_str(),
            provider.api_key_env.as_str(),
            provider.kind.as_str(),
            provider.chat_protocol.as_deref().unwrap_or_default(),
            provider.auth_strategy.as_deref().unwrap_or_default(),
            if provider.openai_compat_prefix {
                "v1"
            } else {
                ""
            },
        ] {
            digest.update((value.len() as u64).to_le_bytes());
            digest.update(value.as_bytes());
        }
        if provider.cloud.is_some() || provider.gateway.is_some() {
            let identity = serde_json::to_vec(&(&provider.cloud, &provider.gateway))
                .expect("Deployment and gateway profiles serialize");
            digest.update((identity.len() as u64).to_le_bytes());
            digest.update(identity);
        }
        format!("{:x}", digest.finalize())
    }

    pub fn select_provider(&mut self, provider: &str) {
        if self.provider != provider {
            self.endpoint.clear();
            self.api_key = None;
            self.provider = provider.to_owned();
        }
    }

    /// Return all providers that have valid API keys available in the environment.
    #[must_use]
    pub fn detected_providers(&self) -> Vec<DetectedProvider> {
        let mut detected = Vec::new();
        for &(name, env_var) in KNOWN_PROVIDER_ENV_VARS {
            let available = std::env::var(env_var).is_ok();
            let configured = self.providers.contains_key(name);
            let is_active = self.provider == name;
            if available || configured {
                detected.push(DetectedProvider {
                    name: name.to_string(),
                    env_var: env_var.to_string(),
                    key_available: available,
                    configured,
                    active: is_active,
                    models: self
                        .providers
                        .get(name)
                        .map(|p| p.models.clone())
                        .unwrap_or(ProviderModels {
                            chat: String::new(),
                            reasoner: None,
                        }),
                });
            }
        }
        detected
    }
}

/// A detected provider with its availability status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedProvider {
    pub name: String,
    pub env_var: String,
    pub key_available: bool,
    pub configured: bool,
    pub active: bool,
    pub models: ProviderModels,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_model: CODINGBUDDY_V32_CHAT_MODEL.to_string(),
            max_think_model: CODINGBUDDY_V32_REASONER_MODEL.to_string(),
            provider: "deepseek".to_string(),
            providers: default_providers(),
            capability_overrides: CapabilityRegistryOverrides::default(),
            model_catalog: ModelCatalogConfig::default(),
            profile: "v3_2".to_string(),
            context_window_tokens: 128_000,
            temperature: 0.2,
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: String::new(),
            api_key: None,
            api_key_env: "DEEPSEEK_API_KEY".to_string(),
            fast_mode: false,
            language: "en".to_string(),
            prompt_cache_enabled: true,
            cache_strategy: CacheStrategy::Auto,
            timeout_seconds: 600,
            max_retries: 3,
            retry_base_ms: 400,
            stream: true,
            openai_compat_prefix: false,
        }
    }
}

pub(crate) fn default_providers() -> std::collections::HashMap<String, ProviderConfig> {
    let mut map = std::collections::HashMap::new();
    map.insert(
        "deepseek".to_string(),
        ProviderConfig {
            kind: "deepseek".to_string(),
            base_url: "https://api.deepseek.com".to_string(),
            api_key_env: "DEEPSEEK_API_KEY".to_string(),
            openai_compat_prefix: false,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: CODINGBUDDY_V32_CHAT_MODEL.to_string(),
                reasoner: Some(CODINGBUDDY_V32_REASONER_MODEL.to_string()),
            },
        },
    );
    map.insert(
        "openai-compatible".to_string(),
        ProviderConfig {
            kind: "openai-compatible".to_string(),
            base_url: "https://api.openai.com".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "gpt-4o-mini".to_string(),
                reasoner: Some("o4-mini".to_string()),
            },
        },
    );
    map.insert(
        "anthropic".to_string(),
        ProviderConfig {
            kind: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "claude-sonnet-4-20250514".to_string(),
                reasoner: Some("claude-sonnet-4-20250514".to_string()),
            },
        },
    );
    map.insert(
        "google".to_string(),
        ProviderConfig {
            kind: "google".to_string(),
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            api_key_env: "GOOGLE_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "gemini-3.8-flash".to_string(),
                reasoner: Some("gemini-3.8-flash".to_string()),
            },
        },
    );
    map.insert(
        "groq".to_string(),
        ProviderConfig {
            kind: "groq".to_string(),
            base_url: "https://api.groq.com/openai".to_string(),
            api_key_env: "GROQ_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "llama-3.3-70b-versatile".to_string(),
                reasoner: None,
            },
        },
    );
    map.insert(
        "openrouter".to_string(),
        ProviderConfig {
            kind: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "anthropic/claude-sonnet-4".to_string(),
                reasoner: Some("deepseek/deepseek-r1".to_string()),
            },
        },
    );
    map.insert(
        "ollama".to_string(),
        ProviderConfig {
            kind: "ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key_env: String::new(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "qwen2.5-coder:7b".to_string(),
                reasoner: None,
            },
        },
    );
    for (kind, model) in [
        ("bedrock", "anthropic.claude-sonnet-4-20250514-v1:0"),
        ("vertex", "gemini-2.5-flash"),
    ] {
        map.insert(
            kind.into(),
            ProviderConfig {
                kind: kind.into(),
                base_url: String::new(),
                api_key_env: String::new(),
                openai_compat_prefix: false,
                chat_protocol: None,
                auth_strategy: None,
                account_id: None,
                cloud: None,
                gateway: None,
                headers: Default::default(),
                discovery: ProviderDiscoveryConfig {
                    enabled: false,
                    ..Default::default()
                },
                payload_options: serde_json::Value::Null,
                models: ProviderModels {
                    chat: model.into(),
                    reasoner: None,
                },
            },
        );
    }
    map.insert(
        "azure".to_string(),
        ProviderConfig {
            kind: "azure".to_string(),
            base_url: String::new(), // Set via AZURE_OPENAI_ENDPOINT
            api_key_env: "AZURE_OPENAI_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "gpt-4o".to_string(),
                reasoner: Some("o4-mini".to_string()),
            },
        },
    );
    map.insert(
        "mistral".to_string(),
        ProviderConfig {
            kind: "mistral".to_string(),
            base_url: "https://api.mistral.ai".to_string(),
            api_key_env: "MISTRAL_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "mistral-large-latest".to_string(),
                reasoner: None,
            },
        },
    );
    map.insert(
        "xai".to_string(),
        ProviderConfig {
            kind: "xai".to_string(),
            base_url: "https://api.x.ai".to_string(),
            api_key_env: "XAI_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "grok-2".to_string(),
                reasoner: None,
            },
        },
    );
    map.insert(
        "together".to_string(),
        ProviderConfig {
            kind: "together".to_string(),
            base_url: "https://api.together.xyz".to_string(),
            api_key_env: "TOGETHER_API_KEY".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo".to_string(),
                reasoner: None,
            },
        },
    );
    map.insert(
        "copilot".to_string(),
        ProviderConfig {
            kind: "copilot".to_string(),
            base_url: "https://api.githubcopilot.com".to_string(),
            api_key_env: "GITHUB_TOKEN".to_string(),
            openai_compat_prefix: true,
            chat_protocol: None,
            auth_strategy: None,
            account_id: None,
            cloud: None,
            gateway: None,
            headers: std::collections::BTreeMap::new(),
            discovery: ProviderDiscoveryConfig::default(),
            payload_options: serde_json::Value::Null,
            models: ProviderModels {
                chat: "gpt-4o".to_string(),
                reasoner: None,
            },
        },
    );
    map
}

fn default_base_url() -> String {
    "https://api.deepseek.com".to_string()
}

fn default_agent_loop_max_editor_apply_retries() -> u64 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentLoopConfig {
    #[serde(default = "default_agent_loop_max_iterations")]
    pub max_iterations: u64,
    #[serde(default = "default_agent_loop_parse_retries")]
    pub architect_parse_retries: u64,
    #[serde(default = "default_agent_loop_parse_retries")]
    pub editor_parse_retries: u64,
    #[serde(default = "default_agent_loop_max_editor_apply_retries")]
    pub max_editor_apply_retries: u64,
    #[serde(default = "default_agent_loop_max_files_per_iteration")]
    pub max_files_per_iteration: u64,
    #[serde(default = "default_agent_loop_max_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default = "default_agent_loop_max_diff_bytes")]
    pub max_diff_bytes: u64,
    #[serde(default = "default_agent_loop_verify_timeout_seconds")]
    pub verify_timeout_seconds: u64,
    #[serde(default = "default_agent_loop_max_context_requests_per_iteration")]
    pub max_context_requests_per_iteration: u64,
    #[serde(default = "default_agent_loop_max_context_range_lines")]
    pub max_context_range_lines: u64,
    #[serde(default = "default_agent_loop_context_bootstrap_enabled")]
    pub context_bootstrap_enabled: bool,
    #[serde(default = "default_agent_loop_context_bootstrap_max_tree_entries")]
    pub context_bootstrap_max_tree_entries: u64,
    #[serde(default = "default_agent_loop_context_bootstrap_max_readme_bytes")]
    pub context_bootstrap_max_readme_bytes: u64,
    #[serde(default = "default_agent_loop_context_bootstrap_max_manifest_bytes")]
    pub context_bootstrap_max_manifest_bytes: u64,
    #[serde(default = "default_agent_loop_context_bootstrap_max_repo_map_lines")]
    pub context_bootstrap_max_repo_map_lines: u64,
    #[serde(default = "default_agent_loop_context_bootstrap_max_audit_findings")]
    pub context_bootstrap_max_audit_findings: u64,
    #[serde(default)]
    pub failure_classifier: FailureClassifierConfig,
    #[serde(default)]
    pub safety_gate: SafetyGateConfig,
    #[serde(default)]
    pub apply_strategy: ApplyStrategy,
    #[serde(default)]
    pub team: TeamOrchestrationConfig,
    #[serde(default)]
    pub lint: LintConfig,
    /// Maximum turns (LLM calls) for the tool-use loop. Defaults to 50.
    #[serde(default = "default_tool_loop_max_turns")]
    pub tool_loop_max_turns: u64,
    /// Enable the Explore→Plan→Execute→Verify phase loop for Complex tasks.
    /// Default: false. When false, all tools are available at all times.
    #[serde(default)]
    pub phase_loop_enabled: bool,
    /// Corrective prompt heuristics; defaults depend on the model family.
    /// See [`heuristics`].
    #[serde(default)]
    pub heuristics: heuristics::HeuristicsConfig,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_agent_loop_max_iterations(),
            architect_parse_retries: default_agent_loop_parse_retries(),
            editor_parse_retries: default_agent_loop_parse_retries(),
            max_editor_apply_retries: default_agent_loop_max_editor_apply_retries(),
            max_files_per_iteration: default_agent_loop_max_files_per_iteration(),
            max_file_bytes: default_agent_loop_max_file_bytes(),
            max_diff_bytes: default_agent_loop_max_diff_bytes(),
            verify_timeout_seconds: default_agent_loop_verify_timeout_seconds(),
            max_context_requests_per_iteration:
                default_agent_loop_max_context_requests_per_iteration(),
            max_context_range_lines: default_agent_loop_max_context_range_lines(),
            context_bootstrap_enabled: default_agent_loop_context_bootstrap_enabled(),
            context_bootstrap_max_tree_entries:
                default_agent_loop_context_bootstrap_max_tree_entries(),
            context_bootstrap_max_readme_bytes:
                default_agent_loop_context_bootstrap_max_readme_bytes(),
            context_bootstrap_max_manifest_bytes:
                default_agent_loop_context_bootstrap_max_manifest_bytes(),
            context_bootstrap_max_repo_map_lines:
                default_agent_loop_context_bootstrap_max_repo_map_lines(),
            context_bootstrap_max_audit_findings:
                default_agent_loop_context_bootstrap_max_audit_findings(),
            failure_classifier: FailureClassifierConfig::default(),
            safety_gate: SafetyGateConfig::default(),
            apply_strategy: ApplyStrategy::default(),
            team: TeamOrchestrationConfig::default(),
            lint: LintConfig::default(),
            tool_loop_max_turns: default_tool_loop_max_turns(),
            phase_loop_enabled: false,
            heuristics: heuristics::HeuristicsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamOrchestrationConfig {
    #[serde(default = "default_team_auto_enabled")]
    pub auto_enabled: bool,
    #[serde(default = "default_team_complexity_threshold")]
    pub complexity_threshold: u64,
    #[serde(default = "default_team_max_lanes")]
    pub max_lanes: u64,
    #[serde(default = "default_team_max_concurrency")]
    pub max_concurrency: u64,
}

impl Default for TeamOrchestrationConfig {
    fn default() -> Self {
        Self {
            auto_enabled: default_team_auto_enabled(),
            complexity_threshold: default_team_complexity_threshold(),
            max_lanes: default_team_max_lanes(),
            max_concurrency: default_team_max_concurrency(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FailureClassifierConfig {
    #[serde(default = "default_failure_classifier_repeat_threshold")]
    pub repeat_threshold: u64,
    #[serde(default = "default_failure_classifier_similarity_threshold")]
    pub similarity_threshold: f32,
    #[serde(default = "default_failure_classifier_fingerprint_lines")]
    pub fingerprint_lines: u64,
}

impl Default for FailureClassifierConfig {
    fn default() -> Self {
        Self {
            repeat_threshold: default_failure_classifier_repeat_threshold(),
            similarity_threshold: default_failure_classifier_similarity_threshold(),
            fingerprint_lines: default_failure_classifier_fingerprint_lines(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyGateConfig {
    #[serde(default = "default_safety_gate_max_files_without_approval")]
    pub max_files_without_approval: u64,
    #[serde(default = "default_safety_gate_max_loc_without_approval")]
    pub max_loc_without_approval: u64,
}

impl Default for SafetyGateConfig {
    fn default() -> Self {
        Self {
            max_files_without_approval: default_safety_gate_max_files_without_approval(),
            max_loc_without_approval: default_safety_gate_max_loc_without_approval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LintConfig {
    /// Whether to run lint automatically after apply, before verify.
    #[serde(default)]
    pub enabled: bool,
    /// Per-language lint commands. Key = language/glob, value = command.
    /// Example: `{"rust": "cargo clippy --fix --allow-dirty", "python": "ruff check --fix"}`
    #[serde(default)]
    pub commands: std::collections::BTreeMap<String, String>,
    /// Maximum lint-fix iterations before giving up.
    #[serde(default = "default_lint_max_iterations")]
    pub max_iterations: u64,
    /// Timeout in seconds for each lint command.
    #[serde(default = "default_lint_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_lint_max_iterations() -> u64 {
    3
}
fn default_lint_timeout_seconds() -> u64 {
    30
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            commands: std::collections::BTreeMap::new(),
            max_iterations: default_lint_max_iterations(),
            timeout_seconds: default_lint_timeout_seconds(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStrategy {
    #[default]
    Auto,
    ThreeWay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitWorkflowConfig {
    #[serde(default = "default_git_auto_commit_on_verify_pass")]
    pub auto_commit_on_verify_pass: bool,
    #[serde(default = "default_git_commit_message_template")]
    pub commit_message_template: String,
    #[serde(default = "default_git_require_signing")]
    pub require_signing: bool,
    #[serde(default)]
    pub allowed_branch_patterns: Vec<String>,
    #[serde(default)]
    pub commit_message_regex: Option<String>,
}

impl Default for GitWorkflowConfig {
    fn default() -> Self {
        Self {
            auto_commit_on_verify_pass: default_git_auto_commit_on_verify_pass(),
            commit_message_template: default_git_commit_message_template(),
            require_signing: default_git_require_signing(),
            allowed_branch_patterns: Vec::new(),
            commit_message_regex: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub enabled: bool,
    /// When true and sandbox is enabled, auto-approve bash commands without prompting.
    pub auto_allow_bash_if_sandboxed: bool,
    pub network: SandboxNetworkConfig,
    pub excluded_commands: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxNetworkConfig {
    pub allowed_domains: Vec<String>,
    pub block_all: bool,
    /// Allow binding to localhost ports (e.g., for dev servers).
    pub allow_local_binding: bool,
    /// Allow Unix domain sockets.
    pub allow_unix_sockets: bool,
}

/// Controls whether edits or bash commands require interactive approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    Ask,
    Always,
    Never,
}

impl std::fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ask => write!(f, "ask"),
            Self::Always => write!(f, "always"),
            Self::Never => write!(f, "never"),
        }
    }
}

impl std::str::FromStr for ApprovalMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "always" => Ok(Self::Always),
            "never" | "false" | "off" => Ok(Self::Never),
            other => Err(anyhow::anyhow!(
                "invalid approval mode '{}' (expected ask|always|never)",
                other
            )),
        }
    }
}

/// Review mode controls which tools are sent to the LLM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewMode {
    /// All tools available (default behavior).
    #[default]
    Off,
    /// All tools sent but write tools require explicit user approval before execution.
    Suggest,
    /// Only read-only tools are sent to the LLM — write tools are omitted entirely.
    Strict,
}

impl ReviewMode {
    /// Returns true if this tool name is read-only and allowed in strict mode.
    pub fn is_read_only_tool(name: &str) -> bool {
        name == "git_log" || is_api_tool_name_read_only(name)
    }
}

/// Sandbox enforcement mode for tool execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    Allowlist,
    Isolated,
    Off,
    ReadOnly,
    WorkspaceWrite,
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allowlist => write!(f, "allowlist"),
            Self::Isolated => write!(f, "isolated"),
            Self::Off => write!(f, "off"),
            Self::ReadOnly => write!(f, "read-only"),
            Self::WorkspaceWrite => write!(f, "workspace-write"),
        }
    }
}

impl std::str::FromStr for SandboxMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allowlist" => Ok(Self::Allowlist),
            "isolated" | "container" | "os-sandbox" | "os_sandbox" => Ok(Self::Isolated),
            "off" => Ok(Self::Off),
            "read-only" | "readonly" => Ok(Self::ReadOnly),
            "workspace-write" | "workspace_write" => Ok(Self::WorkspaceWrite),
            other => Err(anyhow::anyhow!(
                "invalid sandbox mode '{}' (expected allowlist|isolated|off|read-only|workspace-write)",
                other
            )),
        }
    }
}

/// Permission mode for the overall policy engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Ask,
    Auto,
    Locked,
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ask => write!(f, "ask"),
            Self::Auto => write!(f, "auto"),
            Self::Locked => write!(f, "locked"),
        }
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "auto" => Ok(Self::Auto),
            "locked" => Ok(Self::Locked),
            other => Err(anyhow::anyhow!(
                "invalid permission mode '{}' (expected ask|auto|locked)",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub approve_edits: ApprovalMode,
    pub approve_bash: ApprovalMode,
    pub allowlist: Vec<String>,
    pub block_paths: Vec<String>,
    pub redact_patterns: Vec<String>,
    pub sandbox_mode: SandboxMode,
    pub sandbox_wrapper: Option<String>,
    /// Permission mode: "ask" (default), "auto", or "locked".
    pub permission_mode: PermissionMode,
    /// Optional lint command to run automatically after fs.edit (e.g., "cargo fmt --check").
    pub lint_after_edit: Option<String>,
    /// OS-level sandbox configuration.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Review mode: controls which tools are sent to the LLM.
    #[serde(default)]
    pub review_mode: ReviewMode,
    /// User-defined permission rules. Format: `Tool(pattern)` with allow/deny/ask decisions.
    /// Examples: `[{"rule": "Bash(cargo *)", "decision": "allow"}]`
    #[serde(default)]
    pub permission_rules: Vec<PermissionRuleConfig>,
    #[serde(skip)]
    pub configured_denials: std::collections::BTreeMap<String, process_env::Source>,
}

/// A permission rule from user configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRuleConfig {
    /// Rule pattern: `"Bash(cargo *)"`, `"Edit(src/**/*.rs)"`, `"Mcp(server_id)"`
    pub rule: String,
    /// Decision: `"allow"`, `"deny"`, or `"ask"`
    pub decision: String,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            approve_edits: ApprovalMode::Ask,
            approve_bash: ApprovalMode::Ask,
            allowlist: vec![
                "rg".to_string(),
                "git status".to_string(),
                "git diff".to_string(),
                "git show".to_string(),
                "cargo test".to_string(),
                "cargo fmt --check".to_string(),
                "cargo clippy".to_string(),
            ],
            block_paths: vec![
                ".env".to_string(),
                ".ssh".to_string(),
                ".aws".to_string(),
                ".gnupg".to_string(),
                "**/id_*".to_string(),
                "**/secret".to_string(),
            ],
            redact_patterns: vec![
                "(?i)(api[_-]?key|token|secret|password)\\s*[:=]\\s*['\\\"]?[a-z0-9_\\-]{8,}['\\\"]?".to_string(),
                "\\b\\d{3}-\\d{2}-\\d{4}\\b".to_string(),
                "(?i)\\b(mrn|medical_record_number|patient_id)\\s*[:=]\\s*[a-z0-9\\-]{4,}\\b".to_string(),
            ],
            sandbox_mode: SandboxMode::Allowlist,
            sandbox_wrapper: None,
            permission_mode: PermissionMode::Ask,
            lint_after_edit: None,
            sandbox: SandboxConfig::default(),
            review_mode: ReviewMode::Off,
            permission_rules: vec![],
            configured_denials: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    pub enabled: bool,
    pub search_paths: Vec<String>,
    pub enable_hooks: bool,
    pub catalog: PluginCatalogConfig,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            search_paths: vec![".codingbuddy/plugins".to_string(), ".plugins".to_string()],
            enable_hooks: false,
            catalog: PluginCatalogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginCatalogConfig {
    pub enabled: bool,
    pub index_url: String,
    pub signature_key: Option<String>,
    pub refresh_hours: u64,
}

impl Default for PluginCatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            index_url: ".codingbuddy/plugins/catalog.json".to_string(),
            signature_key: Some("codingbuddy-local-dev-key".to_string()),
            refresh_hours: 24,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedSettings {
    pub disable_bypass_permissions_mode: bool,
    pub allow_managed_permission_rules_only: bool,
    pub forced_permission_mode: Option<String>,
    pub blocked_tools: Vec<String>,
}

pub fn managed_settings_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(
            "/Library/Application Support/CodingBuddyCode/managed-settings.json",
        ))
    }
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from("/etc/codingbuddy-code/managed-settings.json"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(PathBuf::from(
            "C:\\Program Files\\CodingBuddyCode\\managed-settings.json",
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

pub fn load_managed_settings() -> Option<ManagedSettings> {
    let path = managed_settings_path()?;
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageConfig {
    pub show_statusline: bool,
    pub cost_per_million_input: f64,
    pub cost_per_million_output: f64,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            show_statusline: true,
            cost_per_million_input: 0.27,
            cost_per_million_output: 1.10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub auto_compact_threshold: f32,
    pub compact_preview: bool,
    /// Number of recent messages to preserve during compaction.
    /// Higher values retain more context at the cost of more tokens.
    #[serde(default)]
    pub compaction_tail_window: Option<usize>,
    /// Tokens reserved for tool definitions + system prompt overhead.
    /// Subtracted from the context window before applying `auto_compact_threshold`.
    #[serde(default)]
    pub reserved_overhead_tokens: u64,
    /// Tokens reserved for the model's response.
    /// Ensures enough space remains for output after filling conversation history.
    #[serde(default)]
    pub response_budget_tokens: u64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            auto_compact_threshold: 0.86,
            compact_preview: true,
            compaction_tail_window: None,
            reserved_overhead_tokens: 4_000,
            response_budget_tokens: 8_192,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutopilotConfig {
    pub default_max_consecutive_failures: u64,
    pub heartbeat_interval_seconds: u64,
    pub persist_checkpoints: bool,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            default_max_consecutive_failures: 10,
            heartbeat_interval_seconds: 5,
            persist_checkpoints: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub enable_tui: bool,
    pub keybindings_path: String,
    pub reduced_motion: bool,
    pub statusline_mode: String,
    /// Thinking visibility mode in TUI.
    /// Allowed values: "concise", "raw".
    #[serde(default = "default_ui_thinking_visibility")]
    pub thinking_visibility: String,
    /// Heartbeat interval for active phase progress in milliseconds.
    #[serde(default = "default_ui_phase_heartbeat_ms")]
    pub phase_heartbeat_ms: u64,
    /// Maximum retained mission-control timeline events.
    #[serde(default = "default_ui_mission_control_max_events")]
    pub mission_control_max_events: u64,
    /// Image fallback mode when terminal protocol does not support inline rendering.
    /// Allowed values: "open", "path", "none".
    pub image_fallback: String,
    /// Optional base URL used when generating teleport handoff links.
    pub handoff_base_url: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            enable_tui: true,
            keybindings_path: "~/.codingbuddy/keybindings.json".to_string(),
            reduced_motion: false,
            statusline_mode: "minimal".to_string(),
            thinking_visibility: default_ui_thinking_visibility(),
            phase_heartbeat_ms: default_ui_phase_heartbeat_ms(),
            mission_control_max_events: default_ui_mission_control_max_events(),
            image_fallback: "open".to_string(),
            handoff_base_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ToolsConfig {
    /// Automatically run language-specific diagnostics after file edits.
    /// Feeds compilation errors back to the LLM for self-correction.
    #[serde(default = "default_true")]
    pub diagnostics_after_edit: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    pub paths: Vec<String>,
    pub hot_reload: bool,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            paths: vec![
                ".codingbuddy/skills".to_string(),
                "~/.codingbuddy/skills".to_string(),
            ],
            hot_reload: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulingConfig {
    pub off_peak: bool,
    pub off_peak_start_hour: u8,
    pub off_peak_end_hour: u8,
    pub defer_non_urgent: bool,
    pub max_defer_seconds: u64,
}

impl Default for SchedulingConfig {
    fn default() -> Self {
        Self {
            off_peak: false,
            off_peak_start_hour: 0,
            off_peak_end_hour: 6,
            defer_non_urgent: false,
            max_defer_seconds: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplayConfig {
    pub strict_mode: bool,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self { strict_mode: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExperimentsConfig {
    pub visual_verification: bool,
    pub wasm_hooks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    pub enabled: bool,
    pub engine: String,
    pub watch_files: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: "tantivy".to_string(),
            watch_files: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetsConfig {
    pub max_turn_duration_secs: u64,
    pub max_reasoner_tokens_per_session: u64,
    pub max_turns: Option<u64>,
    pub max_budget_usd: Option<f64>,
}

impl Default for BudgetsConfig {
    fn default() -> Self {
        Self {
            max_turn_duration_secs: 300,
            max_reasoner_tokens_per_session: 1_000_000,
            max_turns: None,
            max_budget_usd: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// "auto" (detect), "dark", or "light"
    #[serde(default = "default_theme_mode")]
    pub mode: String,
    pub primary: String,
    pub secondary: String,
    pub error: String,
}

fn default_theme_mode() -> String {
    "auto".to_string()
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            primary: "Cyan".to_string(),
            secondary: "Yellow".to_string(),
            error: "Red".to_string(),
        }
    }
}

/// Configuration for post-edit LSP validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LspValidationConfig {
    /// Global enable/disable toggle.
    pub enabled: bool,
    /// Per-language enable/disable (e.g. `{"rust": false}` to skip cargo check).
    pub languages: std::collections::HashMap<String, bool>,
    /// Timeout in seconds for each check command (default: 30).
    pub timeout_seconds: u64,
}

impl Default for LspValidationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            languages: std::collections::HashMap::new(),
            timeout_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacyConfig {
    pub enabled: bool,
    pub sensitive_globs: Vec<String>,
    pub sensitive_regex: Vec<String>,
    pub policy: String,
    pub store_raw_in_logs: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sensitive_globs: vec![
                "**/.env".to_string(),
                "**/.env.*".to_string(),
                "**/*.pem".to_string(),
                "**/*.key".to_string(),
            ],
            sensitive_regex: Vec::new(),
            policy: "redact".to_string(),
            store_raw_in_logs: false,
        }
    }
}

// ── Workspace metadata detection ──
