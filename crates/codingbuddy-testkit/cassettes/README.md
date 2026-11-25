# Provider cassettes

Recorded provider HTTP traffic, replayed by `tests/provider_replay.rs`
through the real `ApiClient` and `AgentEngine` on every test run. No network
and no API keys are needed to replay.

The hand-written fixtures in `provider_certification.rs` check what each
protocol *allows*. Cassettes check what providers *actually send*: how text
is split across stream frames, where a function call lands relative to
text, which usage fields are filled. Bugs in that gap only show up against
real traffic.

## Layout

```
cassettes/<provider>/<scenario>.json
```

`provider` is the settings provider id (`google`, `deepseek`, `anthropic`,
…). Each cassette stores the prompt, the workspace files, every request
path, every response chunk with its timing, and what the engine produced
(tool calls in order and the final answer).

`"source": "synthetic"` marks a hand-written stand-in. Replace those with
real recordings when you can.

## What replay checks

For every cassette:

- the engine makes exactly the recorded requests, to the recorded paths
- it runs the recorded tools in order and returns the recorded answer
- when streaming, `Done` arrives once, last, and the text left on screen
  after the last tool call (with `ClearStreamingText` applied) is the
  final answer, so a correct answer is never erased

## Recording

Recording calls the real API (a few cents per provider) with the key from
your environment or `~/.codingbuddy/settings.json`:

```bash
CODINGBUDDY_RECORD=1 CODINGBUDDY_RECORD_PROVIDERS=google,deepseek \
  cargo test -p codingbuddy-testkit --test provider_replay record_cassettes -- --ignored
```

Optional filters: `CODINGBUDDY_RECORD_SCENARIOS=text_then_tool` and
`CODINGBUDDY_RECORD_MODEL=<model id>`. Scenarios are defined in
`SCENARIOS` in `provider_replay.rs`.

A local proxy sits between the engine and the provider. Request headers are
never stored and credential query parameters (`key`, `api_key`, `token`, …)
are removed from paths. Each recording is replayed before it is saved, so a
cassette that cannot replay is never written. Still, read the diff before
committing a new cassette.

Only API-key HTTP providers can be recorded; cloud-identity routes
(Bedrock, Vertex) are covered by the synthetic certification suite.

## When a cassette fails

A failure means the engine now behaves differently on the same provider
bytes. If the change is a bug, fix it. If it is intended (for example a
prompt heuristic was turned off for that provider), re-record the affected
cassettes and review the new expectations.
