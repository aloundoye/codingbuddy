//! Streaming tool executor — starts executing tools as they arrive during LLM streaming.
//!
//! When the LLM streams a response with tool calls, each tool call's arguments are
//! accumulated incrementally. Once complete, the tool call is sent to this executor.
//! Concurrency-safe tools (reads, searches) start immediately in parallel.
//! Non-safe tools wait until all parallel executions finish.
//!
//! Results are maintained in original order for the LLM's next turn.

use codingbuddy_core::LlmToolCall;
use std::sync::mpsc;

/// A completed tool call ready for execution, sent from the streaming callback.
#[derive(Debug, Clone)]
pub struct ReadyToolCall {
    pub call: LlmToolCall,
    /// Index in the original tool call list (for ordering results).
    pub index: usize,
    /// Whether this tool can run in parallel with other safe tools.
    pub concurrency_safe: bool,
}

/// Receiver half of the streaming executor channel.
/// The tool loop drains this after the LLM stream finishes to get tool calls
/// that were detected during streaming.
pub struct StreamingToolReceiver {
    rx: mpsc::Receiver<ReadyToolCall>,
}

impl StreamingToolReceiver {
    /// Drain all ready tool calls from the channel.
    pub fn drain(&self) -> Vec<ReadyToolCall> {
        let mut calls = Vec::new();
        while let Ok(call) = self.rx.try_recv() {
            calls.push(call);
        }
        calls
    }
}

/// Sender half — cloned into the streaming callback to send ready tool calls.
#[derive(Clone)]
pub struct StreamingToolSender {
    tx: mpsc::Sender<ReadyToolCall>,
}

impl StreamingToolSender {
    /// Send a completed tool call to the executor.
    pub fn send(&self, call: ReadyToolCall) {
        // Best-effort: if receiver is dropped, silently ignore
        let _ = self.tx.send(call);
    }
}

/// Create a paired sender/receiver for streaming tool execution.
pub fn channel() -> (StreamingToolSender, StreamingToolReceiver) {
    let (tx, rx) = mpsc::channel();
    (StreamingToolSender { tx }, StreamingToolReceiver { rx })
}

/// Detect complete tool calls from the accumulated streaming state.
///
/// The OpenAI streaming format sends tool call deltas incrementally:
/// - First chunk: `{index: 0, id: "call_1", function: {name: "fs_read", arguments: ""}}`
/// - Subsequent chunks: `{index: 0, function: {arguments: "{\"path\":"}}`
/// - More chunks: `{index: 0, function: {arguments: "\"src/main.rs\"}"}}`
///
/// A tool call is "ready" when:
/// 1. A new index arrives (the previous tool call is complete)
/// 2. The stream finishes (all accumulated tool calls are complete)
///
/// This function is called from the streaming callback to detect when
/// a tool call transitions from "accumulating" to "ready".
pub fn detect_ready_tool_calls(
    tool_calls: &[LlmToolCall],
    previously_sent: usize,
) -> Vec<(usize, LlmToolCall)> {
    // All tool calls with index < tool_calls.len() - 1 are complete
    // (a new index was started, so the previous one finished).
    // The last one may still be accumulating arguments.
    let mut ready = Vec::new();
    if tool_calls.len() > 1 {
        for (i, tc) in tool_calls.iter().enumerate().skip(previously_sent) {
            if i < tool_calls.len() - 1 {
                ready.push((i, tc.clone()));
            }
        }
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_sends_and_receives() {
        let (tx, rx) = channel();
        tx.send(ReadyToolCall {
            call: LlmToolCall {
                id: "call_1".to_string(),
                name: "fs_read".to_string(),
                arguments: r#"{"path":"test.rs"}"#.to_string(),
            },
            index: 0,
            concurrency_safe: true,
        });
        tx.send(ReadyToolCall {
            call: LlmToolCall {
                id: "call_2".to_string(),
                name: "fs_grep".to_string(),
                arguments: r#"{"pattern":"todo"}"#.to_string(),
            },
            index: 1,
            concurrency_safe: true,
        });
        let drained = rx.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].call.name, "fs_read");
        assert_eq!(drained[1].call.name, "fs_grep");
    }

    #[test]
    fn detect_ready_skips_last_incomplete() {
        let calls = vec![
            LlmToolCall {
                id: "call_1".to_string(),
                name: "fs_read".to_string(),
                arguments: r#"{"path":"a.rs"}"#.to_string(),
            },
            LlmToolCall {
                id: "call_2".to_string(),
                name: "fs_grep".to_string(),
                arguments: r#"{"pat"#.to_string(), // still accumulating
            },
        ];
        let ready = detect_ready_tool_calls(&calls, 0);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].1.name, "fs_read");
    }

    #[test]
    fn detect_ready_respects_previously_sent() {
        let calls = vec![
            LlmToolCall {
                id: "call_1".to_string(),
                name: "fs_read".to_string(),
                arguments: "{}".to_string(),
            },
            LlmToolCall {
                id: "call_2".to_string(),
                name: "fs_grep".to_string(),
                arguments: "{}".to_string(),
            },
            LlmToolCall {
                id: "call_3".to_string(),
                name: "fs_glob".to_string(),
                arguments: r#"{"partial"#.to_string(),
            },
        ];
        // Already sent index 0
        let ready = detect_ready_tool_calls(&calls, 1);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].1.name, "fs_grep");
    }

    #[test]
    fn detect_ready_empty_for_single_tool() {
        let calls = vec![LlmToolCall {
            id: "call_1".to_string(),
            name: "fs_read".to_string(),
            arguments: r#"{"path":"a.rs"}"#.to_string(),
        }];
        let ready = detect_ready_tool_calls(&calls, 0);
        assert!(
            ready.is_empty(),
            "single tool call may still be accumulating"
        );
    }
}
