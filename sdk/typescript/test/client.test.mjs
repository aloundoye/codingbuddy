// Drives a real `codingbuddy serve` through the SDK. Build the binary first
// (`cargo build --bin codingbuddy`) or point CODINGBUDDY_BIN at one.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { CodingBuddyClient, RpcError } from "../dist/index.js";

const binary =
  process.env.CODINGBUDDY_BIN ?? resolve(import.meta.dirname, "../../../target/debug/codingbuddy");

test("a session runs a prompt end to end", async () => {
  const workspace = mkdtempSync(join(tmpdir(), "codingbuddy-sdk-"));
  const client = new CodingBuddyClient({ binary, cwd: workspace, env: { HOME: workspace } });
  try {
    const info = await client.initialize();
    assert.equal(info.name, "codingbuddy-cli");
    assert.ok(info.capabilities.includes("prompt/start"));

    const { session_id } = await client.openSession({ workspace_root: workspace });
    const result = await client.executePrompt({
      session_id,
      prompt: "say hello",
      mock_output: "hello from the sdk",
    });
    assert.match(JSON.stringify(result), /hello from the sdk/);

    await assert.rejects(client.call("no/such_method"), (error) => {
      assert.ok(error instanceof RpcError);
      assert.match(error.message, /method not found/);
      return true;
    });
  } finally {
    await client.close();
    rmSync(workspace, { recursive: true, force: true });
  }
});

test("calls fail cleanly when the server cannot start", async () => {
  const client = new CodingBuddyClient({ binary: "/nonexistent/codingbuddy" });
  await assert.rejects(client.initialize());
  await client.close();
});
