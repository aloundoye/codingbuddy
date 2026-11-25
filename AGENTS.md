# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

## Build, Test, and Lint

```bash
cargo fmt --all -- --check          # format check
cargo clippy --workspace --all-targets -- -D warnings  # lint
cargo test --workspace --all-targets  # all tests
cargo build --workspace             # debug build
cargo build --release --bin codingbuddy # release binary
```

Run a single test:
```bash
cargo test -p codingbuddy-agent test_name
cargo test -p codingbuddy-agent --test tool_use_default test_name  # integration test file
```

CI also runs conformance scripts (Linux only):
```bash
bash scripts/parity_regression_check.sh
bash scripts/runtime_conformance_scan.sh
```

## Workspace Overview

17-crate Rust workspace. Edition 2024, rust-version 1.94.1, resolver v2.

**Core execution crates:**
- `codingbuddy-cli` — CLI entry point, clap dispatch, 29 subcommand handlers in `src/commands/`
- `codingbuddy-agent` — Agent engine, tool-use loop, complexity classifier, prompt construction, team mode, phase loop. Also hosts context enrichment (`src/context/`, `gather_context.rs`, `repo_map.rs`), skill discovery (`src/skills.rs`) and the run log (`src/observe.rs`) — these are modules, not separate crates
- `codingbuddy-core` — Shared types: `AppConfig`, `ChatRequest`, `LlmResponse`, `StreamChunk`, `TaskPhase`, `SessionState`, `EventEnvelope`
- `codingbuddy-llm` — LLM client implementing `LlmClient` trait, streaming, prompt cache, cached API key resolution

**Tool/policy crates:**
- `codingbuddy-tools` — Tool definitions, plugin manager, shell runner, sandbox wrapping
- `codingbuddy-policy` — Permission engine (denylist/allowlist), approval gates, output scanner, `ManagedSettings`, default deny rules, `BypassPermissions` mode
- `codingbuddy-hooks` — 17 lifecycle events (`HookEvent`), `HookRuntime`, once/disabled fields, `PermissionDecision`

**ML/Intelligence crates:**
- `codingbuddy-lsp` — Post-edit validation: runs language-specific checks (`cargo check`, `tsc`, `py_compile`, `go vet`) after file edits and feeds parsed diagnostics back to the LLM for self-correction.

**Infrastructure crates:**
- `codingbuddy-store` — Session persistence (SQLite events and projections, legacy JSONL archive)
- `codingbuddy-memory` — Long-term memory, shadow commits, checkpoints
- `codingbuddy-index` — Full-text code index (Tantivy), RAG retrieval
- `codingbuddy-mcp` — MCP server management (JSON-RPC stdio/http), `list_prompts`, tool discovery
- `codingbuddy-ui` — TUI rendering (ratatui/crossterm), autocomplete, vim mode, history suggestions
- `codingbuddy-diff` — Unified diff parsing, patch staging, git-apply
- `codingbuddy-subagent` — Background tasks, worktree isolation, custom agent definitions
- `codingbuddy-jsonrpc` — JSON-RPC server for IDE integration
- `codingbuddy-testkit` — Test utilities

## Architecture: Tool-Use Loop

The default and primary execution path for all chat modes (`Code`, `Ask`, `Context`):

```
User → LLM (with tools) → Tool calls → Execute → Results → LLM → ... → Done
```

Key files:
- `codingbuddy-agent/src/lib.rs` — `AgentEngine`, `ChatMode` routing, skill/subagent wiring, clone-then-call stream callback
- `codingbuddy-agent/src/tool_loop/mod.rs` — `ToolUseLoop::run()`, tool execution, security scanning, hook dispatch, phase transitions, LSP validation wiring
- `codingbuddy-agent/src/tool_loop/phases.rs` — `TaskPhase` tool filtering (`EXPLORE_TOOLS`, `VERIFY_TOOLS`), phase transition detection, transition messages
- `codingbuddy-agent/src/tool_bridge.rs` — Converts between LLM tool calls and internal `ToolCall`/`ToolResult`
- `codingbuddy-agent/src/complexity.rs` — Simple decision-tree classifier (Simple/Medium/Complex), evidence-driven budget escalation with de-escalation
- `codingbuddy-agent/src/agent_profiles.rs` — Agent profiles (build/explore/plan), profile selection, tool filtering
- `codingbuddy-agent/src/prompts.rs` — Per-model system prompts (chat/reasoner/qwen/gemini) + complexity-based planning injection

