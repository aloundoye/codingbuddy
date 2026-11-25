use super::*;
use proptest::prelude::*;
use serde_json::json;
use std::sync::{Mutex, OnceLock};

fn team_policy_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn denies_path_traversal_and_secret_dirs() {
    let policy = PolicyEngine::default();
    assert!(matches!(
        policy.check_path("../outside"),
        Err(PolicyError::PathTraversal)
    ));
    assert!(matches!(
        policy.check_path(".ssh/id_rsa"),
        Err(PolicyError::SecretPath)
    ));
}

#[test]
fn allowlist_checks_command_prefix_tokens() {
    let policy = PolicyEngine::default();
    assert!(policy.check_command("cargo test --workspace").is_ok());
    assert!(
        policy
            .check_command("cargo test --workspace --all-targets")
            .is_ok()
    );
    assert!(matches!(
        policy.check_command("rm -rf /"),
        Err(PolicyError::DangerousCommand)
    ));
}

#[test]
fn wildcard_allowlist_supports_prefix_forms() {
    let cfg = PolicyConfig {
        allowlist: vec!["npm *".to_string(), "python3*".to_string()],
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    assert!(policy.check_command("npm test").is_ok());
    assert!(policy.check_command("python3.12 -V").is_ok());
}

#[test]
fn command_injection_tokens_are_blocked() {
    let policy = PolicyEngine::default();
    assert!(matches!(
        policy.check_command("git status && rm -rf /"),
        Err(PolicyError::CommandInjection)
    ));
    assert!(matches!(
        policy.check_command("cargo test; echo hacked"),
        Err(PolicyError::CommandInjection)
    ));
}

#[test]
fn pipeline_commands_require_each_segment_to_be_allowlisted() {
    let policy = PolicyEngine::default();
    assert!(
        policy
            .check_command(r#"find . -type f -name "*.rs" | head -20"#)
            .is_ok()
    );
    assert!(matches!(
        policy.check_command(r#"find . -type f -name "*.rs" | wc -l"#),
        Err(PolicyError::CommandNotAllowed)
    ));
}

#[test]
fn redacts_common_secret_patterns() {
    let policy = PolicyEngine::default();
    let out = policy.redact("api_key=abcd1234 token: xyz password = secret ssn=123-45-6789");
    assert!(out.contains("[REDACTED]"));
    assert!(!out.contains("123-45-6789"));
}

#[test]
fn blocks_globbed_secret_paths() {
    let policy = PolicyEngine::default();
    assert!(matches!(
        policy.check_path("src/keys/id_rsa"),
        Err(PolicyError::SecretPath)
    ));
}

#[test]
fn approval_gate_respects_tool_type_and_config() {
    let policy = PolicyEngine::default();
    let bash = ToolCall {
        name: "bash.run".to_string(),
        args: json!({}),
        requires_approval: false,
    };
    let read = ToolCall {
        name: "fs.read".to_string(),
        args: json!({}),
        requires_approval: false,
    };
    assert!(policy.requires_approval(&bash));
    assert!(!policy.requires_approval(&read));
}

#[test]
fn team_policy_override_replaces_allowlist_and_forces_modes() {
    let base = PolicyConfig {
        approve_edits: false,
        approve_bash: false,
        allowlist: vec!["git status".to_string()],
        denied_secret_paths: vec![".env".to_string()],
        denied_command_prefixes: vec!["rm".to_string()],
        redact_patterns: vec!["token".to_string()],
        sandbox_mode: "allowlist".to_string(),
        sandbox_wrapper: None,
        permission_mode: PermissionMode::Ask,
        permission_rules: vec![],
        persistent_bash_approvals: vec![],
    };
    let team = TeamPolicyFile {
        approve_edits: Some("ask".to_string()),
        approve_bash: Some("always".to_string()),
        allowlist: vec!["npm *".to_string()],
        deny_commands: vec!["curl".to_string()],
        block_paths: vec!["**/secrets".to_string()],
        redact_patterns: vec!["password".to_string()],
        sandbox_mode: Some("workspace-write".to_string()),
        sandbox_wrapper: Some("bwrap --cmd {cmd}".to_string()),
        permission_mode: None,
    };
    let merged = apply_team_policy_override(base, &team);
    assert!(merged.approve_edits);
    assert!(merged.approve_bash);
    assert_eq!(merged.allowlist, vec!["npm *"]);
    assert_eq!(merged.sandbox_mode, "workspace-write");
    assert_eq!(
        merged.sandbox_wrapper,
        Some("bwrap --cmd {cmd}".to_string())
    );
    assert!(
        merged
            .denied_command_prefixes
            .iter()
            .any(|rule| rule == "curl")
    );
    assert!(
        merged
            .denied_secret_paths
            .iter()
            .any(|rule| rule == "**/secrets")
    );
    assert!(
        merged
            .redact_patterns
            .iter()
            .any(|pattern| pattern == "password")
    );
}

#[test]
fn sandbox_mode_maps_from_app_config() {
    let _guard = team_policy_env_lock().lock().expect("env lock");
    // SAFETY: test-only process-level env mutation.
    unsafe {
        std::env::remove_var("CODINGBUDDY_TEAM_POLICY_PATH");
    }
    let cfg = codingbuddy_core::PolicyConfig {
        sandbox_mode: codingbuddy_core::SandboxMode::ReadOnly,
        ..codingbuddy_core::PolicyConfig::default()
    };
    let policy = PolicyEngine::from_app_config(&cfg);
    assert_eq!(policy.sandbox_mode(), "read-only");
}

#[test]
fn team_policy_locks_reflect_locked_fields() {
    let _guard = team_policy_env_lock().lock().expect("env lock");
    let dir = std::env::temp_dir().join("codingbuddy-policy-locks");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("team-policy.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "approve_bash": "never",
            "allowlist": ["git status"],
            "sandbox_mode": "workspace-write"
        }))
        .expect("serialize"),
    )
    .expect("write team policy");

    // SAFETY: test-only process-level env mutation.
    unsafe {
        std::env::set_var("CODINGBUDDY_TEAM_POLICY_PATH", &path);
    }
    let locks = team_policy_locks().expect("locks");
    assert!(!locks.approve_edits_locked);
    assert!(locks.approve_bash_locked);
    assert!(locks.allowlist_locked);
    assert!(locks.sandbox_mode_locked);
    assert!(locks.has_permission_locks());
    // SAFETY: test-only process-level env mutation.
    unsafe {
        std::env::remove_var("CODINGBUDDY_TEAM_POLICY_PATH");
    }
}

