# CodingBuddy v2: Full Overhaul Plan

> Goal: Make CodingBuddy a production-grade AI coding CLI that matches or surpasses Claude Code and OpenCode for any model you connect (DeepSeek, Qwen, Gemini, OpenAI-compatible, Ollama, etc.)

---

## Executive Summary

### What the audit found

**CodingBuddy (current state: 6/10)**
- 23-crate Rust workspace — over-engineered for what actually works
- Core agent loop is solid but surrounded by half-finished features
- MCP is a skeleton (3/10) — tools discovered but never invoked
- Per-turn retrieval is documented but not implemented
- Prompt engineering is mediocre (5/10) — 150-line system prompts that waste tokens
- Testing is unit-heavy, integration-weak (4/10)
- TUI is functional but basic — no cost display, no approval previews, no diff rendering
- Local ML adds massive complexity for marginal value
- Complexity classifier exists but has zero impact on behavior

**OpenCode (reference: 8.5/10)**
- TypeScript + Effect library — composable, testable services
- 20+ LLM providers out of the box (Anthropic, OpenAI, Google, Azure, Bedrock, Groq, etc.)
- Fine-grained permission system with pattern matching and per-session overrides
- Multi-agent design (build/plan/explore/compaction) with Tab switching
- Multi-strategy file editing (exact → whitespace-tolerant → Levenshtein fuzzy)
- Git-based snapshots for full revert capability
- Full LSP integration (30+ language servers, diagnostics after edits)
- Complete MCP with OAuth, resources, prompts
- Bash command analysis via tree-sitter AST parsing
- Cost tracking per message in UI

**Claude Code (reference: 9.5/10)**
- Transcript-first persistence (user message saved before API call = crash-proof)
- Atomic file operations (no async between staleness check and write)
- 4-layer permission system (static rules → automation → interactive → mode enforcement)
- Streaming tool execution with parallelism control
- Snip compaction (aggressive memory-bounded history compression)
- 40+ tools, each self-contained (schema, validation, execution, rendering)
- Hooks system at lifecycle points (pre-tool, post-tool, compact, etc.)
- Cost tracking per model with cache token asymmetry
- Bash security: 5 separate validation layers (read-only, sed, path, injection, permissions)

### What we need to do

The gap isn't about adding more crates. It's about **finishing what exists, stealing the best patterns, and cutting the dead weight**. The plan below is ordered by impact — do Phase 1 first, it alone will transform the tool.

---

## Phase 1: Foundation Fixes (Critical — Do First)

These are the things that make CodingBuddy feel broken or unfinished. No new features until these are solid.

### 1.1 Cut Dead Weight

**Problem:** 23 crates is 8 too many. Build times suffer, navigation is hard, abstractions exist for things that don't work.

**Action:**
| Crate | Action | Reason |
|-------|--------|--------|
| `codingbuddy-chrome` | DELETE | 809 lines for a niche feature nobody uses. If needed later, add as MCP server |
| `codingbuddy-observe` | MERGE into `codingbuddy-core` | Thin logging wrapper, doesn't justify a crate |
| `codingbuddy-jsonrpc` | DELETE or FREEZE | 1871 lines for IDE integration that has no IDE plugin. Revisit when there's a VS Code extension |
| `codingbuddy-local-ml` | FEATURE-GATE and DEPRIORITIZE | 5000+ lines, Candle-based, CPU-only, slow. Ghost text is a nice-to-have, not a must-have. Keep behind `--features local-ml` but stop investing |
| `codingbuddy-subagent` | KEEP but SIMPLIFY | Worktree isolation is valuable (Claude Code has it). But current implementation is half-baked. Simplify to core: spawn agent in worktree, collect result |
| `codingbuddy-context` | MERGE into `codingbuddy-agent` | Context enrichment is part of the agent's job, not a separate crate |
| `codingbuddy-skills` | MERGE into `codingbuddy-agent` | Skills are agent capabilities, not standalone |

**Target:** 15-16 crates, each with clear purpose.

### 1.2 Fix MCP Integration (Currently 3/10 → Target 8/10)

**Problem:** MCP tools are discovered and cached but never actually invoked in the tool loop. The `McpExecutor` callback exists but isn't wired.

