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

Build with local ML (Candle backends for real embeddings/completion):
```bash
cargo build --release --bin codingbuddy --features local-ml
```

CI also runs conformance scripts (Linux only):
```bash
bash scripts/parity_regression_check.sh
bash scripts/runtime_conformance_scan.sh
```

## Workspace Overview

20-crate Rust workspace. Edition 2024, rust-version 1.93, resolver v2.

**Core execution crates:**
- `codingbuddy-cli` — CLI entry point, clap dispatch, 24 subcommand handlers in `src/commands/`
- `codingbuddy-agent` — Agent engine, tool-use loop, complexity classifier, prompt construction, team mode, phase loop
- `codingbuddy-core` — Shared types: `AppConfig`, `ChatRequest`, `LlmResponse`, `StreamChunk`, `TaskPhase`, `SessionState`, `EventEnvelope`
- `codingbuddy-llm` — LLM client implementing `LlmClient` trait, streaming, prompt cache, cached API key resolution

**Tool/policy crates:**
- `codingbuddy-tools` — Tool definitions, plugin manager, shell runner, sandbox wrapping
- `codingbuddy-policy` — Permission engine (denylist/allowlist), approval gates, output scanner, `ManagedSettings`, default deny rules, `BypassPermissions` mode
- `codingbuddy-hooks` — 14 lifecycle events, `HookRuntime`, once/disabled fields, `PermissionDecision`

**ML/Intelligence crates:**
- `codingbuddy-local-ml` — Local ML via Candle: embeddings, completion, chunking, vector index, hybrid retrieval, privacy router. Memory-aware loading (`LoadStrategy`), parallel downloads, stall detection. Heavy ML deps feature-gated behind `local-ml`.
- `codingbuddy-lsp` — Post-edit validation: runs language-specific checks (`cargo check`, `tsc`, `py_compile`, `go vet`) after file edits and feeds parsed diagnostics back to the LLM for self-correction.

**Infrastructure crates:**
- `codingbuddy-store` — Session persistence (JSONL event log + SQLite projections)
- `codingbuddy-memory` — Long-term memory, shadow commits, checkpoints
- `codingbuddy-index` — Full-text code index (Tantivy), RAG retrieval
- `codingbuddy-mcp` — MCP server management (JSON-RPC stdio/http), `list_prompts`, tool discovery
- `codingbuddy-ui` — TUI rendering (ratatui/crossterm), autocomplete, vim mode, ML ghost text
- `codingbuddy-diff` — Unified diff parsing, patch staging, git-apply
- `codingbuddy-context` — Context enrichment and analysis
- `codingbuddy-skills` — Skill discovery, forked execution, frontmatter parsing
- `codingbuddy-subagent` — Background tasks, worktree isolation, custom agent definitions
- `codingbuddy-observe` — Structured logging
- `codingbuddy-jsonrpc` — JSON-RPC server for IDE integration
- `codingbuddy-testkit` — Test utilities

## Architecture: Tool-Use Loop

The default and primary execution path for all chat modes (`Code`, `Ask`, `Context`):

```
User → LLM (with tools) → Tool calls → Execute → Results → LLM → ... → Done
```

Key files:
- `codingbuddy-agent/src/lib.rs` — `AgentEngine`, `ChatMode` routing, skill/subagent wiring, clone-then-call stream callback
- `codingbuddy-agent/src/tool_loop.rs` — `ToolUseLoop::run()`, tool execution, security scanning, hook dispatch, phase transitions, LSP validation wiring
- `codingbuddy-agent/src/tool_loop/phases.rs` — `TaskPhase` tool filtering (`EXPLORE_TOOLS`, `VERIFY_TOOLS`), phase transition detection, transition messages
- `codingbuddy-agent/src/tool_bridge.rs` — Converts between LLM tool calls and internal `ToolCall`/`ToolResult`
- `codingbuddy-agent/src/complexity.rs` — Simple decision-tree classifier (Simple/Medium/Complex), evidence-driven budget escalation with de-escalation
- `codingbuddy-agent/src/agent_profiles.rs` — Agent profiles (build/explore/plan), profile selection, tool filtering
- `codingbuddy-agent/src/prompts.rs` — Per-model system prompts (chat/reasoner/qwen/gemini) + complexity-based planning injection

The loop uses `PolicyEngine` + `LocalToolHost` for approval-gated tool execution. `StreamChunk` variants flow events (TextDelta, ToolCallStart/End, SecurityWarning, PhaseTransition, ModelChanged, Done) to the UI layer.

