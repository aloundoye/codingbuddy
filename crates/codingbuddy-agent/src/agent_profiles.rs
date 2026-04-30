//! Agent profiles — constrain tool availability by task type.
//!
//! DeepSeek with 50+ tools frequently picks the wrong one. Constraining tool
//! availability per task type reduces the decision space and improves accuracy.
//! Profiles are selected based on `ChatMode` and prompt content.

use codingbuddy_core::complexity::PromptComplexity;
use codingbuddy_core::{RuntimeToolMetadata, ToolAgentRole};
use std::collections::HashMap;

/// An agent profile constrains tool availability and adds a system prompt addendum.
#[derive(Debug, Clone)]
pub struct AgentProfile {
    /// Profile name for logging/debugging.
    pub name: &'static str,
    /// The runtime role used to derive the tool set from tool metadata.
    pub role: ToolAgentRole,
    /// These tools are always blocked (blocklist). Applied after allowed_tools.
    pub blocked_tools: &'static [&'static str],
    /// Extra text appended to the system prompt.
    pub system_prompt_addendum: &'static str,
    /// Optional turn limit override for this profile.
    pub max_turns: Option<usize>,
}

/// Full tool set minus browser/web. Focus on code writing and testing.
pub const PROFILE_BUILD: AgentProfile = AgentProfile {
    name: "build",
    role: ToolAgentRole::Build,
    blocked_tools: &[],
    system_prompt_addendum: "\n## Agent Profile: Build\n\
        Focus on writing and testing code. Use tools to read, edit, and verify. \
        Do NOT browse the web — all answers must come from the codebase.\n",
    max_turns: None,
};

/// Read-only tools only — no modifications allowed.
pub const PROFILE_EXPLORE: AgentProfile = AgentProfile {
    name: "explore",
    role: ToolAgentRole::Explore,
    blocked_tools: &[],
    system_prompt_addendum: "\n## Agent Profile: Explore\n\
        Read and search only. Do NOT modify files. \
        Gather information with fs_read, fs_grep, fs_glob.\n",
    max_turns: None,
};

/// Like explore but also blocks bash_run — pure read + plan.
pub const PROFILE_PLAN: AgentProfile = AgentProfile {
    name: "plan",
    role: ToolAgentRole::Plan,
    blocked_tools: &[],
    system_prompt_addendum: "\n## Agent Profile: Plan\n\
        Explore then produce a structured plan. Do NOT execute changes. \
        Read files, search the codebase, and analyze — then describe your plan.\n",
    max_turns: None,
};

pub const PROFILE_BASH: AgentProfile = AgentProfile {
    name: "bash",
    role: ToolAgentRole::Bash,
    blocked_tools: &[],
    system_prompt_addendum: "\n## Agent Profile: Bash\n\
        Focus on running and interpreting shell commands. Prefer command output over speculation.\n",
    max_turns: None,
};

pub const PROFILE_GENERAL: AgentProfile = AgentProfile {
    name: "general",
    role: ToolAgentRole::General,
    blocked_tools: &[],
    system_prompt_addendum: "\n## Agent Profile: General\n\
        Use the full delegated capability set when the task genuinely spans multiple domains.\n",
    max_turns: None,
};

/// All named profiles for lookup.
const ALL_PROFILES: &[&AgentProfile] = &[
    &PROFILE_BUILD,
    &PROFILE_EXPLORE,
    &PROFILE_PLAN,
    &PROFILE_BASH,
    &PROFILE_GENERAL,
];

/// Look up a profile by name (case-insensitive).
pub fn profile_by_name(name: &str) -> Option<&'static AgentProfile> {
    ALL_PROFILES
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .copied()
}

/// All available profile names.
pub fn available_profile_names() -> &'static [&'static str] {
    &["build", "explore", "plan", "bash", "general"]
}

/// Handle the `/agent` slash command. Returns display text and the validated profile name.
pub fn handle_agent_command(name: Option<&str>) -> (String, Option<String>) {
    match name {
        Some(name) => {
            if let Some(profile) = profile_by_name(name) {
                (
                    format!("Switched to agent profile: {}", profile.name),
                    Some(profile.name.to_string()),
                )
            } else {
                let names = available_profile_names();
                (
                    format!(
                        "Unknown profile '{}'. Available: {}",
                        name,
                        names.join(", ")
                    ),
                    None,
                )
            }
        }
        None => {
            let names = available_profile_names();
            (
                format!(
                    "Available agent profiles: {}\nUsage: /agent <name>",
                    names.join(", ")
                ),
                None,
            )
        }
    }
}

