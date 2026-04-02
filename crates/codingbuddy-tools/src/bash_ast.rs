//! Tree-sitter based bash command analysis.
//!
//! Parses bash commands into ASTs to detect file operations, network access,
//! destructive commands, and path targets with structural accuracy.
//! This is more reliable than regex/string matching for complex commands
//! involving pipes, subshells, and quoted strings.

/// Classification of a parsed bash command's risk profile.
#[derive(Debug, Default)]
pub struct CommandAnalysis {
    /// Top-level command names extracted from the AST (e.g., "rm", "curl", "git").
    pub commands: Vec<String>,
    /// Whether the command writes to files (redirections, tee, dd, etc.).
    pub has_file_writes: bool,
    /// Whether the command deletes files (rm, unlink, etc.).
    pub has_deletions: bool,
    /// Whether the command accesses the network (curl, wget, ssh, etc.).
    pub has_network_access: bool,
    /// Whether the command modifies permissions (chmod, chown, etc.).
    pub has_permission_changes: bool,
    /// Whether the command has process-control operations (kill, pkill, etc.).
    pub has_process_control: bool,
    /// File paths referenced as arguments (best effort extraction).
    pub referenced_paths: Vec<String>,
    /// Whether the command uses dangerous patterns (eval, curl|sh, etc.).
    pub has_dangerous_patterns: bool,
}

const DESTRUCTIVE_COMMANDS: &[&str] = &["rm", "rmdir", "unlink", "shred"];

const NETWORK_COMMANDS: &[&str] = &[
    "curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat", "nmap", "telnet", "ftp", "httpie",
    "http",
];

const PERMISSION_COMMANDS: &[&str] = &["chmod", "chown", "chgrp", "setfacl"];

const PROCESS_COMMANDS: &[&str] = &["kill", "pkill", "killall", "xkill", "renice"];

const FILE_WRITE_COMMANDS: &[&str] = &["tee", "dd", "install", "truncate"];

/// Parse a bash command string and return structural analysis.
///
/// Uses tree-sitter-bash for reliable parsing of pipes, subshells, and quoting.
/// Falls back to basic token analysis if tree-sitter parsing fails.
pub fn analyze_command(cmd: &str) -> CommandAnalysis {
    match analyze_with_treesitter(cmd) {
        Some(analysis) => analysis,
        None => analyze_fallback(cmd),
    }
}

fn analyze_with_treesitter(cmd: &str) -> Option<CommandAnalysis> {
    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_bash::LANGUAGE;
    parser.set_language(&language.into()).ok()?;
    let tree = parser.parse(cmd, None)?;
    let root = tree.root_node();

    let mut analysis = CommandAnalysis::default();
    let source = cmd.as_bytes();

    collect_from_node(root, source, &mut analysis);
    let cmds = analysis.commands.clone();
    detect_dangerous_patterns(cmd, &cmds, &mut analysis);

    Some(analysis)
}

fn collect_from_node(node: tree_sitter::Node, source: &[u8], analysis: &mut CommandAnalysis) {
    // Walk the tree looking for command_name nodes and redirections.
    if node.kind() == "command_name"
        && let Ok(text) = node.utf8_text(source)
    {
        let cmd_name = text.trim().to_ascii_lowercase();
        classify_command(&cmd_name, analysis);
        analysis.commands.push(cmd_name);
    }

    if node.kind() == "file_redirect" || node.kind() == "heredoc_redirect" {
        analysis.has_file_writes = true;
    }

    if matches!(node.kind(), "word" | "string" | "raw_string")
        && let Ok(text) = node.utf8_text(source)
    {
        let cleaned = text.trim_matches(|c| c == '"' || c == '\'');
        if looks_like_path(cleaned) {
            analysis.referenced_paths.push(cleaned.to_string());
        }
    }

    // Recurse into children
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_from_node(child, source, analysis);
        }
    }
}

fn classify_command(name: &str, analysis: &mut CommandAnalysis) {
    if DESTRUCTIVE_COMMANDS.contains(&name) {
        analysis.has_deletions = true;
    }
    if NETWORK_COMMANDS.contains(&name) {
        analysis.has_network_access = true;
    }
    if PERMISSION_COMMANDS.contains(&name) {
        analysis.has_permission_changes = true;
    }
    if PROCESS_COMMANDS.contains(&name) {
        analysis.has_process_control = true;
    }
    if FILE_WRITE_COMMANDS.contains(&name) {
        analysis.has_file_writes = true;
    }
    // sed -i and perl -i handled below in argument analysis
    if name == "mv" || name == "cp" {
        analysis.has_file_writes = true;
    }
}

