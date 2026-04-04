//! Bash command safety classification.
//!
//! Classifies shell commands as Safe (auto-approve), NeedsApproval (default),
//! or Dangerous (extra warning) based on the command structure.

/// Safety classification for a bash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashSafety {
    /// Read-only command — auto-approve without prompting.
    Safe,
    /// Default — requires normal approval flow.
    NeedsApproval,
    /// Destructive or risky — show warning badge before approval.
    Dangerous,
}

/// Commands that are safe to auto-approve (read-only, no side effects).
const SAFE_COMMANDS: &[&str] = &[
    // File inspection
    "ls",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "file",
    "stat",
    "wc",
    "find",
    "tree",
    "du",
    "df",
    "readlink",
    "realpath",
    "which",
    "whereis",
    "diff",
    // Text search
    "grep",
    "rg",
    "ag",
    "ack",
    "fgrep",
    "egrep",
    // VCS queries
    "git status",
    "git log",
    "git diff",
    "git show",
    "git branch",
    "git tag",
    "git remote",
    "git stash list",
    "git blame",
    // Build/test (read-only analysis)
    "cargo check",
    "cargo test",
    "cargo clippy",
    "cargo bench",
    "cargo doc",
    "cargo fmt -- --check",
    "cargo metadata",
    "npm test",
    "npm run test",
    "npx jest",
    "npx vitest",
    "go test",
    "go vet",
    "go build",
    "pytest",
    "python -m pytest",
    "python -m py_compile",
    "make check",
    "make test",
    "tsc --noEmit",
    "eslint",
    "prettier --check",
    // System info
    "uname",
    "whoami",
    "hostname",
    "date",
    "env",
    "printenv",
    "pwd",
    "echo",
];

/// Command prefixes that are dangerous (destructive, network, or risky).
const DANGEROUS_PREFIXES: &[&str] = &[
    "rm ",
    "rm -",
    "rmdir ",
    "chmod 777",
    "chmod -R 777",
    "chown ",
    "mkfs",
    "dd ",
    "format ",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "git clean -f",
    "curl | sh",
    "curl | bash",
    "wget -O- | sh",
    "sudo ",
    "> /dev/",
    "kill -9",
    "killall ",
    "pkill ",
    "shutdown",
    "reboot",
    "halt",
];

/// Patterns in a command that indicate it needs approval (write/network/process).
const NEEDS_APPROVAL_PATTERNS: &[&str] = &[
    ">",             // Output redirection
    ">>",            // Append redirection
    "|",             // Pipe (could pipe to dangerous command)
    "&",             // Background process
    "curl ",         // Network access
    "wget ",         // Network access
    "ssh ",          // Remote access
    "scp ",          // Remote copy
    "rsync ",        // Sync (could overwrite)
    "mv ",           // Move/rename
    "cp ",           // Copy (could overwrite)
    "mkdir ",        // Create directory
    "touch ",        // Create/modify file
    "npm install",   // Modify node_modules
    "pip install",   // Modify packages
    "cargo install", // Modify binaries
    "apt ",          // Package management
    "brew ",         // Package management
    "docker ",       // Container operations
];

/// Classify a bash command's safety level.
pub fn classify_bash_command(cmd: &str) -> BashSafety {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return BashSafety::NeedsApproval;
    }

    // Check dangerous patterns first (highest priority)
    let lower = trimmed.to_ascii_lowercase();
    for prefix in DANGEROUS_PREFIXES {
        if lower.starts_with(prefix) {
            return BashSafety::Dangerous;
        }
        if let Some(pos) = lower.find("; ")
            && lower[pos + 2..].starts_with(prefix)
        {
            return BashSafety::Dangerous;
        }
    }

    // Check for command chaining with dangerous parts
    if lower.contains("&&") || lower.contains("||") || lower.contains(';') {
        // If ANY part of a chained command is dangerous, the whole thing is dangerous
        for part in lower.split(['&', '|', ';']) {
            let part = part.trim();
            if !part.is_empty() && classify_single_command(part) == BashSafety::Dangerous {
                return BashSafety::Dangerous;
            }
        }
        // Chained commands need approval even if individual parts are safe
        return BashSafety::NeedsApproval;
    }

    // Check for output redirection (needs approval)
    if lower.contains(" > ") || lower.contains(" >> ") {
        return BashSafety::NeedsApproval;
    }

    // Check for pipe to write command
    if lower.contains(" | ") {
        // Pipe is OK if the whole pipeline is read-only
        // But conservatively require approval
        return BashSafety::NeedsApproval;
    }

    // Check for background process
    if trimmed.ends_with('&') {
        return BashSafety::NeedsApproval;
    }

    classify_single_command(&lower)
}

