# Provider implementation and certification

Delivery Order 2 starts from merged Order 1 commit `0d22fec`. The fixture manifest is [`provider-contracts.json`](../crates/codingbuddy-llm/fixtures/provider-contracts.json). It versions the model profiles, expected wire contracts, usage, outcomes, and the source that generates request assertions and response frames.

| Route | Protocol | Fixture model profile | Continuation |
| --- | --- | --- | --- |
| OpenAI Responses | `openai-responses` | `gpt-4.1-mini` | Full eligible history, original output items, encrypted reasoning |
| OpenAI Chat | `openai-chat` | `gpt-4.1-mini` | Ordered messages and linked calls |
| Anthropic | `anthropic-messages` | `claude-sonnet-4-20250514` | Original signed thinking, redacted thinking, text and tool blocks |
| Google Gemini | `gemini-generate-content` | `gemini-2.5-flash` | Original associated parts and thought signatures |
| DeepSeek | `openai-chat` | `deepseek-chat` | Private reasoning retained within tool rounds and omitted from prior user turns |
| OpenRouter | `openai-chat` | `openai/gpt-4.1-mini` | Ordered messages and linked calls |
| Ollama | `openai-chat` | `qwen2.5-coder:7b` | Ordered messages and linked calls, no authentication by default |

All adapters validate completed function arguments against the advertised tool schemas before returning calls to the runner. Integrations with invalid schemas or undeclared tool names must correct them; these now produce typed errors before execution.

These model names identify deterministic fixture profiles. They do not establish current model availability, account access, or successful live execution. Other existing provider defaults remain available with their existing capability rules.

## Selecting Responses

Set the protocol on the existing OpenAI-compatible profile in user settings:

```json
{
  "llm": {
    "provider": "openai-compatible",
    "providers": {
      "openai-compatible": {
        "chat_protocol": "openai-responses",
        "base_url": "https://api.openai.com/v1",
        "models": { "chat": "gpt-4.1-mini" }
      }
    }
  }
}
```

Keep credentials in the configured environment variable or keyring. Explicit `llm.endpoint` overrides take precedence. The default Responses endpoint appends `/v1/responses` once. Gemini sends its API key in `x-goog-api-key`, including for SSE streaming.

Responses sends `store: false`, the complete eligible history, and requests encrypted reasoning content. It keeps output item IDs, provider call IDs, and durable internal invocation IDs distinct. Private continuation is stored in Order 1's private SQLite tables and replays only on the same route. Portable forks and ordinary exports exclude it. Only a completed response with finished, validated calls can reach tool dispatch; item completion and `[DONE]` are insufficient.

Supported Responses options include function tools and tool choice, image inputs allowed by model capabilities, output limits, structured output, log probabilities, and reasoning. `thinking` maps to medium reasoning effort. `payload_options.reasoning` can select an effort and optional summary mode; `parallel_tool_calls` is also supported. Unimplemented options fail before network dispatch. Hosted tools, background execution, WebSockets, PDF upload, provider-managed conversations, Bedrock Converse, and unsigned SigV4 remain unavailable.

The wire implementation follows the [typed Responses streaming contract](https://developers.openai.com/api/docs/guides/streaming-responses.md) and [reasoning continuation contract](https://developers.openai.com/api/docs/guides/reasoning.md).

## Evidence and live verification

`codingbuddy provider status --json` reports three separate states:

- `configured`: the selected local route and credential configuration pass validation.
- `fixture_verified`: a report beside the executable matches this implementation, platform, model profile, and both streaming modes. Source builds without that report remain unverified.
- `live_verified`: this workspace has a successful explicit probe for this credential/account, endpoint, protocol, model and implementation. Each evidence entry identifies the tested mode and features. A credential connection check establishes connectivity only.

Status, doctor and connection views expose the same evidence. Release archives include `provider-certification.json`; installers preserve it beside the executable. Source fingerprints bind evidence to implementation and fixture revisions. Missing pricing is explicitly `null`; supplied usage is normalized for input, output, cached and reasoning tokens. A missing usage object stays unavailable where the provider supplies no usage.

Live probes require an explicit operator command and may use the configured provider's billable API:

```sh
codingbuddy provider verify --provider openai-compatible --model gpt-4.1-mini --json
codingbuddy provider verify --provider anthropic --model claude-sonnet-4-20250514 --non-streaming --json
codingbuddy provider verify --provider ollama --model qwen2.5-coder:7b --json
```

The probe uses a temporary workspace, offers only a constrained built-in `fs_read` contract, validates exactly two sequential reads, limits output and each request's duration, and disables retries. It never installs or downloads models. The optional Ollama probe requires an installed tool-capable model and records its exact digest from `/api/tags`. Successful results are stored per route and mode in the workspace runtime directory. No live provider was contacted during local implementation verification on November 23, 2026.

## Deterministic verification

```sh
cargo test --locked -p codingbuddy-llm --all-targets
CODINGBUDDY_CERTIFICATION_REPORT=/tmp/provider-certification.json \
  cargo test --locked -p codingbuddy-testkit --test provider_certification
```

The harness uses the real transport, AgentEngine and SQLite store against loopback servers with synthetic credentials. Every route performs two linked tool rounds, a final answer and restart replay in both modes. Negative cases cover premature closure, invalid terminal events and malformed data without tool dispatch. Transport checks cover cancellation while awaiting headers and during stalled reads. CI runs the suite on Linux, macOS and Windows; the release workflow requires it for each of the six shipped targets before publication. A separate evidence gate rejects incomplete coverage, failed routes, stale revisions and mismatched platform or architecture.

The [local audit](PROVIDER_CERTIFICATION_AUDIT.md) records actual results separately from workflow requirements. Windows certification and full release-target results require CI; configuring those jobs does not establish that they passed.
