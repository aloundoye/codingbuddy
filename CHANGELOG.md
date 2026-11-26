# Changelog

All notable changes to CodingBuddy are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

Add an entry under **Unreleased** in the same pull request as the change.
Move it under a version heading when you cut a release.

## [Unreleased]

### Added
- Session memory: decisions, preferences and corrections from each
  compaction summary are kept in `.codingbuddy/session_memory/` and shown to
  the model again with every compaction summary, so they are not lost when a long
  session is summarised more than once or resumed.
- After a pause of five minutes or more, large tool outputs from older turns
  are trimmed before the next request, whatever the context size. The
  provider's prompt cache has expired by then, so this costs nothing and
  makes the next request smaller.
- A warning when the conversation reaches 70% of the context window, before
  older messages are compacted (`context_warning` in JSON output).
- `codingbuddy session search <words>` finds past prompts and answers across
  sessions.
- A published JSON Schema for the JSON-RPC protocol
  (`docs/protocol/jsonrpc.schema.json`, also `codingbuddy serve
  --print-schema`), protocol docs, and a TypeScript SDK in `sdk/typescript`.
- Diagnostic log files in `~/.codingbuddy/logs` (or `CODINGBUDDY_LOG_DIR`),
  rotated daily and kept for 7 days. They record each turn, model request,
  retry, tool call, MCP call and hook with timings, and never the prompt,
  response or tool arguments.
- `--debug` now raises the log level, and `--debug mcp,llm` limits debug
  output to those areas. `CODINGBUDDY_LOG` sets the level directly.
- A crash report with the panic, backtrace and recent log lines is saved on
  any crash, and its path is printed.
- `codingbuddy doctor` shows the log file and the last crash report.
  `codingbuddy doctor --bundle` packages redacted diagnostics for a bug
  report.

### Changed
- Saved provider keys and MCP OAuth tokens are stored in private files
  (`~/.codingbuddy/credentials.json` and `~/.codingbuddy/mcp-tokens/`,
  mode 0600) instead of the OS keychain, so CodingBuddy no longer asks for
  your login password. Keys saved in the keychain by older versions are moved
  to the file the first time they are used. `CODINGBUDDY_CREDENTIALS_FILE`
  points the store elsewhere; `CODINGBUDDY_NO_KEYCHAIN=1` skips that
  migration.
- An API key entered in `/connect` is saved by default (Enter saves and
  connects); Ctrl+O uses it for the current session only.
- A provider that answers 503 now reports that the model is overloaded (not
  a key problem), and a 404 that the model is not available to the key,
  instead of a generic failure. `GEMINI_API_KEY` is accepted for Google, and
  Google's `AQ.` API keys are masked in tool output and diagnostics.
- Corrective prompt heuristics (rejecting long answers written without
  tools, unread file names, shell commands printed as text, recovery
  guidance, checklist and verbosity nudges) are now off by default for
  Claude, GPT and Gemini models, which saves requests and stops correct
  answers from being discarded. They stay on for DeepSeek, Qwen, local and
  unknown models. Configure them under `agent_loop.heuristics`, globally or
  per provider; `codingbuddy doctor` shows what is active.
- The agent is async end to end: model requests, tool execution, the tool
  loop and the engine's chat API are awaited on one shared runtime, and
  TUI turns, JSON-RPC background prompts, team lanes and subagents run as
  runtime tasks. Esc during a running shell command stops the turn and kills
  the command within seconds.
- Nothing a turn waits on holds a thread any more. Shell commands (including
  PTY output), hooks, language servers, stdio MCP servers and sandboxed
  launches run as async child processes; model responses stream through an
  async reader; approval prompts, user questions and subagents are awaited.
  Several requests to the same language server or stdio MCP server now run
  at once instead of queuing, and the TUI wakes on keys and agent events
  instead of polling every 33 ms.
- Checkpoints taken before shell commands are much faster (about 30 s →
  0.35 s for 8,000 files): they skip `.gitignore`d folders such as
  `node_modules`, reuse stored copies of unchanged files and flush them in
  one batch.

- `/rename` now saves the session name (it only printed a message before),
  and `codingbuddy session list` shows each session's title (its name, or
  its first prompt) and when it was last active.
- `codingbuddy update` installs new releases for standalone installs after
  verifying the archive checksum and that the new binary runs, keeping a
  `.bak` of the previous one; Homebrew, npm and source installs are told
  their own upgrade command. It no longer runs `git pull` in your project.
- `codingbuddy permissions approvals` (and `/permissions approvals`) lists
  the "always allow" approvals saved for a project;
  `permissions revoke <tool> '<pattern>'` or `revoke --all` removes them.