#[test]
fn locked_mode_blocks_all_non_read_operations() {
    let cfg = PolicyConfig {
        permission_mode: PermissionMode::Locked,
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let write_call = ToolCall {
        name: "fs.write".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let read_call = ToolCall {
        name: "fs.read".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    assert!(policy.requires_approval(&write_call));
    assert!(!policy.requires_approval(&read_call));
    assert_eq!(
        policy.dry_run(&write_call),
        PermissionDryRunResult::Denied("locked mode blocks all non-read operations".to_string())
    );
    assert_eq!(policy.dry_run(&read_call), PermissionDryRunResult::Allowed);
}

#[test]
fn auto_mode_auto_approves_allowlisted_bash() {
    let cfg = PolicyConfig {
        permission_mode: PermissionMode::Auto,
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let allowed_bash = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "cargo test --workspace"}),
        requires_approval: false,
    };
    let blocked_bash = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "unknown_command foo"}),
        requires_approval: false,
    };
    assert!(!policy.requires_approval(&allowed_bash));
    assert!(policy.requires_approval(&blocked_bash));
    assert_eq!(
        policy.dry_run(&allowed_bash),
        PermissionDryRunResult::AutoApproved
    );
    assert_eq!(
        policy.dry_run(&blocked_bash),
        PermissionDryRunResult::NeedsApproval
    );
}

#[test]
fn permission_mode_cycles_correctly() {
    assert_eq!(PermissionMode::Ask.cycle(), PermissionMode::Auto);
    assert_eq!(PermissionMode::Auto.cycle(), PermissionMode::AcceptEdits);
    assert_eq!(PermissionMode::AcceptEdits.cycle(), PermissionMode::Plan);
    assert_eq!(PermissionMode::Plan.cycle(), PermissionMode::DontAsk);
    assert_eq!(PermissionMode::DontAsk.cycle(), PermissionMode::Locked);
    assert_eq!(PermissionMode::Locked.cycle(), PermissionMode::Ask);
}

#[test]
fn permission_mode_from_str_lossy_handles_variants() {
    assert_eq!(PermissionMode::from_str_lossy("ask"), PermissionMode::Ask);
    assert_eq!(PermissionMode::from_str_lossy("auto"), PermissionMode::Auto);
    assert_eq!(PermissionMode::from_str_lossy("plan"), PermissionMode::Plan);
    assert_eq!(
        PermissionMode::from_str_lossy("locked"),
        PermissionMode::Locked
    );
    assert_eq!(
        PermissionMode::from_str_lossy("LOCKED"),
        PermissionMode::Locked
    );
    assert_eq!(
        PermissionMode::from_str_lossy("  Auto "),
        PermissionMode::Auto
    );
    assert_eq!(
        PermissionMode::from_str_lossy("  Plan "),
        PermissionMode::Plan
    );
    assert_eq!(
        PermissionMode::from_str_lossy("invalid"),
        PermissionMode::Ask
    );
}

#[test]
fn plan_mode_requires_approval_for_writes() {
    let cfg = PolicyConfig {
        permission_mode: PermissionMode::Plan,
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let write_call = ToolCall {
        name: "fs.write".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let edit_call = ToolCall {
        name: "fs.edit".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let bash_call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "cargo test"}),
        requires_approval: false,
    };
    assert!(policy.requires_approval(&write_call));
    assert!(policy.requires_approval(&edit_call));
    assert!(policy.requires_approval(&bash_call));
}