The loop uses `PolicyEngine` + `LocalToolHost` for approval-gated tool execution. `StreamChunk` variants flow events (ContentDelta, ReasoningDelta, ToolCallStart/End, SecurityWarning, PhaseTransition, ModelChanged, Done) to the UI layer.

**Intelligence layers (wired end-to-end):**
- **Agent profiles**: `select_profile()` chooses build/explore/plan profile based on `ChatMode` + prompt keywords. `filter_by_profile()` applies allowlist/blocklist to tool definitions. MCP tools always pass through. Reduces the model's decision space from 39+ tools to task-relevant subset.
- **Doom loop detection**: `DoomLoopTracker` maintains rolling window of 10 `(tool_name, args_hash)` pairs. 3+ identical hashes → injects `DOOM_LOOP_GUIDANCE` system message ("STOP — try a DIFFERENT approach"). Resets when model uses different tool.
- **Per-model system prompts**: `build_model_aware_system_prompt()` selects prompt by model family: DeepSeek chat → `CHAT_SYSTEM_PROMPT` (action-biased), DeepSeek reasoner → `REASONER_SYSTEM_PROMPT` (thinking-leveraging), Qwen → `QWEN_SYSTEM_PROMPT` (concise, 1-4 lines emphasis), Gemini → `GEMINI_SYSTEM_PROMPT` (detailed methodology). All prompts include anti-hallucination, tool-result grounding, anti-parrot directives.
- **Explicit phase loop**: Complex tasks follow Explore→Plan→Execute→Verify phases (`TaskPhase` enum in `codingbuddy-core`). Tool filtering per phase: Explore allows read-only tools only, Verify allows read-only + `bash_run`. Phase transitions are automatic (read-only call count, plan keywords, edit count). Simple/Medium tasks bypass phases entirely. Emits `StreamChunk::PhaseTransition`.
- **Post-edit LSP validation**: `EditValidator` (in `codingbuddy-lsp`) runs after `fs_edit`/`fs_write` tool calls. Checks `.rs` (cargo check), `.ts/.tsx` (tsc), `.py` (py_compile), `.go` (go vet). Parsed `Diagnostic` messages appended to tool result so the LLM sees errors immediately. Graceful skip when toolchain unavailable. Configurable per-language via `LspConfig`.
- **Bootstrap context**: `gather_context` + `ContextManager` dependency analysis injected as System message on turn 1 (~10% of context window). Uses natural language headers (`Directory structure:`, `Git status:`, `Build manifests:`, etc.) to prevent models from parroting internal labels. Model starts with project awareness.
- **LLM compaction**: `build_compaction_summary_with_llm()` sends conversation to LLM with structured template (Goal/Completed/In Progress/Key Facts Established/Key Findings/Modified Files). "Key Facts Established" preserves user preferences, decisions, and corrections across compaction. Falls back to code-based `build_compaction_summary()` on error. Two-phase: prune at 80%, compact at 95%.
- **Step snapshots**: `StepSnapshot` captures before/after `FileSnapshot` (content hash + preview) per tool call. `revert_to_snapshot()` for fine-grained undo. Persisted as JSON in `runtime_dir/snapshots/`. Streams `StreamChunk::SnapshotRecorded`.
- **3-tier planning injection**: Complex → full planning protocol (Explore→Plan→Execute + anti-patterns), Medium → lightweight guidance, Simple → no injection. Repo map included for Complex.
- **Model routing**: Complex + escalated tasks route to `deepseek-reasoner` (native 64K thinking). De-escalates back to `deepseek-chat` after 3 consecutive successes.
- **Error recovery**: First escalation injects ERROR RECOVERY guidance. Same error 3+ times triggers STUCK DETECTION with alternative approach suggestions.
- **Privacy filtering**: `apply_privacy_to_output()` filters tool results before LLM sees them (when privacy.enabled is true).
- **MCP auto-injection**: MCP-discovered tools are merged into the tool definitions before the tool loop starts. Tool names use `mcp__{server_id}__{tool_name}` convention. Dispatch goes through an `McpExecutor` callback on `LocalToolHost` to avoid circular crate dependencies.

## Architecture: Tool Loop Intelligence

Production-ready enhancements in the tool-use loop (`codingbuddy-agent/src/tool_loop/mod.rs`):

