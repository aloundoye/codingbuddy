# Delivery Order 3 — Reliable unattended execution

Updated 2026-11-24. Implementation builds on `eb31d9a` (the first durable execution and recovery changes). This document records the behavior and verification limits of the current working tree. Release certification requires clean, revision-matched results from all six native targets; local dirty-tree checks do not satisfy that gate.

## Execution and cancellation

Prompts, verification commands, tools, foreground delegations and background workers carry durable operation identities. Parent links propagate cancellation through SQLite to later local clients and through hierarchical tokens to in-process descendants. Deadlines bound provider requests, commands, MCP requests and language requests. A cancelled child cannot reset or cancel its siblings. Interrupted prompts settle their operation state when the session lease proves that their writer has exited.

Child processes use Unix process groups or Windows Jobs. Pipe capture drains output beyond its retention limit, avoiding blocked writers and unintended broken pipes. Cancellation and timeout stop owned descendants. A failed PTY execution is never retried through pipes. This is ownership and cleanup, not a replacement for the Order 0 filesystem/network sandbox.

Headless commands cannot manufacture fresh approval. Automatic verification, language tooling and derived commands use the prepared shell launch and policy checks. Unsupported isolation or an unavailable approval channel produces a denied/unavailable result. An interactive verification callback can approve one exact prepared command. Explicit denies never invoke that callback. Verification outcomes distinguish passed, failed, denied, unavailable, timeout, cancellation and persistence ambiguity.

Legacy automatic team lanes are bypassed in favor of the shared durable tool loop. They must not start engines with ambient configuration or apply lane patches outside the shared settlement contract. Explicit subagent tools remain available through the scoped worker implementation. Child turn and configured cost ceilings are inherited; cost ceilings are per runner, not an aggregate billing reservation across concurrent children. Provider requests already in flight can consume tokens before cancellation takes effect.

## Background workers and worktrees

The launching CLI creates the durable worker and private log paths before handing off. The worker receives a bounded configuration bootstrap through stdin, including the pinned trust decision and configuration provenance. Provider credentials do not appear in arguments, child environment or durable bootstrap files. The worker reloads managed restrictions and keeps provider credentials in its own provider client. Normal launcher exit leaves the worker alive.

Workers hold process-held ownership leases. `bg list`, `bg attach`, `bg stop`, and task stop inspect/cancel the owned operation rather than killing a stored PID. A cancellation request is `cancelling` until acknowledged. A vanished worker with an unsettled operation is ambiguous; clients never replay it or kill a reused PID. Pre-dispatch cancellation prevents the job from starting.

UUID-owned worktrees capture current tracked bytes, including staged and unstaged changes. Explicit untracked inputs can be included; ignored inputs, symlinks, unmerged entries and submodules are rejected. Git inspection disables executable helpers, filters and hooks. The parent branch and index stay intact. Windows snapshots preserve tracked executable modes from the index.

Execution, reads, checkpoints and file receipts use the child root, while journal events stay in the parent store. File receipts identify their execution root, preventing accidental reconciliation against a same-named parent file. Results are retained worktrees and staged patch artifacts, with parent preimage checks. Legacy `merge` requests stage a patch for local review. They do not apply it. Worktrees are retained on failure and cancellation for inspection; deletion requires explicit cleanup.

## MCP and language services

Discovery and invocation share one initialized MCP session and configuration fingerprint. Stdio children use owned processes and the shared child-environment grant policy. Remote HTTP and explicitly configured legacy SSE apply authentication consistently and reject endpoint redirects across origins. Tool discovery records schemas and aliases; arguments remain structured and are validated before dispatch. Capability checks cover tools, resources and prompts. Tool-list changes invalidate approval/catalog eligibility.

Readers bound frames and bodies, handle fragmented UTF-8 SSE, correlate response IDs and observe cancellation while waiting for headers or bodies. A possibly dispatched call with no confirmed result becomes an unconfirmed side effect; it is not automatically retried. Cancellation attempts a bounded MCP cancellation notification and evicts the unusable session.

`status` and `doctor` expose configured MCP state separately from initialized state. These inspection views do not connect to a server to fabricate readiness. Connection errors do not expose credentials.

Language servers accept a prepared, owned child from the tool host. The host pins workspace trust, policy, environment consumer (`language_tools`), backend and command before startup. Unsandboxed startup without fresh approval is unavailable. There is no automatic installer. Request reads and queued writes observe cancellation, and stalled/flooding servers are terminated and reaped. Post-edit validation coalesces language roots and uses the shared verification runner; it does not independently spawn build commands. A skipped/denied check is never reported as clean diagnostics.

## Checkpoint and step recovery

Version 2 checkpoints retain binary/large contents in hashed blobs and Unix regular-file modes, with canonical workspace and optional session/cursor linkage. Symbolic links are not followed; full snapshots report skipped links as preserved paths, and explicit unsupported paths fail. Runtime and Git administration paths are protected. Unix file modes cannot be restored on non-Unix platforms.

