# CodingBuddy

An open-source, multi-model AI coding agent for the terminal. Written in Rust.

Connect any LLM — DeepSeek, OpenAI, Anthropic, Google, Groq, Ollama, OpenRouter — and get a production-grade coding assistant with tool execution, planning, code search, and safety guardrails.

## Why CodingBuddy?

- **Any model, any provider.** 7 providers preconfigured. Any OpenAI-compatible endpoint works.
- **Fast.** Rust binary, no runtime dependencies. Sub-100ms startup.
- **Safe.** Tree-sitter bash AST analysis, default deny rules, diff previews before edits, per-tool approval.
- **Smart.** 7-strategy fuzzy file editing, per-turn code retrieval, doom loop detection, adaptive complexity.
- **Extensible.** MCP servers, lifecycle hooks, custom skills, plugin system.

## Quickstart

```bash
# Install
curl -fsSL https://raw.githubusercontent.com/aloundoye/codingbuddy/main/scripts/install.sh | bash

# Configure (pick any provider)
export DEEPSEEK_API_KEY="sk-..."      # or OPENAI_API_KEY, ANTHROPIC_API_KEY, etc.

# Chat
codingbuddy chat
```

Switch providers in `.codingbuddy/settings.json`:
```json
{ "llm": { "provider": "anthropic" } }
```

Available providers: `deepseek` (default), `openai-compatible`, `anthropic`, `google`, `groq`, `openrouter`, `ollama`.

## Features

### Agent Intelligence
- **Adaptive complexity**: Simple/Medium/Complex classification with thinking budget escalation
- **Phase overlay**: Complex tasks follow Explore → Plan → Execute → Verify with per-phase tool filtering
- **Agent profiles**: `/agent build|explore|plan|bash|general` — constrains tool set per task type
- **Doom loop detection**: Breaks repeated identical tool calls with corrective guidance
- **Per-turn retrieval**: Vector + BM25 code search fires every 3 turns, removes stale context
- **Model routing**: Complex tasks auto-route to reasoning models

### Tool Execution
- **7-strategy fuzzy editing**: Exact → line-trimmed → block-anchor → whitespace-normalized → indentation-flexible → escape-normalized → context-aware
- **Bash security**: Tree-sitter AST parsing detects file ops, network access, dangerous patterns (`curl|sh`, `eval`, `rm -rf /`)
- **Post-edit validation**: LSP diagnostics (`cargo check`, `tsc`, `py_compile`, `go vet`) fed back to LLM
- **Step snapshots**: Per-tool-call file snapshots with SHA-256 hashing for undo
- **Parallel execution**: Read-only tools run concurrently; write tools run sequentially

### UX
- **Interactive TUI**: Vim mode, @file autocomplete, syntax highlighting, keyboard shortcuts
- **Visual token budget**: `[████████░░] 78%` bar with color (green → yellow → red)
- **Real-time cost tracking**: Per-model pricing for 15+ model families, running total in status bar
- **Diff previews**: See exactly what `/agent` edits will change before approving
- **Ghost text**: Local ML-powered inline completions (Tab to accept)
- **Session resume**: Detects interrupted sessions on startup

### Safety
- **Permission engine**: Ask/auto/plan modes, glob allowlist/denylist, team-managed policy
- **Default deny rules**: Blocks `rm -rf *`, `git push --force`, `.env` edits, `chmod 777`, `curl | sh`
- **Privacy scanning**: 3-layer secret detection with redaction on tool outputs
- **MCP tool description capping**: Prevents OpenAPI bloat from eating context (2048-char limit)

### Extensibility
- **MCP integration**: Stdio/HTTP/SSE transports, tool discovery, resource access
- **14 lifecycle hooks**: SessionStart through TaskCompleted
- **Custom skills**: Markdown-based skill definitions in `.codingbuddy/skills/`
- **Plugin system**: Custom tools via `.codingbuddy/plugins/`
- **Subagents**: Background tasks with worktree isolation

## Architecture

20-crate Rust workspace. Edition 2024, resolver v2.

| Crate | Role |
|-------|------|
| `codingbuddy-cli` | CLI dispatch, 24 subcommand handlers |
| `codingbuddy-agent` | Agent engine, tool-use loop, complexity classifier, profiles, phase loop |
| `codingbuddy-core` | Shared types, config loading, provider registry, session metadata |
| `codingbuddy-llm` | LLM client, streaming, provider transforms, capability detection |
| `codingbuddy-tools` | Tool definitions, bash AST security, fuzzy editing, shell runner, sandbox |
| `codingbuddy-policy` | Permission engine, approval gates, output scanner, default deny rules |
| `codingbuddy-hooks` | 14 lifecycle events, hook runtime |
| `codingbuddy-lsp` | Post-edit validation via language-specific checks |
| `codingbuddy-local-ml` | Local ML: embeddings, completion, chunking, vector index, privacy router |
| `codingbuddy-store` | Session persistence (JSONL + SQLite) |
| `codingbuddy-memory` | Long-term memory, shadow commits, checkpoints |
| `codingbuddy-index` | Full-text code index (Tantivy), RAG retrieval |
| `codingbuddy-mcp` | MCP server management (JSON-RPC stdio/http) |
| `codingbuddy-ui` | TUI rendering (ratatui/crossterm), autocomplete, vim mode, ghost text |
| `codingbuddy-diff` | Unified diff parsing, patch staging |
| `codingbuddy-context` | Context enrichment, dependency analysis |
| `codingbuddy-skills` | Skill discovery, forked execution |
| `codingbuddy-subagent` | Background tasks, worktree isolation |
| `codingbuddy-jsonrpc` | JSON-RPC server for IDE integration |
| `codingbuddy-testkit` | Test utilities |

The execution model is a **ReAct-style tool loop** — the LLM decides what tools to call, tools execute locally with policy gates, and the loop continues until the task is complete. For complex tasks, an Explore → Plan → Execute → Verify phase overlay constrains tool availability per phase.

## Commands

```bash
codingbuddy chat                    # Interactive session (TUI in TTY)
codingbuddy ask "Summarize this repo"  # One-shot question
codingbuddy plan "Implement feature X" # Plan without executing
codingbuddy autopilot "Fix tests" --hours 2  # Unattended loop
codingbuddy review --staged         # Review staged changes
codingbuddy status                  # Session/cost/provider info
codingbuddy doctor                  # Diagnostics
```

### Slash Commands (in chat)

```
/agent <profile>    Switch agent profile (build/explore/plan/bash/general)
/model <name>       Switch model
/provider           Show/switch provider
/cost               Session cost breakdown
/compact            Force conversation compaction
/undo               Show latest snapshot for revert
/clear              Reset conversation
/plan               Enter plan mode
/vim                Toggle vim keybindings
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
    "approve_bash": "ask"
  }
}
```

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --release --bin codingbuddy
```

## License

MIT