fn detect_dangerous_patterns(cmd: &str, commands: &[String], analysis: &mut CommandAnalysis) {
    // curl | sh / curl | bash
    if commands.contains(&"curl".to_string()) || commands.contains(&"wget".to_string()) {
        let lower = cmd.to_ascii_lowercase();
        if lower.contains("| sh")
            || lower.contains("| bash")
            || lower.contains("|sh")
            || lower.contains("|bash")
            || lower.contains("| /bin/sh")
            || lower.contains("| /bin/bash")
        {
            analysis.has_dangerous_patterns = true;
        }
    }
    // eval
    if commands.contains(&"eval".to_string()) {
        analysis.has_dangerous_patterns = true;
    }
    // rm -rf /
    if commands.contains(&"rm".to_string()) {
        let lower = cmd.to_ascii_lowercase();
        if (lower.contains("-rf") || lower.contains("-fr")) && lower.contains(" /") {
            analysis.has_dangerous_patterns = true;
        }
    }
}

fn looks_like_path(s: &str) -> bool {
    if s.is_empty() || s.starts_with('-') {
        return false;
    }
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with("~/")
        || s.contains('/')
}

/// Fallback analysis when tree-sitter parsing fails.
fn analyze_fallback(cmd: &str) -> CommandAnalysis {
    let mut analysis = CommandAnalysis::default();
    let lower = cmd.to_ascii_lowercase();

    for token in cmd.split_whitespace() {
        let clean = token
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '`')
            .to_ascii_lowercase();
        if analysis.commands.is_empty() || token.starts_with('|') {
            let name = clean.trim_start_matches('|');
            if !name.is_empty() {
                classify_command(name, &mut analysis);
                analysis.commands.push(name.to_string());
            }
        }
        if looks_like_path(&clean) {
            analysis.referenced_paths.push(clean);
        }
    }

    if lower.contains(" > ")
        || lower.contains(" >> ")
        || lower.contains("1>")
        || lower.contains("2>")
    {
        analysis.has_file_writes = true;
    }

    let cmds = analysis.commands.clone();
    detect_dangerous_patterns(cmd, &cmds, &mut analysis);
    analysis
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command() {
        let a = analyze_command("ls -la");
        assert_eq!(a.commands, vec!["ls"]);
        assert!(!a.has_deletions);
        assert!(!a.has_network_access);
    }

    #[test]
    fn pipe_detects_both_commands() {
        let a = analyze_command("cat file.txt | grep pattern");
        assert!(a.commands.contains(&"cat".to_string()));
        assert!(a.commands.contains(&"grep".to_string()));
    }

    #[test]
    fn rm_detected_as_destructive() {
        let a = analyze_command("rm -rf /tmp/test");
        assert!(a.has_deletions);
        assert!(a.referenced_paths.iter().any(|p| p.contains("/tmp/test")));
    }

    #[test]
    fn curl_pipe_sh_is_dangerous() {
        let a = analyze_command("curl https://example.com/install.sh | bash");
        assert!(a.has_network_access);
        assert!(a.has_dangerous_patterns);
    }

    #[test]
    fn redirect_detects_file_write() {
        let a = analyze_command("echo hello > /tmp/output.txt");
        assert!(a.has_file_writes);
    }

    #[test]
    fn quoted_strings_not_misclassified() {
        let a = analyze_command(r#"echo "rm -rf /" > log.txt"#);
        // "rm" inside quotes should NOT be detected as a top-level command
        assert!(!a.commands.contains(&"rm".to_string()));
        assert!(a.has_file_writes); // redirect detected
    }

    #[test]
    fn network_commands_detected() {
        let a = analyze_command("wget https://example.com/file.tar.gz");
        assert!(a.has_network_access);
    }

    #[test]
    fn chmod_detected() {
        let a = analyze_command("chmod 777 /etc/passwd");
        assert!(a.has_permission_changes);
    }

    #[test]
    fn eval_is_dangerous() {
        let a = analyze_command("eval $(echo malicious)");
        assert!(a.has_dangerous_patterns);
    }
}
