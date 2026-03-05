use codingbuddy_core::{AppConfig, ModelFamily, ProviderKind};
use codingbuddy_local_ml::RuntimeLifecycleSnapshot;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct ProviderCompatibilityDiagnostics {
    pub provider: String,
    pub family: String,
    pub summary: String,
    pub active_transforms: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct RuntimeOperatorDiagnostics {
    pub summary: String,
    pub highlights: Vec<String>,
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

pub(crate) fn runtime_operator_diagnostics(
    snapshot: &RuntimeLifecycleSnapshot,
) -> RuntimeOperatorDiagnostics {
    let warm = snapshot.warm_models.len();
    let cap = snapshot.max_loaded_models.max(1);
    let metrics = &snapshot.metrics;
    let last_event = snapshot
        .recent_events
        .last()
        .map(|event| match event.model_id.as_deref() {
            Some(model_id) => format!("{}:{model_id}", event.kind),
            None => event.kind.clone(),
        })
        .unwrap_or_else(|| "none".to_string());

    let mut highlights = Vec::new();
    if metrics.total_runner_load_waits > 0 {
        highlights.push(format!("load_waits={}", metrics.total_runner_load_waits));
    }
    if metrics.total_memory_pressure_evictions > 0 {
        highlights.push(format!(
            "memory_pressure_evictions={}",
            metrics.total_memory_pressure_evictions
        ));
    }
    if metrics.total_memory_admission_denied > 0 {
        highlights.push(format!(
            "memory_denied={}",
            metrics.total_memory_admission_denied
        ));
    }
    if metrics.total_runner_load_failures > 0 {
        highlights.push(format!(
            "load_failures={}",
            metrics.total_runner_load_failures
        ));
    }
    if metrics.total_runner_reloads > 0 {
        highlights.push(format!("reloads={}", metrics.total_runner_reloads));
    }
    if highlights.is_empty() {
        highlights.push("steady".to_string());
    }

    let summary = format!(
        "warm={warm}/{cap} queue_peak={} load_waits={} last={last_event}",
        metrics.max_observed_queue_depth, metrics.total_runner_load_waits
    );

    RuntimeOperatorDiagnostics {
        summary,
        highlights,
    }
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
    use codingbuddy_local_ml::RuntimeLifecycleMetrics;

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

    #[test]
    fn runtime_diagnostics_surface_pressure_signals() {
        let snapshot = RuntimeLifecycleSnapshot {
            max_loaded_models: 2,
            keep_warm_secs: 300,
            aggressive_eviction: false,
            scheduler: Default::default(),
            warm_models: vec!["model-a".to_string()],
            metrics: RuntimeLifecycleMetrics {
                total_runner_load_waits: 2,
                total_memory_pressure_evictions: 1,
                total_memory_admission_denied: 1,
                total_runner_load_failures: 1,
                max_observed_queue_depth: 3,
                ..RuntimeLifecycleMetrics::default()
            },
            recent_events: vec![codingbuddy_local_ml::RuntimeLifecycleEvent {
                kind: "runner_load_wait".to_string(),
                model_id: Some("model-b".to_string()),
                at_epoch_secs: 42,
                detail: None,
            }],
        };

        let diagnostics = runtime_operator_diagnostics(&snapshot);
        assert!(diagnostics.summary.contains("warm=1/2"));
        assert!(diagnostics.summary.contains("queue_peak=3"));
        assert!(diagnostics.summary.contains("load_waits=2"));
        assert!(
            diagnostics
                .highlights
                .iter()
                .any(|item| item == "memory_pressure_evictions=1")
        );
    }
}
