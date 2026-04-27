use crate::{
    CODINGBUDDY_CHAT_MAX_OUTPUT_TOKENS, CODINGBUDDY_CHAT_THINKING_MAX_OUTPUT_TOKENS,
    CODINGBUDDY_REASONER_MAX_OUTPUT_TOKENS, CapabilityRegistryOverrides, LlmConfig,
    ModelCapabilities, ModelFamily, ProviderKind, detect_model_family, is_reasoner_model,
    normalize_provider_kind, resolve_model_capabilities,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_MODELS_DEV_URL: &str = "https://models.dev/api.json";
pub const DEFAULT_MODEL_CATALOG_CACHE_TTL_SECONDS: u64 = 6 * 60 * 60;
pub const DEFAULT_MODEL_CATALOG_REFRESH_TIMEOUT_SECONDS: u64 = 5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCatalog {
    pub source: ModelCatalogSource,
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ModelCatalogSource {
    #[default]
    Bundled,
    Remote,
    Cache,
    Configured,
    User,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub display_name: String,
    pub family: ModelFamily,
    pub capability: ProviderCapability,
    pub cost: ModelCost,
    pub limits: ModelLimits,
    pub modalities: Vec<ModelModality>,
    pub status: ModelStatus,
    pub provider_status: ProviderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCatalogConfig {
    pub enabled: bool,
    pub remote_url: String,
    pub cache_path: Option<String>,
    pub overrides_path: Option<String>,
    pub cache_ttl_seconds: u64,
    pub refresh_timeout_seconds: u64,
    pub offline: bool,
    pub overrides: Vec<ModelInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCatalogCache {
    pub fetched_at_unix: u64,
    pub source_url: String,
    pub catalog: ModelCatalog,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProviderCapability {
    pub tool_call: bool,
    pub parallel_tool_call: bool,
    pub reasoning: bool,
    pub thinking_config: bool,
    pub streaming_tool_deltas: bool,
    pub image_input: bool,
    pub fim: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelCost {
    pub input_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelLimits {
    pub context_tokens: u64,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ModelModality {
    #[default]
    Text,
    Image,
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ModelStatus {
    #[default]
    Stable,
    Preview,
    Deprecated,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderStatus {
    Available,
    Degraded,
    Unavailable,
    #[default]
    Unknown,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::bundled()
    }
}

impl Default for ModelCatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            remote_url: DEFAULT_MODELS_DEV_URL.to_string(),
            cache_path: None,
            overrides_path: None,
            cache_ttl_seconds: DEFAULT_MODEL_CATALOG_CACHE_TTL_SECONDS,
            refresh_timeout_seconds: DEFAULT_MODEL_CATALOG_REFRESH_TIMEOUT_SECONDS,
            offline: false,
            overrides: Vec::new(),
        }
    }
}

impl Default for ModelCatalogCache {
    fn default() -> Self {
        Self {
            fetched_at_unix: 0,
            source_url: String::new(),
            catalog: ModelCatalog::empty(ModelCatalogSource::Cache),
        }
    }
}

impl Default for ModelInfo {
    fn default() -> Self {
        Self {
            provider: String::new(),
            id: String::new(),
            display_name: String::new(),
            family: ModelFamily::Generic,
            capability: ProviderCapability::default(),
            cost: ModelCost::default(),
            limits: ModelLimits::default(),
            modalities: vec![ModelModality::Text],
            status: ModelStatus::Unknown,
            provider_status: ProviderStatus::Unknown,
        }
    }
}

impl From<ModelCapabilities> for ProviderCapability {
    fn from(capabilities: ModelCapabilities) -> Self {
        Self {
            tool_call: capabilities.supports_tool_calling,
            parallel_tool_call: capabilities.supports_parallel_tool_calls,
            reasoning: capabilities.thinking_capability.has_reasoning()
                || capabilities.supports_reasoning_mode,
            thinking_config: capabilities.supports_thinking_config,
            streaming_tool_deltas: capabilities.supports_streaming_tool_deltas,
            image_input: capabilities.supports_image_input,
            fim: capabilities.supports_fim,
        }
    }
}

impl ModelCatalogConfig {
    #[must_use]
    pub fn cache_path_for(&self, runtime_dir: &Path) -> PathBuf {
        self.cache_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| runtime_dir.join("model_catalog.json"))
    }

    #[must_use]
    pub fn overrides_path_for(&self, runtime_dir: &Path) -> Option<PathBuf> {
        self.overrides_path
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| Some(runtime_dir.join("model_overrides.json")).filter(|p| p.exists()))
    }
}

impl ModelCatalogCache {
    #[must_use]
    pub fn new(catalog: ModelCatalog, source_url: impl Into<String>, fetched_at_unix: u64) -> Self {
        Self {
            fetched_at_unix,
            source_url: source_url.into(),
            catalog,
        }
    }

    #[must_use]
    pub fn is_fresh_at(&self, now_unix: u64, ttl_seconds: u64) -> bool {
        ttl_seconds > 0
            && self.fetched_at_unix > 0
            && now_unix.saturating_sub(self.fetched_at_unix) <= ttl_seconds
    }

    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(path, bytes)?;
        Ok(())
    }
}

impl ModelCatalog {
    #[must_use]
    pub fn empty(source: ModelCatalogSource) -> Self {
        Self {
            source,
            models: Vec::new(),
        }
    }

    #[must_use]
    pub fn bundled() -> Self {
        let mut catalog = Self::empty(ModelCatalogSource::Bundled);
        for (provider, models) in BUNDLED_MODELS {
            for model in *models {
                catalog.models.push(ModelInfo::from_capabilities(
                    provider,
                    model,
                    None,
                    &CapabilityRegistryOverrides::default(),
                ));
            }
        }
        catalog.sort_models();
        catalog
    }

    #[must_use]
    pub fn configured(config: &LlmConfig) -> Self {
        let mut catalog = Self::empty(ModelCatalogSource::Configured);
        for (provider_id, provider) in &config.providers {
            catalog.upsert(ModelInfo::from_capabilities(
                provider_id,
                &provider.models.chat,
                Some(provider.kind.as_str()),
                &config.capability_overrides,
            ));
            if let Some(reasoner) = &provider.models.reasoner {
                catalog.upsert(ModelInfo::from_capabilities(
                    provider_id,
                    reasoner,
                    Some(provider.kind.as_str()),
                    &config.capability_overrides,
                ));
            }
        }
        if !config.providers.contains_key(&config.provider) {
            let provider = config.active_provider();
            catalog.upsert(ModelInfo::from_capabilities(
                &config.provider,
                &provider.models.chat,
                Some(provider.kind.as_str()),
                &config.capability_overrides,
            ));
            if let Some(reasoner) = &provider.models.reasoner {
                catalog.upsert(ModelInfo::from_capabilities(
                    &config.provider,
                    reasoner,
                    Some(provider.kind.as_str()),
                    &config.capability_overrides,
                ));
            }
        }
        catalog
    }

    #[must_use]
    pub fn inline_overrides(config: &LlmConfig) -> Self {
        let mut catalog = Self::empty(ModelCatalogSource::User);
        for model in &config.model_catalog.overrides {
            catalog.upsert(model.clone());
        }
        catalog
    }

    #[must_use]
    pub fn from_config(config: &LlmConfig) -> Self {
        let base = if config.model_catalog.enabled {
            Self::bundled()
        } else {
            Self::empty(ModelCatalogSource::Merged)
        };
        Self::from_config_with_base(config, base)
    }

    #[must_use]
    pub fn from_config_with_base(config: &LlmConfig, mut base: Self) -> Self {
        base.merge_from(&Self::configured(config));
        base.merge_from(&Self::inline_overrides(config));
        base.source = ModelCatalogSource::Merged;
        base
    }

    pub fn from_models_dev_json(value: &serde_json::Value) -> anyhow::Result<Self> {
        let mut catalog = Self::empty(ModelCatalogSource::Remote);
        let root = value.get("providers").unwrap_or(value);
        let providers = root
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("models.dev catalog must be a JSON object"))?;
        for (provider_id, provider_value) in providers {
            let Some(models_obj) = provider_value
                .get("models")
                .and_then(|v| v.as_object())
                .or_else(|| provider_value.get("model").and_then(|v| v.as_object()))
            else {
                continue;
            };
            for (model_id, model_value) in models_obj {
                catalog.upsert(parse_models_dev_entry(
                    provider_id,
                    model_id,
                    provider_value,
                    model_value,
                ));
            }
        }
        Ok(catalog)
    }

    pub fn from_overrides_json(value: &serde_json::Value) -> anyhow::Result<Self> {
        if let Some(models) = value.as_array() {
            return Ok(Self {
                source: ModelCatalogSource::User,
                models: serde_json::from_value(serde_json::Value::Array(models.clone()))?,
            });
        }
        if let Some(models) = value.get("models").and_then(|v| v.as_array()) {
            return Ok(Self {
                source: ModelCatalogSource::User,
                models: serde_json::from_value(serde_json::Value::Array(models.clone()))?,
            });
        }
        let mut catalog = Self::from_models_dev_json(value)?;
        catalog.source = ModelCatalogSource::User;
        Ok(catalog)
    }

    pub fn load_overrides(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)?;
        Self::from_overrides_json(&serde_json::from_str(&raw)?)
    }

    #[must_use]
    pub fn find(&self, provider: &str, model: &str) -> Option<&ModelInfo> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == model)
    }

    #[must_use]
    pub fn for_provider(&self, provider: &str) -> Vec<&ModelInfo> {
        self.models
            .iter()
            .filter(|m| m.provider == provider)
            .collect()
    }

    pub fn upsert(&mut self, info: ModelInfo) {
        if let Some(existing) = self
            .models
            .iter_mut()
            .find(|m| m.provider == info.provider && m.id == info.id)
        {
            *existing = info;
        } else {
            self.models.push(info);
        }
        self.sort_models();
    }

    pub fn merge_from(&mut self, other: &Self) {
        for model in &other.models {
            self.upsert(model.clone());
        }
        if self.source != other.source || !other.models.is_empty() {
            self.source = ModelCatalogSource::Merged;
        }
    }

    fn sort_models(&mut self) {
        self.models
            .sort_by(|a, b| a.provider.cmp(&b.provider).then_with(|| a.id.cmp(&b.id)));
    }
}

