use std::path::Path;

use codingbuddy_core::SandboxConfig;

#[cfg(any(target_os = "macos", test))]
pub(super) fn build_seatbelt_profile(workspace: &Path, config: &SandboxConfig) -> String {
    let workspace_str = workspace.to_string_lossy();
    let mut profile = String::from("(version 1)\n(deny default)\n");
    profile.push_str("(allow process*)\n");
    profile.push_str("(allow file-read* (subpath \"/usr\") (subpath \"/lib\") (subpath \"/bin\") (subpath \"/System\"))\n");
    profile.push_str(&format!(
        "(allow file-read* file-write* (subpath \"{}\"))\n",
        workspace_str
    ));
    // Allow /tmp access
    profile
        .push_str("(allow file-read* file-write* (subpath \"/tmp\") (subpath \"/private/tmp\"))\n");
    // Network access
    if config.network.block_all {
        profile.push_str("(deny network*)\n");
        // Even when blocking network, allow local binding if configured
        if config.network.allow_local_binding {
            profile.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
        }
        if config.network.allow_unix_sockets {
            profile.push_str("(allow network* (local unix-socket))\n");
        }
    } else {
        profile.push_str("(allow network*)\n");
    }
    profile
}

/// Wrap a command with macOS Seatbelt sandbox.
#[cfg(any(target_os = "macos", test))]
pub(super) fn seatbelt_wrap(cmd: &str, profile: &str) -> String {
    // Escape single quotes in profile
    let escaped_profile = profile.replace('\'', "'\\''");
    format!("sandbox-exec -p '{}' -- {}", escaped_profile, cmd)
}

/// Build a Linux bubblewrap (bwrap) sandboxed command.
#[cfg(any(target_os = "linux", test))]
pub(super) fn build_bwrap_command(workspace: &Path, cmd: &str, config: &SandboxConfig) -> String {
    let workspace_str = workspace.to_string_lossy();
    let mut parts = vec![
        "bwrap".to_string(),
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
    ];
    // Read-only system paths
    for sys_path in &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
        parts.push("--ro-bind".to_string());
        parts.push(sys_path.to_string());
        parts.push(sys_path.to_string());
    }
    // Read-write workspace
    parts.push("--bind".to_string());
    parts.push(workspace_str.to_string());
    parts.push(workspace_str.to_string());
    // Tmp
    parts.push("--tmpfs".to_string());
    parts.push("/tmp".to_string());
    // Network
    if config.network.block_all {
        if config.network.allow_local_binding || config.network.allow_unix_sockets {
            // When local binding or unix sockets are allowed, we can't fully unshare
            // network. Instead we rely on application-level filtering.
            // bwrap doesn't support fine-grained socket filtering natively.
        } else {
            parts.push("--unshare-net".to_string());
        }
    }
    // Proc and dev
    parts.push("--proc".to_string());
    parts.push("/proc".to_string());
    parts.push("--dev".to_string());
    parts.push("/dev".to_string());
    parts.push("--".to_string());
    parts.push(cmd.to_string());
    parts.join(" ")
}

/// Wrap a command with the appropriate OS-level sandbox if enabled.
pub(super) fn sandbox_wrap_command(workspace: &Path, cmd: &str, config: &SandboxConfig) -> String {
    if !config.enabled {
        return cmd.to_string();
    }
    // Skip sandbox wrapping if already inside a container
    if super::detect_container_environment().is_some() {
        return cmd.to_string();
    }
    // Check if command is excluded
    for excluded in &config.excluded_commands {
        if cmd.starts_with(excluded) || cmd.contains(excluded) {
            return cmd.to_string();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let profile = build_seatbelt_profile(workspace, config);
        seatbelt_wrap(cmd, &profile)
    }
    #[cfg(target_os = "linux")]
    {
        build_bwrap_command(workspace, cmd, config)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = workspace;
        cmd.to_string()
    }
}
