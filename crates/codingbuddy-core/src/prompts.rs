//! System prompt for the tool-use agent loop.
//!
//! Single unified prompt used for ALL models. The model adapts to the prompt,
//! not the other way around.

use crate::complexity::PromptComplexity;

/// The unified system prompt — model-agnostic, action-biased, concise.
pub const SYSTEM_PROMPT: &str = r#"You are CodingBuddy, an expert coding assistant in the terminal. Act, don't explain.

## RULES
1. Use tools for everything. NEVER fabricate file contents, paths, or code.
2. Read files before editing. Search before guessing paths.
3. Be concise: 1-3 sentences unless showing code. Under 200 words.
4. After changes, verify with tests or build commands.
5. Trust tool results over your own knowledge.
6. Call multiple tools simultaneously when lookups are independent.
7. Do not add comments, docstrings, or extra changes beyond what was asked.

## WORKFLOW
- Trivial changes: just do it.
- Non-trivial: read → search for impacts → edit → verify.
- Multi-file: state plan briefly, modify in dependency order, test after each file.
- Never edit a file you haven't read. Never change a signature without grepping callers.

## DO NOT
- Guess paths — use `fs_glob` / `fs_grep`.
- Skip verification after changes.
- Output shell commands as text — use tools instead (`fs_read`, `fs_grep`, `fs_glob`).
- Synthesize answers from memory — respond ONLY from tool results.
- Quote project context headers or metadata injected at the start.

Tool descriptions contain detailed usage instructions.
"#;


/// Workspace context injected into the system prompt environment section.
pub struct WorkspaceContext {
    pub cwd: String,
    pub git_branch: Option<String>,
    pub os: String,
}

/// Build the complete system prompt for a tool-use session.
///
/// Layers:
/// 1. Base prompt (unified for all models)
/// 2. Environment context (cwd, git branch, OS)
/// 3. Project memory (CODINGBUDDY.md equivalent)
/// 4. User system prompt override or append
pub fn build_tool_use_system_prompt(
    project_memory: Option<&str>,
    system_prompt_override: Option<&str>,
    system_prompt_append: Option<&str>,
    workspace_context: Option<&WorkspaceContext>,
) -> String {
    // If the user provides a complete override, use it directly
    if let Some(override_prompt) = system_prompt_override {
        let mut prompt = override_prompt.to_string();
        if let Some(ctx) = workspace_context {
            prompt.push_str(&format_environment_section(ctx));
        }
        if let Some(memory) = project_memory {
            prompt.push_str("\n\n# Project Instructions\n\n");
            prompt.push_str(memory);
        }
        return prompt;
    }

    let mut parts = vec![SYSTEM_PROMPT.to_string()];

    if let Some(ctx) = workspace_context {
        parts.push(format_environment_section(ctx));
    }

    if let Some(memory) = project_memory
        && !memory.is_empty()
    {
        parts.push(format!(
            "\n# Project Instructions (CODINGBUDDY.md)\n\n{memory}"
        ));
    }

    if let Some(append) = system_prompt_append
        && !append.is_empty()
    {
        parts.push(format!("\n# Additional Instructions\n\n{append}"));
    }

    parts.join("\n")
}

/// Build system prompt with complexity-based additions.
///
/// The base prompt always includes the working protocol. For Complex tasks,
/// we add a full planning protocol. For Medium, lightweight guidance.
/// For Simple, no extra injection.
pub fn build_tool_use_system_prompt_with_complexity(
    project_memory: Option<&str>,
    system_prompt_override: Option<&str>,
    system_prompt_append: Option<&str>,
    workspace_context: Option<&WorkspaceContext>,
    complexity: PromptComplexity,
    repo_map_summary: Option<&str>,
) -> String {
    let base = build_tool_use_system_prompt(
        project_memory,
        system_prompt_override,
        system_prompt_append,
        workspace_context,
    );

    // Don't inject anything if user provided a full system prompt override.
    if system_prompt_override.is_some() {
        return base;
    }

    match complexity {
        PromptComplexity::Complex => {
            let mut prompt = format!("{base}{COMPLEX_REMINDER}");
            if let Some(repo_map) = repo_map_summary
                && !repo_map.is_empty()
            {
                prompt.push_str(&format!("\n## Project Files\n{repo_map}\n"));
            }
            prompt
        }
        PromptComplexity::Medium => format!("{base}{MEDIUM_GUIDANCE}"),
        PromptComplexity::Simple => base,
    }
}