impl ModelInfo {
    #[must_use]
    pub fn from_capabilities(
        provider_id: &str,
        model: &str,
        kind: Option<&str>,
        overrides: &CapabilityRegistryOverrides,
    ) -> Self {
        let provider_kind = kind
            .and_then(normalize_provider_kind)
            .or_else(|| normalize_provider_kind(provider_id))
            .unwrap_or(ProviderKind::OpenAiCompatible);
        let caps = resolve_model_capabilities(provider_kind, model, Some(overrides)).capabilities;
        let modalities = default_modalities(&caps);
        let output_tokens = default_output_tokens(&caps, model);
        Self {
            provider: provider_id.to_string(),
            id: model.to_string(),
            display_name: model.to_string(),
            family: caps.family,
            capability: caps.into(),
            cost: ModelCost {
                input_per_mtok_usd: caps.cost_per_mtok_input,
                output_per_mtok_usd: caps.cost_per_mtok_output,
            },
            limits: ModelLimits {
                context_tokens: caps.context_window_tokens,
                output_tokens,
            },
            modalities,
            status: ModelStatus::Stable,
            provider_status: ProviderStatus::Available,
        }
    }
}

fn parse_models_dev_entry(
    provider: &str,
    model: &str,
    provider_value: &serde_json::Value,
    value: &serde_json::Value,
) -> ModelInfo {
    let caps = value.get("capabilities").unwrap_or(value);
    let cost = value.get("cost").unwrap_or(value);
    let limits = value
        .get("limits")
        .or_else(|| value.get("limit"))
        .unwrap_or(value);
    let capability = ProviderCapability {
        tool_call: boolish(caps, &["tool_call", "tool_calling", "tools"]),
        parallel_tool_call: boolish(caps, &["parallel_tool_call", "parallel_tool_calls"]),
        reasoning: boolish(caps, &["reasoning", "reasoning_mode"]),
        thinking_config: boolish(caps, &["thinking", "thinking_config"]),
        streaming_tool_deltas: boolish(caps, &["streaming_tool_deltas"]),
        image_input: boolish(caps, &["image", "image_input", "attachment", "vision"]),
        fim: boolish(caps, &["fim"]),
    };
    let modalities = parse_modalities(value, caps, &capability);
    ModelInfo {
        provider: provider.to_string(),
        id: model.to_string(),
        display_name: value
            .get("name")
            .or_else(|| value.get("display_name"))
            .and_then(|v| v.as_str())
            .unwrap_or(model)
            .to_string(),
        family: detect_model_family(model),
        capability,
        cost: ModelCost {
            input_per_mtok_usd: f64ish(cost, &["input", "input_per_mtok_usd"]),
            output_per_mtok_usd: f64ish(cost, &["output", "output_per_mtok_usd"]),
        },
        limits: ModelLimits {
            context_tokens: u64ish(limits, &["context", "context_tokens"]),
            output_tokens: u64ish(limits, &["output", "output_tokens", "max_output"]) as u32,
        },
        modalities,
        status: parse_model_status(value),
        provider_status: parse_provider_status(provider_value, value),
    }
}

