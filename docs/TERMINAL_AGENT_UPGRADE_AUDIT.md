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

## Implemented Foundation

- `models.dev` refresh now has an execution path from the terminal flow: chat
  startup refreshes the cache in the background, `/models` resolves the live
  catalog synchronously, and the TUI model picker reads the runtime cache.
- Model selector items expose auth availability so model listings and picker
  descriptions can show missing provider credentials.
- `codingbuddy-llm` has an explicit chat protocol selector. Native Anthropic
  Messages and Gemini GenerateContent paths are now selected by protocol, while
  OpenAI-compatible behavior remains the compatibility default.

## Remaining Work

- Add native protocol implementations or explicit adapter errors for OpenAI
  Responses, Bedrock Converse, Vertex, Copilot, and other non-chat-completions
  provider paths.
- Move more provider quirks from ad hoc branches into protocol adapters,
  including request options, streaming deltas, cache hints, response parsing,
  and tool schema transformations.
- Expand durable step/session events with stricter tool-call lifecycle state,
  resume/retry semantics, and richer interrupt handling.
- Unify built-in, MCP, plugin, skill, task, and custom tool metadata under one
  registry surface.
- Add agentic benchmark fixtures for provider payloads, multi-file edits,
  permission denials, compaction, subagents, snapshots, and terminal UX flows.
