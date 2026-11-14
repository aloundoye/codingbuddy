/** JSON-RPC 2.0 client for CodingBuddy server. */

let nextId = 1;
const fragmentToken = new URLSearchParams(window.location.hash.slice(1)).get("token");
if (fragmentToken) {
  sessionStorage.setItem("codingbuddy.rpc.token", fragmentToken);
  history.replaceState(null, "", window.location.pathname + window.location.search);
}

export interface RpcResponse<T = unknown> {
  jsonrpc: string;
  id: number;
  result?: T;
  error?: { code: number; message: string };
}

export async function rpc<T = unknown>(
  method: string,
  params: Record<string, unknown> = {},
): Promise<T> {
  const token = sessionStorage.getItem("codingbuddy.rpc.token");
  if (!token) {
    throw new Error("Open the URL printed by the CodingBuddy server to connect.");
  }
  const id = nextId++;
  const res = await fetch("/rpc", {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${token}` },
    body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
  });
  if (!res.ok) {
    throw new Error(res.status === 401
      ? "Open the current URL printed by the CodingBuddy server to reconnect."
      : `CodingBuddy HTTP error ${res.status}`);
  }
  const data: RpcResponse<T> = await res.json();
  if (data.error) {
    throw new Error(`RPC error ${data.error.code}: ${data.error.message}`);
  }
  return data.result as T;
}

export interface SessionOpenResult {
  session_id: string;
  status: string;
}

export interface PromptResult {
  prompt_id: string;
  session_id: string;
  status: string;
  output: string | null;
  error: string | null;
  stream: { prompt_id: string; cursor: number } | null;
}

export interface StreamChunk {
  cursor: number;
  type?: string;
  delta?: string;
  event: Record<string, unknown>;
}

export interface StreamNextResult {
  prompt_id: string;
  chunks: StreamChunk[];
  next_cursor: number;
  done: boolean;
}

export function openSession(workspace = "."): Promise<SessionOpenResult> {
  return rpc("session/open", { workspace_root: workspace });
}

export function executePrompt(
  sessionId: string,
  prompt: string,
): Promise<PromptResult> {
  return rpc("prompt/execute", {
    session_id: sessionId,
    prompt,
    include_partial_messages: true,
  });
}

export function pollStream(
  promptId: string,
  cursor: number,
): Promise<StreamNextResult> {
  return rpc("prompt/stream_next", {
    prompt_id: promptId,
    cursor,
    max_chunks: 32,
  });
}

export function health(): Promise<{ status: string }> {
  return fetch("/health").then((r) => r.json());
}