- **JSON schema validation**: Tool arguments validated against `jsonschema` before execution. Field-level error messages for self-correction.
- **Tool repair middleware**: `repair_tool_call()` in `tool_bridge.rs` — lowercase, `_`/`-` normalization, Levenshtein fuzzy matching (≤2). Invalid tools get helpful error listing available tools.
- **Parallel tool execution**: Independent read-only tool calls detected and executed concurrently.
- **Tool result caching**: `ToolResultCache` with 60s TTL for read-only tools (`fs.read`, `fs.glob`, `fs.grep`). Path-based invalidation when write tools modify cached paths.
- **Dynamic tool filtering**: Tools filtered by context — `core` always, `specialized` added when matching workspace file types.
- **Two-phase context compaction**: Phase 1 prune at 80% (truncate old tool outputs >3 turns), Phase 2 compact at 95% (LLM-based structured summary with code-based fallback).
- **Cost tracking**: `CostTracker` with per-model pricing, cache-hit discount (90%), budget enforcement (`max_budget_usd`). Emits `StreamChunk::UsageUpdate`.
- **Circuit breaker**: `CircuitBreakerState` — same tool fails 3x → disabled for 2 turns with LLM notification.
- **Doom loop detection**: `DoomLoopTracker` — same tool+args hash 3x in rolling window of 10 → injects corrective guidance. Complements circuit breaker (catches "succeeding but repeating" loops).
- **Agent profiles**: `AgentProfile` with allowlist/blocklist per task type. `select_profile()` auto-selects based on `ChatMode` + prompt keywords. `filter_by_profile()` applied before tool loop starts.
- **Per-model system prompts**: Four prompt variants — `CHAT_SYSTEM_PROMPT` (DeepSeek chat), `REASONER_SYSTEM_PROMPT` (DeepSeek reasoner), `QWEN_SYSTEM_PROMPT` (concise), `GEMINI_SYSTEM_PROMPT` (detailed methodology). Selected by model family in `build_model_aware_system_prompt()`.
- **Post-edit validation**: `EditValidator` from `codingbuddy-lsp` crate. Wired via `edit_validator` on `ToolLoopConfig`. Runs after `fs_edit`/`fs_write`, appends diagnostics to tool result.
- **Phase loop**: `TaskPhase` (Explore→Plan→Execute→Verify) for Complex tasks. Per-phase tool filtering in `phases.rs`. Auto-transitions on read-only call count, plan keywords, edit count. `StreamChunk::PhaseTransition` for UI notification.
- **Step snapshots**: `StepSnapshot` with before/after `FileSnapshot` per tool call. SHA-256 content hashing, preview (50 lines), revert support. `StreamChunk::SnapshotRecorded` for UI notification.

## Architecture: Security & Policy

- **Path canonicalization**: `PolicyEngine.workspace_root` + `std::fs::canonicalize()` for symlink escape detection.
- **Command injection hardening**: `contains_forbidden_shell_tokens()` detects process substitution `<(`, `>(`, here-strings `<<<`, background `&`. `has_redirection_operator()` detects `>`, `>>`, `<` outside quoted strings.
- **Default deny rules**: `default_deny_rules()` returns 8 built-in safety rules prepended before user rules in `PolicyEngine::new()`. Denies `rm -rf *`, `node_modules/*` edits; asks confirmation for `git push --force*`, `.env` edits, `DROP TABLE`, `git reset --hard`, `chmod 777`, `curl | sh` patterns.
- **BypassPermissions mode**: `PermissionMode::BypassPermissions` skips all approval checks. Excluded from normal mode cycling (must be set explicitly). Cycles back to `Ask` when rotated.
- **MCP connection pooling**: `McpConnectionPool` — reuses stdio server connections, health checks via `try_wait()`, 5-minute idle timeout, graceful shutdown.

## Architecture: Streaming & UX

- **CancellationToken**: `Arc<AtomicBool>` in `codingbuddy-core`. `ApiClient.set_cancel_token()` — checked between SSE reads, returns partial response with `reason: "cancelled"`.
- **StreamChunk::UsageUpdate**: Real-time cost/progress tracking — `input_tokens`, `output_tokens`, `cache_hit_tokens`, `estimated_cost_usd`. JSON-serialized via `stream_chunk_to_event_json()`.
- **StreamChunk::PhaseTransition**: Emitted when the explicit phase loop transitions (e.g. Explore→Plan). Contains `from`/`to` phase names.
- **StreamChunk::ModelChanged**: Emitted when the active model changes mid-session (via `/model` command). Contains new model name.
- **Session GC**: `Store.gc(archive_days, delete_days)` — archives sessions >30 days, deletes >90 days. `storage_bytes()` for size monitoring.