fn starts_with_command(cmd: &str, prefix: &str) -> bool {
    cmd == prefix
        || (cmd.len() > prefix.len()
            && cmd.starts_with(prefix)
            && matches!(cmd.as_bytes()[prefix.len()], b' ' | b'\t'))
}

fn classify_single_command(lower: &str) -> BashSafety {
    for safe in SAFE_COMMANDS {
        if starts_with_command(lower, safe) {
            return BashSafety::Safe;
        }
    }

    // Check against dangerous prefixes
    for prefix in DANGEROUS_PREFIXES {
        if lower.starts_with(prefix) {
            return BashSafety::Dangerous;
        }
    }

    // Check needs-approval patterns
    for pattern in NEEDS_APPROVAL_PATTERNS {
        if lower.contains(pattern) {
            return BashSafety::NeedsApproval;
        }
    }

    // Default: needs approval for unknown commands
    BashSafety::NeedsApproval
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_commands() {
        assert_eq!(classify_bash_command("ls -la src/"), BashSafety::Safe);
        assert_eq!(classify_bash_command("cargo test"), BashSafety::Safe);
        assert_eq!(classify_bash_command("cargo clippy"), BashSafety::Safe);
        assert_eq!(classify_bash_command("git status"), BashSafety::Safe);
        assert_eq!(classify_bash_command("git log --oneline"), BashSafety::Safe);
        assert_eq!(classify_bash_command("git diff HEAD"), BashSafety::Safe);
        assert_eq!(classify_bash_command("grep -r TODO src/"), BashSafety::Safe);
        assert_eq!(classify_bash_command("cat README.md"), BashSafety::Safe);
        assert_eq!(classify_bash_command("npm test"), BashSafety::Safe);
        assert_eq!(classify_bash_command("go test ./..."), BashSafety::Safe);
        assert_eq!(classify_bash_command("pytest"), BashSafety::Safe);
    }

    #[test]
    fn dangerous_commands() {
        assert_eq!(classify_bash_command("rm -rf /"), BashSafety::Dangerous);
        assert_eq!(classify_bash_command("rm -rf *"), BashSafety::Dangerous);
        assert_eq!(classify_bash_command("chmod 777 ."), BashSafety::Dangerous);
        assert_eq!(
            classify_bash_command("git push --force"),
            BashSafety::Dangerous
        );
        assert_eq!(
            classify_bash_command("git reset --hard"),
            BashSafety::Dangerous
        );
        assert_eq!(
            classify_bash_command("sudo rm -rf /"),
            BashSafety::Dangerous
        );
    }

    #[test]
    fn needs_approval_commands() {
        assert_eq!(
            classify_bash_command("npm install express"),
            BashSafety::NeedsApproval
        );
        assert_eq!(
            classify_bash_command("curl https://example.com"),
            BashSafety::NeedsApproval
        );
        assert_eq!(
            classify_bash_command("echo hello > output.txt"),
            BashSafety::NeedsApproval
        );
        assert_eq!(
            classify_bash_command("cp src/a.rs src/b.rs"),
            BashSafety::NeedsApproval
        );
    }

    #[test]
    fn chained_dangerous_is_dangerous() {
        assert_eq!(
            classify_bash_command("ls && rm -rf /"),
            BashSafety::Dangerous
        );
    }

    #[test]
    fn chained_safe_needs_approval() {
        assert_eq!(
            classify_bash_command("ls && cat README.md"),
            BashSafety::NeedsApproval
        );
    }

    #[test]
    fn empty_needs_approval() {
        assert_eq!(classify_bash_command(""), BashSafety::NeedsApproval);
    }
}