**Intelligence layers (wired end-to-end):**
- **Agent profiles**: `select_profile()` chooses build/explore/plan profile based on `ChatMode` + prompt keywords. `filter_by_profile()` applies allowlist/blocklist to tool definitions. MCP tools always pass through. Reduces the model's decision space from 39+ tools to task-relevant subset.
- **Doom loop detection**: `DoomLoopTracker` maintains rolling window of 10 `(tool_name, args_hash)` pairs. 3+ identical hashes → injects `DOOM_LOOP_GUIDANCE` system message ("STOP — try a DIFFERENT approach"). Resets when model uses different tool.
- **Per-model system prompts**: `build_model_aware_system_prompt()` selects prompt by model family: DeepSeek chat → `CHAT_SYSTEM_PROMPT` (action-biased), DeepSeek reasoner → `REASONER_SYSTEM_PROMPT` (thinking-leveraging), Qwen → `QWEN_SYSTEM_PROMPT` (concise, 1-4 lines emphasis), Gemini → `GEMINI_SYSTEM_PROMPT` (detailed methodology). All prompts include anti-hallucination, tool-result grounding, anti-parrot directives.
- **Explicit phase loop**: Complex tasks follow Explore→Plan→Execute→Verify phases (`TaskPhase` enum in `codingbuddy-core`). Tool filtering per phase: Explore allows read-only tools only, Verify allows read-only + `bash_run`. Phase transitions are automatic (read-only call count, plan keywords, edit count). Simple/Medium tasks bypass phases entirely. Emits `StreamChunk::PhaseTransition`.
- **Post-edit LSP validation**: `EditValidator` (in `codingbuddy-lsp`) runs after `fs_edit`/`fs_write` tool calls. Checks `.rs` (cargo check), `.ts/.tsx` (tsc), `.py` (py_compile), `.go` (go vet). Parsed `Diagnostic` messages appended to tool result so the LLM sees errors immediately. Graceful skip when toolchain unavailable. Configurable per-language via `LspConfig`.
- **Bootstrap context**: `gather_context` + `ContextManager` dependency analysis injected as System message on turn 1 (~10% of context window). Uses natural language headers (`Directory structure:`, `Git status:`, `Build manifests:`, etc.) to prevent models from parroting internal labels. Model starts with project awareness.
- **Per-turn retrieval**: `inject_retrieval_context()` fires every turn (not just turn 1). Budget: `remaining_tokens / 5`, skips when < 500 tokens remain. Incremental index updates via SHA-256 change detection.
- **LLM compaction**: `build_compaction_summary_with_llm()` sends conversation to LLM with structured template (Goal/Completed/In Progress/Key Facts Established/Key Findings/Modified Files). "Key Facts Established" preserves user preferences, decisions, and corrections across compaction. Falls back to code-based `build_compaction_summary()` on error. Two-phase: prune at 80%, compact at 95%.
- **Step snapshots**: `StepSnapshot` captures before/after `FileSnapshot` (content hash + preview) per tool call. `revert_to_snapshot()` for fine-grained undo. Persisted as JSON in `runtime_dir/snapshots/`. Streams `StreamChunk::SnapshotRecorded`.
- **3-tier planning injection**: Complex → full planning protocol (Explore→Plan→Execute + anti-patterns), Medium → lightweight guidance, Simple → no injection. Repo map included for Complex.
- **Model routing**: Complex + escalated tasks route to `deepseek-reasoner` (native 64K thinking). De-escalates back to `deepseek-chat` after 3 consecutive successes.
- **Error recovery**: First escalation injects ERROR RECOVERY guidance. Same error 3+ times triggers STUCK DETECTION with alternative approach suggestions.
- **Privacy filtering**: `apply_privacy_to_output()` filters tool results before LLM sees them (when local_ml.privacy enabled).
- **MCP auto-injection**: MCP-discovered tools are merged into the tool definitions before the tool loop starts. Tool names use `mcp__{server_id}__{tool_name}` convention. Dispatch goes through an `McpExecutor` callback on `LocalToolHost` to avoid circular crate dependencies.

## Architecture: Local ML (codingbuddy-local-ml)

Feature-gated hybrid intelligence layer. `cargo test --workspace` works WITHOUT the `local-ml` feature — all tests use mock backends. Feature forwarding chain: `codingbuddy-cli` → `codingbuddy-agent` → `codingbuddy-local-ml` (each crate has `[features] local-ml = [...]` in Cargo.toml).

