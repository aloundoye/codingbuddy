//! Streaming events for UIs (StreamChunk) and turn cancellation.

/// A single chunk emitted during streaming.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// A content text delta.
    ContentDelta(String),
    /// A reasoning/thinking text delta.
    ReasoningDelta(String),
    /// A tool call has started execution.
    ToolCallStart {
        tool_name: String,
        args_summary: String,
    },
    /// A tool call has completed execution.
    ToolCallEnd {
        tool_name: String,
        duration_ms: u64,
        success: bool,
        summary: String,
    },
    /// Agent mode transition.
    ModeTransition {
        from: String,
        to: String,
        reason: String,
    },
    /// A subagent was spawned for a complex task lane.
    SubagentSpawned {
        run_id: String,
        name: String,
        goal: String,
    },
    /// A subagent completed successfully.
    SubagentCompleted {
        run_id: String,
        name: String,
        summary: String,
    },
    /// A subagent failed.
    SubagentFailed {
        run_id: String,
        name: String,
        error: String,
    },
    /// An image was read and should be displayed inline in the terminal.
    ImageData { data: Vec<u8>, label: String },
    /// Watch mode auto-triggered because comment digest changed.
    WatchTriggered { digest: u64, comment_count: usize },
    /// A security warning detected in tool output (prompt injection, suspicious patterns).
    SecurityWarning { message: String },
    /// A tool call's arguments are fully streamed and ready for execution.
    /// Emitted during LLM streaming to enable early dispatch.
    ToolCallReady {
        id: String,
        name: String,
        arguments: String,
    },
    /// Clear any previously streamed text — the response contains tool calls,
    /// so the interleaved text fragments should be discarded from the display.
    ClearStreamingText,
    /// Streaming token usage update — emitted after each LLM call for cost/progress tracking.
    UsageUpdate {
        input_tokens: u64,
        output_tokens: u64,
        cache_hit_tokens: u64,
        estimated_cost_usd: f64,
    },
    /// A per-step snapshot was recorded for undo capability.
    SnapshotRecorded {
        snapshot_id: String,
        tool_name: String,
        files: Vec<String>,
    },
    /// A recoverable or non-recoverable error occurred during processing.
    Error { message: String, recoverable: bool },
    /// Phase transition in the explicit Explore->Plan->Execute->Verify loop.
    PhaseTransition { from: String, to: String },
    /// The active model was changed mid-session (via `/model` command).
    ModelChanged { model: String },
    /// Config file changed on disk; agent reloaded settings.
    ConfigReloaded { path: String },
    /// Live progress data from a running tool (e.g. bash stdout lines).
    ToolProgress { tool_name: String, data: String },
    /// Rate-limited by the provider; retrying after a delay.
    RateLimited {
        wait_seconds: u64,
        attempt: u8,
        max_attempts: u8,
        provider: String,
    },
    /// The conversation is filling the context window and will be compacted
    /// soon. Sent once per crossing of the warning threshold.
    ContextWarning {
        /// Estimated share of the usable input budget in use, 0-100.
        used_pct: u8,
        /// Share at which automatic compaction starts, 0-100.
        compact_at_pct: u8,
    },
    /// Streaming is done; the final assembled response follows.
    /// An optional reason string explains *why* the agent stopped
    /// (e.g. "max iterations reached", "plan dedup", content filter).
    Done { reason: Option<String> },
}

