//! Saved "always allow" approvals for shell commands.
//!
//! One click on "always" must not turn into a blank cheque. These helpers
//! decide what pattern is saved and what a saved pattern may approve, and
//! they are shared by the policy (deciding), the host (executing) and the TUI
//! (saving) so all three agree on the exact command string.

use crate::bash_classifier::{BashSafety, classify_bash_command};
use crate::rules::glob_command_matches;
use serde_json::Value;

/// Commands whose arguments can run, fetch or delete anything. A saved
/// approval for them is never widened with a wildcard: it covers only the
/// exact command the user approved.
pub const NEVER_WIDEN_COMMANDS: &[&str] = &[
    // shells and interpreters
    "bash",
    "sh",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "pwsh",
    "powershell",
    "cmd",
    "python",
    "python2",
    "python3",
    "node",
    "deno",
    "bun",
    "perl",
    "ruby",
    "php",
    "lua",
    "awk",
    "osascript",
    "eval",
    "exec",
    "source",
    ".",
    // wrappers that run another command
    "env",
    "sudo",
    "doas",
    "su",
    "xargs",
    "find",
    "nohup",
    "timeout",
    "watch",
    "nice",
    "time",
    "command",
    "builtin",
    "npx",
    "uvx",
    "pipx",
    "make",
    // destructive, network or configurable-to-run-anything
    "rm",
    "dd",
    "chmod",
    "chown",
    "curl",
    "wget",
    "ssh",
    "scp",
    "rsync",
    "nc",
    "ncat",
    "socat",
    "git",
    "docker",
    "kubectl",
    "sed",
    "tee",
];

fn never_widen(first_token: &str) -> bool {
    let name = first_token
        .rsplit('/')
        .next()
        .unwrap_or(first_token)
        .to_ascii_lowercase();
    NEVER_WIDEN_COMMANDS.contains(&name.as_str())
}

/// The command a `bash.run` call will execute, read in the host's order
/// (`cmd`, then `command`). Returns `None` when both are present and
/// differ: there is no single command to approve, so nothing may be
/// auto-approved and the host refuses to run it.
pub fn bash_command_arg(args: &Value) -> Option<&str> {
    let cmd = args.get("cmd").and_then(Value::as_str);
    let command = args.get("command").and_then(Value::as_str);
    match (cmd, command) {
        (Some(a), Some(b)) if a != b => None,
        (Some(a), _) => Some(a),
        (None, b) => b,
    }
}

fn normalized(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The pattern to save when the user chooses "always allow" for `command`.
///
/// * `npm test --watch` → `npm test *` (the subcommand stays fixed)
/// * `ls` → `ls *`
/// * anything else (shells, interpreters, wrappers, destructive commands,
///   flags before a subcommand, chained or redirected commands) → the exact
///   command, so only that precise string is approved again.
pub fn approval_pattern_for(command: &str) -> String {
    let exact = normalized(command);
    if !crate::is_single_plain_command(command) {
        return exact;
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    match tokens.as_slice() {
        [first, ..] if never_widen(first) => exact,
        [first] => format!("{first} *"),
        [first, second, ..] if !second.starts_with('-') && !second.contains(['=', '/', '*']) => {
            format!("{first} {second} *")
        }
        _ => exact,
    }
}

/// Whether a saved `pattern` approves `command`.
///
/// An exact match (whitespace-normalised) is always honoured: the user saw
/// and approved that precise string. A wildcard pattern only matches a
/// single plain command that has no variable or brace expansion, is not
/// classified dangerous, and does not start with a command from
/// [`NEVER_WIDEN_COMMANDS`].
pub fn saved_approval_matches(pattern: &str, command: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() || command.trim().is_empty() {
        return false;
    }
    if !pattern.ends_with('*') {
        return normalized(pattern) == normalized(command);
    }
    let tokens: Vec<&str> = pattern.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return false;
    };
    if first.contains('*') || pattern.contains('=') || never_widen(first) {
        return false;
    }
    crate::is_single_plain_command(command)
        && !command.contains(['$', '{', '}'])
        && classify_bash_command(command) != BashSafety::Dangerous
        && glob_command_matches(pattern, command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_and_policy_read_the_same_command() {
        assert_eq!(bash_command_arg(&json!({"cmd": "ls"})), Some("ls"));
        assert_eq!(bash_command_arg(&json!({"command": "ls"})), Some("ls"));
        assert_eq!(
            bash_command_arg(&json!({"cmd": "ls", "command": "ls"})),
            Some("ls")
        );
        assert_eq!(
            bash_command_arg(&json!({"cmd": "ls", "command": "rm -rf ~"})),
            None,
            "conflicting arguments must not be approvable"
        );
    }

    #[test]
    fn saved_patterns_keep_the_subcommand_and_never_widen_interpreters() {
        assert_eq!(approval_pattern_for("npm test --watch"), "npm test *");
        assert_eq!(approval_pattern_for("cargo build"), "cargo build *");
        assert_eq!(approval_pattern_for("ls"), "ls *");
        assert_eq!(approval_pattern_for("python build.py"), "python build.py");
        assert_eq!(
            approval_pattern_for("/usr/bin/env FOO=1 make"),
            "/usr/bin/env FOO=1 make"
        );
        assert_eq!(
            approval_pattern_for("git push origin main"),
            "git push origin main"
        );
        assert_eq!(
            approval_pattern_for("npm --prefix x test"),
            "npm --prefix x test"
        );
        assert_eq!(
            approval_pattern_for("npm test | tee out"),
            "npm test | tee out"
        );
    }

    #[test]
    fn wildcards_cannot_reach_beyond_the_approved_command_family() {
        assert!(saved_approval_matches("npm test *", "npm test --watch"));
        assert!(saved_approval_matches(
            "python build.py",
            "python  build.py"
        ));
        for (pattern, command) in [
            ("npm test *", "npm exec evil"),
            ("python *", "python -c \"__import__('os').system('id')\""),
            ("bash *", "bash -c id"),
            ("* *", "anything"),
            ("env *", "env FOO=1 sh"),
            ("npm test *", "npm test ${HOME}"),
            ("npm test *", "npm test $HOME"),
            ("npm test *", "npm test; rm -rf ~"),
            ("python build.py", "python build.py --evil"),
        ] {
            assert!(
                !saved_approval_matches(pattern, command),
                "{pattern:?} must not approve {command:?}"
            );
        }
    }
}