/// Keywords that indicate the user wants to plan/design without implementing.
const PLANNING_KEYWORDS: &[&str] = &[
    "plan",
    "design",
    "architect",
    "propose",
    "outline",
    "strategy",
    "approach",
    "how would",
    "how should",
    "what's the best way",
    "review",
    "analyze",
    "assess",
    "audit",
    "evaluate",
];

/// Keywords that indicate the user wants actual implementation.
const IMPLEMENT_KEYWORDS: &[&str] = &[
    "implement",
    "fix",
    "write",
    "create",
    "build",
    "add",
    "change",
    "modify",
    "update",
    "refactor",
    "delete",
    "remove",
    "replace",
    "do it",
    "go ahead",
    "make it",
];

/// Select the appropriate agent profile based on chat mode and prompt content.
///
/// Rules:
/// - `Ask` / `Context` → PROFILE_EXPLORE (read-only modes)
/// - `Code` + planning keywords (without implement keywords) → PROFILE_PLAN
/// - `Code` default → PROFILE_BUILD
pub fn select_profile(
    mode: super::ChatMode,
    prompt: &str,
    _complexity: PromptComplexity,
) -> Option<&'static AgentProfile> {
    match mode {
        super::ChatMode::Ask | super::ChatMode::Context => Some(&PROFILE_EXPLORE),
        super::ChatMode::Code => {
            let lower = prompt.to_ascii_lowercase();
            let has_planning = PLANNING_KEYWORDS.iter().any(|kw| lower.contains(kw));
            let has_implement = IMPLEMENT_KEYWORDS.iter().any(|kw| lower.contains(kw));

            if has_planning && !has_implement {
                Some(&PROFILE_PLAN)
            } else {
                Some(&PROFILE_BUILD)
            }
        }
    }
}

/// Filter tool definitions by an agent profile.
///
/// The profile's runtime role is resolved against canonical runtime metadata.
/// Dynamic tools default to conservative Build/General roles unless a trusted
/// adapter supplies narrower metadata.
pub fn filter_by_profile(
    tools: Vec<codingbuddy_core::ToolDefinition>,
    profile: &AgentProfile,
) -> Vec<codingbuddy_core::ToolDefinition> {
    filter_by_profile_with_metadata(tools, profile, &HashMap::new())
}