fn default_modalities(capabilities: &ModelCapabilities) -> Vec<ModelModality> {
    let mut modalities = vec![ModelModality::Text];
    if capabilities.supports_image_input {
        push_modality(&mut modalities, ModelModality::Image);
    }
    modalities
}

fn default_output_tokens(capabilities: &ModelCapabilities, model: &str) -> u32 {
    if capabilities.provider == ProviderKind::Deepseek && is_reasoner_model(model) {
        CODINGBUDDY_REASONER_MAX_OUTPUT_TOKENS
    } else if capabilities.provider == ProviderKind::Deepseek
        && capabilities.supports_thinking_config
        && capabilities.thinking_capability.has_reasoning()
    {
        CODINGBUDDY_CHAT_THINKING_MAX_OUTPUT_TOKENS
    } else {
        CODINGBUDDY_CHAT_MAX_OUTPUT_TOKENS
    }
}

fn parse_modalities(
    value: &serde_json::Value,
    caps: &serde_json::Value,
    capability: &ProviderCapability,
) -> Vec<ModelModality> {
    let mut modalities = vec![ModelModality::Text];
    for key in [
        "modalities",
        "input_modalities",
        "output_modalities",
        "input",
        "output",
    ] {
        if let Some(v) = value.get(key).or_else(|| caps.get(key)) {
            collect_modalities(v, &mut modalities);
        }
    }
    if capability.image_input {
        push_modality(&mut modalities, ModelModality::Image);
    }
    modalities
}