/// Full planning protocol for Complex tasks. Provides step-by-step methodology
/// with explore→plan→execute phases and explicit anti-patterns.
const COMPLEX_REMINDER: &str = "\n\n\
## COMPLEX TASK — Mandatory Planning Protocol\n\n\
This task requires architectural thinking. Before making ANY changes:\n\n\
### Step 1: Explore\n\
- Read ALL files you plan to modify\n\
- `fs_grep` for every type, function, or interface you'll change to find ALL call sites\n\
- Identify the dependency order: which files depend on which\n\n\
### Step 2: Plan (state this explicitly)\n\
- List the files to modify in dependency order (change dependencies BEFORE dependents)\n\
- For each file: what changes, what could break, what to verify\n\
- Identify risks: shared state, concurrent access, type mismatches, missing imports\n\
- Initialize the session checklist with `todo_read`/`todo_write` before editing\n\
- Keep exactly one `in_progress` todo while executing\n\n\
### Step 3: Execute Incrementally\n\
- Modify ONE file at a time\n\
- After each file: run `bash_run` with the build/test command to verify\n\
- If a test fails: fix it BEFORE moving to the next file\n\
- If your plan was wrong: stop, re-read affected files, adjust plan\n\
- Update todos after each meaningful step (`completed` / next `in_progress`)\n\
- If a subagent finishes work, reflect it in parent todos with `todo_write`\n\
- On continuation turns, re-check current step + current todo before the next edit\n\
- Keep subtask handoffs deterministic: include `status`, `summary`, `next_action`, and `resume_session_id`\n\n\
### Anti-Patterns (NEVER do these)\n\
- Editing a file you haven't read in THIS session\n\
- Changing a function signature without grepping for all callers\n\
- Making all changes then testing at the end (test after EACH change)\n\
- Continuing after a test failure without fixing it first\n";

/// Lightweight guidance for Medium-complexity tasks. Not the full protocol,
/// but reminds the model to read-before-write and verify after changes.
const MEDIUM_GUIDANCE: &str = "\n\n\
## Task Guidance\n\
This is a multi-step task. Before making changes:\n\
1. Read the files you plan to modify.\n\
2. If changing an interface (function signature, type, struct field), grep for all usages first.\n\
3. After changes, run tests to verify.\n";

/// Coordinator mode guidance injected for Complex tasks. Teaches the model
/// to break tasks into parallel subtasks using spawn_task.
pub const COORDINATOR_GUIDANCE: &str = "\n\n\
## Coordinator Mode (Complex Task)\n\
For this complex task, you should act as a **coordinator**:\n\
\n\
1. **Analyze** — Identify independent subtasks that can run in parallel.\n\
2. **Spawn workers** — Use `spawn_task` with `run_in_background: true` for each independent subtask:\n\
   - Use `subagent_type: \"explore\"` for research/reading tasks\n\
   - Use `subagent_type: \"bash\"` for build/test tasks\n\
   - Use `subagent_type: \"general-purpose\"` for implementation tasks\n\
3. **Continue working** — Don't wait idle. Work on other subtasks while workers run.\n\
4. **Collect results** — Worker results arrive as system notifications. Read them and synthesize.\n\
5. **Verify** — After all subtasks complete, verify the combined result.\n\
\n\
**When to parallelize:** 2+ independent file edits, research + implementation, test + lint.\n\
**When NOT to parallelize:** Sequential dependencies, single-file changes, simple questions.\n";

/// Build system prompt for any model. Model-agnostic — same base prompt for all.
///
/// Layers complexity guidance and coordinator guidance on top of the unified
/// base prompt. The `_model` parameter is accepted for call-site compatibility
/// but not used — all models get the same prompt.
pub fn build_model_aware_system_prompt(
    project_memory: Option<&str>,
    system_prompt_override: Option<&str>,
    system_prompt_append: Option<&str>,
    workspace_context: Option<&WorkspaceContext>,
    complexity: PromptComplexity,
    repo_map_summary: Option<&str>,
    _model: &str,
) -> String {
    let base = build_tool_use_system_prompt_with_complexity(
        project_memory,
        system_prompt_override,
        system_prompt_append,
        workspace_context,
        complexity,
        repo_map_summary,
    );

    // Add coordinator guidance for complex tasks (skip if user override)
    if system_prompt_override.is_none() && complexity == PromptComplexity::Complex {
        format!("{base}{COORDINATOR_GUIDANCE}")
    } else {
        base
    }
}

