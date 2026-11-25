//! Saved "always allow" approvals for shell commands.
//!
//! One click on "always" must not turn into a blank cheque. These helpers
//! decide what pattern is saved and what a saved pattern may approve, and
//! they are shared by the policy (deciding), the host (executing) and the TUI
//! (saving) so all three agree on the exact command string.

use crate::bash_classifier::{BashSafety, classify_bash_command};
use serde_json::Value;

/// Command families a saved approval may widen, with the flags a widened
/// approval accepts. Everything else is approved only as the exact command
/// the user saw: an allowlist, because no denylist of "dangerous" programs
/// is ever complete (interpreters, wrappers, `npm exec`, `cargo --config`).
struct Widenable {
    program: &'static str,
    /// `None`: no subcommand (e.g. `pytest -q`). `Some`: these subcommands.
    subcommands: Option<&'static [&'static str]>,
    /// Flags allowed after the fixed part. Values that follow a flag are
    /// plain tokens and are accepted when they do not look like flags.
    flags: &'static [&'static str],
}

const CARGO_FLAGS: &[&str] = &[
    "--release",
    "--workspace",
    "--all-targets",
    "--all-features",
    "--lib",
    "--bins",
    "--tests",
    "--benches",
    "--examples",
    "-p",
    "--package",
    "--bin",
    "--test",
    "--locked",
    "--offline",
    "-q",
    "--quiet",
    "-v",
    "--verbose",
    "--",
    "--nocapture",
    "--no-fail-fast",
    "--check",
    "--all",
    "--doc",
    "--no-deps",
];
const JS_TEST_FLAGS: &[&str] = &["--", "--silent", "--watch", "--coverage", "-u", "--ci"];
const GO_FLAGS: &[&str] = &["-v", "-race", "-run", "-count", "-short", "-cover", "./..."];
const PYTEST_FLAGS: &[&str] = &["-q", "-v", "-x", "-k", "-s", "--lf", "--ff", "-m"];
const LIST_FLAGS: &[&str] = &["-l", "-a", "-la", "-al", "-lh", "-R", "-1"];

const WIDENABLE: &[Widenable] = &[
    Widenable {
        program: "cargo",
        subcommands: Some(&[
            "build", "test", "check", "clippy", "fmt", "doc", "bench", "nextest",
        ]),
        flags: CARGO_FLAGS,
    },
    Widenable {
        program: "npm",
        subcommands: Some(&["test"]),
        flags: JS_TEST_FLAGS,
    },
    Widenable {
        program: "pnpm",
        subcommands: Some(&["test"]),
        flags: JS_TEST_FLAGS,
    },
    Widenable {
        program: "yarn",
        subcommands: Some(&["test"]),
        flags: JS_TEST_FLAGS,
    },
    Widenable {
        program: "bun",
        subcommands: Some(&["test"]),
        flags: JS_TEST_FLAGS,
    },
    Widenable {
        program: "go",
        subcommands: Some(&["build", "test", "vet"]),
        flags: GO_FLAGS,
    },
    Widenable {
        program: "pytest",
        subcommands: None,
        flags: PYTEST_FLAGS,
    },
    Widenable {
        program: "tsc",
        subcommands: None,
        flags: &["--noEmit", "-p", "--pretty"],
    },
    Widenable {
        program: "jest",
        subcommands: None,
        flags: JS_TEST_FLAGS,
    },
    Widenable {
        program: "vitest",
        subcommands: None,
        flags: JS_TEST_FLAGS,
    },
    Widenable {
        program: "ls",
        subcommands: None,
        flags: LIST_FLAGS,
    },
];

/// `C:\Tools\Cargo.EXE` → `cargo`.
fn program_name(token: &str) -> String {
    let name = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    name
}

/// The widenable family `tokens` belongs to, and how many leading tokens
/// form its fixed part (program, or program + subcommand).
fn widenable_family(tokens: &[&str]) -> Option<(&'static Widenable, usize)> {
    let program = program_name(tokens.first()?);
    // Only a bare program name may be widened, never a path to one.
    if program != tokens.first()?.to_ascii_lowercase() {
        return None;
    }
    let family = WIDENABLE.iter().find(|family| family.program == program)?;
    match family.subcommands {
        None => Some((family, 1)),
        Some(allowed) => {
            let sub = tokens.get(1)?;
            allowed.contains(sub).then_some((family, 2))
        }
    }
}

