# Provider Equalization Design

**Date:** 2026-08-09
**Status:** Approved
**Goal:** Make CodingBuddy a production-ready agentic CLI that works E2E with any provider — Anthropic, OpenAI, Google, OpenRouter, and others — without DeepSeek-first bias. Add a guided first-time setup wizard.

## Context

CodingBuddy is an 18-crate Rust workspace (~86K lines) with a ratatui TUI, 14 provider kinds, and a sophisticated tool-use agent loop. However:

- The entire codebase has a DeepSeek-first bias: model aliases, profile validation, default endpoints, error messages, and FIM support all assume DeepSeek is the primary provider
- There is no native OpenAI provider module — it uses a generic `OpenAiCompatible` passthrough
- Many features from v4/v5/v6 (coordinator mode, session branching, modals) were coded but never verified E2E against real LLMs
- There is no first-time setup wizard — new users get dropped into a raw TUI with a confusing API key error
- The LLM client (`lib.rs`) is 4,449 lines with giant match arms special-casing DeepSeek

## Architecture Changes

### File-level impact

**Heavily modified (6 files):**
- `crates/codingbuddy-core/src/lib.rs` — remove DeepSeek aliases and constants
- `crates/codingbuddy-core/src/llm_capabilities.rs` — remove DeepSeek special cases
- `crates/codingbuddy-llm/src/lib.rs` — split into `client.rs`, `payload.rs`, `streaming.rs`
- `crates/codingbuddy-llm/src/providers/mod.rs` — add registry, wire new providers
- `crates/codingbuddy-llm/src/protocol.rs` — no provider-hardcoding in defaults
- `crates/codingbuddy-cli/src/commands/chat.rs` — wire setup wizard, runtime switching

**New files (3):**
- `crates/codingbuddy-llm/src/client.rs` — thin `LlmClient` impl, routing, auth
- `crates/codingbuddy-llm/src/providers/openai.rs` — native OpenAI payloads and parsing
- `crates/codingbuddy-llm/src/providers/catalog.rs` — static provider handler registry

**Untouched:** All other crates (~80 files). TUI, tools, agent, policy, store, MCP,
hooks — no changes needed.

### Provider Registry Pattern

The key architectural change. The current `build_chat_payload()` in `lib.rs` is a 500-line
function with giant match arms. The replacement:

```rust
struct ProviderHandler {
    build: fn(&ChatRequest, &Capabilities) -> Result<PreparedPayload>,
    parse: fn(&str) -> Result<LlmResponse>,
    parse_stream: fn(&Value, &mut NativeStreamState, cb: &StreamCallback) -> bool,
    endpoint: fn(&ProviderConfig, &str, bool) -> String,
    auth: AuthStrategy,
    protocol: ChatProtocol,
}

fn handler_for(kind: ProviderKind, protocol: ChatProtocol) -> &'static ProviderHandler {
    match (kind, protocol) {
        (ProviderKind::Anthropic, _) => &ANTHROPIC_HANDLER,
        (ProviderKind::Google, _) => &GOOGLE_HANDLER,
        (ProviderKind::OpenAiCompatible, ChatProtocol::OpenAiChat) => &OPENAI_HANDLER,
        _ => &OPENAI_COMPAT_HANDLER,  // Groq, OpenRouter, xAI, Together, Ollama, etc.
    }
}
```

Each handler is a `const` struct in its provider file. Adding a new provider = one file,
4-5 functions, one match arm. No changes to client code.

## Implementation Phases

### Phase 1: Remove DeepSeek Bias from Core Types

**Goal:** `codingbuddy-core` becomes provider-agnostic. No provider is "special."

1. Remove `normalize_codingbuddy_model()` and `normalize_codingbuddy_profile()` — the generic
   `resolve_model_spec("provider/model-id")` already exists and handles this
2. Remove DeepSeek-specific constants (`CODINGBUDDY_CHAT_MAX_OUTPUT_TOKENS`,
   `CODINGBUDDY_REASONER_MAX_OUTPUT_TOKENS`, `CODINGBUDDY_CHAT_THINKING_MAX_OUTPUT_TOKENS`)
3. Replace with `max_output_tokens_for_model(provider, model, thinking_enabled)` that
   does a capability lookup instead of hardcoded constants
4. Default endpoint becomes empty string — each provider must have explicit `base_url`
5. Remove `v3_2` profile validation (only profile that validated)
6. Remove DeepSeek-only model alias logic

### Phase 2: Split the Monolithic LLM Client

**Goal:** `lib.rs` goes from 4,449 lines to a thin re-export module.

1. Extract `ApiClient` struct + `LlmClient` trait impl into `client.rs`
2. Extract payload building functions into `payload.rs`:
   - `build_openai_compat_payload()` — generic OpenAI-compatible JSON
   - `build_chat_payload()` — routes to handler instead of match arms
3. Extract streaming SSE parsing into `streaming.rs`:
   - OpenAI-compatible SSE: `data:` lines, `[DONE]` sentinel
   - Native SSE: delegates to provider handler
4. `lib.rs` becomes: `pub mod client; pub mod payload; pub mod streaming; pub mod providers; pub mod protocol; pub mod retry; pub mod model_catalog;`
5. `retry.rs` already extracted — stays as-is

### Phase 3: Add Native OpenAI Provider

