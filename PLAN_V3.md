# CodingBuddy v3: Close the Gap Plan

> Goal: Close the remaining distance to Claude Code (9.5/10) and OpenCode (8.5/10). Current state post-v2: **7.5/10**. Target: **9/10**.

---

## What Changed Since v2

The v2 overhaul (75% completed) raised CodingBuddy from 6/10 to 7.5/10:
- ✅ MCP integration wired and working
- ✅ Multi-provider support (10+ providers)
- ✅ Multi-strategy file editing (exact → whitespace → fuzzy)
- ✅ Agent profiles with /agent switching
- ✅ Per-turn retrieval firing every turn
- ✅ Bash AST security via tree-sitter
- ✅ Git-based snapshots & /revert
- ✅ System prompts trimmed 60%
- ✅ Cost tracking per model
- ✅ Token budget bar, profile badges, session resume
- ✅ Integration tests for real file ops

**But the audits reveal the remaining 2-point gap is not about features — it's about quality, polish, and architecture.**

---

## Fresh Audit: What the Reference Implementations Do Better

### Claude Code (9.5/10) — The Gaps That Matter

| Area | Claude Code | CodingBuddy | Gap |
|------|-------------|-------------|-----|
| **TUI Architecture** | Full React-in-terminal with custom Ink renderer, 60fps, ANSI-aware wrapping, responsive layout, bidi text | Basic ratatui, hardcoded colors, no scrollback, no responsive layout | **Critical** |
| **Tool Modularity** | Each tool is a self-contained module (BashTool = 16 files, 1.2MB) with dedicated security, permissions, rendering | Tools defined in big lib.rs with shared helpers | **High** |
| **Subagent System** | Fork with prompt cache sharing, worktree isolation, color-coded agents, coordinator mode, team system | Basic subagent crate, half-baked | **High** |
| **Skills System** | Markdown frontmatter (name, whenToUse, model, effort, paths, tools), bundled + user + project + MCP skills | Basic slash commands, no model override, no discovery | **High** |
| **Hooks System** | 7 lifecycle events (session_start, pre_compact, post_compact, permission_request, post_sampling, tool_use_complete, command_complete) | HookRuntime exists with 14 events but poorly integrated | **Medium** |
| **Memory System** | Auto-extraction from sessions, typed taxonomy (user/feedback/project/reference), team memory | codingbuddy-memory exists but focused on shadow commits, no auto-extraction | **Medium** |
| **Permission Classifier** | ML-based auto-approval for safe bash commands, denial tracking (3 denials → abort) | Rule-based only, no denial tracking | **Medium** |
| **Startup Performance** | Parallel prefetch (MDM + keychain), 135ms import with 65ms parallelization, dead code elimination at build time | ~500ms-1s cold start, no profiling, no optimization | **Medium** |
| **Error Recovery UX** | Graceful fallbacks everywhere, actionable error messages, structured error display | 707 unwraps, some silent failures, cryptic errors | **High** |
| **Markdown Tables** | ANSI-aware wrapping, responsive column widths, vertical fallback for tall rows | No table rendering | **Low** |
| **Tool Deferred Loading** | ToolSearch: only load full schemas when needed, reduces system prompt size | All tools always loaded | **Medium** |
| **Session Search** | LogSelector UI, searchable history, session tags | Session resume only, no search/browse | **Medium** |

### OpenCode (8.5/10) — The Gaps That Matter

| Area | OpenCode | CodingBuddy | Gap |
|------|----------|-------------|-----|
| **Reactive TUI** | Solid.js signals + @opentui at 60fps, responsive, toast notifications, dialog system | ratatui with manual state management | **High** |
| **Real PTY for Bash** | Pseudo-terminal for interactive commands, proper job control | Basic Command::new with piped stdio | **High** |
| **Plugin Architecture** | npm packages + hooks-based, server-side plugins | No plugin system | **Medium** |
| **LSP Auto-Download** | Missing language servers auto-downloaded on first use | Manual installation required | **Medium** |
| **Markdown Commands** | Agents/commands as .md files with frontmatter, hot-reload | Hardcoded agent definitions | **Medium** |
| **Dialog System** | Model picker, command palette, session list, MCP config, help — all as modal dialogs | Basic slash commands only | **Medium** |
| **Provider Auth** | OAuth flows with browser callback, IAM role support | API keys only | **Low** |
| **File Watching** | Chokidar for hot-reload of configs and commands | No file watching | **Low** |