/// Format the environment section for the system prompt.
fn format_environment_section(ctx: &WorkspaceContext) -> String {
    let mut section = String::from("\n# Environment\n\n");
    section.push_str(&format!("- Working directory: {}\n", ctx.cwd));
    if let Some(ref branch) = ctx.git_branch {
        section.push_str(&format!("- Git branch: {branch}\n"));
    }
    section.push_str(&format!("- OS: {}\n", ctx.os));
    section
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_includes_tool_guidance() {
        let prompt = build_tool_use_system_prompt(None, None, None, None);
        assert!(prompt.contains("Use tools"));
        assert!(prompt.contains("Read"));
    }

    #[test]
    fn system_prompt_includes_anti_hallucination_rules() {
        let prompt = build_tool_use_system_prompt(None, None, None, None);
        assert!(prompt.contains("NEVER fabricate"));
        assert!(prompt.contains("tool results"));
    }

    #[test]
    fn system_prompt_always_includes_working_protocol() {
        let prompt = build_tool_use_system_prompt(None, None, None, None);
        assert!(
            prompt.contains("WORKFLOW"),
            "should always include workflow"
        );
        assert!(prompt.contains("read"), "should include read-first rule");
        assert!(
            prompt.contains("Never edit") || prompt.contains("DO NOT"),
            "should include constraints"
        );
        assert!(
            prompt.contains("grep") || prompt.contains("search"),
            "should include impact tracing"
        );
    }

    #[test]
    fn system_prompt_includes_project_memory() {
        let prompt = build_tool_use_system_prompt(
            Some("Always use snake_case in Rust code."),
            None,
            None,
            None,
        );
        assert!(prompt.contains("Always use snake_case in Rust code."));
        assert!(prompt.contains("Project Instructions"));
    }

    #[test]
    fn system_prompt_respects_override() {
        let prompt = build_tool_use_system_prompt(
            Some("project memory"),
            Some("Custom system prompt"),
            None,
            None,
        );
        assert!(prompt.starts_with("Custom system prompt"));
        assert!(prompt.contains("project memory"));
        assert!(!prompt.contains("You are CodingBuddy"));
    }

    #[test]
    fn system_prompt_respects_append() {
        let prompt =
            build_tool_use_system_prompt(None, None, Some("Extra rule: always add tests."), None);
        assert!(prompt.contains("You are CodingBuddy"));
        assert!(prompt.contains("Extra rule: always add tests."));
        assert!(prompt.contains("Additional Instructions"));
    }

    #[test]
    fn system_prompt_empty_memory_not_added() {
        let prompt = build_tool_use_system_prompt(Some(""), None, None, None);
        assert!(!prompt.contains("Project Instructions"));
    }

    #[test]
    fn system_prompt_includes_workspace_context() {
        let ctx = WorkspaceContext {
            cwd: "/home/user/project".to_string(),
            git_branch: Some("main".to_string()),
            os: "linux".to_string(),
        };
        let prompt = build_tool_use_system_prompt(None, None, None, Some(&ctx));
        assert!(prompt.contains("/home/user/project"));
        assert!(prompt.contains("Git branch: main"));
        assert!(prompt.contains("OS: linux"));
    }

    #[test]
    fn system_prompt_is_concise() {
        let lines = SYSTEM_PROMPT.lines().count();
        assert!(
            lines < 60,
            "system prompt should be concise (< 60 lines), got {lines}"
        );
    }

    #[test]
    fn system_prompt_emphasizes_brevity() {
        let prompt = build_tool_use_system_prompt(None, None, None, None);
        assert!(prompt.contains("concise") || prompt.contains("Minimize"));
    }

    // ── Complexity injection ──

    #[test]
    fn complex_gets_full_planning_protocol() {
        let prompt = build_tool_use_system_prompt_with_complexity(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            None,
        );
        assert!(
            prompt.contains("COMPLEX TASK"),
            "complex should get planning protocol"
        );
        assert!(
            prompt.contains("Step 1: Explore"),
            "should include explore step"
        );
        assert!(prompt.contains("Step 2: Plan"), "should include plan step");
        assert!(
            prompt.contains("Step 3: Execute"),
            "should include execute step"
        );
        assert!(
            prompt.contains("Anti-Patterns"),
            "should include anti-patterns"
        );
        assert!(prompt.contains("WORKFLOW"), "should have protocol in base");
    }

    #[test]
    fn medium_gets_lightweight_guidance() {
        let prompt = build_tool_use_system_prompt_with_complexity(
            None,
            None,
            None,
            None,
            PromptComplexity::Medium,
            None,
        );
        assert!(
            prompt.contains("Task Guidance"),
            "medium should get guidance"
        );
        assert!(
            prompt.contains("Read the files"),
            "should include read-before-write"
        );
        assert!(
            prompt.contains("grep for all usages"),
            "should include impact tracing"
        );
        assert!(
            !prompt.contains("COMPLEX TASK"),
            "medium should NOT get full protocol"
        );
    }

    #[test]
    fn simple_gets_no_injection() {
        let prompt = build_tool_use_system_prompt_with_complexity(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
        );
        assert!(prompt.contains("WORKFLOW"), "simple gets base protocol");
        assert!(
            !prompt.contains("COMPLEX TASK"),
            "simple should NOT get complex protocol"
        );
        assert!(
            !prompt.contains("Task Guidance"),
            "simple should NOT get medium guidance"
        );
    }

    #[test]
    fn complex_includes_repo_map() {
        let repo_map = "- src/lib.rs (2048 bytes) score=100\n- src/main.rs (512 bytes) score=50";
        let prompt = build_tool_use_system_prompt_with_complexity(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            Some(repo_map),
        );
        assert!(
            prompt.contains("Project Files"),
            "complex should include project files"
        );
        assert!(
            prompt.contains("src/lib.rs"),
            "should include repo map entries"
        );
        assert!(
            prompt.contains("src/main.rs"),
            "should include all repo map entries"
        );
    }

    #[test]
    fn override_skips_complexity_injection() {
        let prompt = build_tool_use_system_prompt_with_complexity(
            None,
            Some("Custom prompt"),
            None,
            None,
            PromptComplexity::Complex,
            Some("repo map content"),
        );
        assert!(
            !prompt.contains("COMPLEX TASK"),
            "override should skip complexity"
        );
        assert!(
            !prompt.contains("Project Files"),
            "override should skip repo map"
        );
    }

    #[test]
    fn system_prompt_environment_section_no_branch() {
        let ctx = WorkspaceContext {
            cwd: "/tmp/test".to_string(),
            git_branch: None,
            os: "macos".to_string(),
        };
        let prompt = build_tool_use_system_prompt(None, None, None, Some(&ctx));
        assert!(prompt.contains("/tmp/test"));
        assert!(!prompt.contains("Git branch"));
        assert!(prompt.contains("OS: macos"));
    }

    // ── Model-aware prompt is now model-agnostic ──

    #[test]
    fn model_aware_uses_unified_prompt_for_all_models() {
        for model in &[
            "deepseek-chat",
            "deepseek-reasoner",
            "qwen-2.5-coder",
            "gemini-2.0-flash",
            "claude-sonnet-4-20250514",
            "gpt-4o",
        ] {
            let prompt = build_model_aware_system_prompt(
                None,
                None,
                None,
                None,
                PromptComplexity::Simple,
                None,
                model,
            );
            assert!(
                prompt.contains("RULES"),
                "{model} should use unified prompt"
            );
            assert!(
                prompt.contains("Act, don"),
                "{model} should have action bias"
            );
        }
    }

    #[test]
    fn model_aware_complex_gets_coordinator_guidance() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            None,
            "deepseek-chat",
        );
        assert!(
            prompt.contains("COMPLEX TASK"),
            "complex should get planning protocol"
        );
        assert!(
            prompt.contains("Coordinator Mode"),
            "complex should get coordinator guidance"
        );
    }

    #[test]
    fn model_aware_simple_gets_no_extra() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "deepseek-chat",
        );
        assert!(
            !prompt.contains("Coordinator Mode"),
            "simple should not get coordinator guidance"
        );
        assert!(
            !prompt.contains("COMPLEX TASK"),
            "simple should not get complex protocol"
        );
    }

    #[test]
    fn override_skips_model_guidance() {
        let prompt = build_model_aware_system_prompt(
            None,
            Some("Custom override"),
            None,
            None,
            PromptComplexity::Complex,
            None,
            "deepseek-reasoner",
        );
        assert!(
            !prompt.contains("Coordinator Mode"),
            "override should skip all guidance"
        );
    }

    #[test]
    fn model_aware_preserves_memory_and_context() {
        let ctx = WorkspaceContext {
            cwd: "/project".to_string(),
            git_branch: Some("main".to_string()),
            os: "linux".to_string(),
        };
        let prompt = build_model_aware_system_prompt(
            Some("Use tabs."),
            None,
            Some("Be brief."),
            Some(&ctx),
            PromptComplexity::Simple,
            None,
            "deepseek-chat",
        );
        assert!(prompt.contains("Use tabs."));
        assert!(prompt.contains("Be brief."));
        assert!(prompt.contains("/project"));
        assert!(prompt.contains("main"));
    }

    #[test]
    fn prompts_enforce_conciseness_and_grounding() {
        assert!(
            SYSTEM_PROMPT.contains("Under 200 words") || SYSTEM_PROMPT.contains("concise"),
            "should enforce conciseness"
        );
        assert!(
            SYSTEM_PROMPT.contains("tool results") || SYSTEM_PROMPT.contains("Trust"),
            "should ground on tool results"
        );
        assert!(
            SYSTEM_PROMPT.contains("NEVER fabricate"),
            "should prevent fabrication"
        );
    }

    #[test]
    fn prompt_forbids_shell_commands() {
        assert!(
            SYSTEM_PROMPT.contains("shell commands") || SYSTEM_PROMPT.contains("fs_read"),
            "should forbid shell commands or reference tool alternatives"
        );
    }
}
