# CodingBuddy

An open-source, multi-model AI coding agent for the terminal. Written in Rust.

Connect any LLM — DeepSeek, OpenAI, Anthropic, Google, Groq, Ollama, OpenRouter — and get a production-grade coding assistant with tool execution, planning, code search, and safety guardrails.

## Why CodingBuddy?

- **Any model, any provider.** 7 providers preconfigured. Any OpenAI-compatible endpoint works.
- **Fast.** Rust binary, no runtime dependencies. Sub-100ms startup with profiling.
- **Safe.** 0 production unwraps. Tree-sitter bash AST analysis, 4-stage permission system, default deny rules.
- **Smart.** 7-strategy fuzzy editing, coordinator mode for parallel agents, per-turn RAG, doom loop detection.
- **Extensible.** MCP servers, 12 lifecycle hooks, custom skills with model/effort metadata, SendMessage for inter-agent communication.

## Installation

### One-liner (recommended)

**macOS / Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/aloundoye/codingbuddy/main/scripts/install.sh | bash
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/aloundoye/codingbuddy/main/scripts/install.ps1 | iex
```

The installer downloads the correct binary for your platform, verifies the SHA-256 checksum, and adds it to your PATH.

### Other methods

**npm** (any platform with Node.js):
```bash
npm install -g codingbuddy
```

**Homebrew** (macOS / Linux):
```bash
brew install aloundoye/tap/codingbuddy
```

**From source** (requires Rust 1.94.1+):
```bash
cargo build --release --bin codingbuddy
# Binary at target/release/codingbuddy — move it to a directory in your PATH
```

### Installer options

The install scripts accept flags to customize behavior:

```bash
# Install a specific version
curl -fsSL .../install.sh | bash -s -- --version v0.3.0

# Install to a custom directory
curl -fsSL .../install.sh | bash -s -- --install-dir ~/.local/bin

# Dry run (show what would happen)
curl -fsSL .../install.sh | bash -s -- --dry-run
```

On Windows:
```powershell
# Custom install directory
& { irm .../install.ps1 } -InstallDir "C:\tools\codingbuddy\bin"
```

### Verify installation

```bash
codingbuddy --version
codingbuddy doctor    # Check dependencies and config
```

## Quickstart

```bash
# Configure (pick any provider)
export DEEPSEEK_API_KEY="sk-..."      # or OPENAI_API_KEY, ANTHROPIC_API_KEY, etc.

# Chat
codingbuddy chat

