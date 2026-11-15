import { useState, useRef, useCallback, useEffect } from "preact/hooks";
import type { JSX } from "preact";
import { openSession, executePrompt, pollStream, cancelPrompt } from "./api";

interface Message {
  role: "user" | "assistant" | "system";
  content: string;
}

const SESSION_KEY = "codingbuddy.rpc.session";
const PENDING_KEY = "codingbuddy.rpc.pending";

export function App() {
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState("");
  const [loading, setLoading] = useState(false);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [activePrompt, setActivePrompt] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const messagesEnd = useRef<HTMLDivElement>(null);

  const scrollToBottom = useCallback(() => {
    messagesEnd.current?.scrollIntoView({ behavior: "smooth" });
  }, []);

  const replayPrompt = async (promptId: string) => {
    setActivePrompt(promptId);
    let cursor = 0;
    let fullText = "";
    setMessages((prev) => [...prev, { role: "assistant", content: "" }]);
    while (true) {
      const page = await pollStream(promptId, cursor);
      for (const { event } of page.events) {
        if (event.type === "content_delta" && event.text) fullText += event.text;
        if (event.type === "clear_streaming_text") fullText = "";
      }
      cursor = page.next_cursor;
      const visible = page.done && page.output !== null ? page.output : fullText;
      setMessages((prev) => [...prev.slice(0, -1), { role: "assistant", content: visible }]);
      scrollToBottom();
      if (page.done) {
        if (page.status !== "completed") setError(page.error ?? page.status);
        sessionStorage.removeItem(PENDING_KEY);
        return;
      }
      if (page.events.length === 0) await new Promise((resolve) => setTimeout(resolve, 100));
    }
  };

  const reconnect = async () => {
    const opened = await openSession(sessionStorage.getItem(SESSION_KEY));
    sessionStorage.setItem(SESSION_KEY, opened.session_id);
    setSessionId(opened.session_id);
    const history = opened.history ?? [];
    setMessages(history);
    const pendingText = sessionStorage.getItem(PENDING_KEY);
    const pending = pendingText ? JSON.parse(pendingText) as { session: string; request: string; text: string } : null;
    if (pending?.session === opened.session_id) {
      const result = await executePrompt(pending.session, pending.text, pending.request);
      if (result.status === "completed") {
        sessionStorage.removeItem(PENDING_KEY);
        return;
      }
      if (history[history.length - 1]?.content !== pending.text) {
        setMessages((prev) => [...prev, { role: "user", content: pending.text }]);
      }
      await replayPrompt(result.prompt_id);
    } else {
      sessionStorage.removeItem(PENDING_KEY);
      if (opened.latest_prompt && opened.latest_prompt.status !== "completed") {
        await replayPrompt(opened.latest_prompt.prompt_id);
      }
    }
  };

  const run = async (operation: () => Promise<void>) => {
    setLoading(true);
    setError(null);
    try {
      await operation();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Unknown error");
    } finally {
      setActivePrompt(null);
      setLoading(false);
      scrollToBottom();
    }
  };

  useEffect(() => { void run(reconnect); }, []);

  const handleSubmit = async (e: JSX.TargetedEvent<HTMLFormElement>) => {
    e.preventDefault();
    const text = input.trim();
    if (!text || loading || !sessionId) return;
    setInput("");
    setMessages((prev) => [...prev, { role: "user", content: text }]);
    await run(async () => {
      const request = crypto.randomUUID();
      sessionStorage.setItem(PENDING_KEY, JSON.stringify({ session: sessionId, request, text }));
      const result = await executePrompt(sessionId, text, request);
      await replayPrompt(result.prompt_id);
    });
  };

  return (
    <div style={styles.container}>
      <header style={styles.header}>
        <span style={styles.logo}>CodingBuddy</span>
        <span style={styles.badge}>Web UI</span>
      </header>

      <main style={styles.messages}>
        {messages.length === 0 && (
          <div style={styles.empty}>
            <p style={styles.emptyTitle}>Welcome to CodingBuddy</p>
            <p style={styles.emptyHint}>
              Type a message below to start a conversation.
            </p>
          </div>
        )}
        {messages.map((msg, i) => (
          <div
            key={i}
            style={{
              ...styles.message,
              ...(msg.role === "user" ? styles.userMessage : {}),
            }}
          >
            <div style={styles.messageRole}>
              {msg.role === "user" ? "You" : "Assistant"}
            </div>
            <div style={styles.messageContent}>
              <pre style={styles.pre}>{msg.content}</pre>
            </div>
          </div>
        ))}
        {loading && (
          <div style={styles.message}>
            <div style={styles.messageRole}>Assistant</div>
            <div style={{ ...styles.messageContent, ...styles.thinking }}>
              Thinking...
            </div>
          </div>
        )}
        {error && <div style={styles.error}>{error}</div>}
        <div ref={messagesEnd} />
      </main>

      <form style={styles.inputArea} onSubmit={handleSubmit}>
        <input
          type="text"
          value={input}
          onInput={(e) => setInput((e.target as HTMLInputElement).value)}
          placeholder={
            loading ? "Waiting for response..." : "Ask anything..."
          }
          disabled={loading}
          style={styles.input}
          autoFocus
        />
        <button type="submit" disabled={loading || !sessionId || !input.trim()} style={styles.button}>
          Send
        </button>
        {error && !loading && (
          <button type="button" style={styles.button} onClick={() => { void run(reconnect); }}>Reconnect</button>
        )}
        {activePrompt && (
          <button type="button" style={styles.button} onClick={() => {
            void cancelPrompt(activePrompt).catch((err) => setError(String(err)));
          }}>Cancel</button>
        )}
      </form>
    </div>
  );
}