/// Filter tool definitions by an agent profile using runtime metadata supplied
/// by dynamic adapters such as MCP.
pub fn filter_by_profile_with_metadata(
    tools: Vec<codingbuddy_core::ToolDefinition>,
    profile: &AgentProfile,
    metadata_by_name: &HashMap<String, RuntimeToolMetadata>,
) -> Vec<codingbuddy_core::ToolDefinition> {
    tools
        .into_iter()
        .filter(|t| {
            let metadata = metadata_by_name
                .get(&t.function.name)
                .cloned()
                .unwrap_or_else(|| RuntimeToolMetadata::for_api_name(&t.function.name));
            let allowed_by_role = metadata.is_allowed_for_role(profile.role);
            let blocked = profile.blocked_tools.iter().any(|b| *b == t.function.name);
            allowed_by_role && !blocked
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMode;

    fn make_tool(name: &str) -> codingbuddy_core::ToolDefinition {
        codingbuddy_core::ToolDefinition {
            tool_type: "function".to_string(),
            function: codingbuddy_core::FunctionDefinition {
                name: name.to_string(),
                description: format!("Test tool: {name}"),
                parameters: serde_json::json!({}),
                strict: None,
            },
        }
    }

    // ── Profile Selection ──

    #[test]
    fn ask_mode_selects_explore() {
        let profile = select_profile(
            ChatMode::Ask,
            "what does this code do?",
            PromptComplexity::Simple,
        );
        assert_eq!(profile.unwrap().name, "explore");
    }

    #[test]
    fn context_mode_selects_explore() {
        let profile = select_profile(
            ChatMode::Context,
            "show me the auth flow",
            PromptComplexity::Medium,
        );
        assert_eq!(profile.unwrap().name, "explore");
    }

    #[test]
    fn code_mode_planning_only_selects_plan() {
        let profile = select_profile(
            ChatMode::Code,
            "plan the approach for improving the authentication module",
            PromptComplexity::Complex,
        );
        assert_eq!(profile.unwrap().name, "plan");
    }

    #[test]
    fn code_mode_with_implement_selects_build() {
        let profile = select_profile(
            ChatMode::Code,
            "implement the plan for the auth module",
            PromptComplexity::Complex,
        );
        assert_eq!(profile.unwrap().name, "build");
    }

    #[test]
    fn code_mode_default_returns_build() {
        let profile = select_profile(
            ChatMode::Code,
            "fix the bug in main.rs",
            PromptComplexity::Simple,
        );
        assert_eq!(profile.unwrap().name, "build");
    }

    #[test]
    fn code_mode_review_selects_plan() {
        let profile = select_profile(
            ChatMode::Code,
            "review this PR for security issues",
            PromptComplexity::Medium,
        );
        assert_eq!(profile.unwrap().name, "plan");
    }

    // ── Tool Filtering ──

    #[test]
    fn explore_allows_only_read_tools() {
        let tools = vec![
            make_tool("fs_read"),
            make_tool("fs_edit"),
            make_tool("fs_glob"),
            make_tool("bash_run"),
            make_tool("fs_write"),
        ];
        let filtered = filter_by_profile(tools, &PROFILE_EXPLORE);
        let names: Vec<_> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"fs_read"));
        assert!(names.contains(&"fs_glob"));
        assert!(
            !names.contains(&"bash_run"),
            "bash_run should be blocked in explore (not read-only)"
        );
        assert!(
            !names.contains(&"fs_edit"),
            "edit should be blocked in explore"
        );
        assert!(
            !names.contains(&"fs_write"),
            "write should be blocked in explore"
        );
    }

    #[test]
    fn plan_blocks_bash_run() {
        let tools = vec![
            make_tool("fs_read"),
            make_tool("bash_run"),
            make_tool("fs_glob"),
            make_tool("task_create"),
        ];
        let filtered = filter_by_profile(tools, &PROFILE_PLAN);
        let names: Vec<_> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"fs_read"));
        assert!(names.contains(&"fs_glob"));
        assert!(names.contains(&"task_create"));
        assert!(
            !names.contains(&"bash_run"),
            "bash_run should be blocked in plan"
        );
    }

    #[test]
    fn build_blocks_web_tools() {
        let tools = vec![
            make_tool("fs_read"),
            make_tool("fs_edit"),
            make_tool("web_search"),
            make_tool("web_fetch"),
            make_tool("bash_run"),
        ];
        let filtered = filter_by_profile(tools, &PROFILE_BUILD);
        let names: Vec<_> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"fs_read"));
        assert!(names.contains(&"fs_edit"));
        assert!(names.contains(&"bash_run"));
        assert!(
            !names.contains(&"web_search"),
            "web_search should be blocked in build"
        );
        assert!(
            !names.contains(&"web_fetch"),
            "web_fetch should be blocked in build"
        );
    }

    #[test]
    fn general_profile_keeps_extended_execution_tools() {
        let tools = vec![
            make_tool("fs_edit"),
            make_tool("chrome_navigate"),
            make_tool("skill"),
            make_tool("tool_search"),
        ];
        let filtered = filter_by_profile(tools, &PROFILE_GENERAL);
        let names: Vec<_> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"fs_edit"));
        assert!(names.contains(&"chrome_navigate"));
        assert!(names.contains(&"skill"));
        assert!(names.contains(&"tool_search"));
    }

    #[test]
    fn explore_blocks_untrusted_mcp_tools() {
        let tools = vec![
            make_tool("fs_read"),
            make_tool("mcp__github__search"),
            make_tool("mcp__slack__post"),
        ];
        let filtered = filter_by_profile(tools, &PROFILE_EXPLORE);
        let names: Vec<_> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"fs_read"));
        assert!(!names.contains(&"mcp__github__search"));
        assert!(!names.contains(&"mcp__slack__post"));
    }

    #[test]
    fn explore_keeps_trusted_read_only_mcp_tools() {
        let tools = vec![make_tool("fs_read"), make_tool("mcp__github__search")];
        let mut metadata = HashMap::new();
        metadata.insert(
            "mcp__github__search".to_string(),
            RuntimeToolMetadata::trusted_read_only_dynamic(),
        );
        let filtered = filter_by_profile_with_metadata(tools, &PROFILE_EXPLORE, &metadata);
        let names: Vec<_> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"fs_read"));
        assert!(names.contains(&"mcp__github__search"));
    }

    #[test]
    fn build_profile_keeps_untrusted_mcp_tools_execute_scoped() {
        let tools = vec![make_tool("web_search"), make_tool("mcp__web__fetch")];
        let filtered = filter_by_profile(tools, &PROFILE_BUILD);
        let names: Vec<_> = filtered.iter().map(|t| t.function.name.as_str()).collect();
        assert!(
            !names.contains(&"web_search"),
            "web_search should be blocked"
        );
        assert!(
            names.contains(&"mcp__web__fetch"),
            "untrusted MCP tools remain available to build/execute profiles"
        );
    }

    #[test]
    fn build_profile_has_reasonable_defaults() {
        assert_eq!(PROFILE_BUILD.name, "build");
        assert_eq!(PROFILE_BUILD.role, ToolAgentRole::Build);
        assert!(PROFILE_BUILD.blocked_tools.is_empty());
        assert!(PROFILE_BUILD.max_turns.is_none());
    }
}
