# Provider implementation and certification

The original seven Order 2 profiles are listed below. Order 4 adds cloud and gateway profiles. The fixture manifest is [`provider-contracts.json`](../crates/codingbuddy-llm/fixtures/provider-contracts.json). It versions the model profiles, expected wire contracts, usage, outcomes, and the source that generates request assertions and response frames.

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

Supported Responses options include function tools and tool choice, image inputs allowed by model capabilities, output limits, structured output, log probabilities, and reasoning. `thinking` maps to medium reasoning effort. `payload_options.reasoning` can select an effort and optional summary mode; `parallel_tool_calls` is also supported. Unimplemented options fail before network dispatch. Hosted tools, background execution, WebSockets, PDF upload and provider-managed conversations remain outside the Responses implementation. Bedrock Converse uses a distinct signed route described below.

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


## Delivery Order 4 routes

| Provider | Protocol and authentication | Deterministic fixture profile |
| --- | --- | --- |
| AWS Bedrock | Converse and ConverseStream, SigV4 with the AWS SDK credential chain | Claude Sonnet 4 inference profile |
| Google Vertex AI | GenerateContent and streaming SSE, Application Default Credentials | Gemini 2.5 Flash publisher model |
| Azure OpenAI | v1 or dated `2024-10-21` Chat Completions, API key or Microsoft Entra ID | GPT-4o with a distinct deployment name, four combinations |
| Groq, Mistral, xAI, Together | Chat Completions with bearer credentials | One model profile per gateway in the fixture manifest |
| Custom gateway | Chat Completions with a configured path, headers, and stream options | Configuration-only fixture profile |

### Cloud setup

Put deployment settings in user settings under `llm.providers`. Keep model profiles separate from deployment names. Give each Azure deployment its own profile, including deployments used for reasoning. Select a configured profile with `llm.provider`, or pass it to the provider command.

```json
{
  "llm": {
    "provider": "azure",
    "providers": {
      "azure": {
        "cloud": {
          "type": "azure",
          "resource": "your-resource",
          "deployment": "your-deployment",
          "api_version": "v1",
          "credential": "entra"
        },
        "account_id": "work-entra-principal",
        "models": {"chat": "gpt-4o"}
      },
      "vertex": {
        "cloud": {"type": "vertex", "project": "your-project", "location": "us-central1"},
        "account_id": "work-google-principal",
        "models": {"chat": "gemini-2.5-flash"}
      },
      "bedrock": {
        "cloud": {"type": "bedrock", "region": "us-east-1", "profile": "work"},
        "account_id": "work-aws-role",
        "models": {"chat": "us.anthropic.claude-sonnet-4-20250514-v1:0"}
      }
    }
  }
}
```

- **Azure** accepts `api-key` through the profile's usual environment or keyring credential source, which defaults to `AZURE_OPENAI_API_KEY`. For `entra`, use Azure managed identity or the service principal environment variables `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, and `AZURE_CLIENT_SECRET`. Set `api_version` to `2024-10-21` for the dated deployment path.
- **Vertex** uses standard Google ADC with the Cloud Platform scope. Set the location to your region or `global`. The route supports Gemini publisher models. ADC sources that launch executables are disabled.
- **Bedrock** uses the AWS SDK credential chain with explicit region and optional profile. It supports session credentials and AWS SSO. Use a model ID, inference profile ID, or ARN accepted by Converse. AWS credential processes are disabled.
- Use a distinct, stable `account_id` for each rotating cloud principal. Update it when the underlying identity changes, including when reauthenticating through an unchanged SDK profile. Cloud route fingerprints include deployment, credential strategy, and locally available source identifiers. A portable fork is required to transfer history to a different route. Existing non-cloud profiles retain their existing route format.
- Cloud endpoints are derived from deployment fields. Remove `llm.endpoint`, per-profile auth overrides, and custom headers before moving a legacy compatibility profile to a cloud route. Keep a cloud gateway on an explicitly configured compatible profile when it has its own endpoint and credentials.

### Compatible gateways

Use the bundled provider presets, or define a new profile with `kind = "openai-compatible"`, `base_url`, `api_key_env`, and `models.chat`. Optional `gateway.chat_path` is an origin-relative path such as `/v1/chat/completions`. A whole `{model}` path segment encodes the model as one segment. Set `gateway.omit_stream_usage_option = true` when the gateway rejects `stream_options`. Existing `discovery.models_endpoint`, capability overrides, and non-reserved headers remain available. Authentication and routing headers are reserved and must use the route's configured auth strategy.

### Compiled extensions

Implement `ProviderAdapterV1` and register it in the compiled registry. Bedrock demonstrates endpoint and credential integration, request encoding, binary stream validation, usage normalization, and provider-owned replay blocks. Return ordered `TransportPart` values and retain the wire tool IDs. The shared client validates arguments against the tool schemas before the durable runner sees them. Add a manifest route and loopback fixtures for both streaming modes before advertising feature evidence. A breaking extension interface change requires a new version. See [adapter registry](../crates/codingbuddy-llm/src/extension.rs).

### Connectivity, evidence, and task evaluations

`codingbuddy provider check --provider azure --json` records a bounded text-only connectivity probe. `provider verify` additionally checks two linked tool rounds. Status exposes the resolved auth and deployment plus independent connectivity, fixture, and live evidence.

The fixed evaluation suite uses the same fixtures and policy for every selected route. It checks exploration, a two-file fix, recovery from a failed route check, cancellation, and long-context resume. It records patch correctness, preservation of user edits, permission denials, first-event and total latency, and available usage. The suite disables transport retries. Pricing stays unavailable in its report where the route lacks verified pricing.

```sh
cargo test --locked -p codingbuddy-testkit --test provider_eval provider_neutral_fixture_scores
CODINGBUDDY_EVAL_CONFIG_WORKSPACE=/path/to/configured/workspace CODINGBUDDY_EVAL_PROVIDER=vertex CODINGBUDDY_EVAL_MODEL=gemini-2.5-flash cargo test --locked -p codingbuddy-testkit --test provider_eval provider_neutral_live_eval -- --ignored --nocapture
```

The live evaluation and connectivity commands contact the explicitly selected provider and may incur charges. See the [Order 4 audit](DELIVERY_ORDER_4.md) for dated evidence.