#[test]
fn plan_mode_allows_reads() {
    let cfg = PolicyConfig {
        permission_mode: PermissionMode::Plan,
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let read_call = ToolCall {
        name: "fs.read".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let grep_call = ToolCall {
        name: "fs.grep".to_string(),
        args: json!({"pattern": "test"}),
        requires_approval: false,
    };
    let notebook_read = ToolCall {
        name: "notebook.read".to_string(),
        args: json!({"path": "test.ipynb"}),
        requires_approval: false,
    };
    let diagnostics_call = ToolCall {
        name: "diagnostics.check".to_string(),
        args: json!({}),
        requires_approval: false,
    };
    assert!(!policy.requires_approval(&read_call));
    assert!(!policy.requires_approval(&grep_call));
    assert!(!policy.requires_approval(&notebook_read));
    assert!(!policy.requires_approval(&diagnostics_call));
}

#[test]
fn plan_mode_dry_run_needs_approval() {
    let cfg = PolicyConfig {
        permission_mode: PermissionMode::Plan,
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let write_call = ToolCall {
        name: "fs.write".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let read_call = ToolCall {
        name: "fs.read".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    assert_eq!(
        policy.dry_run(&write_call),
        PermissionDryRunResult::NeedsApproval
    );
    assert_eq!(policy.dry_run(&read_call), PermissionDryRunResult::Allowed);
}

proptest! {
    #[test]
    fn parent_dir_paths_are_always_rejected(
        head in "[a-z]{1,8}",
        tail in "[a-z]{1,8}",
    ) {
        let policy = PolicyEngine::default();
        let candidate = format!("{head}/../{tail}");
        prop_assert!(matches!(
            policy.check_path(&candidate),
            Err(PolicyError::PathTraversal)
        ));
    }

    #[test]
    fn commands_with_shell_injection_tokens_are_rejected(
        left in "[a-zA-Z0-9 _\\-]{0,24}",
        right in "[a-zA-Z0-9 _\\-]{0,24}",
        token in prop::sample::select(vec![";", "&&", "||", "`", "$("]),
    ) {
        let policy = PolicyEngine::default();
        let cmd = format!("{left}{token}{right}");
        prop_assert!(matches!(
            policy.check_command(&cmd),
            Err(PolicyError::CommandInjection)
        ));
    }

    #[test]
    fn wildcard_allowlist_accepts_npm_subcommands(
        subcommand in "[a-z]{1,10}",
        arg in "[a-z0-9\\-]{0,10}",
    ) {
        let cfg = PolicyConfig {
            allowlist: vec!["npm *".to_string()],
            ..PolicyConfig::default()
        };
        let policy = PolicyEngine::new(cfg);
        let cmd = if arg.is_empty() {
            format!("npm {subcommand}")
        } else {
            format!("npm {subcommand} {arg}")
        };
        prop_assert!(policy.check_command(&cmd).is_ok());
    }
}

#[test]
fn accept_edits_mode_auto_approves_edits_but_prompts_bash() {
    let cfg = PolicyConfig {
        permission_mode: PermissionMode::AcceptEdits,
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let edit_call = ToolCall {
        name: "fs.edit".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let write_call = ToolCall {
        name: "fs.write".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let bash_call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "unknown_command"}),
        requires_approval: false,
    };
    assert!(!policy.requires_approval(&edit_call));
    assert!(!policy.requires_approval(&write_call));
    assert!(policy.requires_approval(&bash_call));
    assert_eq!(
        policy.dry_run(&edit_call),
        PermissionDryRunResult::AutoApproved
    );
    assert_eq!(
        policy.dry_run(&bash_call),
        PermissionDryRunResult::NeedsApproval
    );
}

#[test]
fn dont_ask_mode_denies_non_allowlisted() {
    let cfg = PolicyConfig {
        permission_mode: PermissionMode::DontAsk,
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let edit_call = ToolCall {
        name: "fs.edit".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let read_call = ToolCall {
        name: "fs.read".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    let allowlisted_bash = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "cargo test --workspace"}),
        requires_approval: false,
    };
    let random_bash = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "unknown_command"}),
        requires_approval: false,
    };
    assert!(policy.requires_approval(&edit_call));
    assert!(!policy.requires_approval(&read_call));
    assert!(!policy.requires_approval(&allowlisted_bash));
    assert!(policy.requires_approval(&random_bash));
    assert_eq!(
        policy.dry_run(&edit_call),
        PermissionDryRunResult::Denied(
            "dontAsk mode denies non-allowlisted operations".to_string()
        )
    );
}

#[test]
fn permission_rule_matches_bash_pattern() {
    let rule = PermissionRule {
        rule: "Bash(npm run *)".to_string(),
        decision: "allow".to_string(),
    };
    let matching = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "npm run test"}),
        requires_approval: false,
    };
    let non_matching = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "cargo test"}),
        requires_approval: false,
    };
    assert_eq!(rule.matches(&matching), Some("allow"));
    assert_eq!(rule.matches(&non_matching), None);
}

#[test]
fn permission_rule_matches_edit_glob() {
    let rule = PermissionRule {
        rule: "Edit(src/**/*.rs)".to_string(),
        decision: "allow".to_string(),
    };
    let matching = ToolCall {
        name: "fs.edit".to_string(),
        args: json!({"path": "src/main.rs"}),
        requires_approval: false,
    };
    let non_matching = ToolCall {
        name: "fs.edit".to_string(),
        args: json!({"path": "tests/test.js"}),
        requires_approval: false,
    };
    assert_eq!(rule.matches(&matching), Some("allow"));
    assert_eq!(rule.matches(&non_matching), None);
}

#[test]
fn permission_rule_matches_glob_base_and_pattern() {
    let rule = PermissionRule {
        rule: "Glob(src/**/*.rs)".to_string(),
        decision: "ask".to_string(),
    };
    let matching = ToolCall {
        name: "fs.glob".to_string(),
        args: json!({"base": "src", "pattern": "**/*.rs"}),
        requires_approval: false,
    };
    let non_matching = ToolCall {
        name: "fs.glob".to_string(),
        args: json!({"base": "docs", "pattern": "**/*.md"}),
        requires_approval: false,
    };
    assert_eq!(rule.matches(&matching), Some("ask"));
    assert_eq!(rule.matches(&non_matching), None);
}

#[test]
fn permission_rule_matches_grep_glob_target() {
    let rule = PermissionRule {
        rule: "Grep(src/**/*.rs)".to_string(),
        decision: "ask".to_string(),
    };
    let matching = ToolCall {
        name: "fs.grep".to_string(),
        args: json!({"pattern": "secret", "base": "src", "glob": "**/*.rs"}),
        requires_approval: false,
    };
    assert_eq!(rule.matches(&matching), Some("ask"));
}

#[test]
fn permission_rule_matches_sensitive_broad_grep_scan() {
    let rule = PermissionRule {
        rule: "Grep(.env*)".to_string(),
        decision: "ask".to_string(),
    };
    let matching = ToolCall {
        name: "fs.grep".to_string(),
        args: json!({"pattern": "TOKEN"}),
        requires_approval: false,
    };
    assert_eq!(rule.matches(&matching), Some("ask"));
}

