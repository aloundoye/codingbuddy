# Changelog

All notable changes to CodingBuddy are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

Add an entry under **Unreleased** in the same pull request as the change.
Move it under a version heading when you cut a release.

## [Unreleased]

### Fixed
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