/// Every argument after the fixed part is a safe flag or a plain value.
fn arguments_are_safe(family: &Widenable, rest: &[&str]) -> bool {
    rest.iter().all(|token| {
        if token.starts_with('-') {
            family.flags.contains(token)
        } else {
            !token.contains(['=', '$', '{', '}', '*', '?', '~'])
        }
    })
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
/// * a known build/test family keeps its fixed part: `npm test --watch` →
///   `npm test *`, `cargo build --release` → `cargo build *`, `pytest -q` →
///   `pytest *`
/// * anything else → the exact command, so only that string is approved
///   again (interpreters, wrappers, `npm exec`, chained commands, ...).
pub fn approval_pattern_for(command: &str) -> String {
    let exact = normalized(command);
    if !crate::is_single_plain_command(command) {
        return exact;
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    match widenable_family(&tokens) {
        Some((_, fixed)) => format!("{} *", tokens[..fixed].join(" ")),
        None => exact,
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
    // A wildcard pattern must be exactly `<fixed part> *` of a widenable
    // family; anything else (including legacy `npm *`) approves nothing.
    let fixed: Vec<&str> = pattern
        .strip_suffix('*')
        .unwrap_or(pattern)
        .split_whitespace()
        .collect();
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some((family, fixed_len)) = widenable_family(&tokens) else {
        return false;
    };
    fixed.len() == fixed_len
        && fixed.iter().zip(&tokens).all(|(want, have)| want == have)
        && crate::is_single_plain_command(command)
        // Quotes and escapes change how the shell splits and unquotes
        // words (`"--config"` reaches cargo as `--config`), so a wildcard
        // never applies to them; such commands only match exactly.
        && !command.contains(['\'', '"', '\\'])
        && classify_bash_command(command) != BashSafety::Dangerous
        && arguments_are_safe(family, &tokens[fixed_len..])
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
    fn only_known_build_and_test_families_are_widened() {
        assert_eq!(approval_pattern_for("npm test --watch"), "npm test *");
        assert_eq!(
            approval_pattern_for("cargo build --release"),
            "cargo build *"
        );
        assert_eq!(approval_pattern_for("pytest -q tests/"), "pytest *");
        assert_eq!(approval_pattern_for("ls -la"), "ls *");
        for exact in [
            "python build.py",
            "pypy run.py",
            "gawk 1 x",
            "npm exec cowsay",
            "npm run deploy",
            "npx something",
            "/usr/bin/cargo build",
            "git push origin main",
            "npm test | tee out",
        ] {
            assert_eq!(
                approval_pattern_for(exact),
                exact,
                "{exact} must stay exact"
            );
        }
    }

    #[test]
    fn wildcards_cannot_reach_beyond_the_approved_command_family() {
        assert!(saved_approval_matches("npm test *", "npm test -- --watch"));
        assert!(saved_approval_matches(
            "cargo test *",
            "cargo test -p core --release"
        ));
        assert!(!saved_approval_matches("CARGO.EXE test *", "cargo test"));
        assert!(saved_approval_matches(
            "python build.py",
            "python  build.py"
        ));
        for (pattern, command) in [
            ("npm test *", "npm exec evil"),
            ("npm *", "npm install"),
            ("python *", "python -c \"__import__('os').system('id')\""),
            ("* *", "anything"),
            ("cargo build *", "cargo build --config target.x.runner=sh"),
            ("cargo build *", "cargo build -Zunstable-options"),
            ("npm test *", "npm test --script-shell=evil"),
            ("npm test *", "npm test $HOME"),
            ("npm test *", "npm test; rm -rf ~"),
            ("cargo test *", "/tmp/evil/cargo test"),
            ("python build.py", "python build.py --evil"),
            (
                "cargo build *",
                "cargo build \"--config\" target.x.runner=sh",
            ),
            ("cargo build *", "cargo build '--config' x"),
            ("cargo build *", "cargo build \\-\\-config x"),
        ] {
            assert!(
                !saved_approval_matches(pattern, command),
                "{pattern:?} must not approve {command:?}"
            );
        }
    }
}