#[test]
fn permission_rule_matches_webfetch_domain() {
    let rule = PermissionRule {
        rule: "WebFetch(domain:example.com)".to_string(),
        decision: "allow".to_string(),
    };
    let matching = ToolCall {
        name: "web.fetch".to_string(),
        args: json!({"url": "https://example.com/api"}),
        requires_approval: false,
    };
    let non_matching = ToolCall {
        name: "web.fetch".to_string(),
        args: json!({"url": "https://evil.com/api"}),
        requires_approval: false,
    };
    assert_eq!(rule.matches(&matching), Some("allow"));
    assert_eq!(rule.matches(&non_matching), None);
}

#[test]
fn webfetch_allow_rules_cannot_be_satisfied_by_lookalike_urls() {
    let fetch = |url: &str| ToolCall {
        name: "web.fetch".to_string(),
        args: json!({ "url": url }),
        requires_approval: false,
    };
    let domain = PermissionRule {
        rule: "WebFetch(domain:example.com)".to_string(),
        decision: "allow".to_string(),
    };
    for allowed in [
        "https://example.com/api",
        "https://docs.example.com/x",
        "https://EXAMPLE.com./",
    ] {
        assert_eq!(domain.matches(&fetch(allowed)), Some("allow"), "{allowed}");
    }
    for rejected in [
        "https://evil.com/?next=example.com",
        "https://example.com.evil.net/",
        "https://notexample.com/",
        "https://evil.com/example.com/page",
        "not a url example.com",
    ] {
        assert_eq!(domain.matches(&fetch(rejected)), None, "{rejected}");
    }

    let prefix = PermissionRule {
        rule: "WebFetch(https://docs.rs)".to_string(),
        decision: "allow".to_string(),
    };
    assert_eq!(
        prefix.matches(&fetch("https://docs.rs/serde")),
        Some("allow")
    );
    assert_eq!(prefix.matches(&fetch("https://docs.rs")), Some("allow"));
    assert_eq!(prefix.matches(&fetch("https://docs.rs.evil.net/")), None);
    assert_eq!(
        prefix.matches(&fetch("https://evil.com/?u=https://docs.rs")),
        None
    );
}

#[test]
fn webfetch_deny_rules_still_match_broadly() {
    let fetch = |url: &str| ToolCall {
        name: "web.fetch".to_string(),
        args: json!({ "url": url }),
        requires_approval: false,
    };
    let deny = PermissionRule {
        rule: "WebFetch(domain:internal.corp)".to_string(),
        decision: "deny".to_string(),
    };
    assert_eq!(
        deny.matches(&fetch("https://api.internal.corp/x")),
        Some("deny")
    );
    // Unparseable input is still denied by substring.
    assert_eq!(
        deny.matches(&fetch("internal.corp:8080/admin")),
        Some("deny")
    );
    // A parseable URL that only mentions the domain is still denied.
    assert_eq!(
        deny.matches(&fetch("https://proxy.example/?u=internal.corp")),
        Some("deny")
    );
}

#[test]
fn webfetch_domain_rules_handle_ports_and_ipv6() {
    let fetch = |url: &str| ToolCall {
        name: "web.fetch".to_string(),
        args: json!({ "url": url }),
        requires_approval: false,
    };
    let allow = |rule: &str| PermissionRule {
        rule: format!("WebFetch(domain:{rule})"),
        decision: "allow".to_string(),
    };
    let with_port = allow("localhost:8080");
    assert_eq!(
        with_port.matches(&fetch("http://localhost:8080/x")),
        Some("allow")
    );
    assert_eq!(with_port.matches(&fetch("http://localhost:9090/x")), None);
    let https_default = allow("example.com:443");
    assert_eq!(
        https_default.matches(&fetch("https://example.com/")),
        Some("allow")
    );
    let ipv6 = allow("::1");
    assert_eq!(ipv6.matches(&fetch("http://[::1]:3000/")), Some("allow"));
    let ipv6_port = allow("[::1]:3000");
    assert_eq!(
        ipv6_port.matches(&fetch("http://[::1]:3000/")),
        Some("allow")
    );
    assert_eq!(ipv6_port.matches(&fetch("http://[::1]:4000/")), None);
    // A malformed port never turns into "any port".
    for bad in ["localhost:abc", "example.com:99999", "example.com:"] {
        assert_eq!(
            allow(bad).matches(&fetch("http://localhost:8080/")),
            None,
            "{bad}"
        );
        assert_eq!(
            allow(bad).matches(&fetch("https://example.com/")),
            None,
            "{bad}"
        );
    }
    // A deny rule with a malformed port still denies the host on any port.
    let deny = PermissionRule {
        rule: "WebFetch(domain:internal.corp:abc)".to_string(),
        decision: "deny".to_string(),
    };
    assert_eq!(
        deny.matches(&fetch("https://api.internal.corp/")),
        Some("deny")
    );
}

#[test]
fn evaluate_rules_deny_wins_over_allow() {
    let rules = vec![
        PermissionRule {
            rule: "Bash(npm *)".to_string(),
            decision: "allow".to_string(),
        },
        PermissionRule {
            rule: "Bash(npm run deploy)".to_string(),
            decision: "deny".to_string(),
        },
    ];
    let deploy = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "npm run deploy"}),
        requires_approval: false,
    };
    let test = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "npm test"}),
        requires_approval: false,
    };
    assert_eq!(
        evaluate_permission_rules(&rules, &deploy),
        Some("deny".to_string())
    );
    assert_eq!(
        evaluate_permission_rules(&rules, &test),
        Some("allow".to_string())
    );
}

