//! System prompts for the tool-use agent loop.
//!
//! Four model-family prompts selected by model name:
//! - CHAT (action-biased, compensates for weaker reasoning) — default / DeepSeek-Chat
//! - REASONER (thinking-leveraging, grants more autonomy) — DeepSeek-Reasoner
//! - QWEN (ultra-concise, token-efficient, aggressive tool use) — Qwen models
//! - GEMINI (thorough, methodical, software-engineering focused) — Gemini models

/// Default/fallback system prompt used by both tiers. Kept as compatibility alias.
pub const TOOL_USE_SYSTEM_PROMPT: &str = CHAT_SYSTEM_PROMPT;

/// System prompt for `deepseek-chat` — action-biased, compensates for weak reasoning.
///
/// Key principles:
/// - "Act, don" bias: minimize planning, maximize action
/// - Aggressive anti-hallucination: never fabricate anything
/// - Explicit DO NOT list to constrain the weaker model
/// - Strict verification requirements after every change
pub const CHAT_SYSTEM_PROMPT: &str = r#"You are CodingBuddy, an expert coding assistant in the terminal. Act, don't explain.

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

/// System prompt for `deepseek-reasoner` — leverages native chain-of-thought.
///
/// Key principles:
/// - Grants autonomy: reasoner can self-plan via thinking
/// - Less prescriptive than chat (reasoner self-plans)
/// - Directs thinking to high-value tasks (planning, error analysis)
/// - Same core safety rules (read before edit, verify)
pub const REASONER_SYSTEM_PROMPT: &str = r#"You are CodingBuddy, an expert coding assistant with extended thinking.

## RULES
1. Use tools for everything. NEVER fabricate file contents, paths, or code.
2. Read files before editing. Search before guessing paths.
3. After changes, verify with tests. Trust tool results over expectations.
4. Be concise in responses. Use thinking for internal planning — show results, not plans.

## THINKING STRATEGY
- Before complex edits: plan changes and trace impacts in thinking.
- After errors: analyze root cause in thinking before retrying.
- Skip thinking for trivial operations (reads, simple commands).

## WORKFLOW
- Read → search for impacts → edit → verify. One file at a time.
- Never edit unread files. Never change signatures without grepping callers.
- Use tools instead of shell commands (`fs_read`, `fs_grep`, `fs_glob`).
"#;

/// System prompt for Qwen models — ultra-concise, token-efficient, action-first.
///
/// Key principles:
/// - Extreme brevity: 4 lines max unless showing code
/// - Minimize output tokens above all else
/// - Aggressive tool use, zero explanation
/// - No preamble, no filler, no pleasantries
pub const QWEN_SYSTEM_PROMPT: &str = r#"You are CodingBuddy, a terminal-based coding assistant. Be extremely concise.

## RULES
1. Use tools for everything. Never fabricate paths, content, or code.
2. Read files before editing. Search before guessing.
3. Respond in 1-4 lines max. No preamble. No explanations unless asked.
4. After changes, verify with tests.
5. Trust tool results over your own knowledge.

## OUTPUT
- Minimize tokens. Show results, not plans.
- NEVER explain what you will do. Just do it.
- Do not add comments, docstrings, or annotations unless asked.
- Use tools (`fs_read`, `fs_grep`, `fs_glob`) instead of shell commands (`cat`, `grep`, `find`).
- The project context injected at the start is for YOUR reference only. Never quote headers or metadata from it.
"#;

/// System prompt for Gemini models — thorough, methodical, software-engineering focused.
///
/// Key principles:
/// - Detailed analysis before action
/// - Methodical approach: explore, understand, then modify
/// - Strong emphasis on testing and verification
/// - Thoroughness over speed
pub const GEMINI_SYSTEM_PROMPT: &str = r#"You are CodingBuddy, an expert coding assistant. Be thorough and methodical.