- **Trait abstractions**: `EmbeddingsBackend`, `LocalGenBackend`, `VectorIndexBackend` — mock implementations always compiled, Candle backends behind `local-ml` feature
- **Chunking**: `chunk_file()` / `chunk_workspace()` / `chunk_workspace_incremental()` with overlapping windows, gitignore-aware, SHA-256 change detection
- **Vector index**: `BruteForceBackend` (O(n) cosine similarity, always compiled) + `UsearchBackend` (HNSW with cosine metric, feature-gated). String chunk_id ↔ u64 key mapping for usearch.
- **Hybrid retrieval**: Reciprocal Rank Fusion (RRF) combining vector + BM25 scores. `HybridRetriever::search()` / `build_index()` / `update_index()` / `new_with_backend()`. Wired via `build_retriever_callback()` in `codingbuddy-agent/src/lib.rs` — uses `CandleEmbeddings` + `UsearchBackend` when `local-ml` enabled, falls back to `MockEmbeddings` + `BruteForceBackend` otherwise.
- **Privacy router**: 3-layer detection (path globs, content regex, builtin secret patterns). Policies: `BlockCloud`, `Redact`, `LocalOnlySummary`. Applied to tool outputs in `execute_tool_call()`.
- **Tool-loop wiring**: Retrieval context injected as System message on every turn (budget: remaining_tokens/5, min 500). Privacy scanning on tool outputs. Bootstrap includes dependency-analysis hub files from `ContextManager`.
- **Ghost text**: ML completion callback wired in `chat.rs`. Model loading runs in a background thread (starts with `MockGenerator`, swaps in `CandleCompletion` when ready) to avoid blocking the TUI main thread. Uses `CandleCompletion` with `local-ml` feature (auto-downloads model via `ModelManager`), falls back to `MockGenerator` without the feature or on any load error. TUI: 200ms debounce, Tab accepts full, Alt+Right accepts word. Priority: ML > history ghost.
- **Model manager**: `ensure_model_with_progress()` for download progress feedback. `list_models()` returns `ModelInfo` with status/cache_path. Parallel file downloads via `std::thread::scope`. 60-second stall detection with automatic retry (`DOWNLOAD_STALL_TIMEOUT_SECS`). Resume from partial state via `PartialDownloadState`.
- **Memory-aware loading**: `LoadStrategy` enum (Full/ReducedContext/CpuOnly/Skip) returned by `determine_load_strategy()` based on `available_memory_mb()` vs model size. Cascades gracefully: reduce context → CPU-only → skip with warning.
- **CandleCompletion**: Multi-token autoregressive generation with cancel flag, timeout, and stop token support. Fixed from single-token to full loop.
- **Model registry**: `model_registry.rs` catalogs supported architectures (`CompletionArchitecture`, `EmbeddingArchitecture`) with `ModelEntry` metadata. `detect_completion_architecture()` / `detect_embedding_architecture()` for auto-detection.
- **Speculative decoding** (deprecated): `speculative.rs` — `verify_draft()` for greedy acceptance of draft tokens. Not production-wired; focus local-ml investment on retrieval/privacy/reranking instead.
- **Reranker**: `RerankerBackend` trait + `MockReranker`. `CandleReranker` (feature-gated) for cross-encoder reranking.
- **Local routing**: `codingbuddy-agent/src/local_routing.rs` — `should_use_local()` routes Simple tasks to local model when enabled. Project-context keywords ("this file", "this code", etc.) always route to API.
- **Model registry convenience**: `default_embedding_model()` returns Jina Code v2, `default_completion_model()` returns DeepSeek Coder 1.3B. Architecture detection functions behind `#[cfg(any(test, feature = "local-ml"))]`.

## Architecture: Tool Loop Intelligence

Production-ready enhancements in the tool-use loop (`codingbuddy-agent/src/tool_loop.rs`):

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
- `tool_use_default.rs` (20 tests) — core tool-use loop behavior
- `retrieval_wiring.rs` (4 tests) — retrieval context injection, privacy router wiring
- `runtime_conformance.rs`, `analysis_bootstrap.rs`, `team_orchestration.rs`

**Unit tests**: Most crates use `#[cfg(test)] mod tests` at the bottom of `lib.rs`.

**Environment safety**: Rust 2024 requires `unsafe` for `std::env::set_var`/`remove_var`. Tests that manipulate env vars must use `unsafe {}` blocks and avoid parallel access (combine related env tests or check return types rather than exact values).

## Configuration Precedence

Settings merge in order (later wins):
1. `.codingbuddy/config.toml` (legacy)
2. `~/.codingbuddy/settings.json` (user)
3. `.codingbuddy/settings.json` (project)
4. `.codingbuddy/settings.local.json` (local overrides)

`ManagedSettings` (team/enterprise) can cap `max_turns`, force `permission_mode`, and disable bypass.

## Key Conventions

- `StreamChunk` enum is the universal event type (17 variants including `PhaseTransition` and `ModelChanged`). When adding variants, update all match sites in `codingbuddy-cli/src/commands/chat.rs` (JSON events, non-JSON streaming, TUI callback, text mode) and `stream_chunk_to_event_json()` in `codingbuddy-core/src/lib.rs`.
- Tool definitions in `codingbuddy-tools/src/lib.rs` use enriched descriptions (200-500 words of behavioral instructions per tool).
- Anti-hallucination: per-question `tool_choice=required` (resets each turn, stripped for reasoner), nudge at 300 chars (up to 3 attempts, fires regardless of prior tool use), structural file-reference validation with 30+ extensions (threshold: 1+ unverified refs), shell command pattern detection (`contains_shell_command_pattern()`).
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