#[test]
fn permission_mode_from_str_handles_new_modes() {
    assert_eq!(
        PermissionMode::from_str_lossy("acceptEdits"),
        PermissionMode::AcceptEdits
    );
    assert_eq!(
        PermissionMode::from_str_lossy("accept-edits"),
        PermissionMode::AcceptEdits
    );
    assert_eq!(
        PermissionMode::from_str_lossy("dontAsk"),
        PermissionMode::DontAsk
    );
    assert_eq!(
        PermissionMode::from_str_lossy("dont-ask"),
        PermissionMode::DontAsk
    );
}

#[test]
fn team_policy_locks_permission_mode() {
    let base = PolicyConfig {
        permission_mode: PermissionMode::Ask,
        ..PolicyConfig::default()
    };
    let team = TeamPolicyFile {
        approve_edits: None,
        approve_bash: None,
        allowlist: vec![],
        deny_commands: vec![],
        block_paths: vec![],
        redact_patterns: vec![],
        sandbox_mode: None,
        sandbox_wrapper: None,
        permission_mode: Some("locked".to_string()),
    };
    let merged = apply_team_policy_override(base, &team);
    assert_eq!(merged.permission_mode, PermissionMode::Locked);
}

#[test]
fn team_policy_permission_mode_locked_flag() {
    let _guard = team_policy_env_lock().lock().expect("env lock");
    let dir = std::env::temp_dir().join("codingbuddy-policy-pm-lock");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("team-policy.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "permission_mode": "locked"
        }))
        .expect("serialize"),
    )
    .expect("write team policy");

    // SAFETY: test-only process-level env mutation.
    unsafe {
        std::env::set_var("CODINGBUDDY_TEAM_POLICY_PATH", &path);
    }
    let locks = team_policy_locks().expect("locks");
    assert!(locks.permission_mode_locked);
    assert!(locks.has_permission_locks());
    // SAFETY: test-only process-level env mutation.
    unsafe {
        std::env::remove_var("CODINGBUDDY_TEAM_POLICY_PATH");
    }
}

#[test]
fn bypass_permissions_waives_prompts_but_keeps_denies() {
    let cfg = PolicyConfig {
        permission_mode: PermissionMode::BypassPermissions,
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let bash_call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "rm -rf /"}),
        requires_approval: true,
    };
    let write_call = ToolCall {
        name: "fs.write".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: true,
    };
    let read_call = ToolCall {
        name: "fs.read".to_string(),
        args: json!({"path": "test.txt"}),
        requires_approval: false,
    };
    assert!(policy.denies_tool(&bash_call));
    assert!(!policy.requires_approval(&write_call));
    assert!(!policy.requires_approval(&read_call));
    assert!(matches!(
        policy.dry_run(&bash_call),
        PermissionDryRunResult::Denied(_)
    ));
    assert_eq!(
        policy.dry_run(&write_call),
        PermissionDryRunResult::AutoApproved
    );
}

#[test]
fn bypass_permissions_from_str_lossy() {
    assert_eq!(
        PermissionMode::from_str_lossy("bypassPermissions"),
        PermissionMode::BypassPermissions
    );
    assert_eq!(
        PermissionMode::from_str_lossy("bypass"),
        PermissionMode::BypassPermissions
    );
    assert_eq!(
        PermissionMode::from_str_lossy("bypass-permissions"),
        PermissionMode::BypassPermissions
    );
}

#[test]
fn bypass_permissions_not_in_cycle() {
    // BypassPermissions cycles back to Ask (it's excluded from normal rotation)
    assert_eq!(
        PermissionMode::BypassPermissions.cycle(),
        PermissionMode::Ask
    );
}

#[test]
fn mcp_permission_rule_matches_server_wildcard() {
    let rule = PermissionRule {
        rule: "Mcp(myserver)".to_string(),
        decision: "allow".to_string(),
    };
    let matching = ToolCall {
        name: "mcp__myserver__read_file".to_string(),
        args: json!({}),
        requires_approval: true,
    };
    let non_matching = ToolCall {
        name: "mcp__otherserver__read_file".to_string(),
        args: json!({}),
        requires_approval: true,
    };
    assert_eq!(rule.matches(&matching), Some("allow"));
    assert_eq!(rule.matches(&non_matching), None);
}

#[test]
fn mcp_permission_rule_matches_specific_tool() {
    let rule = PermissionRule {
        rule: "Mcp(myserver__read_file)".to_string(),
        decision: "deny".to_string(),
    };
    let matching = ToolCall {
        name: "mcp__myserver__read_file".to_string(),
        args: json!({}),
        requires_approval: true,
    };
    let non_matching = ToolCall {
        name: "mcp__myserver__write_file".to_string(),
        args: json!({}),
        requires_approval: true,
    };
    assert_eq!(rule.matches(&matching), Some("deny"));
    assert_eq!(rule.matches(&non_matching), None);
}

#[test]
fn mcp_permission_rule_wildcard_all() {
    let rule = PermissionRule {
        rule: "Mcp(*)".to_string(),
        decision: "allow".to_string(),
    };
    let call = ToolCall {
        name: "mcp__anyserver__anytool".to_string(),
        args: json!({}),
        requires_approval: true,
    };
    assert_eq!(rule.matches(&call), Some("allow"));
}

#[test]
fn mcp_permission_rule_ignores_non_mcp_tools() {
    let rule = PermissionRule {
        rule: "Mcp(myserver)".to_string(),
        decision: "allow".to_string(),
    };
    let call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "ls"}),
        requires_approval: false,
    };
    assert_eq!(rule.matches(&call), None);
}

