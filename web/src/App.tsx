import { useState, useRef, useCallback } from "preact/hooks";
import type { JSX } from "preact";
import { openSession, executePrompt, pollStream } from "./api";

interface Message {
  role: "user" | "assistant" | "system";
  content: string;
}

export function App() {
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState("");
  const [loading, setLoading] = useState(false);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const messagesEnd = useRef<HTMLDivElement>(null);

  const scrollToBottom = useCallback(() => {
    messagesEnd.current?.scrollIntoView({ behavior: "smooth" });
  }, []);

  const ensureSession = async (): Promise<string> => {
    if (sessionId) return sessionId;
    const result = await openSession();
    setSessionId(result.session_id);
    return result.session_id;
  };

  const handleSubmit = async (e: JSX.TargetedEvent<HTMLFormElement>) => {
    e.preventDefault();
    const text = input.trim();
    if (!text || loading) return;

    setInput("");
    setError(null);
    setMessages((prev) => [...prev, { role: "user", content: text }]);
    setLoading(true);

    try {
      const sid = await ensureSession();
      const result = await executePrompt(sid, text);

      if (result.status === "failed") {
        setError(result.error || "Unknown error");
        setLoading(false);
        return;
      }

      // If streaming is available, poll for chunks
      if (result.stream) {
        let cursor = 0;
        let fullText = "";
        const promptId = result.stream.prompt_id;

        // Add placeholder assistant message
        setMessages((prev) => [...prev, { role: "assistant", content: "" }]);

        let done = false;
        while (!done) {
          const streamResult = await pollStream(promptId, cursor);
          for (const chunk of streamResult.chunks) {
            if (chunk.delta) {
              fullText += chunk.delta;
              setMessages((prev) => [
                ...prev.slice(0, -1),
                { role: "assistant" as const, content: fullText },
              ]);
            }
          }
          cursor = streamResult.next_cursor;
          done = streamResult.done;
          scrollToBottom();
          if (!done && streamResult.chunks.length === 0) {
            await new Promise((r) => setTimeout(r, 50));
          }
        }
      } else if (result.output) {
        setMessages((prev) => [
          ...prev,
          { role: "assistant", content: result.output! },
        ]);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Unknown error");
    } finally {
      setLoading(false);
      scrollToBottom();
    }
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
        <button type="submit" disabled={loading || !input.trim()} style={styles.button}>
          Send
        </button>
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
