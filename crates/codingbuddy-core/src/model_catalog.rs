use crate::{
    CapabilityRegistryOverrides, LlmConfig, ModelCapabilities, ModelFamily, ProviderKind,
    detect_model_family, normalize_provider_kind, resolve_model_capabilities,
};
use serde::{Deserialize, Serialize};

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
    pub status: ModelStatus,
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
pub enum ModelStatus {
    #[default]
    Stable,
    Preview,
    Deprecated,
    Unknown,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::bundled()
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
            status: ModelStatus::Unknown,
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

impl ModelCatalog {
    #[must_use]
    pub fn bundled() -> Self {
        let mut catalog = Self {
            source: ModelCatalogSource::Bundled,
            models: Vec::new(),
        };
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
        catalog
    }

    #[must_use]
    pub fn from_config(config: &LlmConfig) -> Self {
        let mut catalog = Self::bundled();
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
        catalog.source = ModelCatalogSource::Merged;
        catalog
    }

    pub fn from_models_dev_json(value: &serde_json::Value) -> anyhow::Result<Self> {
        let mut catalog = Self {
            source: ModelCatalogSource::Remote,
            models: Vec::new(),
        };
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
                catalog.upsert(parse_models_dev_entry(provider_id, model_id, model_value));
            }
        }
        Ok(catalog)
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
                output_tokens: 0,
            },
            status: ModelStatus::Stable,
        }
    }
}

fn parse_models_dev_entry(provider: &str, model: &str, value: &serde_json::Value) -> ModelInfo {
    let caps = value.get("capabilities").unwrap_or(value);
    let cost = value.get("cost").unwrap_or(value);
    let limits = value
        .get("limits")
        .or_else(|| value.get("limit"))
        .unwrap_or(value);
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
        capability: ProviderCapability {
            tool_call: boolish(caps, &["tool_call", "tool_calling", "tools"]),
            parallel_tool_call: boolish(caps, &["parallel_tool_call", "parallel_tool_calls"]),
            reasoning: boolish(caps, &["reasoning", "reasoning_mode"]),
            thinking_config: boolish(caps, &["thinking", "thinking_config"]),
            streaming_tool_deltas: boolish(caps, &["streaming_tool_deltas"]),
            image_input: boolish(caps, &["image", "image_input", "attachment"]),
            fim: boolish(caps, &["fim"]),
        },
        cost: ModelCost {
            input_per_mtok_usd: f64ish(cost, &["input", "input_per_mtok_usd"]),
            output_per_mtok_usd: f64ish(cost, &["output", "output_per_mtok_usd"]),
        },
        limits: ModelLimits {
            context_tokens: u64ish(limits, &["context", "context_tokens"]),
            output_tokens: u64ish(limits, &["output", "output_tokens"]) as u32,
        },
        status: parse_status(value),
    }
}

fn boolish(value: &serde_json::Value, keys: &[&str]) -> bool {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .any(|v| v.as_bool().unwrap_or(false))
}

fn f64ish(value: &serde_json::Value, keys: &[&str]) -> f64 {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(|v| v.as_f64())
        .unwrap_or(0.0)
}

fn u64ish(value: &serde_json::Value, keys: &[&str]) -> u64 {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(|v| v.as_u64())
        .unwrap_or(0)
}

fn parse_status(value: &serde_json::Value) -> ModelStatus {
    match value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("stable")
        .to_ascii_lowercase()
        .as_str()
    {
        "stable" | "available" => ModelStatus::Stable,
        "preview" | "experimental" | "beta" => ModelStatus::Preview,
        "deprecated" => ModelStatus::Deprecated,
        _ => ModelStatus::Unknown,
    }
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

    #[test]
    fn bundled_catalog_contains_default_deepseek_models() {
        let catalog = ModelCatalog::bundled();
        assert!(catalog.find("deepseek", "deepseek-chat").is_some());
        assert!(catalog.find("deepseek", "deepseek-reasoner").is_some());
    }

    #[test]
    fn parses_models_dev_style_catalog() {
        let catalog = ModelCatalog::from_models_dev_json(&json!({
            "providers": {
                "openrouter": {
                    "models": {
                        "example/model": {
                            "name": "Example Model",
                            "capabilities": {
                                "tool_call": true,
                                "reasoning": true,
                                "image": true
                            },
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
        assert_eq!(model.cost.input_per_mtok_usd, 1.25);
        assert_eq!(model.limits.context_tokens, 128000);
        assert_eq!(model.status, ModelStatus::Preview);
    }
}