## RULES
1. Use tools for everything. NEVER fabricate file contents, paths, or code.
2. Read files before editing. Search before guessing paths.
3. Understand full context before changing anything. Check tests for expected behavior.
4. After changes, verify with tests. Run the full suite when done.
5. Trust tool results over expectations. Call multiple tools simultaneously when independent.

## METHODOLOGY
1. **Explore**: Read all relevant files. Grep for references to things you'll change.
2. **Analyze**: Consider edge cases, trace callers of public APIs, check dependency order.
3. **Execute**: Modify in dependency order. Test after each file, not just at the end.
4. **Verify**: Run full test suite. Confirm build succeeds.

## DO NOT
- Edit files you haven't read. Change interfaces without grepping callers.
- Skip verification. Make changes beyond what was requested.
- Output shell commands as text — use tools (`fs_read`, `fs_grep`, `fs_glob`).
- Quote project context headers or metadata.
"#;

/// System prompt for strong models (Claude, GPT-4o) — minimal constraints, trust the model.
pub const STRONG_MODEL_SYSTEM_PROMPT: &str = r#"You are CodingBuddy, an expert coding assistant in the terminal.

## RULES
1. Use tools to gather information. Never fabricate file contents or paths.
2. Read before editing. Verify after changing.
3. Be concise. Show results, not plans.
4. Trust tool results over your own knowledge.
5. Use tools (`fs_read`, `fs_grep`, `fs_glob`) instead of shell commands in text.

Tool descriptions contain detailed usage instructions.
"#;

use crate::complexity::PromptComplexity;

/// Workspace context injected into the system prompt environment section.
pub struct WorkspaceContext {
    pub cwd: String,
    pub git_branch: Option<String>,
    pub os: String,
}

/// Build the complete system prompt for a tool-use session.
///
/// Layers:
/// 1. Base tool-use prompt (always includes planning/verification guidance)
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

    let mut parts = vec![TOOL_USE_SYSTEM_PROMPT.to_string()];

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

/// Chain-of-thought injection for non-reasoner models on medium-complexity tasks.
/// Forces the model to think briefly before acting.
const COT_GUIDANCE: &str = "\n\n\
## Before Editing\n\
This is a multi-step task. Before each edit, briefly state:\n\
1. What you understand the problem to be\n\
2. Which files are involved\n\
3. What change you plan to make\n\
Then proceed with the edit. After editing, verify with tests.\n";

/// Additional system prompt section for deepseek-reasoner model.
/// The reasoner has native chain-of-thought, so we guide it to use thinking
/// for planning and verification rather than just acting.
pub const REASONER_GUIDANCE: &str = "\n\n\
## Model: DeepSeek-Reasoner\n\
You have extended thinking capabilities. Use them strategically:\n\
- **Before complex edits**: Think through the change, its impacts, and verify your understanding.\n\
- **After errors**: Use thinking to analyze why the error occurred before retrying.\n\
- **For multi-file changes**: Think through the dependency order and plan before acting.\n\
Do NOT use thinking for trivial operations (reading files, running commands).\n";

/// Additional prescriptive guidance for deepseek-chat when handling Complex tasks
/// without thinking mode. Since chat lacks native reasoning, we compensate with
/// explicit step-by-step instructions.
pub const CHAT_PRESCRIPTIVE_GUIDANCE: &str = "\n\n\
## Explicit Verification Protocol\n\
After EVERY file modification, you MUST:\n\
1. State what you changed and why (one sentence)\n\
2. Run the build/test command to verify\n\
3. If the test fails, re-read the error FULLY before making another edit\n\
\n\
Every 5 tool calls, ask yourself: have I verified all file paths exist? \
Am I working on the right files? Have I read the files I'm about to edit?\n";

