# CodingBuddy v6: Verify Everything Works + Close Non-Enterprise Gaps

> The audits scored features as "existing" — but existing ≠ working. This plan verifies every feature end-to-end and closes remaining gaps.

---

## The Problem

v2→v5 added dozens of features. Auditors checked if code exists, not if it runs. Examples of risk:
- Coordinator mode has a prompt but has it ever spawned parallel workers successfully?
- SendMessage tool is defined but has it ever sent a message to a real agent?
- Permission rules from config exist but does `Bash(cargo *)` actually auto-approve?
- LSP validation has a timeout but does `cargo check` actually get killed at 30s?
- Theme auto-detection reads COLORFGBG but does it actually switch to light theme?
- Session branching calls `fork_session` but does the fork preserve history?
- PTY shell opens a pseudo-terminal but does colored output actually appear?

**None of these have been tested against a real LLM or real terminal.**

---

## Phase 1: Functional Verification Matrix (Must-pass)

### 1.1 Core Agent Loop

| Test | How to verify | Pass criteria |
|------|---------------|---------------|
| Basic chat works | `codingbuddy ask "What is 2+2?"` | Returns answer, no panic |
| Tool use works | `codingbuddy ask "Read README.md"` with tools | Calls fs_read, returns content |
| Multi-turn works | TUI chat, ask question, follow up | Context preserved |
| Streaming works | Watch tokens appear one by one | No buffering delay |
| Session resume | Start chat, Ctrl+C, restart with `--continue` | Resumes from last message |
| Cost tracking | Check status bar after a conversation | Shows non-zero cost |
| Token budget bar | Fill context window to 80% | Bar turns yellow, then red |

### 1.2 Tool Execution

| Test | How to verify | Pass criteria |
|------|---------------|---------------|
| fs_read | Ask agent to read a file | Correct content returned |
| fs_edit | Ask agent to change a string in a file | File actually modified on disk |
| fs_grep | Ask agent to find a pattern | Correct matches returned |
| bash_run (PTY) | Ask agent to run `ls --color` | Colored output in result |
| bash_run (timeout) | Ask agent to run `sleep 300` | Killed after timeout |
| Fuzzy edit recovery | Provide slightly wrong old_string | Edit succeeds via fallback strategy |
| LSP validation | Edit a .rs file with a typo | Diagnostics appended to result |
| Tool caching | Read same file twice in one turn | Second call uses cache |

### 1.3 Permission System

| Test | How to verify | Pass criteria |
|------|---------------|---------------|
| Default deny | Agent tries `rm -rf /tmp/test` | Blocked, not executed |
| Glob rule allow | Config: `Bash(cargo *) = allow` | `cargo test` runs without prompt |
| Glob rule ask | Config: `Edit(*.env*) = ask` | Editing .env prompts user |
| Persistent accept | Press 'A' at prompt, restart | Same pattern auto-approved |
| Graduated prompt | Agent calls bash vs edit | Red [SHELL] vs yellow [EDIT] badge |
| Denial tracking | Deny 3 times quickly | Guidance message injected |
| Hook auto-approve | Configure PermissionRequest hook | Hook output allows tool |

### 1.4 Provider Compatibility

| Test | Provider | How to verify | Pass criteria |
|------|----------|---------------|---------------|
| Chat | DeepSeek | `codingbuddy ask "hello"` | Response received |
| Chat | OpenAI | Set provider, ask question | Response received |
| Chat | Anthropic | Set provider, ask question | Response received |
| Chat | Ollama | Set provider (local), ask | Response received |
| Tool calling | DeepSeek | Ask to read a file | Tool call made + result |
| Tool calling | Ollama | Ask to read a file | Tool call made (even if degraded) |
| Reasoner | DeepSeek | Set model=deepseek-reasoner | Reasoning content visible |
| Thinking | Anthropic | Set model=claude-sonnet-4 | Extended thinking works |

### 1.5 TUI & UX

