# Delivery Order 2 verification record

Date: November 23, 2026. Baseline: merged Order 1, `dev` at `0d22fec`. Results apply to the working implementation and the source fingerprint recorded by the fixture harness. No live credentials or provider accounts were exercised.

## Implemented scope

The seven fixture routes are OpenAI Responses and Chat Completions, Anthropic Messages, Gemini GenerateContent, DeepSeek Chat, OpenRouter Chat, and Ollama OpenAI-compatible Chat. Protocol selection controls request validation, payloads, endpoint construction, headers and decoding. Unsupported protocols, unsigned SigV4 and unknown auth strategies fail before dispatch.

Responses uses stateless full-history continuation with encrypted reasoning and `store: false`. Its assembler validates indexed item identities, fragmented arguments, completed output agreement and the successful response terminal. Function calls undergo schema validation before a model step can dispatch any tool. Typed errors distinguish malformed output, provider failure, incomplete terminal responses, EOF without a terminal, authentication, rate limiting, unsupported features and cancellation. Errors are sanitized before CLI/RPC publication. Streaming completion notifications follow validation.

Anthropic blocks retain signed and redacted thinking in their original positions. Gemini retains thought signatures with associated parts. DeepSeek preserves current-tool-round reasoning after reopening the store and excludes it from later user turns. Private replay remains route bound. Portable projections and public model events remain redacted.

## Deterministic evidence

| Check | Local result |
| --- | --- |
| LLM unit and transport/payload regressions | Passed on Linux ARM64 and macOS ARM64: 129 unit checks and 18 integration checks |
| Seven-route AgentEngine/SQLite workflow | Passed on Linux ARM64 and macOS ARM64, streaming and non-streaming, with two linked tool rounds and final output |
| Reopen SQLite and client between every model step | Passed for all seven routes in both modes |
| Invalid Responses terminals, malformed frames and EOF | Passed; unfinished calls dispatch zero tools |
| Completed Responses call with schema-invalid arguments | Passed; zero tools dispatch |
| Explicit bounded probe implementation against loopback fixtures | Passed for six hosted-route profiles, with synthetic credentials and read-only fixture files |
| Responses through CLI, HTTP prompt cursor replay and legacy IDE stdio | Passed on Linux ARM64 on one saved SQLite session |
| Full CLI JSON regressions, including P0 HTTP/workspace/environment and cross-client replay | Passed on Linux ARM64: all 24 checks |
| P0 agent trust checks and Order 1 browser reauthorization | Passed on Linux ARM64 |
| Store regression suite | Passed on Linux ARM64: 24 unit checks and 8 integration/crash checks |
| JSON-RPC regression suite | Passed on Linux ARM64: 22 checks |
| Patch settlement/recovery regressions | Passed on Linux ARM64: all 13 checks |
| MCP project activation and child-environment regression | Passed on Linux ARM64 |
| Actual CLI verification status | Passed: matching fixture report verifies its profile; stale report is rejected; live status remains separate |
| Workspace Clippy with warnings denied | Passed on Linux ARM64 |
| Web TypeScript check/build and VS Code extension compile | Passed after installing declared development dependencies locally |
| Release evidence validation | Passed: missing coverage, failed results, duplicate entries, stale revisions and wrong platform/architecture are rejected |
| Windows execution and complete six-target release matrix | Pending CI |
| Live provider/model/feature verification | Not invoked; no live results recorded |

The manifest versions model profiles, expected requests, frames, usage and terminal outcomes. Reports identify the implementation fingerprint, revision, operating system, architecture and each route/model/mode. Release packaging requires a current complete report and Order 1 regressions. CI covers Linux, macOS and Windows; publication depends on all six native target jobs. Configuring these jobs does not establish their execution results.

The local volume imposes slow native linking; macOS tests use temporary workspaces on the host. Linux checks use a Rust 1.94.1 ARM64 container and loopback networking. Windows and x86_64 behavior cannot be inferred from local results.

## Rollout and limitations

See [provider support and commands](PROVIDER_SUPPORT.md). Existing defaults are unchanged; Responses is explicit through `chat_protocol`. Responses rejects unimplemented payload options instead of forwarding storage, background or hosted-tool controls. A successful connection check establishes connectivity. Fixture and explicit live evidence remain separate, scoped states. Missing usage and pricing remain unavailable. Live Ollama verification requires an installed tool-capable model and records its digest without installing a model.

Hosted tools, background responses, WebSockets, PDF upload, provider-managed conversations, cloud identity and the extension contract remain outside this milestone. Full sign-off remains pending Windows and complete CI/release-target results. No release was published.

## Recorded snapshot

Locked dependency graph SHA-256: `fdee9d9e2fc0586256095d6d0d3109867f82da6964edbe97a71151aa3f0e7665`. Only workspace dependency edges changed; no registry package versions were upgraded for this milestone.

[Linux ARM64 fixture report](provider-certification/2026-11-23-linux-aarch64.json) and [macOS ARM64 fixture report](provider-certification/2026-11-23-macos-aarch64.json). Both have implementation fingerprint `2c58db064062d943c716d763ae2c9b14b39145839a70da8f697437346e64f2b9`. The baseline is documented above; the implementation fingerprints identify the tested working-tree code. These changes have not been committed or published.

For the expanded Bedrock, Vertex, Azure, and gateway fixtures, see the [Delivery Order 4 audit](DELIVERY_ORDER_4.md).