#[test]
fn mcp_tools_require_approval_in_ask_mode() {
    let policy = PolicyEngine::default();
    let mcp_call = ToolCall {
        name: "mcp__server__tool".to_string(),
        args: json!({}),
        requires_approval: true,
    };
    assert!(policy.requires_approval(&mcp_call));
}

#[test]
fn mcp_tools_allowed_by_permission_rules() {
    let cfg = PolicyConfig {
        permission_rules: vec![PermissionRule {
            rule: "Mcp(myserver)".to_string(),
            decision: "allow".to_string(),
        }],
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let allowed = ToolCall {
        name: "mcp__myserver__read_file".to_string(),
        args: json!({}),
        requires_approval: true,
    };
    let not_allowed = ToolCall {
        name: "mcp__otherserver__read_file".to_string(),
        args: json!({}),
        requires_approval: true,
    };
    assert!(!policy.requires_approval(&allowed));
    assert!(policy.requires_approval(&not_allowed));
}

#[test]
fn sandbox_config_alone_does_not_approve_bash() {
    let mut config = codingbuddy_core::PolicyConfig::default();
    config.sandbox.enabled = true;
    config.sandbox.auto_allow_bash_if_sandboxed = true;
    let policy = PolicyEngine::from_app_config(&config);
    let call = ToolCall {
        name: "bash.run".to_string(),
        args: serde_json::json!({"cmd": "npm install"}),
        requires_approval: true,
    };
    assert!(policy.requires_approval(&call));
}

#[test]
fn managed_settings_disable_bypass_mode() {
    let mut engine = PolicyEngine::new(PolicyConfig {
        permission_mode: PermissionMode::BypassPermissions,
        ..PolicyConfig::default()
    });
    assert_eq!(engine.permission_mode(), PermissionMode::BypassPermissions);

    let managed = ManagedSettings {
        disable_bypass_permissions_mode: true,
        ..Default::default()
    };
    apply_managed_settings(&mut engine, &managed);
    // Bypass should have been downgraded to Ask
    assert_eq!(engine.permission_mode(), PermissionMode::Ask);
}

#[test]
fn managed_settings_force_permission_mode() {
    let mut engine = PolicyEngine::new(PolicyConfig {
        permission_mode: PermissionMode::Auto,
        ..PolicyConfig::default()
    });
    let managed = ManagedSettings {
        permission_mode: Some("locked".to_string()),
        ..Default::default()
    };
    apply_managed_settings(&mut engine, &managed);
    assert_eq!(engine.permission_mode(), PermissionMode::Locked);
}

#[test]
fn managed_settings_replace_permission_rules() {
    let mut engine = PolicyEngine::new(PolicyConfig {
        permission_rules: vec![PermissionRule {
            rule: "Bash(npm *)".to_string(),
            decision: "allow".to_string(),
        }],
        ..PolicyConfig::default()
    });
    let managed = ManagedSettings {
        allow_managed_permission_rules_only: true,
        permission_rules: vec![PermissionRule {
            rule: "Bash(cargo *)".to_string(),
            decision: "allow".to_string(),
        }],
        ..Default::default()
    };
    apply_managed_settings(&mut engine, &managed);
    // User rules replaced by managed rules
    assert_eq!(engine.cfg.permission_rules.len(), 1);
    assert!(engine.cfg.permission_rules[0].rule.contains("cargo"));
}

#[test]
fn mcp_server_allowed_by_managed_settings() {
    let managed = ManagedSettings {
        allowed_mcp_servers: vec!["server_a".to_string(), "server_b".to_string()],
        denied_mcp_servers: vec![],
        ..Default::default()
    };
    assert!(is_mcp_server_allowed("server_a", &managed));
    assert!(is_mcp_server_allowed("server_b", &managed));
    assert!(!is_mcp_server_allowed("server_c", &managed));
}

#[test]
fn mcp_server_denied_by_managed_settings() {
    let managed = ManagedSettings {
        allowed_mcp_servers: vec![],
        denied_mcp_servers: vec!["evil_server".to_string()],
        ..Default::default()
    };
    assert!(is_mcp_server_allowed("good_server", &managed));
    assert!(!is_mcp_server_allowed("evil_server", &managed));
}

#[test]
fn managed_settings_path_returns_some() {
    // On any platform, this should return a path.
    let path = managed_settings_path();
    assert!(path.is_some());
}

// ── P1-12: persistent bash approval tests ───────────────────────────

#[test]
fn policy_with_persistent_approvals_skips_prompt() {
    let cfg = PolicyConfig {
        persistent_bash_approvals: vec!["npm test".to_string()],
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let bash = ToolCall {
        name: "bash.run".to_string(),
        args: serde_json::json!({"cmd": "npm test"}),
        requires_approval: false,
    };
    assert!(!policy.requires_approval(&bash));
}

#[test]
fn persistent_approvals_not_applied_to_edits() {
    let cfg = PolicyConfig {
        persistent_bash_approvals: vec!["npm test".to_string()],
        ..PolicyConfig::default()
    };
    let policy = PolicyEngine::new(cfg);
    let edit = ToolCall {
        name: "patch.apply".to_string(),
        args: serde_json::json!({"path": "foo.txt"}),
        requires_approval: false,
    };
    // Edit approval depends on mode, not persistent bash approvals
    assert!(policy.requires_approval(&edit));
}

// ── P5-18: Admin settings ──────────────────────────────────────────────

#[test]
fn admin_overrides_user() {
    // Verify ManagedSettings deserializes with new fields
    let json = serde_json::json!({
        "disable_bypass_permissions_mode": true,
        "allowed_tools": ["fs_read", "fs_glob"],
        "blocked_tools": ["bash_run"],
        "max_turns": 25,
        "max_budget_usd": 5.0
    });
    let managed: ManagedSettings = serde_json::from_value(json).unwrap();
    assert!(managed.disable_bypass_permissions_mode);
    assert_eq!(managed.allowed_tools, vec!["fs_read", "fs_glob"]);
    assert_eq!(managed.blocked_tools, vec!["bash_run"]);
    assert_eq!(managed.max_turns, Some(25));
    assert_eq!(managed.max_budget_usd, Some(5.0));

    // Defaults should be empty/none
    let defaults = ManagedSettings::default();
    assert!(defaults.allowed_tools.is_empty());
    assert!(defaults.blocked_tools.is_empty());
    assert!(defaults.max_turns.is_none());
    assert!(defaults.max_budget_usd.is_none());
}

// ── P7-05: Managed settings applied to full config ──────────────────

#[test]
fn managed_overrides_max_turns() {
    let managed = ManagedSettings {
        max_turns: Some(10),
        ..Default::default()
    };
    let mut cfg = codingbuddy_core::AppConfig::default();
    cfg.agent_loop.max_iterations = 50;

    managed.apply_to_config(&mut cfg);
    assert_eq!(
        cfg.agent_loop.max_iterations, 10,
        "managed max_turns should cap config"
    );
}

#[test]
fn managed_disables_bypass() {
    let managed = ManagedSettings {
        disable_bypass_permissions_mode: true,
        ..Default::default()
    };
    let mut cfg = codingbuddy_core::AppConfig::default();
    // Default permission mode is Ask, not Bypass, so this should be no-op
    managed.apply_to_config(&mut cfg);
    assert_eq!(
        cfg.policy.permission_mode,
        codingbuddy_core::PermissionMode::Ask
    );

    // Verify that max_turns without managed override stays unchanged
    let managed_no_turns = ManagedSettings::default();
    let mut cfg2 = codingbuddy_core::AppConfig::default();
    let original = cfg2.agent_loop.max_iterations;
    managed_no_turns.apply_to_config(&mut cfg2);
    assert_eq!(
        cfg2.agent_loop.max_iterations, original,
        "no override = unchanged"
    );
}

// ── T4.1: Path canonicalization tests ──────────────────────────────

#[test]
fn absolute_path_allowed_under_workspace_root() {
    let dir = std::env::temp_dir();
    let mut engine = PolicyEngine::new(PolicyConfig::default());
    engine.set_workspace_root(dir.clone());
    // A path under temp_dir should be allowed
    let test_path = dir.join("test_file.rs");
    let result = engine.check_path(test_path.to_str().unwrap());
    assert!(
        result.is_ok(),
        "absolute path under workspace root should be allowed"
    );
}

#[test]
fn absolute_path_blocked_outside_workspace_root() {
    let mut engine = PolicyEngine::new(PolicyConfig::default());
    engine.set_workspace_root(std::path::PathBuf::from("/workspace/project"));
    let result = engine.check_path("/etc/passwd");
    assert!(
        result.is_err(),
        "absolute path outside workspace root should be blocked"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn rooted_windows_path_blocked_outside_workspace_root() {
    let mut engine = PolicyEngine::new(PolicyConfig::default());
    engine.set_workspace_root(std::path::PathBuf::from(r"C:\workspace\project"));
    let result = engine.check_path(r"\Windows\System32\drivers\etc\hosts");
    assert!(
        result.is_err(),
        "rooted Windows path outside workspace root should be blocked"
    );
}

#[test]
fn symlink_escape_blocked() {
    // Create a temp dir and a symlink pointing outside it
    let temp = std::env::temp_dir().join("codingbuddy_policy_test_symlink");
    let _ = std::fs::create_dir_all(&temp);
    let link_path = temp.join("escape_link");
    let _ = std::fs::remove_file(&link_path); // clean up any previous test
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let _ = symlink("/etc", &link_path);
    }

    let mut engine = PolicyEngine::new(PolicyConfig::default());
    engine.set_workspace_root(temp.clone());

    #[cfg(unix)]
    {
        let result = engine.check_path(link_path.to_str().unwrap());
        assert!(
            result.is_err(),
            "symlink pointing outside workspace should be blocked"
        );
    }

    // Cleanup
    let _ = std::fs::remove_file(&link_path);
    let _ = std::fs::remove_dir(&temp);
}

// ── T4.2: Command injection hardening tests ────────────────────────

#[test]
fn blocks_output_redirection() {
    let engine = PolicyEngine::new(PolicyConfig::default());
    assert!(engine.check_command("echo hello > /tmp/out").is_err());
    assert!(engine.check_command("echo hello >> /tmp/out").is_err());
    assert!(engine.check_command("cat < /etc/passwd").is_err());
}

#[test]
fn blocks_background_execution() {
    let engine = PolicyEngine::new(PolicyConfig::default());
    assert!(engine.check_command("sleep 100 &").is_err());
}

#[test]
fn blocks_process_substitution() {
    let engine = PolicyEngine::new(PolicyConfig::default());
    assert!(engine.check_command("diff <(ls /a) <(ls /b)").is_err());
    assert!(engine.check_command("tee >(grep foo)").is_err());
}

#[test]
fn blocks_here_string() {
    let engine = PolicyEngine::new(PolicyConfig::default());
    assert!(engine.check_command("cat <<< 'hello'").is_err());
}

#[test]
fn quoted_angle_brackets_not_blocked() {
    // Inside quotes, > and < are not redirections
    assert!(!shell_parse::has_redirection_operator(
        "awk '{if ($1 > 5) print}'"
    ));
    assert!(!shell_parse::has_redirection_operator(r#"echo "a > b""#));
}

#[test]
fn unquoted_angle_brackets_blocked() {
    assert!(shell_parse::has_redirection_operator(
        "echo hello > out.txt"
    ));
    assert!(shell_parse::has_redirection_operator("cat < input.txt"));
}

#[test]
fn bypass_mode_creates_audit_entry() {
    let engine = PolicyEngine::new(PolicyConfig {
        permission_mode: PermissionMode::BypassPermissions,
        ..PolicyConfig::default()
    });
    assert!(engine.audit_entries().is_empty());

    let call = ToolCall {
        name: "fs.edit".to_string(),
        args: serde_json::json!({"path": "/tmp/test.rs"}),
        requires_approval: false,
    };
    let needs = engine.requires_approval(&call);
    assert!(!needs, "bypass mode should auto-approve");

    let entries = engine.audit_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tool_name, "fs.edit");
    assert_eq!(entries[0].mode, PermissionMode::BypassPermissions);
    assert_eq!(entries[0].decision, AuditDecision::AutoApproved);
}

// ── default_deny_rules tests ────────────────────────────────────────

#[test]
fn default_deny_rules_returns_expected_count() {
    let rules = default_deny_rules();
    assert_eq!(rules.len(), 8, "expected 8 default safety rules");
}

#[test]
fn default_deny_rules_present_in_new_engine() {
    let engine = PolicyEngine::new(PolicyConfig::default());
    // Default PolicyConfig has no user rules, so all rules come from defaults.
    assert!(
        engine.cfg.permission_rules.len() >= 8,
        "engine should contain at least 8 default deny rules, got {}",
        engine.cfg.permission_rules.len()
    );
    // First rule should be the rm -rf deny.
    assert_eq!(engine.cfg.permission_rules[0].rule, "Bash(rm -rf *)");
    assert_eq!(engine.cfg.permission_rules[0].decision, "deny");
}

#[test]
fn default_deny_rules_prepended_before_user_rules() {
    let cfg = PolicyConfig {
        permission_rules: vec![PermissionRule {
            rule: "Bash(npm *)".to_string(),
            decision: "allow".to_string(),
        }],
        ..PolicyConfig::default()
    };
    let engine = PolicyEngine::new(cfg);
    // 8 defaults + 1 user rule
    assert_eq!(engine.cfg.permission_rules.len(), 9);
    // Last rule should be the user rule.
    assert_eq!(engine.cfg.permission_rules[8].rule, "Bash(npm *)");
    assert_eq!(engine.cfg.permission_rules[8].decision, "allow");
}

#[test]
fn default_deny_rm_rf_blocks_destructive_command() {
    let engine = PolicyEngine::default();
    let call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "rm -rf /"}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &call);
    assert_eq!(result, Some("deny".to_string()));
}

#[test]
fn default_deny_rm_rf_blocks_wildcard_variant() {
    let engine = PolicyEngine::default();
    let call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "rm -rf ."}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &call);
    assert_eq!(result, Some("deny".to_string()));
}

