//! Post-tool verification helpers.

use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn post_edit_diagnostics(
    validator: Option<&Arc<codingbuddy_lsp::EditValidator>>,
    modified_paths: &[PathBuf],
) -> String {
    let Some(validator) = validator else {
        return String::new();
    };

    let mut diagnostics_text = String::new();
    let mut checked_extensions = std::collections::HashSet::new();
    for path in modified_paths {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        if !checked_extensions.insert(ext) {
            continue;
        }
        if let Ok(diags) = validator.check_file(path)
            && !diags.is_empty()
        {
            diagnostics_text.push_str(&codingbuddy_lsp::EditValidator::format_for_llm(&diags));
            diagnostics_text.push('\n');
        }
    }
    diagnostics_text
}
