use crate::{CODINGBUDDY_V32_CHAT_MODEL, CODINGBUDDY_V32_REASONER_MODEL};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Deepseek,
    OpenAiCompatible,
    Anthropic,
    Google,
    Groq,
    OpenRouter,
    Ollama,
    Azure,
    Bedrock,
    Vertex,
    MistralApi,
    Xai,
    Together,
    Copilot,
}

impl ProviderKind {
    #[must_use]
    pub fn as_key(self) -> &'static str {
        match self {
            ProviderKind::Deepseek => "deepseek",
            ProviderKind::OpenAiCompatible => "openai-compatible",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Google => "google",
            ProviderKind::Groq => "groq",
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Azure => "azure",
            ProviderKind::Bedrock => "bedrock",
            ProviderKind::Vertex => "vertex",
            ProviderKind::MistralApi => "mistral",
            ProviderKind::Xai => "xai",
            ProviderKind::Together => "together",
            ProviderKind::Copilot => "copilot",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelFamily {
    Deepseek,
    Qwen,
    Gemini,
    OpenAi,
    Claude,
    Llama,
    Mistral,
    Generic,
}

impl ModelFamily {
    #[must_use]
    pub fn as_key(self) -> &'static str {
        match self {
            ModelFamily::Deepseek => "deepseek",
            ModelFamily::Qwen => "qwen",
            ModelFamily::Gemini => "gemini",
            ModelFamily::OpenAi => "openai",
            ModelFamily::Llama => "llama",
            ModelFamily::Claude => "claude",
            ModelFamily::Mistral => "mistral",
            ModelFamily::Generic => "generic",
        }
    }
}

/// How a model supports extended reasoning/thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingCapability {
    /// Model does not support thinking/reasoning mode.
    #[default]
    None,
    /// Native reasoning (DeepSeek-R1 style): model always thinks, no config needed.
    /// ThinkingConfig must NOT be sent; strip temperature/top_p/tool_choice.
    NativeReasoning,
    /// Configurable extended thinking (Anthropic style): send ThinkingConfig with budget.
    ExtendedThinking,
    /// Implicit reasoning (OpenAI o1/o3 style): model reasons internally,
    /// use max_completion_tokens instead of max_tokens.
    ImplicitReasoning,
}

impl ThinkingCapability {
    /// Whether this model accepts an explicit ThinkingConfig in the payload.
    #[must_use]
    pub fn accepts_thinking_config(self) -> bool {
        matches!(self, Self::ExtendedThinking)
    }