**What OpenCode does right:**
- MCP tools treated identically to built-in tools in the registry
- Tool invocation goes through same dispatch path
- OAuth support with local callback server
- Graceful degradation when server unavailable
- Resources and prompts integrated alongside tools

**What Claude Code does right:**
- MCP tools are first-class in the query loop, system prompt, and permission system
- Tool descriptions capped at 2048 chars (prevents OpenAPI bloat from eating context)
- Transport types: stdio, SSE, StreamableHTTP, WebSocket, in-process
- Elicitation handling for auth flows

**Action in `codingbuddy-mcp` and `codingbuddy-agent`:**

1. **Wire MCP tool dispatch into the tool loop** (`tool_loop.rs`):
   - When `LocalToolHost` receives a tool call with `mcp__` prefix, route to `McpExecutor`
   - MCP tool results should flow through the same `ToolResult` pipeline as built-in tools
   - Apply same security scanning and output filtering

2. **Cap tool descriptions** to 2048 chars when injecting into system prompt (stolen from Claude Code)

3. **Add MCP server health monitoring**:
   - Track server status (connected/failed/needs_restart)
   - Auto-restart crashed stdio servers (max 3 retries)
   - Timeout for tool calls (default 120s, configurable)

4. **Implement resource support**:
   - `mcp__server__list_resources` and `mcp__server__read_resource` as tool calls
   - Resources included in context when relevant

5. **Add prompt support**:
   - `mcp__server__list_prompts` discovers available prompts
   - Prompts can be invoked as slash commands

### 1.3 Fix Per-Turn Retrieval (Currently Broken)

**Problem:** CLAUDE.md says "per-turn retrieval fires every turn" but the code only does retrieval in bootstrap. This is a documentation lie that also means the agent gets dumber as conversations get longer.

**Action in `codingbuddy-agent/src/tool_loop.rs`:**

1. Before each LLM call (not just turn 1), call `inject_retrieval_context()`:
   ```
   if remaining_tokens > 500 {
       let budget = remaining_tokens / 5;
       let context = retriever.search(&last_user_message, budget).await;
       messages.push(system_message(context));
   }
   ```

2. Use the user's latest message as the retrieval query (not the full conversation)

