# CodingBuddy JSON-RPC protocol

`codingbuddy serve` exposes the agent to editors and other programs over
[JSON-RPC 2.0](https://www.jsonrpc.org/specification).

The parameters of every method are published as JSON Schema in
[`jsonrpc.schema.json`](jsonrpc.schema.json). You can also print it with
`codingbuddy serve --print-schema`. The schema is generated from typed structs
in `crates/codingbuddy-jsonrpc/src/protocol.rs`. Tests fail when the file is out
of date, and when a test call sends a field the schema doesn't list. To
regenerate it:

```bash
CODINGBUDDY_UPDATE_SCHEMA=1 cargo test -p codingbuddy-jsonrpc published_schema
```

For Node.js, [`sdk/typescript`](../../sdk/typescript/README.md) wraps the stdio
transport in a typed client.

## Transports

| Transport | Start | Framing |
|-----------|-------|---------|
| stdio (default) | `codingbuddy serve` | One JSON object per line, on stdin and stdout |
| HTTP | `codingbuddy serve --transport http [--port 8199]` | `POST /rpc` with `Authorization: Bearer <token>` |

With `--json`, the HTTP server prints its address and token on startup.
Otherwise it prints them to stderr.

The HTTP transport accepts only a few methods: `session/open`,
`prompt/execute`, `prompt/start`, `prompt/events`, `prompt/cancel` and
`prompt/stream_next`. It rejects any request that includes a parameter not
in that method's HTTP list. `GET /health` reports whether the server is up.

## Errors

| Code | Meaning |
|------|---------|
| `-32700` | The line was not valid JSON-RPC |
| `-32603` | The method failed. `message` explains why, including `method not found: <name>` |

## A typical session

```jsonc
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
← {"jsonrpc":"2.0","id":1,"result":{"name":"codingbuddy-cli","version":"…","capabilities":[…]}}

→ {"jsonrpc":"2.0","id":2,"method":"session/open","params":{"workspace_root":"/path/to/repo"}}
← {"jsonrpc":"2.0","id":2,"result":{"session_id":"0193…", …}}

// Start in the background, with an idempotency key, and poll for output.
→ {"jsonrpc":"2.0","id":3,"method":"prompt/start","params":{"session_id":"0193…","request_id":"0193…","prompt":"Why does the login test fail?"}}
← {"jsonrpc":"2.0","id":3,"result":{"prompt_id":"0193…","status":"running"}}

→ {"jsonrpc":"2.0","id":4,"method":"prompt/stream_next","params":{"prompt_id":"0193…","cursor":0}}
← {"jsonrpc":"2.0","id":4,"result":{"chunks":[{"type":"content_delta","delta":"The fixture…"}],"next_cursor":1,"done":false}}

→ {"jsonrpc":"2.0","id":5,"method":"cancel","params":{"prompt_id":"0193…"}}
→ {"jsonrpc":"2.0","id":6,"method":"shutdown"}
```

Use `prompt/execute` instead to block until the answer is ready.

When a tool needs approval, the prompt's events include the pending
invocation. Answer with `tool/approve` or `tool/deny`, passing its
`invocation_id`.

## Stream chunks

`prompt/stream_next` and `prompt/events` return the same event objects as
`codingbuddy chat --json`. Each object's `type` is one of the following:

`content_delta`, `reasoning_delta`, `tool_start`, `tool_end`,
`tool_call_ready`, `tool_progress`, `mode_transition`, `phase_transition`,
`subagent_spawned`, `subagent_completed`, `subagent_failed`, `image`,
`watch_triggered`, `security_warning`, `context_warning`,
`clear_streaming_text`, `usage_update`, `snapshot_recorded`, `error`,
`model_changed`, `config_reloaded`, `rate_limited`, `done`.

Their fields are defined by `stream_chunk_to_event_json` in
`crates/codingbuddy-core/src/stream.rs`. Ignore types you don't recognize, so
your client keeps working when new ones are added.