fn collect_modalities(value: &serde_json::Value, modalities: &mut Vec<ModelModality>) {
    match value {
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "text" | "tokens" | "language" => push_modality(modalities, ModelModality::Text),
            "image" | "vision" | "images" => push_modality(modalities, ModelModality::Image),
            "audio" | "speech" => push_modality(modalities, ModelModality::Audio),
            "video" => push_modality(modalities, ModelModality::Video),
            _ => {}
        },
        serde_json::Value::Array(values) => {
            for value in values {
                collect_modalities(value, modalities);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if value_to_bool(value) {
                    collect_modalities(&serde_json::Value::String(key.clone()), modalities);
                }
            }
        }
        _ => {}
    }
}

fn push_modality(modalities: &mut Vec<ModelModality>, modality: ModelModality) {
    if !modalities.contains(&modality) {
        modalities.push(modality);
    }
}

fn boolish(value: &serde_json::Value, keys: &[&str]) -> bool {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .any(value_to_bool)
}

fn value_to_bool(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(value) => value.as_u64().unwrap_or(0) > 0,
        serde_json::Value::String(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "y" | "1" | "supported" | "available"
        ),
        serde_json::Value::Array(values) => !values.is_empty(),
        serde_json::Value::Object(map) => !map.is_empty(),
        serde_json::Value::Null => false,
    }
}

fn f64ish(value: &serde_json::Value, keys: &[&str]) -> f64 {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(value_to_f64)
        .unwrap_or(0.0)
}

fn value_to_f64(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|v| v.trim().parse().ok()))
}

fn u64ish(value: &serde_json::Value, keys: &[&str]) -> u64 {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(value_to_u64)
        .unwrap_or(0)
}

fn value_to_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .map(|v| v.trim().replace('_', ""))
            .and_then(|v| v.parse().ok())
    })
}

fn parse_model_status(value: &serde_json::Value) -> ModelStatus {
    match status_text(value)
        .unwrap_or_else(|| "stable".to_string())
        .as_str()
    {
        "stable" | "available" | "ok" | "active" => ModelStatus::Stable,
        "preview" | "experimental" | "beta" => ModelStatus::Preview,
        "deprecated" | "retired" => ModelStatus::Deprecated,
        _ => ModelStatus::Unknown,
    }
}

fn parse_provider_status(
    provider_value: &serde_json::Value,
    model_value: &serde_json::Value,
) -> ProviderStatus {
    let status = model_value
        .get("provider_status")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| status_text(provider_value))
        .unwrap_or_else(|| "available".to_string());
    match status.as_str() {
        "available" | "stable" | "ok" | "active" | "online" => ProviderStatus::Available,
        "degraded" | "partial" | "limited" => ProviderStatus::Degraded,
        "unavailable" | "down" | "offline" | "disabled" => ProviderStatus::Unavailable,
        _ => ProviderStatus::Unknown,
    }
}

fn status_text(value: &serde_json::Value) -> Option<String> {
    value
        .get("status")
        .or_else(|| value.get("availability"))
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_ascii_lowercase())
}

