//! Post-tool verification helpers.
use std::path::PathBuf;
use std::sync::Arc;

pub(super) async fn post_edit_diagnostics(
    validator: Option<&Arc<codingbuddy_lsp::EditValidator>>,
    modified_paths: &[PathBuf],
    host: &(dyn codingbuddy_core::ToolHost + Send + Sync),
    context: codingbuddy_core::ToolExecutionContext<'_>,
) -> String {
    let Some(validator) = validator else {
        return String::new();
    };
    let mut reports = Vec::new();
    for plan in validator.command_plans(modified_paths) {
        reports.push(
            host.verify_command(&plan.command, &plan.cwd, plan.timeout_seconds, context)
                .await
                .to_string(),
        );
    }
    reports.join("\n")
}