---

## The Plan: 6 Phases, Ordered by Impact

### Phase 1: Production Hardening (Critical — Week 1-2)

The 707 unwraps and silent failures are not a polish problem — they're a trust problem. Users who hit panics uninstall.

#### 1.1 Unwrap Audit & Fix

**Problem:** 707 `.unwrap()` calls across the codebase. Every one is a potential panic in production.

**Action:**
1. Run `grep -rn '\.unwrap()' crates/ --include='*.rs' | grep -v test | grep -v mock` to get the non-test list
2. Categorize:
   - **Lock poisoning** (`.lock().unwrap()`) → Replace with `.lock().expect("description")` or handle gracefully
   - **JSON serialization** (`serde_json::to_string().unwrap()`) → These should NEVER panic; use `?` or `.unwrap_or_default()`
   - **Option unwrap** (`.get().unwrap()`, `.next().unwrap()`) → Use `if let`, `?`, or `.ok_or()`
   - **Config loading** → Use `?` propagation, sensible defaults on error
3. Target: zero unwraps in hot paths (tool_loop, streaming, LLM client, TUI rendering)
4. Acceptable: unwraps in test code, one-time initialization with `expect("reason")`

#### 1.2 Structured Error Messages

**Problem:** Errors are sometimes logged with `eprintln!` and lost. Users see cryptic messages.

**Action:**
1. Replace all `eprintln!` error logging with structured `tracing::error!` (or `codingbuddy-observe` equivalent)
2. User-facing errors must include:
   - What happened (clear, non-technical)
   - What the user can do about it
   - Example: "API key expired. Run `codingbuddy config set api_key` to update." not "401 Unauthorized"
3. Tool errors should suggest recovery: "File not found: src/main.rs. Did you mean src/lib.rs?"

#### 1.3 Crate Consolidation (Finally)

**Problem:** Still 20 crates. v2 target was 15-16. This was deferred.

