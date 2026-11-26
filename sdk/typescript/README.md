# codingbuddy-sdk

Drive the CodingBuddy agent from Node.js. The client starts `codingbuddy serve`
and talks to it over the JSON-RPC protocol described in
[`docs/protocol`](../../docs/protocol/README.md).

```ts
import { CodingBuddyClient } from "codingbuddy-sdk";

const client = new CodingBuddyClient({ cwd: "/path/to/repo" });
await client.initialize();
const { session_id } = await client.openSession({ workspace_root: "/path/to/repo" });

const { prompt_id } = await client.startPrompt({ session_id, prompt: "Why does the login test fail?" });
for await (const chunk of client.stream(prompt_id)) {
  if (chunk.type === "content_delta") process.stdout.write(String(chunk.delta));
}
await client.close();
```

Any method in [`jsonrpc.schema.json`](../../docs/protocol/jsonrpc.schema.json)
can be called with `client.call(method, params)`.

## Development

```bash
cargo build --bin codingbuddy   # the tests drive the real binary
npm install && npm test
```
