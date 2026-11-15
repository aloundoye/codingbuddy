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
  history?: { role: "user" | "assistant"; content: string }[];
  latest_prompt?: PromptResult;
}

export interface PromptResult {
  prompt_id: string;
  session_id: string;
  status: string;
}

export interface PromptEvents {
  events: { seq: number; event: { type: string; text?: string } }[];
  next_cursor: number;
  status: string;
  output: string | null;
  error: string | null;
  done: boolean;
}

export function openSession(sessionId?: string | null): Promise<SessionOpenResult> {
  return rpc("session/open", { workspace_root: ".", session_id: sessionId ?? undefined });
}

export function executePrompt(sessionId: string, prompt: string, requestId: string): Promise<PromptResult> {
  return rpc("prompt/start", { session_id: sessionId, prompt, request_id: requestId });
}

export function pollStream(promptId: string, cursor: number): Promise<PromptEvents> {
  return rpc("prompt/events", { prompt_id: promptId, cursor });
}

export function cancelPrompt(promptId: string): Promise<PromptResult> {
  return rpc("prompt/cancel", { prompt_id: promptId });
}

export function health(): Promise<{ status: string }> {
  return fetch("/health").then((r) => r.json());
}