/// Canonical stream-json event representation used across CLI/TUI/RPC adapters.
pub fn stream_chunk_to_event_json(chunk: &StreamChunk) -> serde_json::Value {
    match chunk {
        StreamChunk::ContentDelta(text) => serde_json::json!({
            "type": "content_delta",
            "text": text,
        }),
        StreamChunk::ReasoningDelta(text) => serde_json::json!({
            "type": "reasoning_delta",
            "text": text,
        }),
        StreamChunk::ToolCallStart {
            tool_name,
            args_summary,
        } => serde_json::json!({
            "type": "tool_start",
            "tool_name": tool_name,
            "args_summary": args_summary,
        }),
        StreamChunk::ToolCallEnd {
            tool_name,
            duration_ms,
            success,
            summary,
        } => serde_json::json!({
            "type": "tool_end",
            "tool_name": tool_name,
            "duration_ms": duration_ms,
            "success": success,
            "summary": summary,
        }),
        StreamChunk::ModeTransition { from, to, reason } => serde_json::json!({
            "type": "mode_transition",
            "from": from,
            "to": to,
            "reason": reason,
        }),
        StreamChunk::SubagentSpawned { run_id, name, goal } => serde_json::json!({
            "type": "subagent_spawned",
            "run_id": run_id,
            "name": name,
            "goal": goal,
        }),
        StreamChunk::SubagentCompleted {
            run_id,
            name,
            summary,
        } => serde_json::json!({
            "type": "subagent_completed",
            "run_id": run_id,
            "name": name,
            "summary": summary,
        }),
        StreamChunk::SubagentFailed {
            run_id,
            name,
            error,
        } => serde_json::json!({
            "type": "subagent_failed",
            "run_id": run_id,
            "name": name,
            "error": error,
        }),
        StreamChunk::ImageData { label, .. } => serde_json::json!({
            "type": "image",
            "label": label,
        }),
        StreamChunk::WatchTriggered {
            digest,
            comment_count,
        } => serde_json::json!({
            "type": "watch_triggered",
            "digest": digest,
            "comment_count": comment_count,
        }),
        StreamChunk::SecurityWarning { message } => serde_json::json!({
            "type": "security_warning",
            "message": message,
        }),
        StreamChunk::ContextWarning {
            used_pct,
            compact_at_pct,
        } => serde_json::json!({
            "type": "context_warning",
            "used_pct": used_pct,
            "compact_at_pct": compact_at_pct,
        }),
        StreamChunk::ToolCallReady {
            id,
            name,
            arguments,
        } => serde_json::json!({
            "type": "tool_call_ready",
            "id": id,
            "name": name,
            "arguments": arguments,
        }),
        StreamChunk::ClearStreamingText => serde_json::json!({
            "type": "clear_streaming_text",
        }),
        StreamChunk::UsageUpdate {
            input_tokens,
            output_tokens,
            cache_hit_tokens,
            estimated_cost_usd,
        } => serde_json::json!({
            "type": "usage_update",
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_hit_tokens": cache_hit_tokens,
            "estimated_cost_usd": estimated_cost_usd,
        }),
        StreamChunk::SnapshotRecorded {
            snapshot_id,
            tool_name,
            files,
        } => serde_json::json!({
            "type": "snapshot_recorded",
            "snapshot_id": snapshot_id,
            "tool_name": tool_name,
            "files": files,
        }),
        StreamChunk::Error {
            message,
            recoverable,
        } => serde_json::json!({
            "type": "error",
            "message": message,
            "recoverable": recoverable,
        }),
        StreamChunk::PhaseTransition { from, to } => serde_json::json!({
            "type": "phase_transition",
            "from": from,
            "to": to,
        }),
        StreamChunk::ModelChanged { model } => serde_json::json!({
            "type": "model_changed",
            "model": model,
        }),
        StreamChunk::ConfigReloaded { path } => serde_json::json!({
            "type": "config_reloaded",
            "path": path,
        }),
        StreamChunk::ToolProgress { tool_name, data } => serde_json::json!({
            "type": "tool_progress",
            "tool_name": tool_name,
            "data": data,
        }),
        StreamChunk::RateLimited {
            wait_seconds,
            attempt,
            max_attempts,
            provider,
        } => serde_json::json!({
            "type": "rate_limited",
            "wait_seconds": wait_seconds,
            "attempt": attempt,
            "max_attempts": max_attempts,
            "provider": provider,
        }),
        StreamChunk::Done { reason } => {
            let mut obj = serde_json::json!({ "type": "done" });
            if let Some(r) = reason {
                obj["reason"] = serde_json::json!(r);
            }
            obj
        }
    }
}

/// Callback type for receiving streaming chunks.
/// Uses `Arc<dyn Fn>` so it can be cloned across multiple turns in a chat loop.
pub type StreamCallback = std::sync::Arc<dyn Fn(StreamChunk) + Send + Sync>;

/// A thread-safe cancellation token for aborting in-progress streaming requests.
/// Set by the UI layer (e.g. on Ctrl+C), checked by the LLM streaming loop.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    state: std::sync::Arc<CancellationState>,
}

#[derive(Debug)]
struct CancellationState {
    cancelled: std::sync::atomic::AtomicBool,
    parent: Option<std::sync::Arc<CancellationState>>,
    /// Wakes tasks awaiting [`CancellationToken::cancelled`] on this link.
    notify: tokio::sync::Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            state: std::sync::Arc::new(CancellationState {
                cancelled: false.into(),
                parent: None,
                notify: tokio::sync::Notify::new(),
            }),
        }
    }
    /// A child observes its ancestors, but cannot cancel its parent or siblings.
    pub fn child_token(&self) -> Self {
        Self {
            state: std::sync::Arc::new(CancellationState {
                cancelled: false.into(),
                parent: Some(self.state.clone()),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }
    pub fn cancel(&self) {
        self.state
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.state.notify.notify_waiters();
    }

    /// Resolves once this token or any ancestor is cancelled. Wakes as soon
    /// as `cancel()` runs; nothing polls.
    pub async fn cancelled(&self) {
        loop {
            // Register on every link before re-checking the flags, so a
            // cancel that lands between the check and the await still wakes
            // us (notify_waiters only wakes registered waiters).
            let mut chain = Vec::new();
            let mut state = Some(self.state.as_ref());
            while let Some(current) = state {
                chain.push(current);
                state = current.parent.as_deref();
            }
            let mut waits: Vec<_> = chain
                .iter()
                .map(|link| Box::pin(link.notify.notified()))
                .collect();
            for wait in &mut waits {
                wait.as_mut().enable();
            }
            if self.is_cancelled() {
                return;
            }
            std::future::poll_fn(|cx| {
                for wait in &mut waits {
                    if wait.as_mut().poll(cx).is_ready() {
                        return std::task::Poll::Ready(());
                    }
                }
                std::task::Poll::Pending
            })
            .await;
        }
    }
    pub fn is_cancelled(&self) -> bool {
        let mut state = Some(self.state.as_ref());
        while let Some(current) = state {
            if current.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                return true;
            }
            state = current.parent.as_deref();
        }
        false
    }
    /// Compatibility only: active scopes must create fresh tokens. A shared or
    /// parented token cannot be reset and resurrect an owned operation.
    pub fn reset(&self) {
        if self.state.parent.is_none() && std::sync::Arc::strong_count(&self.state) == 1 {
            self.state
                .cancelled
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}