/// Build system prompt with model-specific base prompt selection.
///
/// Selects the appropriate system prompt by model family:
/// - Qwen models → `QWEN_SYSTEM_PROMPT`
/// - Gemini models → `GEMINI_SYSTEM_PROMPT`
/// - DeepSeek reasoner → `REASONER_SYSTEM_PROMPT`
/// - Everything else → `CHAT_SYSTEM_PROMPT`
///
/// Then layers complexity and environment context on top.
pub fn build_model_aware_system_prompt(
    project_memory: Option<&str>,
    system_prompt_override: Option<&str>,
    system_prompt_append: Option<&str>,
    workspace_context: Option<&WorkspaceContext>,
    complexity: PromptComplexity,
    repo_map_summary: Option<&str>,
    model: &str,
) -> String {
    // If user provided a complete override, skip model selection
    if system_prompt_override.is_some() {
        return build_tool_use_system_prompt_with_complexity(
            project_memory,
            system_prompt_override,
            system_prompt_append,
            workspace_context,
            complexity,
            repo_map_summary,
        );
    }

    // Select base prompt by model family, then by model tier.
    // Non-DeepSeek model families are checked first so that e.g. "qwen-reasoner"
    // still gets the Qwen prompt rather than the generic reasoner prompt.
    use crate::{ModelFamily, detect_model_family};
    let family = detect_model_family(model);
    let is_reasoner = crate::is_reasoner_model(model);

    let base_prompt = match family {
        ModelFamily::Qwen => QWEN_SYSTEM_PROMPT,
        ModelFamily::Gemini => GEMINI_SYSTEM_PROMPT,
        ModelFamily::Claude | ModelFamily::OpenAi => STRONG_MODEL_SYSTEM_PROMPT,
        _ if is_reasoner => REASONER_SYSTEM_PROMPT,
        _ => CHAT_SYSTEM_PROMPT,
    };
    let is_strong = matches!(family, ModelFamily::Claude | ModelFamily::OpenAi);

    // Build with model-specific base
    let base = build_tool_use_system_prompt_with_base(
        base_prompt,
        project_memory,
        system_prompt_append,
        workspace_context,
        complexity,
        repo_map_summary,
    );

    // Add model-tier-specific guidance on top.
    let is_qwen = family == ModelFamily::Qwen;
    let is_gemini = family == ModelFamily::Gemini;
    if is_qwen || is_gemini || is_strong {
        // Strong models and specialized prompts are self-contained.
        base
    } else if is_reasoner {
        format!("{base}{REASONER_GUIDANCE}")
    } else if complexity == PromptComplexity::Complex {
        format!("{base}{CHAT_PRESCRIPTIVE_GUIDANCE}")
    } else if complexity == PromptComplexity::Medium {
        // Chain-of-thought for non-reasoner models on medium tasks
        format!("{base}{COT_GUIDANCE}")
    } else {
        base
    }
}

