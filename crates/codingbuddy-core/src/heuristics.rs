//! Corrective prompt heuristics, enabled per model family.
//!
//! The tool loop can steer a model with extra nudges: rejecting a long answer
//! written without tools, rejecting answers that name unread files, asking
//! for tool use when shell commands are printed as text, injecting recovery
//! guidance after failures, and trimming verbose answers. They help weaker
//! models stay grounded, but each one can also throw away a correct answer
//! and costs an extra request. Strong tool-using models (Claude, GPT,
//! Gemini) do not need them, so they default to off there.
//!
//! Safety limits are not heuristics and are not configured here: the
//! doom-loop stop and the tool circuit breaker always apply.
//!
//! Settings (`agent_loop.heuristics` in settings.json), later wins:
//! 1. model-family default
//! 2. top-level fields, e.g. `"grounding_nudge": false`
//! 3. `providers.<provider id>`, e.g. `"providers": {"google": {"verbosity_nudge": true}}`

use crate::llm_capabilities::{ModelFamily, detect_model_family};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One layer of overrides; `None` means "inherit".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeuristicOverrides {
    /// Reject a long answer written before any tool ran this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grounding_nudge: Option<bool>,
    /// Reject an answer naming files no tool touched or mentioned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unverified_file_refs: Option<bool>,
    /// Ask for tool use when shell commands are printed as text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_command_nudge: Option<bool>,
    /// Inject recovery and "stuck" guidance after repeated tool failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_recovery_guidance: Option<bool>,
    /// Remind complex tasks to keep the todo checklist updated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complex_checklist_nudge: Option<bool>,
    /// Ask for a shorter answer when prose exceeds 400 words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity_nudge: Option<bool>,
}

/// `agent_loop.heuristics`: global overrides plus per-provider overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeuristicsConfig {
    #[serde(flatten)]
    pub overrides: HeuristicOverrides,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, HeuristicOverrides>,
}

/// The effective switches for one model on one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolvedHeuristics {
    pub grounding_nudge: bool,
    pub unverified_file_refs: bool,
    pub shell_command_nudge: bool,
    pub error_recovery_guidance: bool,
    pub complex_checklist_nudge: bool,
    pub verbosity_nudge: bool,
}

impl ResolvedHeuristics {
    #[must_use]
    pub const fn all(enabled: bool) -> Self {
        Self {
            grounding_nudge: enabled,
            unverified_file_refs: enabled,
            shell_command_nudge: enabled,
            error_recovery_guidance: enabled,
            complex_checklist_nudge: enabled,
            verbosity_nudge: enabled,
        }
    }

    /// Defaults for a model family: off for strong tool-using models, on
    /// for everything else (including unknown models).
    #[must_use]
    pub fn for_family(family: ModelFamily) -> Self {
        match family {
            ModelFamily::Claude | ModelFamily::OpenAi | ModelFamily::Gemini => Self::all(false),
            ModelFamily::Deepseek
            | ModelFamily::Qwen
            | ModelFamily::Llama
            | ModelFamily::Mistral
            | ModelFamily::Generic => Self::all(true),
        }
    }

    fn apply(&mut self, overrides: &HeuristicOverrides) {
        let HeuristicOverrides {
            grounding_nudge,
            unverified_file_refs,
            shell_command_nudge,
            error_recovery_guidance,
            complex_checklist_nudge,
            verbosity_nudge,
        } = overrides;
        for (target, value) in [
            (&mut self.grounding_nudge, grounding_nudge),
            (&mut self.unverified_file_refs, unverified_file_refs),
            (&mut self.shell_command_nudge, shell_command_nudge),
            (&mut self.error_recovery_guidance, error_recovery_guidance),
            (&mut self.complex_checklist_nudge, complex_checklist_nudge),
            (&mut self.verbosity_nudge, verbosity_nudge),
        ] {
            if let Some(value) = value {
                *target = *value;
            }
        }
    }
}

impl Default for ResolvedHeuristics {
    /// Everything on: the behaviour before heuristics were configurable.
    fn default() -> Self {
        Self::all(true)
    }
}

impl HeuristicsConfig {
    /// Resolve the switches for `model` served by `provider`.
    #[must_use]
    pub fn resolve(&self, provider: &str, model: &str) -> ResolvedHeuristics {
        let mut resolved = ResolvedHeuristics::for_family(detect_model_family(model));
        resolved.apply(&self.overrides);
        if let Some(provider_overrides) = self.providers.get(provider) {
            resolved.apply(provider_overrides);
        }
        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_defaults_are_off_for_strong_models_only() {
        let config = HeuristicsConfig::default();
        for model in [
            "claude-sonnet-4-20250514",
            "gpt-4.1-mini",
            "gemini-2.5-flash",
        ] {
            assert_eq!(
                config.resolve("any", model),
                ResolvedHeuristics::all(false),
                "{model}"
            );
        }
        for model in [
            "deepseek-chat",
            "qwen2.5-coder:7b",
            "llama3.1",
            "some-unknown-model",
        ] {
            assert_eq!(
                config.resolve("any", model),
                ResolvedHeuristics::all(true),
                "{model}"
            );
        }
    }

    #[test]
    fn provider_overrides_beat_global_overrides_beat_family_defaults() {
        let config: HeuristicsConfig = serde_json::from_value(serde_json::json!({
            "grounding_nudge": true,
            "verbosity_nudge": true,
            "providers": {"google": {"verbosity_nudge": false}}
        }))
        .expect("valid config");

        let gemini = config.resolve("google", "gemini-2.5-flash");
        assert!(gemini.grounding_nudge, "global override turns it on");
        assert!(!gemini.verbosity_nudge, "provider override wins");
        assert!(
            !gemini.shell_command_nudge,
            "untouched field keeps family default"
        );

        let other = config.resolve("openrouter", "gemini-2.5-flash");
        assert!(
            other.verbosity_nudge,
            "provider override is scoped to its provider"
        );
    }

    #[test]
    fn unknown_fields_are_rejected_and_empty_config_round_trips() {
        assert!(
            serde_json::from_value::<HeuristicsConfig>(serde_json::json!({
                "providers": {"google": {"grounding_nudges": false}}
            }))
            .is_err()
        );
        let json = serde_json::to_value(HeuristicsConfig::default()).expect("serialize");
        assert_eq!(json, serde_json::json!({}));
    }
}
