//! Permission rules

use super::*;

/// A permission rule in `Tool(specifier)` format.
/// Evaluation order: deny > ask > allow (first match wins).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    /// The rule string, e.g. "Bash(npm run *)", "Edit(src/**/*.rs)", "WebFetch(domain:example.com)"
    pub rule: String,
    /// The decision: "allow", "deny", or "ask".
    pub decision: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicyTargets {
    pub paths: Vec<String>,
    pub command: Option<String>,
    pub url: Option<String>,
    pub agent: Option<String>,
    pub broad_file_scan: bool,
}

impl ToolPolicyTargets {
    #[must_use]
    pub fn from_call(call: &ToolCall) -> Self {
        let mut targets = Self::default();
        match call.name.as_str() {
            "bash.run" => {
                targets.command = call
                    .args
                    .get("cmd")
                    .or_else(|| call.args.get("command"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            "fs.read" | "fs.edit" | "fs.write" => {
                push_string_arg(&mut targets.paths, &call.args, "path");
                push_string_arg(&mut targets.paths, &call.args, "file_path");
            }
            "fs.glob" => {
                let base = call
                    .args
                    .get("base")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let pattern = call
                    .args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("**/*");
                add_scan_targets(&mut targets, base, pattern);
            }
            "fs.grep" => {
                let base = call
                    .args
                    .get("base")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let glob = call
                    .args
                    .get("glob")
                    .and_then(|v| v.as_str())
                    .unwrap_or("**/*");
                add_scan_targets(&mut targets, base, glob);
            }
            "web.fetch" => {
                targets.url = call
                    .args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            "spawn_task" => {
                targets.agent = call
                    .args
                    .get("subagent_type")
                    .or_else(|| call.args.get("role"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
        targets.paths.sort();
        targets.paths.dedup();
        targets
    }

    fn matches_path_specifier(&self, specifier: &str) -> bool {
        if specifier == "*" {
            return true;
        }
        if self
            .paths
            .iter()
            .any(|target| permission_path_matches(specifier, target))
        {
            return true;
        }
        self.broad_file_scan && is_sensitive_or_specific_path_rule(specifier)
    }
}

pub(crate) fn push_string_arg(paths: &mut Vec<String>, args: &serde_json::Value, key: &str) {
    if let Some(path) = args.get(key).and_then(|v| v.as_str())
        && !path.trim().is_empty()
    {
        paths.push(path.to_string());
    }
}

pub(crate) fn add_scan_targets(targets: &mut ToolPolicyTargets, base: &str, pattern: &str) {
    let base = if base.trim().is_empty() { "." } else { base };
    let pattern = if pattern.trim().is_empty() {
        "**/*"
    } else {
        pattern
    };
    targets.paths.push(base.to_string());
    targets.paths.push(pattern.to_string());
    if base == "." {
        targets.paths.push(pattern.to_string());
    } else {
        targets
            .paths
            .push(format!("{}/{}", base.trim_end_matches('/'), pattern));
    }
    targets.broad_file_scan = matches!(pattern, "*" | "**" | "**/*" | "./**/*");
}

impl PermissionRule {
    /// Parse a rule and check if it matches a tool call.
    /// Returns `Some(decision)` if matched, `None` otherwise.
    pub fn matches(&self, call: &ToolCall) -> Option<&str> {
        let (tool_prefix, specifier) = parse_rule_syntax(&self.rule)?;

        if tool_prefix.eq_ignore_ascii_case("tool") {
            let specifier = codingbuddy_core::ToolName::from_api_name(&specifier)
                .map_or(specifier.as_str(), |tool| tool.as_internal());
            return (specifier == "*" || specifier == call.name).then_some(&self.decision);
        }

        // MCP tool rules: Mcp(server_id) or Mcp(server_id__tool_name)
        if tool_prefix.eq_ignore_ascii_case("mcp") {
            if !call.name.starts_with("mcp__") {
                return None;
            }
            // specifier is either "server_id" (matches all tools) or "server_id__tool_name"
            let call_rest = &call.name["mcp__".len()..];
            if specifier == "*" {
                return Some(&self.decision);
            }
            if specifier.contains("__") {
                // Exact server+tool match
                if call_rest == specifier {
                    return Some(&self.decision);
                }
            } else {
                // Server-only match: "server_id" matches "mcp__server_id__*"
                if call_rest.starts_with(&specifier)
                    && call_rest[specifier.len()..].starts_with("__")
                {
                    return Some(&self.decision);
                }
            }
            return None;
        }

        let tool_name = match tool_prefix.as_str() {
            "Bash" | "bash" => "bash.run",
            "Read" | "read" => "fs.read",
            "Edit" | "edit" => "fs.edit",
            "Write" | "write" => "fs.write",
            "WebFetch" | "webfetch" | "web_fetch" => "web.fetch",
            "Task" | "task" => "spawn_task",
            "Glob" | "glob" => "fs.glob",
            "Grep" | "grep" => "fs.grep",
            _ => return None,
        };

        if call.name != tool_name {
            return None;
        }

        let targets = ToolPolicyTargets::from_call(call);

        // Check specifier match.
        match tool_name {
            "bash.run" => {
                let cmd = targets.command.as_deref().unwrap_or("");
                if glob_command_matches(&specifier, cmd)
                    || self.decision != "allow"
                        && split_pipeline_segments(cmd).is_ok_and(|stages| {
                            stages
                                .iter()
                                .any(|stage| glob_command_matches(&specifier, stage))
                        })
                {
                    Some(&self.decision)
                } else {
                    None
                }
            }
            "fs.read" | "fs.edit" | "fs.write" | "fs.glob" | "fs.grep" => {
                if targets.matches_path_specifier(&specifier) {
                    return Some(&self.decision);
                }
                None
            }
            "web.fetch" => {
                let url = targets.url.as_deref().unwrap_or("");
                // Allow rules must match precisely; deny/ask rules may
                // over-match, which only means more prompting.
                let strict = self.decision == "allow";
                let matched = match specifier.strip_prefix("domain:") {
                    Some(domain) => url_host_matches(url, domain, strict),
                    None => url_prefix_matches(url, &specifier, strict),
                };
                matched.then_some(self.decision.as_str())
            }
            "spawn_task" => {
                let agent = targets.agent.as_deref().unwrap_or("");
                if agent == specifier || specifier == "*" {
                    Some(&self.decision)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

pub(crate) fn permission_path_matches(specifier: &str, target: &str) -> bool {
    let specifier = normalize_policy_path(specifier);
    let target = normalize_policy_path(target);
    if specifier == "*" || target == specifier {
        return true;
    }
    if let Ok(pattern) = Pattern::new(&specifier)
        && (pattern.matches(&target) || pattern.matches_path(Path::new(&target)))
    {
        return true;
    }
    if let Some(file_name) = Path::new(&target).file_name().and_then(|v| v.to_str())
        && let Ok(pattern) = Pattern::new(&specifier)
        && pattern.matches(file_name)
    {
        return true;
    }
    !is_glob_pattern(&specifier) && target.ends_with(&specifier)
}

pub(crate) fn normalize_policy_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .to_ascii_lowercase()
}

pub(crate) fn is_sensitive_or_specific_path_rule(specifier: &str) -> bool {
    let specifier = normalize_policy_path(specifier);
    specifier.contains(".env")
        || specifier.contains("secret")
        || specifier.contains(".ssh")
        || specifier.contains(".aws")
        || specifier.contains("id_*")
}

/// Parse "Tool(specifier)" syntax. Returns (tool, specifier).
pub(crate) fn parse_rule_syntax(rule: &str) -> Option<(String, String)> {
    let rule = rule.trim();
    let paren_pos = rule.find('(')?;
    if !rule.ends_with(')') {
        return None;
    }
    let tool = rule[..paren_pos].trim().to_string();
    let specifier = rule[paren_pos + 1..rule.len() - 1].trim().to_string();
    if tool.is_empty() || specifier.is_empty() {
        return None;
    }
    Some((tool, specifier))
}

/// Check if a glob-like command pattern matches a command string.
pub(crate) fn glob_command_matches(pattern: &str, cmd: &str) -> bool {
    // Simple glob: "npm run *" matches "npm run test", "npm run build", etc.
    let pattern_tokens: Vec<&str> = pattern.split_whitespace().collect();
    let cmd_tokens: Vec<&str> = cmd.split_whitespace().collect();
    if pattern_tokens.is_empty() {
        return false;
    }
    for (i, pt) in pattern_tokens.iter().enumerate() {
        if *pt == "*" {
            return true; // wildcard matches rest
        }
        if i >= cmd_tokens.len() {
            return false;
        }
        if let Some(prefix) = pt.strip_suffix('*') {
            if !cmd_tokens[i].starts_with(prefix) {
                return false;
            }
        } else if !pt.eq_ignore_ascii_case(cmd_tokens[i]) {
            return false;
        }
    }
    cmd_tokens.len() >= pattern_tokens.len()
}

/// Returns a baseline set of safety-oriented permission rules that are always
/// present regardless of user configuration. These protect against catastrophic
/// actions (e.g. `rm -rf *`) and prompt for confirmation on destructive git
/// operations and sensitive file edits.
///
/// Evaluation order (enforced by `evaluate_permission_rules`): deny > ask > allow.
/// User rules are appended *after* these defaults, so a user "allow" cannot
/// override a default "deny", but a user can still add stricter rules.
#[must_use]
pub fn default_deny_rules() -> Vec<PermissionRule> {
    vec![
        // Catastrophic file deletion -- always deny
        PermissionRule {
            rule: "Bash(rm -rf *)".to_string(),
            decision: "deny".to_string(),
        },
        // Force-push -- ask for confirmation
        PermissionRule {
            rule: "Bash(git push --force*)".to_string(),
            decision: "ask".to_string(),
        },
        PermissionRule {
            rule: "Bash(git push -f*)".to_string(),
            decision: "ask".to_string(),
        },
        // Editing secret / environment files -- ask
        PermissionRule {
            rule: "Edit(.env)".to_string(),
            decision: "ask".to_string(),
        },
        PermissionRule {
            rule: "Edit(.env.*)".to_string(),
            decision: "ask".to_string(),
        },
        // Editing inside node_modules -- always deny
        PermissionRule {
            rule: "Edit(node_modules/**)".to_string(),
            decision: "deny".to_string(),
        },
        // Hard reset -- ask for confirmation
        PermissionRule {
            rule: "Bash(git reset --hard*)".to_string(),
            decision: "ask".to_string(),
        },
        // Discarding local changes -- ask for confirmation
        PermissionRule {
            rule: "Bash(git checkout -- *)".to_string(),
            decision: "ask".to_string(),
        },
    ]
}

/// Evaluate permission rules against a tool call.
/// Returns: "allow", "deny", "ask", or None if no rule matched.
/// Evaluation order: deny > ask > allow (strongest match wins).
#[must_use]
pub fn evaluate_permission_rules(rules: &[PermissionRule], call: &ToolCall) -> Option<String> {
    let mut result: Option<&str> = None;
    for rule in rules {
        if let Some(decision) = rule.matches(call) {
            match decision {
                "deny" => return Some("deny".to_string()), // deny wins immediately
                "ask" => {
                    if result.is_none() || result == Some("allow") {
                        result = Some("ask");
                    }
                }
                "allow" => {
                    if result.is_none() {
                        result = Some("allow");
                    }
                }
                _ => {}
            }
        }
    }
    result.map(|s| s.to_string())
}

/// `domain:` rules compare against the URL's host: an exact match or a
/// subdomain at a dot boundary. Matching the raw URL text would let
/// `https://evil.com/?example.com` or `https://example.com.evil.net/`
/// satisfy an allow rule for `example.com`. A specifier with a port
/// (`example.com:8080`) also requires that port; IPv6 brackets are ignored.
///
/// Deny/ask rules (`strict == false`) additionally keep the original
/// substring match, so tightening allow rules never weakens a deny.
pub(crate) fn url_host_matches(url: &str, domain: &str, strict: bool) -> bool {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return false;
    }
    let substring = || url.to_ascii_lowercase().contains(&domain);
    let Ok(parsed) = url::Url::parse(url) else {
        // An unparseable URL never satisfies an allow rule.
        return !strict && substring();
    };
    let Some(host) = parsed.host_str() else {
        return !strict && substring();
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let wanted = domain.trim_start_matches('[');
    // `host:port` specifier (but not a bare IPv6 address, which has colons).
    let (wanted_host, wanted_port) = match wanted.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') || name.ends_with(']') => {
            match port.parse::<u16>() {
                Ok(port) => (name.trim_end_matches(']'), Some(port)),
                // A port that does not parse must not widen an allow rule
                // to every port: the rule simply never matches.
                Err(_) => return !strict && substring(),
            }
        }
        _ => (wanted.trim_end_matches(']'), None),
    };
    let host_ok = host == wanted_host || host.ends_with(&format!(".{wanted_host}"));
    let port_ok = wanted_port.is_none_or(|port| parsed.port_or_known_default() == Some(port));
    (host_ok && port_ok) || (!strict && substring())
}

/// URL rules match as a prefix that ends at a path boundary, so
/// `https://docs.rs` covers `https://docs.rs/serde` but not
/// `https://docs.rs.evil.net` or a URL that merely mentions it. Deny/ask
/// rules keep substring matching.
pub(crate) fn url_prefix_matches(url: &str, specifier: &str, strict: bool) -> bool {
    if specifier.is_empty() {
        return false;
    }
    if !strict {
        return url.contains(specifier);
    }
    match url.strip_prefix(specifier) {
        Some(rest) => {
            specifier.ends_with('/') || rest.is_empty() || rest.starts_with(['/', '?', '#'])
        }
        None => false,
    }
}