    /// Whether this model has any form of reasoning capability.
    #[must_use]
    pub fn has_reasoning(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreferredEditTool {
    FsEdit,
    MultiEdit,
    PatchDirect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelCapabilities {
    pub provider: ProviderKind,
    pub family: ModelFamily,
    pub supports_tool_calling: bool,
    pub supports_tool_choice: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_reasoning_mode: bool,
    pub supports_thinking_config: bool,
    pub supports_streaming_tool_deltas: bool,
    pub supports_fim: bool,
    /// Whether chat payloads may include image inputs.
    pub supports_image_input: bool,
    /// Whether outbound chat messages should be strictly filtered for empty content.
    pub strict_empty_content_filtering: bool,
    /// Whether tool call ids should be normalized to provider-safe identifiers.
    pub normalize_tool_call_ids: bool,
    pub max_safe_tool_count: usize,
    pub preferred_edit_tool: PreferredEditTool,
    /// Whether max_tokens should be renamed to max_completion_tokens (OpenAI reasoning models).
    pub prefers_max_completion_tokens: bool,
    /// Whether Gemini-style JSON schema sanitization is needed (enum→string, remove invalid fields).
    pub requires_schema_sanitization: bool,
    /// Whether tool_choice="required" should be downgraded to "auto".
    pub downgrades_tool_choice_required: bool,
    /// Whether max_tokens should be placed in `options.num_predict` (Ollama).
    pub uses_options_num_predict: bool,
    /// Whether Mistral-style message sequence repair is needed (insert assistant between tool→user).
    pub requires_message_sequence_repair: bool,
    /// Context window size in tokens (0 = unknown/use config default).
    pub context_window_tokens: u64,
    /// Cost per million input tokens in USD (0.0 = unknown/free).
    pub cost_per_mtok_input: f64,
    /// Cost per million output tokens in USD (0.0 = unknown/free).
    pub cost_per_mtok_output: f64,
    /// Thinking capability for this model.
    pub thinking_capability: ThinkingCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct CapabilityOverride {
    pub supports_tool_calling: Option<bool>,
    pub supports_tool_choice: Option<bool>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_reasoning_mode: Option<bool>,
    pub supports_thinking_config: Option<bool>,
    pub supports_streaming_tool_deltas: Option<bool>,
    pub supports_fim: Option<bool>,
    pub supports_image_input: Option<bool>,
    pub strict_empty_content_filtering: Option<bool>,
    pub normalize_tool_call_ids: Option<bool>,
    pub max_safe_tool_count: Option<usize>,
    pub preferred_edit_tool: Option<PreferredEditTool>,
    pub prefers_max_completion_tokens: Option<bool>,
    pub requires_schema_sanitization: Option<bool>,
    pub downgrades_tool_choice_required: Option<bool>,
    pub uses_options_num_predict: Option<bool>,
    pub requires_message_sequence_repair: Option<bool>,
}

impl CapabilityOverride {
    fn apply_to(&self, capabilities: &mut ModelCapabilities) {
        if let Some(value) = self.supports_tool_calling {
            capabilities.supports_tool_calling = value;
        }
        if let Some(value) = self.supports_tool_choice {
            capabilities.supports_tool_choice = value;
        }
        if let Some(value) = self.supports_parallel_tool_calls {
            capabilities.supports_parallel_tool_calls = value;
        }
        if let Some(value) = self.supports_reasoning_mode {
            capabilities.supports_reasoning_mode = value;
        }
        if let Some(value) = self.supports_thinking_config {
            capabilities.supports_thinking_config = value;
        }
        if let Some(value) = self.supports_streaming_tool_deltas {
            capabilities.supports_streaming_tool_deltas = value;
        }
        if let Some(value) = self.supports_fim {
            capabilities.supports_fim = value;
        }
        if let Some(value) = self.supports_image_input {
            capabilities.supports_image_input = value;
        }
        if let Some(value) = self.strict_empty_content_filtering {
            capabilities.strict_empty_content_filtering = value;
        }
        if let Some(value) = self.normalize_tool_call_ids {
            capabilities.normalize_tool_call_ids = value;
        }
        if let Some(value) = self.max_safe_tool_count {
            capabilities.max_safe_tool_count = value.max(1);
        }
        if let Some(value) = self.preferred_edit_tool {
            capabilities.preferred_edit_tool = value;
        }
        if let Some(value) = self.prefers_max_completion_tokens {
            capabilities.prefers_max_completion_tokens = value;
        }
        if let Some(value) = self.requires_schema_sanitization {
            capabilities.requires_schema_sanitization = value;
        }
        if let Some(value) = self.downgrades_tool_choice_required {
            capabilities.downgrades_tool_choice_required = value;
        }
        if let Some(value) = self.uses_options_num_predict {
            capabilities.uses_options_num_predict = value;
        }
        if let Some(value) = self.requires_message_sequence_repair {
            capabilities.requires_message_sequence_repair = value;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct CapabilityRegistryOverrides {
    /// Family-level overrides keyed by either:
    /// - `<family>` (e.g. `qwen`)
    /// - `<provider>@<family>` (e.g. `ollama@qwen`)
    pub families: BTreeMap<String, CapabilityOverride>,
    /// Model-level overrides keyed by either:
    /// - exact model id (e.g. `qwen2.5-coder:7b`)
    /// - prefix wildcard (e.g. `qwen2.5-coder:*`)
    /// - scoped exact/prefix (e.g. `ollama@qwen2.5-coder:*`)
    pub models: BTreeMap<String, CapabilityOverride>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityResolution {
    pub capabilities: ModelCapabilities,
    pub applied_rules: Vec<String>,
}

#[must_use]
pub fn normalize_provider_kind(name: &str) -> Option<ProviderKind> {
    match name.trim().to_ascii_lowercase().as_str() {
        "deepseek" => Some(ProviderKind::Deepseek),
        "openai-compatible" | "openai-compat" | "openai_compat" | "openai" | "custom" | "local" => {
            Some(ProviderKind::OpenAiCompatible)
        }
        "anthropic" | "claude" => Some(ProviderKind::Anthropic),
        "google" | "gemini" => Some(ProviderKind::Google),
        "groq" => Some(ProviderKind::Groq),
        "openrouter" | "open-router" => Some(ProviderKind::OpenRouter),
        "ollama" => Some(ProviderKind::Ollama),
        "azure" | "azure-openai" | "azure_openai" => Some(ProviderKind::Azure),
        "bedrock" | "aws-bedrock" | "aws_bedrock" => Some(ProviderKind::Bedrock),
        "vertex" | "google-vertex" | "vertex-ai" => Some(ProviderKind::Vertex),
        "mistral" | "mistral-ai" | "mistralai" => Some(ProviderKind::MistralApi),
        "xai" | "grok" | "x-ai" => Some(ProviderKind::Xai),
        "together" | "togetherai" | "together-ai" => Some(ProviderKind::Together),
        "copilot" | "github-copilot" | "github_copilot" => Some(ProviderKind::Copilot),
        _ => None,
    }
}

#[must_use]
pub fn detect_model_family(model: &str) -> ModelFamily {
    let lower = model.trim().to_ascii_lowercase();
    if lower.contains("qwen") || lower.contains("qwq") {
        ModelFamily::Qwen
    } else if lower.contains("gemini") {
        ModelFamily::Gemini
    } else if lower.contains("deepseek")
        || lower == CODINGBUDDY_V32_CHAT_MODEL
        || lower == CODINGBUDDY_V32_REASONER_MODEL
    {
        ModelFamily::Deepseek
    } else if lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        ModelFamily::OpenAi
    } else if lower.contains("claude") || lower.contains("opus") || lower.contains("sonnet") {
        ModelFamily::Claude
    } else if lower.contains("llama") {
        ModelFamily::Llama
    } else if lower.contains("mistral") {
        ModelFamily::Mistral
    } else {
        ModelFamily::Generic
    }
}

#[must_use]
pub fn resolve_model_capabilities(
    provider: ProviderKind,
    model: &str,
    registry: Option<&CapabilityRegistryOverrides>,
) -> CapabilityResolution {
    let family = detect_model_family(model);
    let mut capabilities = base_capabilities(provider, model, family);
    let mut applied_rules = vec![format!("base:{}:{}", provider.as_key(), family.as_key())];

    if let Some((rule, family_override)) = built_in_family_override(provider, family) {
        family_override.apply_to(&mut capabilities);
        applied_rules.push(rule.to_string());
    }

    if let Some((rule, model_override)) = built_in_model_override(provider, model) {
        model_override.apply_to(&mut capabilities);
        applied_rules.push(rule.to_string());
    }

    if let Some(registry) = registry {
        for (rule, override_entry) in config_family_overrides(registry, provider, family) {
            override_entry.apply_to(&mut capabilities);
            applied_rules.push(rule);
        }
        for (rule, override_entry) in config_model_overrides(registry, provider, model) {
            override_entry.apply_to(&mut capabilities);
            applied_rules.push(rule);
        }
    }

    CapabilityResolution {
        capabilities,
        applied_rules,
    }
}

#[must_use]
pub fn model_capabilities(provider: ProviderKind, model: &str) -> ModelCapabilities {
    resolve_model_capabilities(provider, model, None).capabilities
}

#[must_use]
pub fn model_capabilities_with_registry(
    provider: ProviderKind,
    model: &str,
    registry: &CapabilityRegistryOverrides,
) -> ModelCapabilities {
    resolve_model_capabilities(provider, model, Some(registry)).capabilities
}

fn base_capabilities(
    provider: ProviderKind,
    model: &str,
    family: ModelFamily,
) -> ModelCapabilities {
    match provider {
        ProviderKind::Deepseek => {
            let is_reasoner = crate::is_reasoner_model(model);
            ModelCapabilities {
                provider,
                family,
                supports_tool_calling: true,
                supports_tool_choice: !is_reasoner,
                supports_parallel_tool_calls: !is_reasoner,
                supports_reasoning_mode: is_reasoner,
                supports_thinking_config: !is_reasoner,
                supports_streaming_tool_deltas: true,
                supports_fim: true,
                supports_image_input: !is_reasoner,
                strict_empty_content_filtering: false,
                normalize_tool_call_ids: false,
                max_safe_tool_count: if is_reasoner { 10 } else { 14 },
                preferred_edit_tool: PreferredEditTool::FsEdit,
                prefers_max_completion_tokens: false,
                requires_schema_sanitization: false,
                downgrades_tool_choice_required: false,
                uses_options_num_predict: false,
                requires_message_sequence_repair: false,
                context_window_tokens: if is_reasoner { 64_000 } else { 128_000 },
                cost_per_mtok_input: if is_reasoner { 0.55 } else { 0.27 },
                cost_per_mtok_output: if is_reasoner { 2.19 } else { 1.10 },
                thinking_capability: if is_reasoner {
                    ThinkingCapability::NativeReasoning
                } else {
                    ThinkingCapability::ExtendedThinking
                },
            }
        }
        ProviderKind::OpenAiCompatible => {
            let is_reasoning = {
                let l = model.trim().to_ascii_lowercase();
                l.starts_with("o1")
                    || l.starts_with("o3")
                    || l.starts_with("o4")
                    || l.contains("reasoning")
            };
            ModelCapabilities {
                provider,
                family,
                supports_tool_calling: true,
                supports_tool_choice: true,
                supports_parallel_tool_calls: true,
                supports_reasoning_mode: false,
                supports_thinking_config: false,
                supports_streaming_tool_deltas: true,
                supports_fim: false,
                supports_image_input: true,
                strict_empty_content_filtering: true,
                normalize_tool_call_ids: false,
                max_safe_tool_count: 18,
                preferred_edit_tool: PreferredEditTool::PatchDirect,
                prefers_max_completion_tokens: is_reasoning,
                requires_schema_sanitization: family == ModelFamily::Gemini,
                downgrades_tool_choice_required: family == ModelFamily::Gemini,
                uses_options_num_predict: false,
                requires_message_sequence_repair: family == ModelFamily::Mistral,
                context_window_tokens: 128_000,
                cost_per_mtok_input: 2.50,
                cost_per_mtok_output: 10.0,
                thinking_capability: if is_reasoning {
                    ThinkingCapability::ImplicitReasoning
                } else {
                    ThinkingCapability::None
                },
            }
        }
        ProviderKind::Anthropic => ModelCapabilities {
            provider,
            family,
            supports_tool_calling: true,
            supports_tool_choice: true,
            supports_parallel_tool_calls: true,
            supports_reasoning_mode: true,
            supports_thinking_config: true,
            supports_streaming_tool_deltas: true,
            supports_fim: false,
            supports_image_input: true,
            strict_empty_content_filtering: false,
            normalize_tool_call_ids: false,
            max_safe_tool_count: 40,
            preferred_edit_tool: PreferredEditTool::FsEdit,
            prefers_max_completion_tokens: false,
            requires_schema_sanitization: false,
            downgrades_tool_choice_required: false,
            uses_options_num_predict: false,
            requires_message_sequence_repair: false,
            context_window_tokens: 200_000,
            cost_per_mtok_input: 3.0,
            cost_per_mtok_output: 15.0,
            thinking_capability: ThinkingCapability::ExtendedThinking,
        },
        ProviderKind::Google => ModelCapabilities {
            provider,
            family,
            supports_tool_calling: true,
            supports_tool_choice: true,
            supports_parallel_tool_calls: true,
            supports_reasoning_mode: false,
            supports_thinking_config: false,
            supports_streaming_tool_deltas: true,
            supports_fim: false,
            supports_image_input: true,
            strict_empty_content_filtering: false,
            normalize_tool_call_ids: false,
            max_safe_tool_count: 24,
            preferred_edit_tool: PreferredEditTool::FsEdit,
            prefers_max_completion_tokens: false,
            requires_schema_sanitization: true,
            downgrades_tool_choice_required: true,
            uses_options_num_predict: false,
            requires_message_sequence_repair: false,
            context_window_tokens: 1_000_000,
            cost_per_mtok_input: 1.25,
            cost_per_mtok_output: 10.0,
            thinking_capability: ThinkingCapability::None,
        },
        ProviderKind::Groq => ModelCapabilities {
            provider,
            family,
            supports_tool_calling: true,
            supports_tool_choice: true,
            supports_parallel_tool_calls: true,
            supports_reasoning_mode: false,
            supports_thinking_config: false,
            supports_streaming_tool_deltas: true,
            supports_fim: false,
            supports_image_input: false,
            strict_empty_content_filtering: true,
            normalize_tool_call_ids: false,
            max_safe_tool_count: 18,
            preferred_edit_tool: PreferredEditTool::FsEdit,
            prefers_max_completion_tokens: false,
            requires_schema_sanitization: false,
            downgrades_tool_choice_required: false,
            uses_options_num_predict: false,
            requires_message_sequence_repair: false,
            context_window_tokens: 128_000,
            cost_per_mtok_input: 0.59,
            cost_per_mtok_output: 0.79,
            thinking_capability: ThinkingCapability::None,
        },
        ProviderKind::OpenRouter => ModelCapabilities {
            provider,
            family,
            supports_tool_calling: true,
            supports_tool_choice: true,
            supports_parallel_tool_calls: true,
            supports_reasoning_mode: false,
            supports_thinking_config: false,
            supports_streaming_tool_deltas: true,
            supports_fim: false,
            supports_image_input: true,
            strict_empty_content_filtering: false,
            normalize_tool_call_ids: false,
            max_safe_tool_count: 24,
            preferred_edit_tool: PreferredEditTool::FsEdit,
            prefers_max_completion_tokens: false,
            requires_schema_sanitization: false,
            downgrades_tool_choice_required: false,
            uses_options_num_predict: false,
            requires_message_sequence_repair: false,
            context_window_tokens: 128_000,
            cost_per_mtok_input: 3.0,
            cost_per_mtok_output: 15.0,
            thinking_capability: ThinkingCapability::None,
        },
        ProviderKind::Ollama => ModelCapabilities {
            provider,
            family,
            supports_tool_calling: true,
            supports_tool_choice: true,
            supports_parallel_tool_calls: false,
            supports_reasoning_mode: false,
            supports_thinking_config: false,
            supports_streaming_tool_deltas: true,
            supports_fim: false,
            supports_image_input: false,
            strict_empty_content_filtering: true,
            normalize_tool_call_ids: true,
            max_safe_tool_count: 12,
            preferred_edit_tool: PreferredEditTool::FsEdit,
            prefers_max_completion_tokens: false,
            requires_schema_sanitization: false,
            downgrades_tool_choice_required: true,
            uses_options_num_predict: true,
            requires_message_sequence_repair: family == ModelFamily::Mistral,
            context_window_tokens: 32_000,
            cost_per_mtok_input: 0.0,
            cost_per_mtok_output: 0.0,
            thinking_capability: ThinkingCapability::None,
        },
        // All remaining providers use OpenAI-compatible API with provider-specific defaults
        ProviderKind::Azure
        | ProviderKind::Bedrock
        | ProviderKind::Vertex
        | ProviderKind::MistralApi
        | ProviderKind::Xai
        | ProviderKind::Together
        | ProviderKind::Copilot => {
            let (input_cost, output_cost) = match provider {
                ProviderKind::Azure => (2.50, 10.0),
                ProviderKind::Bedrock => (3.0, 15.0),
                ProviderKind::Vertex => (1.25, 10.0),
                ProviderKind::MistralApi => (2.0, 6.0),
                ProviderKind::Xai => (2.0, 10.0),
                ProviderKind::Together => (0.80, 0.80),
                ProviderKind::Copilot => (0.0, 0.0),
                _ => (0.0, 0.0),
            };
            ModelCapabilities {
                provider,
                family,
                supports_tool_calling: true,
                supports_tool_choice: true,
                supports_parallel_tool_calls: true,
                supports_reasoning_mode: false,
                supports_thinking_config: false,
                supports_streaming_tool_deltas: true,
                supports_fim: false,
                supports_image_input: true,
                strict_empty_content_filtering: false,
                normalize_tool_call_ids: false,
                max_safe_tool_count: 18,
                preferred_edit_tool: PreferredEditTool::FsEdit,
                prefers_max_completion_tokens: false,
                requires_schema_sanitization: false,
                downgrades_tool_choice_required: false,
                uses_options_num_predict: false,
                requires_message_sequence_repair: false,
                context_window_tokens: 128_000,
                cost_per_mtok_input: input_cost,
                cost_per_mtok_output: output_cost,
                thinking_capability: ThinkingCapability::None,
            }
        }
    }
}

fn built_in_family_override(
    provider: ProviderKind,
    family: ModelFamily,
) -> Option<(&'static str, CapabilityOverride)> {
    match (provider, family) {
        (ProviderKind::Ollama, ModelFamily::Qwen) => Some((
            "builtin_family:ollama@qwen",
            CapabilityOverride {
                max_safe_tool_count: Some(14),
                preferred_edit_tool: Some(PreferredEditTool::MultiEdit),
                supports_parallel_tool_calls: Some(false),
                ..CapabilityOverride::default()
            },
        )),
        (ProviderKind::Ollama, ModelFamily::Llama) => Some((
            "builtin_family:ollama@llama",
            CapabilityOverride {
                supports_tool_choice: Some(false),
                max_safe_tool_count: Some(9),
                preferred_edit_tool: Some(PreferredEditTool::PatchDirect),
                ..CapabilityOverride::default()
            },
        )),
        (ProviderKind::Ollama, ModelFamily::Deepseek) => Some((
            "builtin_family:ollama@deepseek",
            CapabilityOverride {
                supports_tool_choice: Some(false),
                supports_parallel_tool_calls: Some(false),
                supports_reasoning_mode: Some(true),
                supports_thinking_config: Some(false),
                max_safe_tool_count: Some(8),
                preferred_edit_tool: Some(PreferredEditTool::PatchDirect),
                ..CapabilityOverride::default()
            },
        )),
        (ProviderKind::OpenAiCompatible, ModelFamily::Gemini) => Some((
            "builtin_family:openai-compatible@gemini",
            CapabilityOverride {
                supports_parallel_tool_calls: Some(false),
                max_safe_tool_count: Some(14),
                ..CapabilityOverride::default()
            },
        )),
        (ProviderKind::OpenAiCompatible, ModelFamily::Qwen) => Some((
            "builtin_family:openai-compatible@qwen",
            CapabilityOverride {
                supports_parallel_tool_calls: Some(false),
                max_safe_tool_count: Some(16),
                preferred_edit_tool: Some(PreferredEditTool::FsEdit),
                ..CapabilityOverride::default()
            },
        )),
        (ProviderKind::OpenAiCompatible, ModelFamily::Llama) => Some((
            "builtin_family:openai-compatible@llama",
            CapabilityOverride {
                supports_parallel_tool_calls: Some(false),
                max_safe_tool_count: Some(12),
                preferred_edit_tool: Some(PreferredEditTool::FsEdit),
                ..CapabilityOverride::default()
            },
        )),
        // Note: prefers_max_completion_tokens set via detect_model_family
        // for o1/o3/o4 models, not all OpenAI models.
        _ => None,
    }
}

fn built_in_model_override(
    provider: ProviderKind,
    model: &str,
) -> Option<(&'static str, CapabilityOverride)> {
    let lower = model.trim().to_ascii_lowercase();
    if provider != ProviderKind::Deepseek
        && (lower.contains("deepseek-r1") || lower.contains("deepseek-reasoner"))
    {
        return Some((
            "builtin_model:deepseek-r1",
            CapabilityOverride {
                supports_tool_choice: Some(false),
                supports_parallel_tool_calls: Some(false),
                supports_reasoning_mode: Some(true),
                supports_thinking_config: Some(false),
                max_safe_tool_count: Some(8),
                preferred_edit_tool: Some(PreferredEditTool::PatchDirect),
                ..CapabilityOverride::default()
            },
        ));
    }
    if lower.contains("qwen2.5-coder:1.5b") || lower.contains("qwen2.5-coder-1.5b") {
        return Some((
            "builtin_model:qwen2.5-coder-1.5b",
            CapabilityOverride {
                max_safe_tool_count: Some(10),
                ..CapabilityOverride::default()
            },
        ));
    }
    if lower.contains("qwen2.5-coder:3b") || lower.contains("qwen2.5-coder-3b") {
        return Some((
            "builtin_model:qwen2.5-coder-3b",
            CapabilityOverride {
                max_safe_tool_count: Some(12),
                ..CapabilityOverride::default()
            },
        ));
    }
    if lower.contains("qwen2.5-coder:7b") || lower.contains("qwen2.5-coder-7b") {
        return Some((
            "builtin_model:qwen2.5-coder-7b",
            CapabilityOverride {
                max_safe_tool_count: Some(14),
                preferred_edit_tool: Some(PreferredEditTool::MultiEdit),
                ..CapabilityOverride::default()
            },
        ));
    }
    if lower.contains("llama3.1:8b") || lower.contains("llama3:8b") {
        return Some((
            "builtin_model:llama3-8b",
            CapabilityOverride {
                max_safe_tool_count: Some(8),
                supports_tool_choice: Some(false),
                ..CapabilityOverride::default()
            },
        ));
    }
    if lower.contains("gemini-2.0-flash") {
        return Some((
            "builtin_model:gemini-2.0-flash",
            CapabilityOverride {
                max_safe_tool_count: Some(12),
                ..CapabilityOverride::default()
            },
        ));
    }
    if lower.contains("llava")
        || lower.contains("bakllava")
        || lower.contains("moondream")
        || lower.contains("vision")
    {
        return Some((
            "builtin_model:vision-family",
            CapabilityOverride {
                supports_image_input: Some(true),
                ..CapabilityOverride::default()
            },
        ));
    }
    None
}

fn config_family_overrides(
    registry: &CapabilityRegistryOverrides,
    provider: ProviderKind,
    family: ModelFamily,
) -> Vec<(String, &CapabilityOverride)> {
    let mut out = Vec::new();
    let generic_key = family.as_key().to_string();
    if let Some(override_entry) = registry.families.get(&generic_key) {
        out.push((format!("config_family:{generic_key}"), override_entry));
    }
    let scoped_key = format!("{}@{}", provider.as_key(), family.as_key());
    if let Some(override_entry) = registry.families.get(&scoped_key) {
        out.push((format!("config_family:{scoped_key}"), override_entry));
    }
    out
}

fn config_model_overrides<'a>(
    registry: &'a CapabilityRegistryOverrides,
    provider: ProviderKind,
    model: &str,
) -> Vec<(String, &'a CapabilityOverride)> {
    let model_norm = model.trim().to_ascii_lowercase();
    let mut matches = Vec::new();
    for (raw_key, override_entry) in &registry.models {
        if let Some(rank) = model_override_match_rank(raw_key, provider, &model_norm) {
            matches.push((rank, raw_key.clone(), override_entry));
        }
    }
    matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    matches
        .into_iter()
        .map(|(_, key, override_entry)| (format!("config_model:{key}"), override_entry))
        .collect()
}

fn model_override_match_rank(key: &str, provider: ProviderKind, model_norm: &str) -> Option<u8> {
    let (scoped_provider, pattern) = split_provider_scope(key);
    if let Some(scope) = scoped_provider
        && scope != provider
    {
        return None;
    }
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return None;
    }
    let wildcard = pattern.ends_with('*');
    let matched = if wildcard {
        let prefix = pattern.trim_end_matches('*').trim();
        !prefix.is_empty() && model_norm.starts_with(prefix)
    } else {
        model_norm == pattern
    };
    if !matched {
        return None;
    }
    let rank = match (scoped_provider.is_some(), wildcard) {
        (false, true) => 0,
        (false, false) => 1,
        (true, true) => 2,
        (true, false) => 3,
    };
    Some(rank)
}

fn split_provider_scope(key: &str) -> (Option<ProviderKind>, String) {
    let normalized = key.trim().to_ascii_lowercase();
    let Some((provider_raw, pattern_raw)) = normalized.split_once('@') else {
        return (None, normalized);
    };
    let Some(provider) = normalize_provider_kind(provider_raw) else {
        return (None, normalized);
    };
    (Some(provider), pattern_raw.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_qwen_family_override_applies() {
        let caps = model_capabilities(ProviderKind::Ollama, "qwen2.5-coder:7b");
        assert_eq!(caps.family, ModelFamily::Qwen);
        assert_eq!(caps.preferred_edit_tool, PreferredEditTool::MultiEdit);
        assert_eq!(caps.max_safe_tool_count, 14);
        assert!(!caps.supports_image_input);
        assert!(caps.strict_empty_content_filtering);
        assert!(caps.normalize_tool_call_ids);
    }

    #[test]
    fn ollama_deepseek_r1_model_override_disables_tool_choice() {
        let caps = model_capabilities(ProviderKind::Ollama, "deepseek-r1:14b");
        assert!(!caps.supports_tool_choice);
        assert!(caps.supports_reasoning_mode);
        assert_eq!(caps.preferred_edit_tool, PreferredEditTool::PatchDirect);
    }

    #[test]
    fn preferred_edit_tool_contracts_match_provider_and_model() {
        let cases = [
            (
                ProviderKind::Deepseek,
                "deepseek-chat",
                PreferredEditTool::FsEdit,
            ),
            (
                ProviderKind::OpenAiCompatible,
                "gpt-4o-mini",
                PreferredEditTool::PatchDirect,
            ),
            (
                ProviderKind::OpenAiCompatible,
                "qwen2.5-coder:7b",
                PreferredEditTool::MultiEdit,
            ),
            (
                ProviderKind::Ollama,
                "qwen2.5-coder:7b",
                PreferredEditTool::MultiEdit,
            ),
            (
                ProviderKind::Ollama,
                "llama3.1:8b",
                PreferredEditTool::PatchDirect,
            ),
        ];

        for (provider, model, expected) in cases {
            let caps = model_capabilities(provider, model);
            assert_eq!(
                caps.preferred_edit_tool,
                expected,
                "unexpected preferred edit tool for provider={} model={}",
                provider.as_key(),
                model
            );
        }
    }

    #[test]
    fn config_overrides_apply_in_specificity_order() {
        let mut registry = CapabilityRegistryOverrides::default();
        registry.families.insert(
            "qwen".to_string(),
            CapabilityOverride {
                max_safe_tool_count: Some(11),
                ..CapabilityOverride::default()
            },
        );
        registry.families.insert(
            "ollama@qwen".to_string(),
            CapabilityOverride {
                max_safe_tool_count: Some(13),
                ..CapabilityOverride::default()
            },
        );
        registry.models.insert(
            "qwen2.5-coder:*".to_string(),
            CapabilityOverride {
                max_safe_tool_count: Some(9),
                ..CapabilityOverride::default()
            },
        );
        registry.models.insert(
            "ollama@qwen2.5-coder:7b".to_string(),
            CapabilityOverride {
                max_safe_tool_count: Some(7),
                supports_tool_choice: Some(false),
                ..CapabilityOverride::default()
            },
        );

        let resolved =
            resolve_model_capabilities(ProviderKind::Ollama, "qwen2.5-coder:7b", Some(&registry));
        assert_eq!(resolved.capabilities.max_safe_tool_count, 7);
        assert!(!resolved.capabilities.supports_tool_choice);
        assert!(
            resolved
                .applied_rules
                .iter()
                .any(|rule| rule.contains("config_model:ollama@qwen2.5-coder:7b"))
        );
    }

    #[test]
    fn scoped_override_ignores_other_provider() {
        let mut registry = CapabilityRegistryOverrides::default();
        registry.models.insert(
            "deepseek@qwen2.5-coder:*".to_string(),
            CapabilityOverride {
                max_safe_tool_count: Some(3),
                ..CapabilityOverride::default()
            },
        );

        let caps =
            model_capabilities_with_registry(ProviderKind::Ollama, "qwen2.5-coder:7b", &registry);
        assert_ne!(caps.max_safe_tool_count, 3);
    }

    #[test]
    fn vision_family_model_override_enables_image_input() {
        let caps = model_capabilities(ProviderKind::Ollama, "llava:13b");
        assert!(caps.supports_image_input);
    }

    #[test]
    fn transform_flags_can_be_overridden_from_registry() {
        let mut registry = CapabilityRegistryOverrides::default();
        registry.models.insert(
            "ollama@qwen2.5-coder:7b".to_string(),
            CapabilityOverride {
                supports_image_input: Some(true),
                strict_empty_content_filtering: Some(false),
                normalize_tool_call_ids: Some(false),
                ..CapabilityOverride::default()
            },
        );
        let caps =
            model_capabilities_with_registry(ProviderKind::Ollama, "qwen2.5-coder:7b", &registry);
        assert!(caps.supports_image_input);
        assert!(!caps.strict_empty_content_filtering);
        assert!(!caps.normalize_tool_call_ids);
    }
}
