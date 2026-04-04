//! E2E tests for bash command safety classification.

use codingbuddy_policy::bash_classifier::{BashSafety, classify_bash_command};

#[test]
fn safe_read_commands_auto_approve() {
    let safe_commands = [
        "ls -la",
        "cat README.md",
        "grep -r TODO src/",
        "git status",
        "git log --oneline -10",
        "git diff HEAD",
        "cargo test --workspace",
        "cargo clippy",
        "npm test",
        "go test ./...",
        "pytest -v",
        "find . -name '*.rs'",
        "wc -l src/lib.rs",
    ];
    for cmd in &safe_commands {
        assert_eq!(
            classify_bash_command(cmd),
            BashSafety::Safe,
            "expected Safe for: {cmd}"
        );
    }
}

#[test]
fn dangerous_commands_flagged() {
    let dangerous = [
        "rm -rf /",
        "rm -rf *",
        "chmod 777 .",
        "git push --force",
        "git reset --hard",
        "sudo apt install",
    ];
    for cmd in &dangerous {
        assert_eq!(
            classify_bash_command(cmd),
            BashSafety::Dangerous,
            "expected Dangerous for: {cmd}"
        );
    }
}

#[test]
fn write_commands_need_approval() {
    let needs_approval = [
        "npm install express",
        "curl https://example.com",
        "echo hello > file.txt",
        "cp src/a.rs src/b.rs",
        "mv old.rs new.rs",
        "docker run hello-world",
    ];
    for cmd in &needs_approval {
        assert_eq!(
            classify_bash_command(cmd),
            BashSafety::NeedsApproval,
            "expected NeedsApproval for: {cmd}"
        );
    }
}

#[test]
fn chained_commands_with_dangerous_part() {
    assert_eq!(
        classify_bash_command("ls && rm -rf /"),
        BashSafety::Dangerous
    );
}

#[test]
fn empty_command_needs_approval() {
    assert_eq!(classify_bash_command(""), BashSafety::NeedsApproval);
    assert_eq!(classify_bash_command("   "), BashSafety::NeedsApproval);
}
