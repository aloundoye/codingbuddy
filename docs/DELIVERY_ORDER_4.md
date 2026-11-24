# Delivery Order 4 — Cloud providers and adapter extensions

Updated November 24, 2026. The implementation was restored onto `dev` at `20da702`, which includes the Windows worktree path fix and macOS patch-lock retries. The [provider support matrix](PROVIDER_SUPPORT.md) documents configuration, supported wire behavior, and the opt-in verification commands.

## Implementation

- Bedrock uses the AWS Rust credential chain, SigV4 over the exact request bytes, and CRC-checked AWS event-stream decoding. Vertex Gemini uses Google ADC and the existing Gemini codec. Azure uses its deployment name in Chat Completions and supports v1 and the dated `2024-10-21` endpoint with API key and Entra authentication.
- The versioned compiled adapter registry is exercised by Bedrock. The shared runner receives completed, schema-validated calls and retains provider-native continuation in its private route-bound store. Legacy profile fingerprints retain the existing format.
- Cloud routes derive the credential destination from validated deployment fields. Redirects and credential header overrides are rejected. AWS credential processes and executable ADC are disabled. Cloud identity acquisition observes cancellation and a timeout. Configure a distinct local account identifier when changing the credential principal behind an SDK profile. Ambient profile and principal hints participate in the route fingerprint, but labels still require correct operator configuration.
- Groq, Mistral, xAI, Together, and a configuration-only gateway join the deterministic manifest. `provider check` records connectivity. `provider verify` separately records successful linked tool rounds. The live coding evaluation is an explicitly selected ignored test. It scores fixed tasks and records estimated cost only when an exact model price is configured.

## Evidence

| Check | Result |
| --- | --- |
| Linux and macOS aarch64 provider certification | 18 route profiles across 36 streaming/unary combinations. Positive routes, two linked tool rounds, restart replay, route isolation, usage, and incomplete-stream rejection passed on both platforms and their reports passed the evidence gate. Linux also passed the five scripted evaluation tasks. |
| macOS focused regression | 133 LLM unit tests, 12 testkit unit tests, and 26 CLI integrations passed. The five scripted evaluation tasks passed through the durable runner. |
| Quality and gates | Workspace Clippy with warnings denied, formatting, workflow YAML parsing, and provider/release gate unit checks passed. |
| RustSec | Six release targets audited with zero unexcepted findings. Zero active vulnerabilities in the locked graph at advisory DB `1d328b6bb9f2f350bf65c1c81ab68781cb9e018a`. Updated `url` from 2.5.8 to 2.5.9 for RUSTSEC-2026-0592. Five unmaintained-package warnings remain. |
| Native Windows and full release matrix | Pending CI. Publication requires the existing gates plus the expanded route manifest on all six native targets. |
| Live provider and SDK credential service checks | Pending explicit live invocation. Loopback tests use synthetic bearer credentials or sign requests with synthetic AWS session credentials. |

Fixture evidence covers the listed model profiles and synthetic usage fields. Cloud account access, regional model availability, actual credential refresh, and model quality require an explicit live probe or evaluation. No live cloud endpoint was contacted for this audit.

Pre-restore lockfile SHA-256: `6549c7d2da4f0bc8688b2fabb1dc38ffc040b3413c8bde4251ea0e44fdad1b1d`. Local checks were run on working tree changes. Packaged evidence must come from the release workflow for the published revision.

Fixture implementation digest for the pre-restore Linux and macOS reports: `cc28547186b54f70aa40467933ef54978b222e61e09e1868fc1e3df58cbc5087`.

Lockfile SHA-256 after restoring onto `dev`: `5093a050a694a41a1b8a707bc646569fcf38ee4d398ff1efb4fce7004890e05e`.
