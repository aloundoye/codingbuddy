use codingbuddy_core::{AppConfig, AppliedCompatibility, ModelFamily, ProviderKind};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct ProviderCompatibilityDiagnostics {
    pub provider: String,
    pub family: String,
    pub summary: String,
    pub active_transforms: Vec<String>,
}

pub(crate) fn summarize_applied_compatibility(
    compatibility: Option<&AppliedCompatibility>,
) -> Option<String> {
    let compatibility = compatibility?;
    if compatibility.transforms.is_empty() && compatibility.degraded_inputs.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !compatibility.transforms.is_empty() {
        parts.push(format!("applied={}", compatibility.transforms.join(",")));
    }
    if !compatibility.degraded_inputs.is_empty() {
        parts.push(format!(
            "degraded={}",
            compatibility.degraded_inputs.join(",")
        ));
    }
    Some(parts.join(" "))
}

pub(crate) fn provider_compatibility_diagnostics(
    cfg: &AppConfig,
    model: &str,
) -> Option<ProviderCompatibilityDiagnostics> {
    let resolution = cfg.llm.capability_resolution_for_model(model)?;
    let caps = resolution.capabilities;
    let provider = cfg.llm.active_provider();
    let mut active = Vec::new();

    active.push("tool-name-repair".to_string());

    if caps.normalize_tool_call_ids {
        active.push("tool-id-normalization".to_string());
    }
    if caps.strict_empty_content_filtering {
        active.push("strict-empty-filtering".to_string());
    }

    match caps.provider {
        ProviderKind::OpenAiCompatible => {
            active.push("thinking->reasoning_effort".to_string());
            if prefers_max_completion_tokens(model) {
                active.push("max_tokens->max_completion_tokens".to_string());
                active.push("sampling-strip-on-reasoning".to_string());
            }
            if caps.family == ModelFamily::Gemini {
                active.push("gemini-schema-sanitize".to_string());
                active.push("required->auto-tool_choice".to_string());
                active.push("max_output_tokens-alias".to_string());
            }
            if looks_like_litellm_proxy(&provider.base_url, &cfg.llm.endpoint) {
                active.push("litellm-placeholder-tool".to_string());
            }
        }
        ProviderKind::Ollama => {
            active.push("required->auto-tool_choice".to_string());
            active.push("max_tokens->options.num_predict".to_string());
        }
        ProviderKind::Anthropic => {
            active.push("anthropic-cache-control".to_string());
        }
        ProviderKind::Google => {
            active.push("gemini-schema-sanitize".to_string());
            active.push("max_output_tokens-alias".to_string());
        }
        ProviderKind::Groq
        | ProviderKind::OpenRouter
        | ProviderKind::Azure
        | ProviderKind::Bedrock
        | ProviderKind::Vertex
        | ProviderKind::MistralApi
        | ProviderKind::Xai
        | ProviderKind::Together
        | ProviderKind::Copilot => {}
        ProviderKind::Deepseek => {}
    }

    let summary = active.join(", ");
    Some(ProviderCompatibilityDiagnostics {
        provider: caps.provider.as_key().to_string(),
        family: caps.family.as_key().to_string(),
        summary,
        active_transforms: active,
    })
}

fn prefers_max_completion_tokens(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.contains("reasoning")
}

fn looks_like_litellm_proxy(base_url: &str, endpoint: &str) -> bool {
    let lower_base = base_url.to_ascii_lowercase();
    let lower_endpoint = endpoint.to_ascii_lowercase();
    lower_base.contains("litellm") || lower_endpoint.contains("litellm")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codingbuddy_core::AppConfig;

    #[test]
    fn provider_diagnostics_include_openai_reasoning_and_litellm_shims() {
        let mut cfg = AppConfig::default();
        cfg.llm.provider = "openai-compatible".to_string();
        if let Some(provider) = cfg.llm.providers.get_mut("openai-compatible") {
            provider.base_url = "https://litellm.internal".to_string();
            provider.models.chat = "o3-mini".to_string();
        }
        cfg.llm.endpoint = "https://litellm.internal/v1/chat/completions".to_string();

        let diagnostics = provider_compatibility_diagnostics(&cfg, "o3-mini").expect("diagnostics");
        assert!(diagnostics.summary.contains("thinking->reasoning_effort"));
        assert!(
            diagnostics
                .active_transforms
                .iter()
                .any(|item| item == "max_tokens->max_completion_tokens")
        );
        assert!(
            diagnostics
                .active_transforms
                .iter()
                .any(|item| item == "litellm-placeholder-tool")
        );
    }
}