Restore preview records concrete expected/target states, affected paths, preserved files and conflicts. Only agent-owned matching postimages can be rolled back automatically. Unrelated files survive, and later edits make a plan stale. Steps use the same blob journal and restore planner. Legacy snapshots without verifiable provenance are blocked for manual inspection/export.

Applying needs explicit local approval, policy revalidation and workspace/session leases. The journal moves through prepared, applying, applied or recovery-required states. Files are replaced atomically and verified. Recovery verifies target images before settling an interrupted plan; mixed images require a local decision. Conversation restore creates an idempotent linked fork at the recorded safe cursor, retaining eligible same-route private replay metadata without putting it in exports. A plan cannot be retried under a different restore mode.

CLI: `rewind --preview`, followed by `rewind --plan <id> --mode files|conversation|both --yes`; `rewind --plan <id> --reconcile` inspects interrupted application. Local RPC adds `checkpoint/preview`, `checkpoint/apply`, and `checkpoint/reconcile`, advertising `checkpoint.v2` and `execution.v1`. A step preview can also be applied through its stored plan. HTTP does not expose these mutation capabilities. VS Code provides **CodingBuddy: Restore Checkpoint**, shows the preview and requires explicit confirmation.

## Compatibility and operations

- Integrations requiring inherited environment variables must use exact user/managed `process_env.grants` for the intended consumer. Project files cannot grant variables. Language startup uses `language_tools`; shell, named hooks and MCP servers retain their distinct consumers and source checks.
- Saved shell approval cannot replace fresh approval for unsandboxed execution. Headless build/test/lint and language startup may now be denied until an enforceable backend and policy permit them.
- Legacy background jobs without ownership records cannot be stopped through raw PID data. Inspect them manually. Existing OAuth token files without source binding require reauthentication; server IDs are hashed for safe token filenames.
- Execution subagents do not retry unknown side effects automatically. Read-only exploration/planning can use bounded retries. An explicit execution retry creates a new invocation.
- Restores and worktree results preserve artifacts rather than silently discarding user edits. External side effects cannot be undone by a filesystem restore.

## Verification and release gate

`scripts/unattended_conformance.py` runs 11 fixture groups against real owned processes, temporary Git workspaces, SQLite stores and loopback mocks. It rejects cross-compiled execution and records host target, checkout revision, lockfile SHA-256, source cleanliness, group counts and output fingerprints. Each group must execute tests with zero failures or ignored tests.

`scripts/unattended_conformance_gate.py` rejects missing, duplicate, stale, dirty, failed or incomplete evidence. CI runs Linux, macOS and Windows checks and compiles the VS Code client. Release builds run on all six shipped native targets and upload their reports. Publication requires the six matching reports in addition to the existing dependency, trust/isolation and provider gates. Live providers are not used by these checks.

Local evidence and remaining platform results are recorded below. This milestone is not release-certified until the required native matrix passes.

### Local evidence (2026-11-24)

| Check | Result |
| --- | --- |
| Linux aarch64, all 11 Order 3 groups | 31 passed; no ignored/failed tests. [Local report](unattended-conformance/2026-11-24-linux-aarch64.json). Dirty-tree evidence; not publication authorization. |
| Linux agent unit tests | 296 passed |
| Linux subagent unit tests | 24 passed |
| Linux store unit tests | 26 passed, including interrupted prompt/operation settlement |
| Linux agent tool-use integrations | 36 passed, including P0 trust/approval and Order 0 denial precedence |
| Linux CLI integrations | 25 passed, including P0 HTTP/trust/grants, Order 1 replay and Order 2 Responses clients |
| Provider certification harness | 5 passed; deterministic loopback fixtures, no live providers |
| Parity script and runtime conformance scan | Passed |
| Workspace Clippy, all targets, warnings denied | Passed |
| Formatting and VS Code TypeScript compilation | Passed |
| Release audit/provider/unattended gate unit checks | 2 / 1 / 3 passed; workflow YAML parsed |
| Windows x86_64 core cross-check | Passed; compilation evidence only, no native runtime claims |
| macOS aarch64 native library run | All nine library groups passed (28 checks), including MCP, restore, cancellation and worktrees. The accepted-socket blocking-mode fix resolves the earlier fixture failure. CLI/agent integration groups were verified on Linux. |
| Other native target results and VS Code interactive UI | Not run locally; required native release matrix remains pending |

Lockfile SHA-256: `d8f519ffcc011c9477d4d842d40645e09a097eba7c854cdd5bec8e6980bf458b`. The only lockfile change adds the existing `tempfile` dependency to the LSP crate's development dependencies; no release dependency versions change.

The macOS failure exposed inherited nonblocking mode on accepted mock sockets. The fixture now explicitly restores blocking mode, as does the MCP OAuth callback listener. A delayed OAuth-request fixture covers this production fix. Five MCP checks passed in the native run; repeated native runs confirmed the fix. The complete six-target clean release evidence remains required.