# Or run the first-time project setup
codingbuddy chat
> /init
```

Switch providers in `.codingbuddy/settings.json`:
```json
{ "llm": { "provider": "anthropic" } }
```

Available providers: `deepseek` (default), `openai-compatible`, `anthropic`, `google`, `groq`, `openrouter`, `ollama`.

## Features

### Agent Intelligence
- **Coordinator mode**: Complex tasks get parallel worker spawning with synthesized results
- **Adaptive complexity**: Simple/Medium/Complex classification with thinking budget escalation
- **Phase overlay**: Complex tasks follow Explore → Plan → Execute → Verify with per-phase tool filtering
- **Agent profiles**: `/agent build|explore|plan|bash|general` — constrains tool set per task type
- **Doom loop + denial tracking**: Breaks repeated tool calls; tracks user denials with guidance after 3x
- **Per-turn retrieval**: Vector + BM25 code search fires every turn, deduplicates against injected context
- **Model routing**: Complex tasks auto-route to reasoning models
- **Memory extraction**: Auto-extracts corrections, preferences, and decisions during compaction

### Tool Execution
- **PTY shell runner**: Real pseudo-terminal via `openpty()` — programs see `isatty()=true` for colored output
- **7-strategy fuzzy editing**: Exact → line-trimmed → block-anchor → whitespace-normalized → indentation-flexible → escape-normalized → context-aware
- **Bash security**: Tree-sitter AST parsing detects file ops, network access, dangerous patterns
- **LSP validation**: Post-edit diagnostics (`cargo check`, `tsc`, `py_compile`, `go vet`) with 30s timeout, command caching, per-language config
- **Tool result caching**: SHA-256 keyed cache with TTL (60s/120s), path-based invalidation on writes
- **Step snapshots**: Per-tool-call file snapshots with SHA-256 hashing for undo
- **SendMessage tool**: Follow up with running/completed subagents by ID or name

### TUI & UX
- **Full theme system**: 30+ semantic colors, dark/light auto-detection via COLORFGBG, hex color support
- **Help modal**: F1 shows scrollable keybindings, slash commands, and tips
- **Model picker**: `/model` with provider filtering, viewport scrolling, pricing display
- **Session branching**: `/branch create <name>` forks session, `/branch list` shows branches
- **Undo/redo**: `/undo` reverts last turn + file changes via checkpoint system
- **Scroll mode**: PageUp pauses TUI for native terminal scrollback
- **Vim mode**: Toggle with `/vim`, full normal/insert/visual mode
- **Visual token budget**: `[████████░░] 78%` bar with theme-aware colors
- **Real-time cost tracking**: Per-model pricing with running total in status bar
- **Graduated approval prompts**: [SHELL] red, [EDIT] yellow, [ACTION] cyan severity badges

### Safety & Permissions
- **4-stage permission system**: Auto-deny → glob pattern rules → hook auto-approve → graduated user prompt
- **Persistent acceptance**: Press 'A' to always allow a pattern (saved to SQLite, survives sessions)
- **Permission rules from config**: `[{"rule": "Bash(cargo *)", "decision": "allow"}]`
- **Default deny rules**: Blocks `rm -rf *`, `git push --force`, `.env` edits, `chmod 777`, `curl | sh`
- **Privacy scanning**: 3-layer secret detection with redaction on tool outputs

### Extensibility
- **MCP integration**: Stdio/HTTP/SSE transports, tool discovery, `codingbuddy mcp init` scaffold
- **12 lifecycle hooks**: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure, PermissionRequest, SubagentStart/Stop, PreCompact, SessionEnd, TaskCompleted, Stop
- **Custom skills**: Markdown frontmatter with `model`, `effort`, `when_to_use` metadata
- **Skill catalog**: Skills with `when_to_use` injected into system prompt so LLM auto-invokes
- **Custom agents**: `.codingbuddy/agents/*.md` with model override, max_turns, tool restrictions

### Local ML (Feature-Gated)
- **Embeddings**: Jina Code v2 via Candle for local vector search
- **Ghost text completion**: DeepSeek Coder 1.3B autocomplete with Tab/Alt+Right accept
- **Privacy router**: 3-layer detection (path globs, content regex, secret patterns) with Block/Redact/LocalOnly policies
- **Memory-aware loading**: Cascades Full → ReducedContext → CpuOnly → Skip based on available memory
- **Hybrid retrieval**: Reciprocal Rank Fusion combining vector + BM25 scores

Enable with: `cargo build --release --bin codingbuddy --features local-ml`

## Architecture

18-crate Rust workspace. Edition 2024, Rust 1.94.1, resolver v2.

| Crate | Role |
|-------|------|
| `codingbuddy-cli` | CLI dispatch, 30+ subcommand handlers |
| `codingbuddy-agent` | Agent engine, tool-use loop, profiles, phase loop, skills, context |
| `codingbuddy-core` | Shared types, config, provider registry, complexity classifier, prompts, profiler |
| `codingbuddy-llm` | LLM client, streaming, capability-driven provider transforms |
| `codingbuddy-tools` | Tool definitions, bash AST security, fuzzy editing, PTY shell, sandbox |
| `codingbuddy-policy` | Permission engine, glob rules, approval gates, output scanner |
| `codingbuddy-hooks` | 14 lifecycle events, hook runtime, permission decisions |
| `codingbuddy-lsp` | Post-edit validation with timeout, command caching, per-language config |
| `codingbuddy-local-ml` | Local ML: embeddings, completion, chunking, vector index, privacy router |
| `codingbuddy-store` | Session persistence (JSONL + SQLite), persistent approvals |
| `codingbuddy-memory` | Long-term memory, shadow commits, checkpoints, auto-extraction |
| `codingbuddy-index` | Full-text code index (Tantivy), RAG retrieval |
| `codingbuddy-mcp` | MCP server management (JSON-RPC stdio/http/SSE) |
| `codingbuddy-ui` | TUI rendering (ratatui), themes, modals, vim mode, ghost text |
| `codingbuddy-diff` | Unified diff parsing, patch staging |
| `codingbuddy-subagent` | Background tasks, worktree isolation, custom agent definitions |
| `codingbuddy-jsonrpc` | JSON-RPC server for IDE integration |
| `codingbuddy-testkit` | Test utilities and mocks |

## Commands

```bash
codingbuddy chat                    # Interactive session (TUI in TTY)
codingbuddy ask "Summarize this repo"  # One-shot question
codingbuddy plan "Implement feature X" # Plan without executing
codingbuddy autopilot "Fix tests" --hours 2  # Unattended loop
codingbuddy review --staged         # Review staged changes
codingbuddy agents list             # List custom agents
codingbuddy skills list             # List available skills
codingbuddy mcp list                # List MCP servers
codingbuddy mcp init my-server      # Scaffold new MCP server
codingbuddy status                  # Session/cost/provider info
codingbuddy doctor                  # Diagnostics
codingbuddy completions --shell zsh # Shell completions
```

### Slash Commands (in chat)

```
/help               Help modal (or press F1)
/model <name>       Switch model (picker with provider filtering)
/agent <profile>    Switch agent profile (build/explore/plan/bash/general)
/branch create <n>  Create session branch
/branch list        List session branches
/undo               Revert last turn + file changes
/redo               Revert to specific checkpoint
/rewind             Pick checkpoint to revert to
/cost               Session cost breakdown
/compact            Force conversation compaction
/clear              Reset conversation
/provider           Show/switch provider
/skills             List available skills
/mcp                Manage MCP servers
/vim                Toggle vim keybindings
/exit               Exit CodingBuddy
```

## Configuration

Settings merge in order (later wins):
1. `~/.codingbuddy/settings.json` (user)
2. `.codingbuddy/settings.json` (project)
3. `.codingbuddy/settings.local.json` (local overrides)

```json
{
  "llm": {
    "provider": "deepseek",
    "context_window_tokens": 128000
  },
  "policy": {
    "approve_edits": "ask",
    "approve_bash": "ask",
    "permission_rules": [
      {"rule": "Bash(cargo *)", "decision": "allow"},
      {"rule": "Bash(npm test *)", "decision": "allow"},
      {"rule": "Edit(*.env*)", "decision": "ask"}
    ]
  },
  "lsp": {
    "enabled": true,
    "languages": {"rust": true, "typescript": true},
    "timeout_seconds": 30
  },
  "theme": {
    "mode": "auto"
  }
}
```

## Web UI

CodingBuddy includes an optional web interface for browser-based access:

```bash
# Start HTTP server with web UI
codingbuddy serve --transport http --web

# Open http://127.0.0.1:8199 in your browser
```

The web frontend connects to the same JSON-RPC backend used by IDE integrations. Build it from the `web/` directory:

```bash
cd web && npm install && npm run build
```

For development with hot-reload:
```bash
# Terminal 1: start the backend
codingbuddy serve --transport http

# Terminal 2: start the dev server (proxies /rpc to backend)
cd web && npm run dev
```

## Development

```bash
cargo fmt --all -- --check          # Format check
cargo clippy --workspace --all-targets -- -D warnings  # Lint (0 warnings)
cargo test --workspace --all-targets  # 1,204+ tests (0 failures)
cargo build --release --bin codingbuddy  # Release binary (LTO + strip)
```

### Startup Profiling

```bash
CODINGBUDDY_STARTUP_TRACE=1 codingbuddy chat
# Outputs:
# [startup-trace] Startup timing:
#   cli_parse                      +  1.2ms  (total   1.2ms)
#   config_load                    + 12.4ms  (total  13.6ms)
#   llm_client                     +  0.1ms  (total  13.7ms)
#   engine_init                    + 85.3ms  (total  99.0ms)
```

## Quality Metrics

| Metric | Value |
|--------|-------|
| Crates | 18 |
| Production lines | ~97K |
| Tests | 1,204+ (0 failures) |
| Production unwraps | 0 |
| Clippy warnings | 0 |
| Hooks fired | 12/14 |
| Providers | 7 + any OpenAI-compatible |
| Capability flags | 19 per provider |
| Release profile | LTO (thin) + strip + codegen-units=1 |

## License

MIT
