# Terminal Agent Upgrade Audit

This document tracks the first implementation pass for making CodingBuddy a
terminal-first agent competitive with Claude Code and OpenCode.

## Baseline

- CodingBuddy remains the implementation base.
- The initial target is the terminal agent: CLI, TUI, tool execution, provider
  routing, model selection, safety, and session reliability.
- OpenCode is treated as an architectural reference. Its strongest reusable
  ideas are broad provider/protocol routing, a refreshable `models.dev` catalog,
  dynamic tool metadata, durable stream events, and polished model/session UX.
- The Claude Code source audit is blocked until a valid local source path is
  provided. The requested path did not exist:
  `/Users/aloutndoye/Downloads/claude-code-main/src`.
  Re-checked on 2026-05-14; the path is still absent.

## Implemented Foundation

- `models.dev` refresh now has an execution path from the terminal flow: chat
  startup refreshes the cache in the background, `/models` resolves the live
  catalog synchronously, and the TUI model picker reads the runtime cache.
- Model selector items expose auth availability so model listings and picker
  descriptions can show missing provider credentials.
- `codingbuddy-llm` has an explicit chat protocol selector. Native Anthropic
  Messages and Gemini GenerateContent paths are now selected by protocol, while
  OpenAI-compatible behavior remains the compatibility default.
- Provider configs now have explicit serde-default fields for `chat_protocol`,
  `auth_strategy`, custom `headers`, and discovery behavior while preserving
  existing `payload_options.protocol` compatibility.
- Protocol adapter metadata now covers OpenAI Chat, OpenAI Responses,
  Anthropic Messages, Gemini GenerateContent, and Bedrock Converse. Runtime
  execution emits precise unsupported-protocol errors for registered adapters
  that are not yet wired end to end.
- Model selector JSON, `/models`, setup/status, and the TUI picker now expose
  protocol/auth/status/no-tool information so unsafe model choices are visible
  before a switch.
- `/model` switching now blocks known no-tool models while in code/tool mode
  and directs the user to `/ask`, `/context`, or `/read-only on` for chat-only
  operation.
- `EventKind` now includes durable agent step, tool input/execution, retry,
  interrupt, permission-decision, and compaction-snapshot lifecycle records.
- `codingbuddy-tools` now exposes an initial `ToolRegistry`/`RegisteredTool`
  facade with definition, metadata, validator, executor, permission-target, and
  truncation policy slots.
- `codingbuddy-testkit` now includes terminal-agent benchmark fixtures covering
  provider deltas, multi-file edits, permission denials, compaction, snapshots,
  subagents, and terminal workflows.

## Remaining Work

- Wire OpenAI Responses and Bedrock Converse into full request/response and
  streaming execution paths.
- Continue migrating provider quirks from ad hoc branches into protocol
  adapters, including request options, streaming deltas, cache hints, response
  parsing, and tool schema transformations.
- Persist the new durable lifecycle events from the active tool loop and use
  them to harden resume/retry boundaries.
- Migrate built-in, MCP, plugin, skill, task, and custom tool execution through
  the central registry facade, then remove duplicate metadata paths.
- Turn the benchmark fixtures into scripted provider, CLI, and TUI acceptance
  runners in CI.