## DeepSeek API

Only two models: `deepseek-chat` (non-thinking) and `deepseek-reasoner` (thinking/extended).
- V3.2: `deepseek-reasoner` supports tool calls — no need to block tools
- `deepseek-reasoner` thinks natively — do NOT send `ThinkingConfig` (that's for `deepseek-chat` only)
- `deepseek-reasoner` rejects `tool_choice` — dual defense: stripped in `tool_loop.rs` (sets `ToolChoice::auto()`) AND in `build_chat_payload()` (safety net checks model name). Both are required.
- Thinking mode is incompatible with: temperature, top_p, presence_penalty, frequency_penalty (strip them)
- Output token limits: chat 8K (no thinking), chat 32K (with thinking), reasoner 64K
- `reasoning_content`: keep during tool-call loops, clear from prior turns on new user question
- `ApiClient` caches the API key at construction (`resolve_key_from_config()`) to avoid env var races during logout. `resolve_api_key()` prefers cached key, falls back to live env+config check. `build_chat_payload()` returns `Result<Value>` (not `Value`) — serialization errors propagate instead of silently producing `null`.
- DeepSeek uses automatic server-side prefix caching — no client-side `cache_control` annotations
- Base URL: `api.deepseek.com` with `/v1` (OpenAI-compatible) and `/beta` (strict tools, FIM) paths
- API is stateless — every request includes full conversation history

## Test Patterns

**ScriptedToolLlm**: The standard mock for testing tool-use loops. A `VecDeque<LlmResponse>` that pops responses in order. Defined per-file (not in testkit) because each test needs slightly different response shapes.

```rust
struct ScriptedToolLlm { responses: Mutex<VecDeque<LlmResponse>> }
impl LlmClient for ScriptedToolLlm { /* pop_front on each call */ }
```

**Integration tests** live in `crates/codingbuddy-agent/tests/`:
- `tool_use_default.rs` (40 tests) — core tool-use loop behavior
- `privacy_wiring.rs` — privacy filtering and keyword search tool registration
- `runtime_conformance.rs`, `analysis_bootstrap.rs`, `team_orchestration.rs`, `functional_verification.rs`, `real_file_operations.rs`, `feature_coverage.rs`, `coding_quality_benchmark.rs`

**Provider cassettes**: `crates/codingbuddy-testkit/cassettes/<provider>/<scenario>.json` hold provider HTTP traffic (real recordings, or `"source": "synthetic"` stand-ins). `tests/provider_replay.rs` replays every cassette through the real `ApiClient` + `AgentEngine` and checks request count/paths, tool sequence, final answer, and that the on-screen answer after the last tool call equals the output. `codingbuddy_testkit::cassette` has `ReplayServer` and `RecordingProxy`. Record with `record_cassettes` (ignored, needs `CODINGBUDDY_RECORD=1`); see `cassettes/README.md`. A failing cassette means behaviour changed on identical provider bytes.

**Unit tests**: Most crates use `#[cfg(test)] mod tests` at the bottom of `lib.rs`.

**Environment safety**: Rust 2024 requires `unsafe` for `std::env::set_var`/`remove_var`. Tests that manipulate env vars must use `unsafe {}` blocks and avoid parallel access (combine related env tests or check return types rather than exact values).

## Logging & Diagnostics

- `codingbuddy_core::logging` owns process diagnostics. `init_logging()` is called once in `main` (`init_diagnostics`). Output goes **only to files** — never stdout/stderr, which would corrupt the TUI and `--json` output. Location: `CODINGBUDDY_LOG_DIR`, else `~/.codingbuddy/logs/codingbuddy.YYYY-MM-DD.log` (daily rotation, 7 kept).
- Level: `CODINGBUDDY_LOG` (full `EnvFilter` syntax) > `--debug [categories]` (`--debug mcp,llm` → `codingbuddy_mcp=debug,codingbuddy_llm=debug`) > `info`. Noisy deps (aws, hyper, rustls, reqwest, tantivy) are capped at `warn`.
- Use `tracing::{info,warn,debug}!` in library code, not `eprintln!`. Never log prompts, responses, tool arguments or keys — log names, ids, counts, timings. Existing spans: `turn` (tool loop), `tool{name,call_id}` (tool execution); request outcomes under `codingbuddy_llm::request`, retries under `codingbuddy_llm::retry`; MCP calls and hooks log their own outcome.
- `install_crash_handler()` writes `crash-<ts>-<pid>.log` (panic, backtrace, last 200 log lines) next to the logs for every panic, including caught worker panics. A full-screen UI holds `claim_terminal()` so background-thread panics don't print over it; the TUI's own hook restores the terminal only for panics on the TUI thread.
- `codingbuddy doctor` reports the log file and last crash; `doctor --bundle` writes a redacted `.tar.gz` (doctor payload, config, recent logs, crash reports) into the log directory. Redaction: secret-looking field names plus `OutputScanner`.
- `CODINGBUDDY_DEBUG_PANIC=1` forces a panic at startup to verify crash reporting.

## Configuration Precedence

Settings merge in order (later wins):
1. `.codingbuddy/config.toml` (legacy)
2. `~/.codingbuddy/settings.json` (user)
3. `.codingbuddy/settings.json` (project)
4. `.codingbuddy/settings.local.json` (local overrides)

`ManagedSettings` (team/enterprise) can cap `max_turns`, force `permission_mode`, and disable bypass.

## Key Conventions

- `StreamChunk` enum is the universal event type (22 variants including `PhaseTransition` and `ModelChanged`). When adding variants, update all match sites in `codingbuddy-cli/src/commands/chat.rs` (JSON events, non-JSON streaming, TUI callback, text mode) and `stream_chunk_to_event_json()` in `codingbuddy-core/src/lib.rs`.
- Tool definitions in `codingbuddy-tools/src/lib.rs` use enriched descriptions (200-500 words of behavioral instructions per tool).
- Anti-hallucination: per-question `tool_choice=required` (resets each turn, stripped for reasoner), nudge on long answers (>150 chars) only when no tool has run in the current turn (up to 3 attempts; earlier turns' tool use does not exempt a new question — a long answer after this turn's tool results is the grounded answer and must not be discarded), structural file-reference validation with 30+ extensions (threshold: 1+ unverified refs), shell command pattern detection (`contains_shell_command_pattern()`).
- Thinking budgets: Simple 8K / Medium 16K / Complex 32K initial. Evidence-driven escalation on failures (+8K per failure, capped at 64K). De-escalation after 3 consecutive successes.
- Model routing: Complex + escalated → `deepseek-reasoner` (native thinking, no ThinkingConfig). De-escalation returns to `deepseek-chat`. Temperature and tool_choice stripped for reasoner.
- Error recovery: First escalation injects guidance. 3+ identical errors trigger stuck detection. Recovery state resets on success.
- Bootstrap context: `initial_context` field on `ToolLoopConfig`. Populated by `build_bootstrap_context()` using `gather_context` + `ContextManager`.
- Compaction: Primary: `build_compaction_summary_with_llm()` (structured LLM-based, template: Goal/Completed/InProgress/Key Facts Established/Findings/ModifiedFiles). Fallback: `build_compaction_summary()` (code-based, extracts files/errors/decisions). `PreCompact` hook fires before compaction.
- Multi-provider: `ProviderConfig` + `ProviderModels` structs in `codingbuddy-core`. `LlmConfig.providers` map with `active_provider()` method (falls back to legacy fields). DeepSeek is the default provider; any OpenAI-compatible endpoint works (GLM-5, Qwen, Ollama, OpenRouter, etc.). `--model` flag on `ChatArgs` for per-session override.
- Prompt hardening: All four system prompts (chat/reasoner/qwen/gemini) enforce anti-hallucination directives, tool-result grounding ("trust tool results over expectations"), anti-parrot for bootstrap context. Chat/reasoner enforce 200-word limit. Qwen emphasizes extreme conciseness (1-4 lines). Gemini emphasizes methodical exploration. Shell commands in text output explicitly forbidden.
- MCP tools: Auto-discovered and injected into tool definitions. Named `mcp__{server_id}__{tool_name}`. Dispatch via `McpExecutor` callback on `LocalToolHost`. Tool host splits on `__` to route to the correct MCP server.
- `gen` is a reserved keyword in Rust 2024 edition — use `generator` or `backend` instead.
- Deleted commands (no longer exist): `leadership`, `visual`, `teleport`, `remote_env`, `intelligence`, `profile`, `benchmark`.