- `/rewind` can now undo changes made by shell commands (`mv`, `sed -i`,
  generators, formatters), not only by the file-editing tools. Mutating
  commands are checkpointed first and their changes recorded; read-only
  commands skip the checkpoint. Edits you make afterwards stay protected.
- Pressing Esc now stops a turn immediately in more places: during retry
  back-off (even with a long Retry-After), while `web_fetch`/`web_search`
  wait on a slow site, while prompt URLs are fetched, and in remote MCP
  calls. Model requests reuse connections across calls.

### Fixed
- An "always allow" approval now applies to the very next command in the
  same session instead of after a restart.
- "Always allow" for a shell command now works: saved approvals were never
  matched, so the same command kept asking. They now cover that command
  family (`npm *`) but never a chained, piped or redirected command.
- Commands such as `npm test $(…)`, `cat a>b`, `cat x|sh` or `a & b` were
  auto-approved as read-only in permissive modes; they now ask.
- Fewer ways to crash: a panic in one part of the TUI or JSON-RPC server no
  longer cascades through shared locks; a provider missing from settings,
  an incomplete restore plan, or an unexpected Bedrock message now report
  an error instead of panicking.
- `WebFetch(domain:…)` and URL allow rules matched any URL that merely
  contained the text (for example `https://evil.com/?example.com`). Allow
  rules now match the parsed host or a URL prefix; deny rules still match
  broadly.
- A correct answer that names files mentioned in a tool's output (for
  example a summary of a checklist that lists `Cargo.toml`) is no longer
  discarded and erased from the screen as an "unverified file reference".
- Retry log lines no longer include the request URL, which carries the API
  key for providers such as Gemini.
- A panic caught inside an agent turn no longer drops the TUI out of
  full-screen mode or prints over it.
- Engine warnings no longer print to stderr during a TUI session unless
  `--verbose` is set; they go to the log file.
- Streamed text that arrived before a tool call is no longer erased, and the
  turn completes cleanly after the stream ends.
- A long answer written after this turn's tool results is kept instead of
  being discarded by the grounding nudge.
- An interrupted prompt is closed in the session store, so it no longer
  replays when the session resumes.
- The config watcher reports only settings-file changes, not SQLite writes.
- Analysis answers no longer have to use fixed section headings.

## [0.5.0] - 2026-11-24

### Added
- Provider routes for OpenAI Responses, AWS Bedrock Converse, Vertex AI
  Gemini, Azure OpenAI and OpenAI-compatible gateways, with gateway path and
  tool-schema capability profiles.
- Durable SQLite sessions that keep ordered responses, calls and results,
  stay on their original provider route, and can be forked portably.
- Recoverable background jobs: inspect or cancel owned jobs from a later
  client; worktrees return a patch for review; checkpoint previews protect
  unrelated or newer edits.
- `codingbuddy provider check` and `codingbuddy provider verify`, with a
  deterministic certification suite covering 18 profiles.

### Changed
- Explicit permission denies apply before saved approvals and hooks.
- Project runtime settings require workspace trust; child processes inherit a
  limited environment (`process_env.grants`); browser RPC uses a fresh
  loopback token.
- Unsandboxed commands require fresh approval; commands that require
  isolation are rejected on Windows.

See [docs/releases/0.5.0.md](docs/releases/0.5.0.md) for upgrade steps.

## [0.4.0] - 2026-11-14

### Added
- Streaming tool progress, worktree isolation, permission rules and new hook
  events.
- Provider/model CLI syntax, capability registry, model-aware prompts and a
  runtime model catalog with protocol routing.
- Full LSP client tools (hover, definition, references, symbols).
- Keychain storage for MCP OAuth tokens, MCP prompts, and an HTTP transport.
- Web UI, JSON-RPC config/model methods, npm and Homebrew distribution.
- GitHub PR/issue tools through the `gh` CLI.
- `/doctor` diagnostics, `/init` project detection and soft onboarding.

### Fixed
- Gemini tool names, config hot-reload, and shell commands printed as text
  instead of run.

## Earlier releases

See the GitHub release notes for
[v0.3.0](https://github.com/aloundoye/codingbuddy/releases/tag/v0.3.0),
[v0.2.0](https://github.com/aloundoye/codingbuddy/releases/tag/v0.2.0) and
[v0.1.8](https://github.com/aloundoye/codingbuddy/releases/tag/v0.1.8).

[Unreleased]: https://github.com/aloundoye/codingbuddy/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/aloundoye/codingbuddy/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/aloundoye/codingbuddy/compare/v0.3.0...v0.4.0