**Goal:** First-class OpenAI support — no longer just generic OpenAI-compatible.

Create `providers/openai.rs`:

- `OPENAI_HANDLER` — registers for `ProviderKind::OpenAiCompatible` with explicit `openai` provider config
- `build_payload()` — handles:
  - o1/o3 `reasoning_effort` parameter (different from `thinking` config)
  - `response_format: { type: "json_schema", ... }` for structured outputs
  - `prediction` parameter for faster/cheaper outputs
  - Streaming via `stream_options: { include_usage: true }`
- `parse_response()` — extracts `choices[0].message.{content, tool_calls, refusal}`
- `parse_streaming_chunk()` — handles `choices[0].delta.{content, tool_calls}`
- `endpoint()` — `/v1/chat/completions`
- Auth: `Bearer` with `Authorization: Bearer $OPENAI_API_KEY`

Differentiation from `OpenAiCompatible` (generic):
- The generic path strips unknown parameters to avoid 400s from non-OpenAI servers
- The native OpenAI path sends the full payload with all OpenAI-specific knobs

### Phase 4: Provider Registry + Equal Routing

**Goal:** Zero special-casing. Provider dispatch is a single table lookup.

1. Create `providers/catalog.rs` — static `PROVIDER_REGISTRY: &[(ProviderKind, ChatProtocol, &ProviderHandler)]`
2. Refactor `build_chat_payload()` — `handler_for(kind, protocol).build(req, caps)` instead of match
3. Refactor `complete_chat_inner()` — `handler_for().parse(body)` instead of match
4. Refactor `complete_chat_streaming_inner()` — `handler_for().parse_stream()` instead of match
5. Refactor `resolved_endpoint()` — generic: `{base_url}{/v1?}/chat/completions`
6. Remove ALL `if provider == ProviderKind::Deepseek` branches
7. Remove FIM-only DeepSeek paths (FIM stays but routes through handler lookup)

### Phase 5: First-Time Setup Wizard

**Goal:** New users get a guided onboarding.

1. Add `commands/setup.rs` with:
   - Auto-detect available API keys from env vars (walks `KNOWN_PROVIDER_ENV_VARS`)
   - Present detected providers as numbered options
   - Let user pick provider → pick model → test connection → save config
   - Show "You're all set!" with keybindings reference
2. Modify `run_chat()`: if no config exists or no API key detected, route to setup
3. Add runtime provider switching:
   - `/provider <name>` — recreates `ApiClient` with new provider config, swaps into engine
   - `/model <id>` — switches model within current provider
   - Existing model picker (Alt+P) now actually works for switching

### Phase 6: E2E Verification

**Goal:** Prove each provider works against a real API.

Manual verification checklist:
- [ ] Anthropic: streaming chat with tool calls (claude-sonnet-4-5)
- [ ] OpenAI: streaming chat with tool calls (gpt-4o)
- [ ] OpenAI: o1 reasoning mode (gpt-o3)
- [ ] Google: streaming chat with tool calls (gemini-2.5-pro)
- [ ] OpenRouter: multi-model routing
- [ ] Groq: fast inference via OpenAI-compatible path
- [ ] First-time setup wizard: clean config → guided setup → working chat
- [ ] Runtime model switching: start with one model → `/model` switch → continue chatting
- [ ] Runtime provider switching: `/provider anthropic` → `/provider openai` → works
- [ ] Model picker displays correct protocol/auth/capability badges

## First-Time Setup UX

### The current experience

```
$ codingbuddy
[shark ASCII logo]
❯ Error: DEEPSEEK_API_KEY not set and llm.api_key is empty
```

### The target experience

```
$ codingbuddy
╔══════════════════════════════════════════╗
║         Welcome to CodingBuddy!         ║
║       AI coding agent for terminal      ║
╚══════════════════════════════════════════╝

Detected API keys:
  ✓ Anthropic (ANTHROPIC_API_KEY)
  ✓ OpenAI (OPENAI_API_KEY)
  - Google (GOOGLE_API_KEY) — not set

Which provider? [1] Anthropic [2] OpenAI [3] Enter key manually [4] Skip

> 1

Select model: [1] claude-sonnet-4-5 (fast, capable)
              [2] claude-opus-4 (most capable)
              [3] claude-haiku-4-5 (fastest, cheapest)

> 1

Testing connection... ✓ Connected (latency: 234ms)

Config saved. You're all set!
Type /help for commands, F1 for keyboard shortcuts.
```

## Verification

### Build verification
```bash
cargo build --release --bin codingbuddy
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### Runtime verification
```bash
# First-time setup
ANTHROPIC_API_KEY=... codingbuddy  # Should trigger setup wizard

# Direct chat
codingbuddy --provider anthropic --model claude-sonnet-4-5 "Write a Rust function"

# Provider switching at runtime
codingbuddy
> /provider openai
> /model gpt-4o
> Write a Python script

# Non-interactive mode
codingbuddy "Explain this code" --print
```

### What we explicitly skip
- BedrockConverse protocol execution (adapter metadata exists, runtime disabled — enterprise)
- OpenAiResponses protocol execution (same — enterprise)
- MCP OAuth, desktop GUI, remote execution, team collaboration
- Local ML features (feature-gated, not in critical path)
- Full integration test suite (deferred — needs real API keys in CI)