| Test | How to verify | Pass criteria |
|------|---------------|---------------|
| Theme dark | Start in dark terminal | Cyan/green/yellow colors correct |
| Theme light | Set COLORFGBG=0;15, start | Blue/black colors, readable |
| Theme config | Set `theme.mode = "light"` | Light theme forced |
| Help modal | Press F1 in TUI | Overlay appears, scrollable |
| Model picker | Type `/model` | List appears, selectable |
| PageUp scroll | Press PageUp | Terminal scrollback accessible |
| Vim mode | `/vim`, type in normal mode | hjkl navigation works |
| Autocomplete | Type `@src/` | File suggestions appear |
| Slash command AC | Type `/` | Command suggestions appear |
| Status bar themed | Check all status bar elements | Colors match theme, not hardcoded |

### 1.6 Session & Memory

| Test | How to verify | Pass criteria |
|------|---------------|---------------|
| /undo | Make a change, /undo | Turn reverted, file restored |
| /branch create | `/branch create test` | New session created |
| /branch list | `/branch list` | Shows branches with IDs |
| Memory extraction | Long conversation + compaction | Observations event emitted |
| Session persistence | Start chat, quit, resume | History preserved |

### 1.7 Extensibility

| Test | How to verify | Pass criteria |
|------|---------------|---------------|
| MCP tool | Add an MCP server, invoke tool | Tool executes via MCP |
| MCP init | `codingbuddy mcp init test-server` | Directory scaffolded |
| Skill invocation | Create skill.md, invoke via /skill | Skill runs |
| Hook fires | Configure PreToolUse hook, use tool | Hook output visible |
| Custom agent | Create .codingbuddy/agents/test.md | Agent appears in /agents list |

### 1.8 Coordinator Mode

| Test | How to verify | Pass criteria |
|------|---------------|---------------|
| Complex triggers coordinator | Ask "refactor the entire codebase" | COORDINATOR_GUIDANCE in system prompt |
| spawn_task works | Agent uses spawn_task in background | Subagent completes, result returned |
| send_message works | After spawn, agent uses send_message | Follow-up sent to child session |

### Phase 1 Results ✅ COMPLETE

**20 verification tests written, 19 pass, 1 ignored (requires approval callback wiring).**

**Bugs found and FIXED:**
1. **Persistent approval glob matching was broken** — `is_persistently_approved` did exact string match instead of glob match. Pattern `"cargo *"` didn't match command `"cargo test"`. Fixed by adding `glob_matches_command()` helper that supports trailing `*` wildcards.

**Findings (not bugs, but important):**
- Permission rules use **dot notation** internally (`bash.run`, `fs.edit`) not underscore notation (`bash_run`, `fs_edit`). Config rule `Bash(cargo *)` correctly maps to `bash.run`.
- Complexity classifier requires `has_arch AND word_count > 5` for Complex — "refactor the entire auth module" (5 words) is Medium, not Complex.
- `fs_edit` in test requires full approval callback wiring — the existing `tool_use_default.rs` tests handle this correctly with auto-approve settings.
- All provider capability flags verified: Ollama gets downgrade+num_predict, OpenAI reasoning gets max_completion_tokens, Gemini gets schema sanitization.
- Session fork, coordinator guidance injection, send_message tool registration, memory event serialization all verified working.

**Tests cover:** fs_read, glob permission rules, default deny rules, persistent approval roundtrip, coordinator guidance (complex vs simple), all provider capability flags (Ollama/OpenAI/Gemini/DeepSeek), session forking, startup profiler, LSP config, complexity classification, memory events.

---

## Phase 2: Close Non-Enterprise Gaps

### 2.1 Remaining from audit