const styles: Record<string, Record<string, string | number>> = {
  container: {
    display: "flex",
    flexDirection: "column",
    height: "100%",
    maxWidth: 900,
    margin: "0 auto",
  },
  header: {
    display: "flex",
    alignItems: "center",
    gap: 8,
    padding: "12px 16px",
    borderBottom: "1px solid var(--border)",
  },
  logo: {
    fontWeight: 700,
    fontSize: 16,
    color: "var(--accent)",
    fontFamily: "var(--font-mono)",
  },
  badge: {
    fontSize: 11,
    padding: "2px 6px",
    borderRadius: 4,
    background: "var(--accent-dim)",
    color: "var(--text)",
  },
  messages: {
    flex: 1,
    overflowY: "auto",
    padding: "16px",
  },
  empty: {
    textAlign: "center",
    marginTop: 120,
    color: "var(--text-muted)",
  },
  emptyTitle: {
    fontSize: 20,
    fontWeight: 600,
    marginBottom: 8,
    color: "var(--text)",
  },
  emptyHint: {
    fontSize: 14,
  },
  message: {
    marginBottom: 16,
    padding: "12px 16px",
    borderRadius: 8,
    background: "var(--surface)",
  },
  userMessage: {
    background: "var(--accent-dim)",
    marginLeft: 48,
  },
  messageRole: {
    fontSize: 11,
    fontWeight: 600,
    textTransform: "uppercase",
    color: "var(--text-muted)",
    marginBottom: 4,
    letterSpacing: 0.5,
  },
  messageContent: {
    fontSize: 14,
    lineHeight: 1.6,
  },
  pre: {
    fontFamily: "var(--font-mono)",
    fontSize: 13,
    whiteSpace: "pre-wrap",
    wordBreak: "break-word",
    margin: 0,
    color: "var(--text)",
  },
  thinking: {
    color: "var(--text-muted)",
    fontStyle: "italic",
  },
  error: {
    padding: "8px 12px",
    borderRadius: 6,
    background: "rgba(247, 118, 142, 0.15)",
    color: "var(--error)",
    fontSize: 13,
    marginBottom: 12,
  },
  inputArea: {
    display: "flex",
    gap: 8,
    padding: "12px 16px",
    borderTop: "1px solid var(--border)",
    background: "var(--bg)",
  },
  input: {
    flex: 1,
    padding: "10px 14px",
    borderRadius: 8,
    border: "1px solid var(--border)",
    background: "var(--surface)",
    color: "var(--text)",
    fontSize: 14,
    fontFamily: "var(--font-sans)",
    outline: "none",
  },
  button: {
    padding: "10px 20px",
    borderRadius: 8,
    border: "none",
    background: "var(--accent)",
    color: "#1a1b26",
    fontSize: 14,
    fontWeight: 600,
    cursor: "pointer",
  },
};
