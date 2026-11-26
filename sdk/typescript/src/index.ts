/**
 * A small client for the CodingBuddy JSON-RPC protocol
 * (docs/protocol/README.md). It starts `codingbuddy serve` as a child
 * process and talks to it over stdio, one JSON object per line.
 */
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createInterface } from "node:readline";

export interface ClientOptions {
  /** Path to the codingbuddy binary (default: `codingbuddy` on PATH). */
  binary?: string;
  /** Working directory of the server, which is the workspace it serves. */
  cwd?: string;
  /** Extra environment variables for the server process. */
  env?: Record<string, string>;
}

/** An error returned by the server for one request. */
export class RpcError extends Error {
  constructor(
    readonly code: number,
    message: string,
    readonly method: string,
  ) {
    super(`${method}: ${message}`);
    this.name = "RpcError";
  }
}

export interface InitializeResult {
  name: string;
  version: string;
  capabilities: string[];
  [key: string]: unknown;
}

export interface SessionOpenParams {
  workspace_root?: string;
  baseline_commit?: string;
}

export interface PromptParams {
  session_id: string;
  prompt: string;
  /** Idempotency key: retrying with the same id returns the same prompt. */
  request_id?: string;
  include_partial_messages?: boolean;
  repo_root?: string;
  debug_context?: boolean;
  force_max_think?: boolean;
  tools?: boolean;
  /** `code` (default), `ask`, `context` or `plan`. */
  mode?: string;
  teammate_mode?: string;
  additional_dirs?: string[];
  /** Testing only: skip the model and answer with this text. */
  mock_output?: string;
}

/** One stream event; see the "Stream chunks" section of the protocol docs. */
export interface StreamChunk {
  type: string;
  [key: string]: unknown;
}

export interface StreamPage {
  prompt_id: string;
  chunks: StreamChunk[];
  next_cursor: number;
  done: boolean;
  [key: string]: unknown;
}

type Pending = {
  method: string;
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
};

export class CodingBuddyClient {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly pending = new Map<number, Pending>();
  private nextId = 1;
  private closed: Error | null = null;
  private stderr = "";

  constructor(options: ClientOptions = {}) {
    this.child = spawn(options.binary ?? "codingbuddy", ["serve"], {
      cwd: options.cwd,
      env: { ...process.env, ...options.env },
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child.stderr.on("data", (data: Buffer) => {
      // Keep the tail only, for error messages.
      this.stderr = (this.stderr + data.toString()).slice(-4000);
    });
    createInterface({ input: this.child.stdout }).on("line", (line) => this.onLine(line));
    this.child.on("error", (error) => this.fail(error));
    this.child.on("exit", (code, signal) =>
      this.fail(
        new Error(
          `codingbuddy serve exited (${signal ?? code})${this.stderr ? `: ${this.stderr.trim()}` : ""}`,
        ),
      ),
    );
  }

  /** Send any method with raw params. */
  call<T = unknown>(method: string, params: object = {}): Promise<T> {
    if (this.closed) {
      return Promise.reject(this.closed);
    }
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { method, resolve: resolve as (value: unknown) => void, reject });
      this.child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });
  }

  initialize(): Promise<InitializeResult> {
    return this.call("initialize");
  }

  openSession(params: SessionOpenParams = {}): Promise<{ session_id: string; [key: string]: unknown }> {
    return this.call("session/open", params);
  }

  /** Run a prompt and wait for its final result. */
  executePrompt(params: PromptParams): Promise<Record<string, unknown>> {
    return this.call("prompt/execute", params);
  }

  /** Start a prompt in the background. A `request_id` is generated if absent. */
  startPrompt(params: PromptParams): Promise<{ prompt_id: string; status: string; [key: string]: unknown }> {
    return this.call("prompt/start", { request_id: randomUUID(), ...params });
  }

  streamNext(promptId: string, cursor = 0, maxChunks?: number): Promise<StreamPage> {
    return this.call("prompt/stream_next", {
      prompt_id: promptId,
      cursor,
      ...(maxChunks === undefined ? {} : { max_chunks: maxChunks }),
    });
  }

  /**
   * Yield a background prompt's stream chunks until it is done, polling
   * every `intervalMs` while no new chunks are available.
   */
  async *stream(promptId: string, intervalMs = 100): AsyncGenerator<StreamChunk> {
    let cursor = 0;
    for (;;) {
      const page = await this.streamNext(promptId, cursor, 128);
      yield* page.chunks;
      cursor = page.next_cursor;
      if (page.done) {
        return;
      }
      if (page.chunks.length === 0) {
        await new Promise((resolve) => setTimeout(resolve, intervalMs));
      }
    }
  }

  cancel(promptId?: string): Promise<{ cancelled: boolean; status: string }> {
    return this.call("cancel", promptId ? { prompt_id: promptId } : {});
  }

  approveTool(sessionId: string, invocationId: string): Promise<unknown> {
    return this.call("tool/approve", { session_id: sessionId, invocation_id: invocationId });
  }

  denyTool(sessionId: string, invocationId: string): Promise<unknown> {
    return this.call("tool/deny", { session_id: sessionId, invocation_id: invocationId });
  }

  /** Ask the server to stop and wait for the process to exit. */
  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    const exited = new Promise<void>((resolve) => this.child.once("exit", () => resolve()));
    try {
      await this.call("shutdown");
    } catch {
      // Already gone.
    }
    this.child.stdin.end();
    await exited;
  }

  private onLine(line: string): void {
    let message: { id?: number; result?: unknown; error?: { code: number; message: string } };
    try {
      message = JSON.parse(line);
    } catch {
      return; // Not a protocol line.
    }
    if (typeof message.id !== "number") {
      return;
    }
    const pending = this.pending.get(message.id);
    if (!pending) {
      return;
    }
    this.pending.delete(message.id);
    if (message.error) {
      pending.reject(new RpcError(message.error.code, message.error.message, pending.method));
    } else {
      pending.resolve(message.result);
    }
  }

  private fail(error: Error): void {
    this.closed ??= error;
    for (const pending of this.pending.values()) {
      pending.reject(error);
    }
    this.pending.clear();
  }
}