#[test]
fn default_deny_git_force_push_asks() {
    let engine = PolicyEngine::default();
    let call_long = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "git push --force origin main"}),
        requires_approval: false,
    };
    let call_short = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "git push -f origin main"}),
        requires_approval: false,
    };
    let result_long = evaluate_permission_rules(&engine.cfg.permission_rules, &call_long);
    let result_short = evaluate_permission_rules(&engine.cfg.permission_rules, &call_short);
    assert_eq!(result_long, Some("ask".to_string()));
    assert_eq!(result_short, Some("ask".to_string()));
}

#[test]
fn default_deny_edit_env_asks() {
    let engine = PolicyEngine::default();
    let call = ToolCall {
        name: "fs.edit".to_string(),
        args: json!({"path": ".env"}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &call);
    assert_eq!(result, Some("ask".to_string()));
}

#[test]
fn default_deny_edit_env_variant_asks() {
    let engine = PolicyEngine::default();
    let call = ToolCall {
        name: "fs.edit".to_string(),
        args: json!({"path": ".env.local"}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &call);
    assert_eq!(result, Some("ask".to_string()));
}

#[test]
fn default_deny_edit_node_modules_blocks() {
    let engine = PolicyEngine::default();
    let call = ToolCall {
        name: "fs.edit".to_string(),
        args: json!({"path": "node_modules/lodash/index.js"}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &call);
    assert_eq!(result, Some("deny".to_string()));
}

#[test]
fn default_deny_git_reset_hard_asks() {
    let engine = PolicyEngine::default();
    let call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "git reset --hard HEAD~1"}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &call);
    assert_eq!(result, Some("ask".to_string()));
}

#[test]
fn default_deny_git_checkout_discard_asks() {
    let engine = PolicyEngine::default();
    let call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "git checkout -- src/main.rs"}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &call);
    assert_eq!(result, Some("ask".to_string()));
}

#[test]
fn default_deny_rules_do_not_match_safe_commands() {
    let engine = PolicyEngine::default();
    let safe = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "git status"}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &safe);
    // None of the default rules match "git status", so result should be None.
    assert_eq!(result, None);
}

#[test]
fn default_deny_user_allow_cannot_override_deny() {
    // User tries to allow rm -rf, but default deny wins.
    let cfg = PolicyConfig {
        permission_rules: vec![PermissionRule {
            rule: "Bash(rm -rf *)".to_string(),
            decision: "allow".to_string(),
        }],
        ..PolicyConfig::default()
    };
    let engine = PolicyEngine::new(cfg);
    let call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": "rm -rf /tmp/stuff"}),
        requires_approval: false,
    };
    let result = evaluate_permission_rules(&engine.cfg.permission_rules, &call);
    // deny wins over allow in evaluate_permission_rules
    assert_eq!(result, Some("deny".to_string()));
}
