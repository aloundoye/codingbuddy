//! Post-tool verification helpers.
use codingbuddy_core::ToolHostBlockingExt;

use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn post_edit_diagnostics(
    validator: Option<&Arc<codingbuddy_lsp::EditValidator>>,
    modified_paths: &[PathBuf],
    host: &(dyn codingbuddy_core::ToolHost + Send + Sync),
    context: codingbuddy_core::ToolExecutionContext<'_>,
) -> String {
    let Some(validator) = validator else {
        return String::new();
    };
    validator
        .command_plans(modified_paths)
        .iter()
        .map(|plan| {
            host.verify_command_blocking(&plan.command, &plan.cwd, plan.timeout_seconds, context)
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
