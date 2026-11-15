pub(super) fn is_isolated_sandbox_mode(mode: &str) -> bool {
    matches!(mode, "isolated" | "container" | "os-sandbox" | "os_sandbox")
}

pub(crate) fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}