**Action:**
| Crate | Action | Into |
|-------|--------|------|
| `codingbuddy-context` | MERGE | `codingbuddy-agent` |
| `codingbuddy-skills` | MERGE | `codingbuddy-agent` |
| `codingbuddy-observe` | MERGE | `codingbuddy-core` |
| `codingbuddy-jsonrpc` | FREEZE (don't delete yet) | — |
| `codingbuddy-local-ml` | Ensure feature-gated, deprioritize | — |

**Target:** 16 active crates.

#### 1.4 Startup Performance

**Problem:** 500ms-1s cold start. Claude Code does 135ms with parallel prefetch.

**What to steal from Claude Code:**
```rust
// Fire async operations BEFORE heavy imports
let config_handle = std::thread::spawn(|| load_config());
let key_handle = std::thread::spawn(|| resolve_api_key());
// Initialize heavy modules while config loads
let syntect_set = load_syntax_set();  // This is the slow part
let (config, key) = (config_handle.join(), key_handle.join());
```

**Action:**
1. Profile startup with `CODINGBUDDY_PROFILE=1` (add checkpoint timing like Claude Code's `profileCheckpoint`)
2. Parallelize: config loading + API key resolution + MCP server enumeration
3. Lazy-load syntect syntax definitions (load on first highlight, not on startup)
4. Lazy-load tree-sitter parsers (load per-language on first parse)
5. Target: <300ms cold start, <150ms warm start

---

### Phase 2: TUI Overhaul (High Impact — Week 2-4)

The TUI is the most visible gap. Both Claude Code and OpenCode have sophisticated terminal rendering. CodingBuddy's ratatui setup is functional but basic.

#### 2.1 Rendering Improvements

**Priority changes (keeping ratatui, not rewriting):**

1. **Scrollback & Paging**
   - Implement virtual scrolling for conversation history
   - Page Up/Down, Home/End keys
   - Search in history (Ctrl+F or `/search`)
   - Keep last N turns in memory, older turns on disk (from session store)

2. **Responsive Layout**
   - Terminal resize handling (ratatui already supports this — wire it properly)
   - Adaptive widths: narrow terminal → compact mode, wide → full mode
   - Min/max constraints on layout sections

3. **Diff Rendering**
   - Unified diff with red/green highlighting for file edits
   - Show diff BEFORE approval (permission UX from v2, but make it pretty)
   - Side-by-side diff for wide terminals (optional)
   - Git-style `+`/`-` line prefixes

4. **Progress & Feedback**
   - Animated spinner during LLM response (not just static "thinking...")
   - Tool execution progress: `[bash] running cargo test... (12s)`
   - Streaming cost display: `$0.42 ↑` updating in real-time
   - Token count in status bar updating per-chunk

5. **Markdown Table Rendering**
   - Steal Claude Code's approach: ANSI-aware text wrapping, responsive column widths
   - Vertical fallback when rows too tall
   - Proper borders and alignment

#### 2.2 Dialog System (from OpenCode)

**Problem:** Everything is a slash command. No visual menus for complex selections.

**Action:**
1. **Model Picker Dialog** — searchable list of available models grouped by provider, show pricing/context window
2. **Session Browser Dialog** — list past sessions with timestamps, first message preview, search
3. **Command Palette** — fuzzy-searchable list of all slash commands (Ctrl+P or /)
4. **MCP Status Dialog** — show connected servers, health, available tools
5. **Help Dialog** — keybindings, commands, tips

Implementation: Modal overlays in ratatui (Popup widget pattern). Each dialog is a state machine: Open → Input → Select → Close.

#### 2.3 Theme System

**Problem:** Colors hardcoded in theme.rs. No dark/light detection. No user customization.

**Action:**
1. Detect terminal background color (OSC 11 query, like Claude Code and OpenCode)
2. Auto-select dark/light palette
3. Allow theme override in config: `[ui] theme = "dark"` / `"light"` / `"custom"`
4. Custom theme support: user-defined colors for key elements (prompt, code, error, success, warning)

---

### Phase 3: Tool System Maturity (High Impact — Week 3-5)

#### 3.1 Real PTY for Bash (from OpenCode)

**Problem:** `Command::new("bash").arg("-c").arg(cmd)` misses interactive output, job control, and TTY-dependent programs.

**Action:**
1. Use `portable-pty` or `pty-process` crate for pseudo-terminal
2. Benefits:
   - Programs that check `isatty()` work correctly (e.g., colored output from cargo/pytest)
   - Interactive programs can be supported (with timeout/auto-input)
   - Proper signal handling (Ctrl+C forwarding)
   - Better output streaming (line-by-line from PTY, not buffered)
3. Fallback to `Command::new` when PTY unavailable (CI environments)

#### 3.2 Tool Modularization

**Problem:** Tools are defined in a big `codingbuddy-tools/src/lib.rs` with shared helpers. Hard to add new tools, hard to customize per-tool security.

**What Claude Code does:** Each tool is a directory with:
- `ToolName.ts` — main logic
- `toolSecurity.ts` — tool-specific security checks
- `toolPermissions.ts` — tool-specific permission rules
- `prompt.ts` — tool description for LLM

**Action:**
```
codingbuddy-tools/src/
├── lib.rs                    # Tool registry, dispatch
├── tool_trait.rs             # Tool trait definition
├── bash/
│   ├── mod.rs                # BashTool implementation
│   ├── security.rs           # AST parsing, forbidden patterns
│   ├── output.rs             # Output truncation, binary detection
│   └── permissions.rs        # Bash-specific permission rules
├── fs_edit/
│   ├── mod.rs                # FileEditTool
│   ├── strategies.rs         # Exact, whitespace, fuzzy matching
│   └── validation.rs         # Post-edit validation
├── fs_read/
│   └── mod.rs                # FileReadTool (simple, one file)
├── grep/
│   └── mod.rs
├── glob/
│   └── mod.rs
├── web/
│   ├── fetch.rs
│   └── search.rs
└── agent/
    ├── spawn.rs              # SubagentTool
    └── task.rs               # TaskTool
```

Benefits: each tool is independently testable, security rules are co-located with the tool they protect, new tools are just new directories.

#### 3.3 Tool Deferred Loading (from Claude Code)

**Problem:** All 35+ tool schemas are injected into every system prompt. This wastes tokens, especially with smaller context models.

**What Claude Code does:** `ToolSearch` — tools are listed by name only in the system prompt. The LLM can call `ToolSearch` to get the full schema of specific tools it needs.

**Action:**
1. Classify tools into tiers:
   - **Core** (always loaded): `fs_read`, `fs_edit`, `fs_write`, `bash_run`, `fs_grep`, `fs_glob`
   - **Standard** (loaded by name, schema on demand): `web_fetch`, `web_search`, `git_*`, `notebook_*`, `task_*`, `spawn_task`
   - **MCP** (always deferred): `mcp__*` tools
2. Add `tool_search` tool that returns full schemas for requested tools
3. System prompt lists standard/MCP tools by name + 5-word description only
4. Saves ~2000-5000 tokens per turn on models with small context windows

#### 3.4 Denial Tracking (from Claude Code)

**Problem:** If the user denies a tool call 3 times, CodingBuddy keeps trying. Claude Code aborts the query after 3 denials.

**Action:**
```rust
struct DenialTracker {
    counts: HashMap<String, u32>,
    window: Duration, // 30 seconds
}

impl DenialTracker {
    fn record_denial(&mut self, tool: &str) {
        let count = self.counts.entry(tool.into()).or_insert(0);
        *count += 1;
        if *count >= 3 {
            // Inject message: "You've denied this tool 3 times. I'll try a different approach."
            // Abort current query
        }
    }
}
```

---

### Phase 4: Agent Intelligence v2 (Medium Impact — Week 4-6)

#### 4.1 Skills System (from Claude Code)

**Problem:** CodingBuddy has slash commands but no extensible skills system. Claude Code's skills are markdown files with frontmatter that users can create, share, and customize.

**Action:**

1. **Skill format** (markdown with frontmatter):
   ```markdown
   ---
   name: deploy
   description: Deploy current branch to production
   when_to_use: "When user says deploy, ship, or release"
   model: deepseek-reasoner  # Override for this skill
   allowed_tools: [bash_run, fs_read, fs_grep]
   effort: high
   paths: ["deploy/", "infrastructure/"]
   ---

   ## Instructions
   1. Run the test suite first
   2. Check for uncommitted changes
   3. Build the release binary
   4. Deploy using the deploy script
   ```

2. **Skill discovery** — scan these locations:
   - `.codingbuddy/skills/` (project-level)
   - `~/.codingbuddy/skills/` (user-level)
   - Built-in skills (bundled in binary)

3. **Skill invocation** — `/deploy` triggers skill lookup, injects instructions as system message, applies tool filtering and model override

4. **Built-in skills** to port:
   - `/commit` — create a well-formatted git commit
   - `/review` — review changes on current branch
   - `/simplify` — review changed code for quality
   - `/plan` — enter planning mode (read-only tools)

#### 4.2 Subagent System Overhaul (from Claude Code)

**Problem:** `codingbuddy-subagent` exists but is half-baked. Claude Code's subagent system is sophisticated: forking, worktree isolation, prompt cache sharing, color coding, team coordination.

**Action:**

1. **Proper worktree isolation:**
   - `spawn_agent(worktree=true)` creates a git worktree in a temp directory
   - Agent operates on the worktree, not the main working directory
   - On completion, diff is presented to user for review/merge
   - Automatic cleanup on exit

2. **Agent forking:**
   - Subagent inherits parent's system prompt + conversation summary (not full history)
   - Cost tracked separately but aggregated for budget enforcement
   - Permission context inherited (don't re-ask for already-approved operations)

3. **Color-coded output:**
   - Main agent: default color
   - Each subagent gets a distinct color in the TUI
   - Clear visual separation in conversation display

4. **Coordinator mode (future):**
   - Main agent can spawn multiple subagents for independent tasks
   - Results collected and synthesized by coordinator
   - Not MVP — implement after core subagent works

#### 4.3 LSP Diagnostics Feedback Loop (from v2, incomplete)

**Problem:** LSP diagnostics are retrieved but not structured for the LLM. The agent doesn't use them effectively.

**Action:**
1. After `fs_edit`/`fs_write`, run language-specific check
2. Parse diagnostics into structured format:
   ```json
   {
     "file": "src/main.rs",
     "line": 42,
     "severity": "error",
     "message": "cannot find value `foo` in this scope",
     "suggestion": "did you mean `bar`?"
   }
   ```
3. Append to tool result so LLM sees errors immediately
4. Track diagnostic count — if diagnostics increase after an edit, flag it: "Warning: your edit introduced 3 new errors"

#### 4.4 Memory Auto-Extraction (from Claude Code)

**Problem:** CodingBuddy's memory system is focused on shadow commits and checkpoints. Claude Code automatically extracts learnings from conversations.

**Action:**
1. At session end (or compaction), analyze conversation for:
   - User corrections ("no, don't do X" → save as feedback memory)
   - Architecture decisions ("we use X pattern because Y" → save as project memory)
   - Tool preferences ("always use cargo test, not cargo nextest" → save as feedback memory)
2. Suggest memories to save (user approves before persisting)
3. Store in structured format matching Claude Code's taxonomy:
   - `user/` — role, preferences, knowledge level
   - `feedback/` — corrections, validated approaches
   - `project/` — decisions, constraints, deadlines
   - `reference/` — external resources, links

---

### Phase 5: Hooks & Extensibility (Medium Impact — Week 5-7)

#### 5.1 Hooks System Integration

**Problem:** `codingbuddy-hooks` has 14 lifecycle events defined but integration is shallow. Claude Code has 7 events but they're deeply wired.

**Action — Wire these hooks properly:**

| Hook | When | Use Case |
|------|------|----------|
| `session_start` | Session begins | Run `git status`, load project context |
| `pre_tool` | Before tool execution | Custom permission checks, logging |
| `post_tool` | After tool execution | Notify, log, validate |
| `pre_compact` | Before compaction | Save important context |
| `post_compact` | After compaction | Verify summary quality |
| `permission_request` | Tool needs approval | Auto-approve patterns, custom rules |
| `post_response` | After LLM response | Extract memories, validate output |
| `session_end` | Session ends | Cleanup, save summary |

**Configuration:**
```toml
[hooks]
session_start = [
  { script = "git status --short", skip_errors = true }
]
permission_request = [
  { tool = "bash_run", pattern = "^echo ", action = "approve" },
  { tool = "bash_run", pattern = "^cargo (build|test|check|clippy)", action = "approve" }
]
```

#### 5.2 Plugin System (from OpenCode)

**Problem:** CodingBuddy has no plugin system. OpenCode has npm-based plugins with hooks.

**Action (minimal viable plugin system):**
1. Plugins are Rust dylib or WASM modules that implement a `Plugin` trait
2. Actually, simpler: plugins are **MCP servers**. This is the right abstraction.
3. Instead of a custom plugin API, invest in making MCP servers easy to develop and connect
4. Add `codingbuddy mcp init` command that scaffolds a new MCP server project
5. Add `codingbuddy mcp install <url>` that adds an MCP server from a registry

**Why MCP over custom plugins:** MCP is a standard. Plugins built for CodingBuddy also work with Claude Code, Cursor, etc. No vendor lock-in.

#### 5.3 Markdown-Based Agent/Command Definitions (from OpenCode)

**Problem:** Agent profiles are hardcoded in Rust. Adding a new agent requires recompilation.

**Action:**
1. Agent definitions as markdown files:
   ```markdown
   ---
   name: security-reviewer
   description: Review code for security vulnerabilities
   model: deepseek-reasoner
   tools: [fs_read, fs_grep, fs_glob, bash_run]
   deny_tools: [fs_edit, fs_write]
   ---

   You are a security reviewer. Analyze code for OWASP Top 10 vulnerabilities...
   ```

2. Load from:
   - `.codingbuddy/agents/` (project)
   - `~/.codingbuddy/agents/` (user)
   - Built-in agents (compiled in)

3. Hot-reload: watch agent directory, reload definitions on change

---

### Phase 6: Testing & Polish (Ongoing — Week 6-8)

#### 6.1 End-to-End API Tests

**Problem:** No tests run against real LLM APIs. Can't catch regressions in prompt engineering or provider compatibility.

**Action:**
1. Create E2E test framework:
   - 5 benchmark scenarios (add function, fix error, refactor, find bug, add command)
   - Run against real DeepSeek API (nightly CI, behind env var gate)
   - Score: compiles? tests pass? correct output?
2. Track scores over time in a JSON file committed to repo
3. Alert on regressions (score drops >10%)

#### 6.2 Provider Compatibility Tests

**Action:**
1. For each supported provider, test:
   - Authentication works
   - Streaming works
   - Tool calls work (if supported)
   - Thinking/reasoning works (if supported)
   - Error messages are clear when features unsupported
2. Mock-based tests for format differences (Anthropic vs OpenAI vs Google response shapes)

#### 6.3 Session Persistence Tests

**Action:**
1. Test session save + resume roundtrip
2. Test compaction preserves critical information
3. Test concurrent session access (if applicable)
4. Test session migration between versions

#### 6.4 Final Polish

1. **Binary size optimization:** Feature-gate tree-sitter parsers per language. Only compile parsers for languages detected in workspace.
2. **Compile time:** Reduce with `cargo-udeps` (find unused deps), `cargo-bloat` (find size contributors)
3. **Documentation:** CLI `--help` text for every subcommand, not just top-level
4. **Shell completions:** Generate bash/zsh/fish completions from clap

---

## Priority Matrix

```
                    HIGH IMPACT
                        │
   Phase 1: Hardening   │   Phase 2: TUI
   (unwraps, startup,   │   (scrollback, dialogs,
    crate consolidation) │    themes, diffs)
                        │
LOW EFFORT ─────────────┼──────────────── HIGH EFFORT
                        │
   Phase 3: Tools       │   Phase 4: Agent
   (PTY, modular,       │   (skills, subagents,
    deferred loading)   │    memory extraction)
                        │
                    LOW IMPACT
```

## Execution Order

```
Week 1-2:  Phase 1 (Hardening) — unwrap audit, error messages, crate merge, startup
Week 2-4:  Phase 2 (TUI) — scrollback, diffs, dialogs, themes
Week 3-5:  Phase 3 (Tools) — PTY, modularize, deferred loading, denial tracking
Week 4-6:  Phase 4 (Agent) — skills, subagents, LSP feedback, memory
Week 5-7:  Phase 5 (Hooks) — lifecycle hooks, MCP plugins, markdown agents
Week 6-8:  Phase 6 (Testing) — E2E, provider compat, polish

Phases overlap — start Phase 2 while finishing Phase 1, etc.
```

---

## What NOT To Do (Updated)

1. **Don't rewrite the TUI framework.** ratatui is capable enough. The gap is in what we render, not how we render it. Don't switch to Solid.js/@opentui (wrong language) or cursive (less capable).

2. **Don't build a custom plugin API.** MCP IS the plugin system. Invest in MCP ergonomics, not a parallel plugin architecture.

3. **Don't add OAuth/IAM auth flows.** API keys work for 95% of users. OAuth is complexity for minimal gain. Revisit if enterprise customers demand it.

4. **Don't implement ML-based permission classification.** Rule-based with cascading approval is good enough. Claude Code's ML classifier is only for bash commands — not worth the investment for us.

5. **Don't chase feature parity with Claude Code's enterprise features** (MDM settings, managed policies, team sync, XAA auth). These are Anthropic-specific. Focus on individual developer experience.

6. **Don't add more crates.** Every new module goes into an existing crate. The next crate added should be the one that replaces three.

7. **Don't invest in coordinator/team mode yet.** Get single-agent subagent spawning right first. Multi-agent coordination is a v4 feature.

---

## The Thesis (Updated)

v2 proved the architecture works. MCP is wired, multi-provider is real, tools are solid. The remaining gap is not about capabilities — it's about:

1. **Trust** — 707 unwraps and cryptic errors make users doubt the tool. Fix the production quality.
2. **Feel** — The TUI is where users spend 100% of their time. Make it feel as good as Claude Code's terminal experience.
3. **Extensibility** — Skills and markdown agents let users customize without recompiling. This is how OpenCode and Claude Code build ecosystems.
4. **Polish** — Startup speed, responsive rendering, clear errors, denial tracking. The details that separate "works" from "works well."

The goal: a user should be able to install CodingBuddy, connect any model (DeepSeek, Claude, GPT-4o, Gemini, local Ollama), and have an experience that feels as polished as Claude Code — with the advantage of model freedom and no vendor lock-in.

**Current: 7.5/10. Target: 9/10. Gap: quality + polish + extensibility.**