const BUNDLED_MODELS: &[(&str, &[&str])] = &[
    ("deepseek", &["deepseek-chat", "deepseek-reasoner"]),
    ("openai-compatible", &["gpt-4o-mini", "o4-mini"]),
    ("anthropic", &["claude-sonnet-4-5"]),
    ("google", &["gemini-2.5-pro"]),
    ("openrouter", &["openrouter/auto"]),
    ("ollama", &["qwen2.5-coder:7b"]),
    ("mistral", &["mistral-large-latest"]),
    ("groq", &["llama-3.3-70b-versatile"]),
    ("xai", &["grok-4"]),
    ("together", &["meta-llama/Llama-3.3-70B-Instruct-Turbo"]),
    ("azure", &["gpt-4o-mini"]),
    ("bedrock", &["anthropic.claude-3-5-sonnet-20241022-v2:0"]),
    ("vertex", &["gemini-2.5-pro"]),
    ("copilot", &["gpt-4.1"]),
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn bundled_catalog_contains_default_deepseek_models() {
        let catalog = ModelCatalog::bundled();
        assert!(catalog.find("deepseek", "deepseek-chat").is_some());
        let reasoner = catalog
            .find("deepseek", "deepseek-reasoner")
            .expect("reasoner");
        assert_eq!(
            reasoner.limits.output_tokens,
            CODINGBUDDY_REASONER_MAX_OUTPUT_TOKENS
        );
    }

    #[test]
    fn parses_models_dev_style_catalog() {
        let catalog = ModelCatalog::from_models_dev_json(&json!({
            "providers": {
                "openrouter": {
                    "status": "degraded",
                    "models": {
                        "example/model": {
                            "name": "Example Model",
                            "capabilities": {
                                "tool_call": true,
                                "reasoning": true,
                                "image": true
                            },
                            "modalities": ["text", "image"],
                            "cost": {
                                "input": 1.25,
                                "output": 5.0
                            },
                            "limits": {
                                "context": 128000,
                                "output": 8192
                            },
                            "status": "preview"
                        }
                    }
                }
            }
        }))
        .expect("catalog");
        let model = catalog
            .find("openrouter", "example/model")
            .expect("model entry");
        assert_eq!(model.display_name, "Example Model");
        assert!(model.capability.tool_call);
        assert!(model.capability.reasoning);
        assert!(model.capability.image_input);
        assert!(model.modalities.contains(&ModelModality::Image));
        assert_eq!(model.cost.input_per_mtok_usd, 1.25);
        assert_eq!(model.limits.context_tokens, 128000);
        assert_eq!(model.status, ModelStatus::Preview);
        assert_eq!(model.provider_status, ProviderStatus::Degraded);
    }

    #[test]
    fn config_models_and_overrides_win_over_bundled_entries() {
        let mut config = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..Default::default()
        };
        config.model_catalog.overrides.push(ModelInfo {
            provider: "openai-compatible".to_string(),
            id: "gpt-4o-mini".to_string(),
            display_name: "Custom Mini".to_string(),
            cost: ModelCost {
                input_per_mtok_usd: 0.01,
                output_per_mtok_usd: 0.02,
            },
            ..ModelInfo::default()
        });

        let catalog = ModelCatalog::from_config(&config);
        let model = catalog
            .find("openai-compatible", "gpt-4o-mini")
            .expect("override model");
        assert_eq!(model.display_name, "Custom Mini");
        assert_eq!(model.cost.input_per_mtok_usd, 0.01);
    }

    #[test]
    fn cache_round_trips_and_honors_ttl() {
        let dir = tempdir().expect("tempdir");
        let cache_path = dir.path().join("catalog.json");
        let cache = ModelCatalogCache::new(ModelCatalog::bundled(), "https://example.test", 100);
        cache.save(&cache_path).expect("save cache");

        let loaded = ModelCatalogCache::load(&cache_path).expect("load cache");
        assert_eq!(loaded.source_url, "https://example.test");
        assert!(loaded.is_fresh_at(120, 60));
        assert!(!loaded.is_fresh_at(200, 60));
    }

    #[test]
    fn override_file_accepts_model_array() {
        let value = json!({
            "models": [{
                "provider": "ollama",
                "id": "local-test",
                "display_name": "Local Test",
                "modalities": ["text"]
            }]
        });
        let catalog = ModelCatalog::from_overrides_json(&value).expect("overrides");
        assert_eq!(catalog.source, ModelCatalogSource::User);
        assert!(catalog.find("ollama", "local-test").is_some());
    }
}
