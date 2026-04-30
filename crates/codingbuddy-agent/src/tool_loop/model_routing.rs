//! Model and thinking-budget routing for the tool-use loop.

use super::types::ToolLoopConfig;

pub(super) fn next_request_route(
    config: &ToolLoopConfig,
    escalation: &codingbuddy_core::complexity::EscalationSignals,
) -> (String, Option<codingbuddy_core::ThinkingConfig>, u32) {
    // Always use the user-configured model. No mid-session model switching.
    // Thinking budget escalates based on evidence (compile errors, test failures).
    let thinking = config.thinking.as_ref().map(|base| {
        if escalation.should_escalate() {
            codingbuddy_core::ThinkingConfig::enabled(escalation.budget())
        } else {
            base.clone()
        }
    });
    (config.model.clone(), thinking, config.max_tokens)
}