3. Deduplicate against already-injected context (don't re-inject bootstrap files)

4. Index updates should be incremental (SHA-256 change detection already exists — make sure it fires)

### 1.4 Fix the Permission UX

**Problem:** CodingBuddy has a permission engine with modes (ask/auto/bypass) but the UX is unclear. Users don't see diffs before edits. There's no "approve once for similar operations" pattern.

**What to steal from OpenCode:**
- Ask/Allow/Deny model with glob patterns (`*.env` → always ask)
- Diff preview before edit approval
- Per-session permission overrides
- Cascading approvals (approve once → similar operations auto-approved)

**What to steal from Claude Code:**
- 4-layer stack: static rules → automation → interactive → mode enforcement
- Deny-list evaluated before allow-list (secure by default)
- Permission denials tracked and reported

**Action in `codingbuddy-policy`:**

1. Add `show_diff_preview()` before edit approval:
   - Generate unified diff of proposed change
   - Display in terminal with syntax highlighting
   - User sees exactly what will change before approving

2. Add cascading approval:
   - When user approves `fs_edit` on `src/foo.rs`, auto-approve subsequent `fs_edit` on `src/*.rs` for this session
   - Pattern: `(tool_name, path_glob)` → approved

3. Add glob-based rules to config:
   ```toml
   [permissions]
   "*.env" = "ask"
   "*.lock" = "deny"
   "src/**/*.rs" = "allow"
   ```

### 1.5 Make Streaming Feel Responsive

**Problem:** Tool loop waits for entire LLM response before processing. Long responses feel frozen.

**Action:**
- Stream text tokens to TUI as they arrive (already partially works via `StreamChunk::TextDelta`)
- Show tool call names as soon as `ToolCallStart` arrives (before args are complete)
- Add spinner/progress indicator during tool execution
- Display cost accumulation in the status bar in real-time

---

## Phase 2: Steal the Best Patterns (High Impact)

### 2.1 Multi-Strategy File Editing (from OpenCode)

**Problem:** CodingBuddy's `fs_edit` does exact string matching only. If the model gets whitespace wrong, the edit fails. This is the #1 source of wasted tool calls.

**OpenCode's approach (3-tier fallback):**
1. **SimpleReplacer** — exact match (fast, reliable)
2. **LineTrimmedReplacer** — match after trimming whitespace per line (handles indentation drift)
3. **BlockAnchorReplacer** — Levenshtein distance similarity on first/last lines of the block (handles minor content drift)

**Action in `codingbuddy-tools`:**

```rust
fn find_and_replace(content: &str, old: &str, new: &str) -> Result<String> {
    // Strategy 1: Exact match
    if let Some(result) = exact_replace(content, old, new) {
        return Ok(result);
    }
    // Strategy 2: Whitespace-normalized match
    if let Some(result) = whitespace_tolerant_replace(content, old, new) {
        return Ok(result);
    }
    // Strategy 3: Fuzzy block anchor match (Levenshtein ≤ 2 on anchor lines)
    if let Some(result) = fuzzy_anchor_replace(content, old, new) {
        return Ok(result);
    }
    Err("No match found. Verify the old_string by reading the file first.")
}
```

### 2.2 Real Cost Tracking in UI (from both)

**Problem:** Users run sessions and have no idea what they cost until they check the API dashboard.

**What to implement:**
- Per-model pricing table (DeepSeek chat: $0.27/$1.10 per 1M tokens, reasoner: $0.55/$2.19, etc.)
- Track input/output/cache tokens per message
- Running total in status bar: `$0.42 | 12.3K tokens | deepseek-chat`
- Per-message cost annotation in conversation display
- Cache hit discount tracking (DeepSeek server-side caching)
- Budget enforcement: `max_budget_usd` in config, warn at 80%, hard stop at 100%

**Action:** The `CostTracker` already exists in `tool_loop.rs`. Wire it to:
1. The TUI status bar (currently not displayed)
2. The `StreamChunk::UsageUpdate` events (already emitted but ignored by UI)
3. Add model-specific pricing in `codingbuddy-llm`

### 2.3 Git-Based Snapshots & Revert (from OpenCode)

**Problem:** `StepSnapshot` exists but uses JSON files with SHA-256 hashes. OpenCode uses a separate internal git repo per project — simpler, more robust, supports full revert with `git checkout`.

**Action in `codingbuddy-memory`:**

1. On every file edit, commit the before-state to an internal git repo (`.codingbuddy/.snapshots/`)
2. Enable `/revert` command that:
   - Lists recent snapshots with diffs
   - Reverts all files to a specific snapshot
   - Uses `git checkout` internally (atomic, handles binary files)
3. Auto-prune snapshots older than 7 days

### 2.4 Bash Command Security (from Claude Code)

**Problem:** CodingBuddy's bash tool does basic forbidden-token checking. Claude Code has 5 separate validation layers. OpenCode parses commands with tree-sitter.

**Action in `codingbuddy-tools` (bash_run):**

1. **Add AST-based command parsing** (use `tree-sitter-bash` crate):
   - Parse command into AST
   - Extract: command name, arguments, redirections, pipes
   - Detect file operations (rm, mv, cp) and their targets
   - Detect network operations (curl, wget) and their URLs

2. **Add path validation**:
   - Commands that reference paths outside workspace → require approval
   - Detect `~/.ssh`, `~/.aws`, `/etc` access
   - Expand globs before evaluation

3. **Add output management**:
   - Truncate output > 1MB (Claude Code pattern)
   - Detect binary output and skip
   - Auto-background commands running > 30s (inform user)

### 2.5 Multi-Provider Support (from OpenCode)

**Problem:** CodingBuddy targets DeepSeek primarily. The `provider_transform.rs` layer exists but is DeepSeek-centric. OpenCode supports 20+ providers via the AI SDK.

**Action in `codingbuddy-llm`:**

1. **Add provider registry**:
   ```rust
   enum Provider {
       DeepSeek,    // existing
       OpenAI,      // gpt-4o, o1, o3
       Anthropic,   // claude-3.5/4
       Google,      // gemini-2.5
       Groq,        // llama, mixtral
       Ollama,      // local models
       OpenRouter,  // any model via API
       Custom(String), // any OpenAI-compatible endpoint
   }
   ```

2. **Per-provider configuration**:
   ```toml
   [providers.openai]
   api_key_env = "OPENAI_API_KEY"
   base_url = "https://api.openai.com/v1"
   default_model = "gpt-4o"

   [providers.anthropic]
   api_key_env = "ANTHROPIC_API_KEY"
   base_url = "https://api.anthropic.com/v1"
   default_model = "claude-sonnet-4-20250514"
   ```

3. **Provider-specific features**:
   - Anthropic: prompt caching, extended thinking, `cache_control` annotations
   - OpenAI: Responses API, structured outputs, reasoning effort
   - Google: grounding, code execution tool
   - DeepSeek: reasoner mode, FIM (keep existing)

4. **Model capability detection**:
   - Does the model support tool calls? (some Ollama models don't)
   - Does the model support thinking/reasoning?
   - Max context window, max output tokens
   - Pricing per 1M tokens

### 2.6 LSP Integration Overhaul (from OpenCode)

**Problem:** `codingbuddy-lsp` exists and runs post-edit checks, but it's basic — just runs `cargo check`, `tsc`, etc. OpenCode spawns actual language servers and gets real diagnostics.

**Action in `codingbuddy-lsp`:**

1. **Add language server spawning**:
   - Detect project type from workspace files
   - Spawn appropriate server (rust-analyzer, typescript-language-server, pyright, gopls)
   - Maintain persistent connection (don't restart per edit)

2. **Use LSP diagnostics instead of CLI tools**:
   - `textDocument/didChange` → get diagnostics
   - Diagnostics include line numbers, severity, and fix suggestions
   - Feed diagnostics back to LLM as structured data

3. **Add references/definition lookup**:
   - Agent can use `textDocument/references` to find usages
   - `textDocument/definition` for go-to-definition
   - Useful for refactoring tasks

**Note:** This is a significant effort. Start with rust-analyzer and typescript-language-server only.

---

## Phase 3: Agent Intelligence (Medium Impact)

### 3.1 Agent Profiles That Actually Work

**Problem:** `AgentProfile` exists with build/explore/plan profiles but auto-selection is keyword-based and weak. OpenCode has agents as first-class citizens with Tab switching.

**Action:**

1. **Define 4 clear agents** (inspired by OpenCode + Claude Code):

   | Agent | Model | Tools | Use Case |
   |-------|-------|-------|----------|
   | `code` | default | all | Primary coding agent, full access |
   | `plan` | default | read-only | Planning, architecture discussion |
   | `explore` | fast/cheap | read + grep + glob | Quick codebase search |
   | `compact` | fast/cheap | none | Summarize conversation for compaction |

2. **Add agent switching** via `/agent code`, `/agent plan`, `/agent explore`
3. **Auto-select** based on slash command: `/plan` → plan agent, default → code agent
4. **Per-agent permissions**: plan agent can't edit files, explore agent can't run bash

### 3.2 Better Compaction

**Problem:** LLM-based compaction exists but the template is basic. Claude Code's snip compaction is more aggressive and memory-bounded.

**Action:**

1. **Structured compaction template** (stolen from Claude Code):
   ```
   ## Session Summary
   ### Goal
   [What the user is trying to accomplish]
   ### Key Decisions Made
   [User preferences, corrections, architectural choices]
   ### Work Completed
   [Files modified, tests passed, features added]
   ### Current State
   [What's in progress, what's blocked, what's next]
   ### Important Context
   [Error patterns seen, environment details, constraints]
   ```

2. **Two-phase compaction** (keep existing):
   - Phase 1 (80%): Prune old tool outputs (keep last 3 turns intact)
   - Phase 2 (95%): Full LLM-based summary

3. **Preserve user corrections across compaction**:
   - "Key Decisions Made" section explicitly captures things like "user said don't use mocks" or "user prefers single PR"
   - These survive compaction and inform future behavior

### 3.3 Tool Result Caching Improvements

**Problem:** `ToolResultCache` exists with 60s TTL for read-only tools. But path-based invalidation is basic.

**Action:**
1. Extend TTL to 120s for `fs_read` (files don't change that fast)
2. Add content-hash-based invalidation (not just path-based)
3. Cache `fs_glob` results per directory (invalidate on any write to that directory)
4. Skip cache for files the agent just edited (force re-read to verify edit worked)

### 3.4 Parallel Tool Execution

**Problem:** Parallel execution for read-only tools exists but is conservative.

**What Claude Code does:**
- `StreamingToolExecutor` with concurrency control
- Concurrent-safe tools (read, grep, glob) run in parallel
- Exclusive tools (edit, bash) run sequentially
- Errors in one parallel tool don't block siblings

**Action:**
1. Mark each tool as `concurrent_safe: bool` in tool definitions
2. When LLM returns multiple tool calls, partition into safe/exclusive
3. Run all safe tools concurrently, then exclusive tools sequentially
4. Aggregate results and send back in original order

---

## Phase 4: UX Polish (Visible Impact)

### 4.1 TUI Improvements

**Current state:** ratatui/crossterm based, functional but basic.

**Priority additions:**

1. **Cost display in status bar**: `$0.42 | 14K tok | deepseek-chat | 3 tools`
2. **Diff rendering for edits**: Show unified diff with red/green highlighting when asking for edit approval
3. **Progress indicators**: Spinner during LLM response, tool name displayed during execution
4. **Scrollback improvements**: Page up/down for long outputs, search in history
5. **Agent indicator**: Show current agent name in status bar, highlight when in plan/explore mode
6. **Token budget bar**: Visual indicator of context window usage (e.g., `[████████░░] 78%`)

### 4.2 Slash Commands

**Add essential slash commands** (inspired by Claude Code + OpenCode):

| Command | Action |
|---------|--------|
| `/model <name>` | Switch model mid-session |
| `/cost` | Show session cost breakdown |
| `/compact` | Force conversation compaction |
| `/revert` | Revert to last snapshot |
| `/agent <name>` | Switch agent profile |
| `/clear` | Clear conversation (keep system context) |
| `/resume` | Resume last session |
| `/config` | Show/edit configuration |
| `/provider <name>` | Switch provider |

### 4.3 Session Resume

**Problem:** If CLI crashes, conversation is lost. Claude Code saves transcripts before API calls.

**Action:**
1. Save user message to session store BEFORE calling LLM
2. On startup, check for incomplete sessions (user message with no response)
3. Offer to resume: "Found incomplete session from 5m ago. Resume? (y/n)"
4. Replay conversation history and continue from where it left off

---

## Phase 5: Testing & Reliability (Ongoing)

### 5.1 Integration Tests

**Problem:** Tests mock everything. No validation that the actual tool loop + real files + real commands works.

**Action:**
1. Create test fixtures: small Rust/TypeScript/Python projects
2. Write integration tests that:
   - Start an agent with `ScriptedToolLlm`
   - Execute a real edit on a real file
   - Verify the file was correctly modified
   - Verify LSP diagnostics fire
   - Verify snapshots were created
3. Add CI step that runs integration tests on each PR

### 5.2 End-to-End Tests

**Action:**
1. Create 5 benchmark scenarios:
   - "Add a function to an existing Rust file"
   - "Fix a compilation error"
   - "Refactor a function to use a new pattern"
   - "Find and fix a bug given a test failure"
   - "Add a new CLI command"
2. Run each scenario against real DeepSeek API (nightly CI)
3. Score: did the edit compile? Did tests pass? Was the output correct?
4. Track scores over time to detect regressions

### 5.3 Streaming Tests

**Action:**
1. Test interrupted streams (kill connection mid-response)
2. Test malformed JSON in streamed tool calls
3. Test partial responses (model hits max_tokens)
4. Test cancellation token during tool execution

---

## Phase 6: Prompt Engineering Overhaul

### 6.1 Reduce System Prompt Size

**Problem:** 150-200 line system prompts waste context. DeepSeek has smaller context windows than Claude.

**Action:**
1. Cut system prompts to 50-80 lines max
2. Move tool usage instructions INTO tool descriptions (where they belong)
3. Remove anti-patterns that models already know ("don't hallucinate" — every model is told this)
4. Keep only: role definition, key constraints, output format, tool usage philosophy

### 6.2 Model-Specific Optimizations

**Action per model family:**

| Model | Optimization |
|-------|-------------|
| DeepSeek Chat | Action-biased, concise. Let tools do the work. 8K output limit. |
| DeepSeek Reasoner | Leverage native thinking. No ThinkingConfig. Strip tool_choice. 64K output. |
| GPT-4o/o3 | Structured outputs, function calling. Reasoning effort parameter for o3. |
| Claude 3.5/4 | Prompt caching, extended thinking, cache_control annotations. |
| Gemini 2.5 | Large context window, grounding. Code execution tool. |
| Qwen | Chinese/English bilingual. Concise outputs. |
| Ollama/Local | May not support tool calls — add text-based tool call parsing fallback. |

### 6.3 Add Chain-of-Thought for Non-Reasoner Models

**Problem:** DeepSeek Chat just acts without thinking. For complex tasks, this leads to poor edits.

**Action:**
- For Medium/Complex tasks on non-reasoner models, inject:
  ```
  Before editing, briefly state:
  1. What you understand the problem to be
  2. What files are involved
  3. What change you plan to make
  Then proceed with the edit.
  ```
- For Simple tasks, skip this (just act)

---

## Priority Order & Dependencies

```
Phase 1 (Foundation) — MUST DO FIRST, 2-3 weeks
  1.1 Cut dead weight (1-2 days)
  1.2 Fix MCP integration (3-4 days)
  1.3 Fix per-turn retrieval (1 day)
  1.4 Fix permission UX (2-3 days)
  1.5 Streaming responsiveness (1-2 days)

Phase 2 (Steal Best Patterns) — HIGH IMPACT, 3-4 weeks
  2.1 Multi-strategy file editing (2 days)
  2.2 Cost tracking in UI (1-2 days)
  2.3 Git-based snapshots (2-3 days)
  2.4 Bash security (3-4 days)
  2.5 Multi-provider support (5-7 days) ← biggest effort
  2.6 LSP overhaul (5-7 days) ← biggest effort

Phase 3 (Agent Intelligence) — MEDIUM IMPACT, 2 weeks
  3.1 Agent profiles (2-3 days)
  3.2 Better compaction (2 days)
  3.3 Tool caching improvements (1 day)
  3.4 Parallel tool execution (2 days)

Phase 4 (UX Polish) — VISIBLE IMPACT, 1-2 weeks
  4.1 TUI improvements (3-4 days)
  4.2 Slash commands (2-3 days)
  4.3 Session resume (2 days)

Phase 5 (Testing) — ONGOING
  5.1-5.3 Continuous, start with Phase 1

Phase 6 (Prompts) — DO ALONGSIDE Phase 2
  6.1-6.3 Can be done incrementally
```

---

## What NOT To Do

1. **Don't rewrite in TypeScript.** Rust is a strength — faster startup, lower memory, no Node.js dependency. The architectural patterns from OpenCode/Claude Code can be implemented in Rust.

2. **Don't add more crates.** Consolidate. Every new crate should replace two old ones.

3. **Don't invest in local ML.** Ghost text via Candle is a novelty. The real value is in agent quality, not local inference. Keep it feature-gated but don't prioritize.

4. **Don't build an IDE extension yet.** Get the CLI right first. The jsonrpc crate can wait.

5. **Don't try to compensate for weak models with more engineering.** Instead: support strong models (Claude, GPT-4o) alongside DeepSeek so users can choose. A good agent loop with Claude 4 will outperform a genius agent loop with a weak model.

6. **Don't add features nobody asked for.** Chrome automation, visual verification, speculative decoding — these are distractions. Focus on: edit files correctly, run commands safely, manage context well, support many models.

---

## The Thesis

CodingBuddy tried to compensate for DeepSeek's limitations with engineering complexity (23 crates, complexity classifiers, phase loops, thinking budget escalation). The result: lots of clever machinery, but the user experience doesn't match Claude Code or OpenCode.

The path forward:
1. **Stop fighting the model.** Support better models alongside DeepSeek.
2. **Finish what exists.** MCP, retrieval, permissions — all half-done.
3. **Steal proven patterns.** Multi-strategy editing, git snapshots, LSP, cost tracking.
4. **Cut the complexity.** Fewer crates, shorter prompts, simpler code.

The goal isn't to have the most sophisticated agent loop. It's to have the one that **actually works reliably** for real coding tasks, with any model the user connects.