/// Build system prompt from an explicit base prompt (used by model-aware builder).
fn build_tool_use_system_prompt_with_base(
    base_prompt: &str,
    project_memory: Option<&str>,
    system_prompt_append: Option<&str>,
    workspace_context: Option<&WorkspaceContext>,
    complexity: PromptComplexity,
    repo_map_summary: Option<&str>,
) -> String {
    let mut parts = vec![base_prompt.to_string()];

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

    let base = parts.join("\n");

    // Apply complexity injection (same logic as build_tool_use_system_prompt_with_complexity)
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
        let chat_lines = CHAT_SYSTEM_PROMPT.lines().count();
        assert!(
            chat_lines < 60,
            "chat system prompt should be concise (< 60 lines), got {chat_lines}"
        );
        let reasoner_lines = REASONER_SYSTEM_PROMPT.lines().count();
        assert!(
            reasoner_lines < 35,
            "reasoner prompt should be concise (< 35 lines), got {reasoner_lines}"
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

    // ── T3.2: Model-aware prompt tests ──

    #[test]
    fn reasoner_gets_thinking_guidance() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            None,
            "deepseek-reasoner",
        );
        assert!(
            prompt.contains("Reasoner"),
            "reasoner should get thinking guidance"
        );
        assert!(
            prompt.contains("extended thinking"),
            "should mention thinking capability"
        );
    }

    #[test]
    fn chat_complex_gets_prescriptive_guidance() {
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
            prompt.contains("Verification Protocol"),
            "chat on complex should get prescriptive guidance"
        );
        assert!(
            prompt.contains("EVERY file modification"),
            "should emphasize verification"
        );
    }

    #[test]
    fn chat_simple_gets_no_model_guidance() {
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
            !prompt.contains("Verification Protocol"),
            "simple should not get prescriptive guidance"
        );
        assert!(
            !prompt.contains("Reasoner"),
            "chat should not get reasoner guidance"
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
            !prompt.contains("Reasoner"),
            "override should skip model guidance"
        );
    }

    // ── T3.3: Dual-prompt model-tier tests ──

    #[test]
    fn chat_prompt_has_action_bias() {
        assert!(
            CHAT_SYSTEM_PROMPT.contains("RULES"),
            "chat should have prime directive"
        );
        assert!(
            CHAT_SYSTEM_PROMPT.contains("Act, don"),
            "chat should have action bias"
        );
        assert!(
            CHAT_SYSTEM_PROMPT.contains("DO NOT"),
            "chat should have explicit DO NOT section"
        );
    }

    #[test]
    fn reasoner_prompt_has_thinking_strategy() {
        assert!(
            REASONER_SYSTEM_PROMPT.contains("THINKING STRATEGY"),
            "reasoner should have thinking strategy"
        );
        assert!(
            REASONER_SYSTEM_PROMPT.contains("extended thinking"),
            "reasoner should mention thinking capability"
        );
        assert!(
            REASONER_SYSTEM_PROMPT.contains("THINKING STRATEGY"),
            "reasoner should have thinking strategy section"
        );
    }

    #[test]
    fn both_prompts_share_core_rules() {
        assert!(CHAT_SYSTEM_PROMPT.contains("NEVER fabricate"));
        assert!(REASONER_SYSTEM_PROMPT.contains("NEVER fabricate"));
        assert!(CHAT_SYSTEM_PROMPT.contains("Read files before editing"));
        assert!(REASONER_SYSTEM_PROMPT.contains("Read"));
    }

    #[test]
    fn model_aware_selects_correct_base() {
        let chat = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "deepseek-chat",
        );
        let reasoner = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "deepseek-reasoner",
        );
        assert!(chat.contains("RULES"), "chat should use CHAT_SYSTEM_PROMPT");
        assert!(
            reasoner.contains("THINKING STRATEGY"),
            "reasoner should use REASONER_SYSTEM_PROMPT"
        );
        assert!(
            !chat.contains("THINKING STRATEGY"),
            "chat should NOT have reasoner content"
        );
        assert!(
            reasoner.contains("THINKING STRATEGY"),
            "reasoner should have thinking-specific content"
        );
    }

    #[test]
    fn model_aware_complexity_injection_works_on_both_tiers() {
        let chat_complex = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            None,
            "deepseek-chat",
        );
        let reasoner_complex = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            None,
            "deepseek-reasoner",
        );
        assert!(
            chat_complex.contains("COMPLEX TASK"),
            "chat complex should get planning"
        );
        assert!(
            reasoner_complex.contains("COMPLEX TASK"),
            "reasoner complex should get planning"
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
        // Both prompts should enforce word limit
        assert!(
            CHAT_SYSTEM_PROMPT.contains("Under 200 words")
                || CHAT_SYSTEM_PROMPT.contains("concise"),
            "chat should enforce conciseness"
        );
        assert!(
            REASONER_SYSTEM_PROMPT.contains("concise")
                || REASONER_SYSTEM_PROMPT.contains("results"),
            "reasoner should enforce conciseness"
        );

        // Both should have tool-result grounding
        assert!(
            CHAT_SYSTEM_PROMPT.contains("tool results") || CHAT_SYSTEM_PROMPT.contains("Trust"),
            "chat should ground on tool results"
        );
        assert!(
            REASONER_SYSTEM_PROMPT.contains("tool results")
                || REASONER_SYSTEM_PROMPT.contains("Trust"),
            "reasoner should ground on tool results"
        );

        // Both should have anti-fabrication rules
        assert!(
            CHAT_SYSTEM_PROMPT.contains("NEVER fabricate"),
            "chat should prevent fabrication"
        );
        assert!(
            REASONER_SYSTEM_PROMPT.contains("NEVER fabricate"),
            "reasoner should prevent fabrication"
        );
    }

    #[test]
    fn both_prompts_forbid_shell_commands() {
        assert!(
            CHAT_SYSTEM_PROMPT.contains("shell commands"),
            "chat should forbid shell commands"
        );
        assert!(
            REASONER_SYSTEM_PROMPT.contains("fs_read")
                && REASONER_SYSTEM_PROMPT.contains("fs_grep"),
            "reasoner should reference tool alternatives"
        );
    }

    // ── Qwen and Gemini prompt selection tests ──

    #[test]
    fn qwen_prompt_is_concise_and_action_oriented() {
        assert!(
            QWEN_SYSTEM_PROMPT.contains("1-4 lines"),
            "qwen should enforce brevity"
        );
        assert!(
            QWEN_SYSTEM_PROMPT.contains("Minimize tokens"),
            "qwen should minimize tokens"
        );
        assert!(
            QWEN_SYSTEM_PROMPT.contains("Never fabricate"),
            "qwen should have anti-hallucination"
        );
        assert!(
            QWEN_SYSTEM_PROMPT.contains("Trust tool results"),
            "qwen should trust tool results"
        );
        let qwen_lines = QWEN_SYSTEM_PROMPT.lines().count();
        assert!(
            qwen_lines < 25,
            "qwen prompt should be short (< 25 lines), got {qwen_lines}"
        );
    }

    #[test]
    fn gemini_prompt_is_thorough_and_methodical() {
        assert!(
            GEMINI_SYSTEM_PROMPT.contains("METHODOLOGY"),
            "gemini should have methodology section"
        );
        assert!(
            GEMINI_SYSTEM_PROMPT.contains("Explore"),
            "gemini should emphasize exploration"
        );
        assert!(
            GEMINI_SYSTEM_PROMPT.contains("Analyze"),
            "gemini should emphasize analysis"
        );
        assert!(
            GEMINI_SYSTEM_PROMPT.contains("Verify"),
            "gemini should emphasize verification"
        );
        assert!(
            GEMINI_SYSTEM_PROMPT.contains("NEVER fabricate"),
            "gemini should have anti-hallucination"
        );
    }

    #[test]
    fn model_aware_selects_qwen_prompt() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "qwen-2.5-coder",
        );
        assert!(
            prompt.contains("1-4 lines"),
            "qwen model should use QWEN_SYSTEM_PROMPT"
        );
        assert!(
            prompt.contains("1-4 lines"),
            "qwen should have conciseness directive"
        );
        assert!(
            !prompt.contains("THINKING STRATEGY"),
            "qwen should NOT get reasoner prompt"
        );
        assert!(
            !prompt.contains("METHODOLOGY"),
            "qwen should NOT get gemini prompt"
        );
    }

    #[test]
    fn model_aware_selects_gemini_prompt() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "gemini-2.0-flash",
        );
        assert!(
            prompt.contains("METHODOLOGY"),
            "gemini model should use GEMINI_SYSTEM_PROMPT"
        );
        assert!(
            prompt.contains("METHODOLOGY"),
            "gemini should have methodology section"
        );
        assert!(
            !prompt.contains("THINKING STRATEGY"),
            "gemini should NOT get reasoner prompt"
        );
        assert!(
            !prompt.contains("1-4 lines"),
            "gemini should NOT get qwen prompt"
        );
    }

    #[test]
    fn model_aware_qwen_case_insensitive() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "Qwen-2.5-72B",
        );
        assert!(
            prompt.contains("1-4 lines"),
            "qwen matching should be case-insensitive"
        );
    }

    #[test]
    fn model_aware_gemini_case_insensitive() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "Gemini-Pro-1.5",
        );
        assert!(
            prompt.contains("METHODOLOGY"),
            "gemini matching should be case-insensitive"
        );
    }

    #[test]
    fn qwen_gets_complexity_injection() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            None,
            "qwen-2.5-coder",
        );
        assert!(
            prompt.contains("COMPLEX TASK"),
            "qwen complex should get planning protocol"
        );
        // Qwen should NOT get the extra prescriptive guidance (that is chat-only)
        assert!(
            !prompt.contains("Verification Protocol"),
            "qwen should not get chat prescriptive guidance"
        );
    }

    #[test]
    fn gemini_gets_complexity_injection() {
        let prompt = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            None,
            "gemini-2.0-flash",
        );
        assert!(
            prompt.contains("COMPLEX TASK"),
            "gemini complex should get planning protocol"
        );
        assert!(
            !prompt.contains("Verification Protocol"),
            "gemini should not get chat prescriptive guidance"
        );
    }

    #[test]
    fn qwen_no_extra_tier_guidance() {
        // Qwen prompts should not get REASONER_GUIDANCE or CHAT_PRESCRIPTIVE_GUIDANCE
        let simple = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "qwen-2.5-coder",
        );
        let complex = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Complex,
            None,
            "qwen-2.5-coder",
        );
        assert!(
            !simple.contains("Model: DeepSeek-Reasoner"),
            "qwen simple should not get reasoner guidance"
        );
        assert!(
            !complex.contains("Explicit Verification Protocol"),
            "qwen complex should not get chat prescriptive guidance"
        );
    }

    #[test]
    fn gemini_preserves_memory_and_context() {
        let ctx = WorkspaceContext {
            cwd: "/workspace".to_string(),
            git_branch: Some("feature".to_string()),
            os: "linux".to_string(),
        };
        let prompt = build_model_aware_system_prompt(
            Some("Use 4 spaces."),
            None,
            Some("Check types."),
            Some(&ctx),
            PromptComplexity::Simple,
            None,
            "gemini-2.0-flash",
        );
        assert!(prompt.contains("Use 4 spaces."));
        assert!(prompt.contains("Check types."));
        assert!(prompt.contains("/workspace"));
        assert!(prompt.contains("feature"));
    }

    #[test]
    fn qwen_and_gemini_forbid_shell_commands() {
        assert!(
            QWEN_SYSTEM_PROMPT.contains("fs_read")
                && QWEN_SYSTEM_PROMPT.contains("fs_grep")
                && QWEN_SYSTEM_PROMPT.contains("fs_glob"),
            "qwen should reference tool alternatives"
        );
        assert!(
            GEMINI_SYSTEM_PROMPT.contains("fs_read")
                && GEMINI_SYSTEM_PROMPT.contains("fs_grep")
                && GEMINI_SYSTEM_PROMPT.contains("fs_glob"),
            "gemini should reference tool alternatives"
        );
    }

    #[test]
    fn qwen_and_gemini_have_anti_parrot() {
        assert!(
            QWEN_SYSTEM_PROMPT.contains("YOUR reference only"),
            "qwen should prevent parroting context"
        );
        assert!(
            GEMINI_SYSTEM_PROMPT.contains("context") || GEMINI_SYSTEM_PROMPT.contains("DO NOT"),
            "gemini should have constraints section"
        );
    }

    #[test]
    fn deepseek_models_unaffected_by_new_families() {
        // Ensure deepseek-chat still gets CHAT_SYSTEM_PROMPT
        let chat = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "deepseek-chat",
        );
        assert!(
            chat.contains("RULES"),
            "deepseek-chat should still use CHAT_SYSTEM_PROMPT"
        );

        // Ensure deepseek-reasoner still gets REASONER_SYSTEM_PROMPT
        let reasoner = build_model_aware_system_prompt(
            None,
            None,
            None,
            None,
            PromptComplexity::Simple,
            None,
            "deepseek-reasoner",
        );
        assert!(
            reasoner.contains("THINKING STRATEGY"),
            "deepseek-reasoner should still use REASONER_SYSTEM_PROMPT"
        );
    }
}