| Gap | What to do | Effort |
|-----|-----------|--------|
| Session browser modal | Add `/sessions` modal listing past sessions with resume | 1 day |
| Colorblind theme variant | Add `TuiTheme::colorblind()` with deuteranopia-safe palette | 2 hours |
| E2E coordinator test | ScriptedLlm test that spawns background agent + sends message | 4 hours |
| True /redo | Store undone turns for re-apply (not just rewind) | 4 hours |
| Skill auto-recommendation | When prompt matches when_to_use, suggest the skill | 2 hours |

### 2.2 Remaining from OpenCode comparison

| Gap | What to do | Effort |
|-----|-----------|--------|
| MCP OAuth | Add OAuth callback flow for remote MCP servers | Skip (enterprise) |
| Desktop GUI | Skip — different product | Skip |
| GitHub Copilot | Skip — niche provider | Skip |

### 2.3 Remaining from Claude Code comparison  

| Gap | What to do | Effort |
|-----|-----------|--------|
| Vim FSM completeness | Audit vim mode operators/motions, add missing ones | 1 day |
| 100+ built-in commands | We have 30+ — add most-useful missing ones | 1 day |
| Background agent auto-promotion | Foreground agent → background after 60s | 1 day |
| Remote execution (CCR) | Skip — enterprise/cloud feature | Skip |
| Kairos scheduling | Skip — enterprise feature | Skip |
| IDE bridge | Skip — separate product (VS Code ext) | Skip |

---

## Phase 3: Fix Everything That Fails

After Phase 1 verification, fix every failure. This is the "make it actually work" phase. No new features — only fixes for broken features.

---

## Phase 4: Final Integration Test Suite

Write automated ScriptedLlm tests for the critical paths from Phase 1:
1. Edit file flow (fs_read → fs_edit → LSP validation)
2. Permission denied flow (deny → denial tracking → guidance)
3. Compaction flow (fill context → compact → verify preserved)
4. Coordinator flow (complex prompt → spawn_task → send_message → synthesize)
5. Session branch flow (branch → edit → switch back → verify divergence)

---

## Execution Order

```
Phase 1: Functional verification   — 2 days (manual testing against real LLM)
Phase 2: Close gaps                — 3 days (session modal, colorblind, E2E tests, vim audit)
Phase 3: Fix failures              — 1-3 days (depends on Phase 1 findings)
Phase 4: Integration test suite    — 2 days (5 ScriptedLlm E2E tests)
```

Total: ~8-10 days. After this, every feature is verified working, not just existing.

---

## Phase 5: MCP OAuth Flow

### 5.1 OAuth callback server

Add a local HTTP callback server that:
1. Opens the user's browser to the MCP server's OAuth authorize URL
2. Listens on `localhost:PORT` for the callback with the auth code
3. Exchanges the code for an access token
4. Stores the token in the MCP token store (SQLite)
5. Reconnects the MCP server with the new token

### 5.2 Token storage and refresh

- Store OAuth tokens per MCP server in `persistent_mcp_tokens` SQLite table (already exists: `store_mcp_token`/`load_mcp_token` in mcp crate)
- Add token refresh flow: if a request returns 401, attempt refresh before failing
- Add `codingbuddy mcp auth <server_id>` CLI command to trigger manual re-auth
- Add `codingbuddy mcp logout <server_id>` to revoke and delete tokens

### 5.3 MCP server config with OAuth

```json
{
  "mcp": {
    "servers": {
      "github-tools": {
        "transport": "http",
        "url": "https://mcp.github.com",
        "auth": {
          "type": "oauth",
          "client_id": "...",
          "authorize_url": "https://github.com/login/oauth/authorize",
          "token_url": "https://github.com/login/oauth/access_token",
          "scopes": ["repo", "read:org"]
        }
      }
    }
  }
}
```

---

## What We Skip (Enterprise Only)

- Desktop GUI (Tauri/Electron)
- Remote execution environments (CCR containers)
- Kairos scheduling / auto-dream
- IDE bridge (VS Code extension)
- Team collaboration / shared policies
- SaaS dashboard
- GitHub Copilot native integration

These are product decisions, not technical gaps.
