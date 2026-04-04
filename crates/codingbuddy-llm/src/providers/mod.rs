//! Provider-specific implementations for LLM APIs.
//!
//! Each provider handles payload building, response parsing, endpoint routing,
//! and authentication. Anthropic and Google use native API formats; all others
//! use the OpenAI-compatible pipeline in `provider_transform.rs`.

pub mod anthropic;
pub mod google;

use codingbuddy_core::{LlmToolCall, StreamChunk};

/// Unified streaming event type used by the streaming loop.
/// Provider-specific parsers convert their native events into this type.
pub enum ProviderStreamEvent {
    /// Incremental text content.
    ContentDelta(String),
    /// Incremental reasoning/thinking content.
    ReasoningDelta(String),
    /// A tool call start (Anthropic: content_block_start with tool_use).
    ToolStart { id: String, name: String },
    /// Incremental tool arguments JSON (Anthropic: input_json_delta).
    ToolArgsDelta(String),
    /// A complete tool call in one chunk (Google: functionCall).
    ToolCallComplete(LlmToolCall),
    /// Finish reason for the response.
    Finish(String),
    /// Token usage information.
    Usage {
        input: u64,
        output: u64,
        cache_hit: u64,
    },
    /// Stream is done.
    Done,
    /// No meaningful event (skip).
    None,
}

/// Convert Anthropic StreamEvent into unified ProviderStreamEvent.
pub fn from_anthropic_event(event: anthropic::StreamEvent) -> ProviderStreamEvent {
    match event {
        anthropic::StreamEvent::TextDelta(s) => ProviderStreamEvent::ContentDelta(s),
        anthropic::StreamEvent::ThinkingDelta(s) => ProviderStreamEvent::ReasoningDelta(s),
        anthropic::StreamEvent::ToolStart { id, name } => {
            ProviderStreamEvent::ToolStart { id, name }
        }
        anthropic::StreamEvent::ToolArgsDelta(s) => ProviderStreamEvent::ToolArgsDelta(s),
        anthropic::StreamEvent::Finish(s) => ProviderStreamEvent::Finish(s),
        anthropic::StreamEvent::Usage {
            input,
            output,
            cache_hit,
        } => ProviderStreamEvent::Usage {
            input,
            output,
            cache_hit,
        },
        anthropic::StreamEvent::Done => ProviderStreamEvent::Done,
        anthropic::StreamEvent::None => ProviderStreamEvent::None,
    }
}

/// Convert Google StreamEvent into a list of unified ProviderStreamEvents.
pub fn from_google_event(event: google::StreamEvent) -> Vec<ProviderStreamEvent> {
    match event {
        google::StreamEvent::TextDelta(s) => vec![ProviderStreamEvent::ContentDelta(s)],
        google::StreamEvent::ToolCall {
            id,
            name,
            arguments,
        } => vec![ProviderStreamEvent::ToolCallComplete(LlmToolCall {
            id,
            name,
            arguments,
        })],
        google::StreamEvent::Finish(s) => vec![ProviderStreamEvent::Finish(s)],
        google::StreamEvent::Usage {
            input,
            output,
            cache_hit,
        } => vec![ProviderStreamEvent::Usage {
            input,
            output,
            cache_hit,
        }],
        google::StreamEvent::Multiple(events) => {
            events.into_iter().flat_map(from_google_event).collect()
        }
        google::StreamEvent::None => vec![],
    }
}

/// Mutable state for accumulating native provider streaming results.
#[derive(Default)]
pub struct NativeStreamState {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<LlmToolCall>,
    pub current_tool_id: Option<String>,
    pub current_tool_name: Option<String>,
    pub current_tool_args: String,
    pub finish_reason: Option<String>,
    pub usage: Option<codingbuddy_core::TokenUsage>,
}

impl NativeStreamState {
    /// Process a streaming event, calling back for UI updates. Returns true on Done.
    pub fn handle_event(
        &mut self,
        event: ProviderStreamEvent,
        stream_cb: &dyn Fn(StreamChunk),
    ) -> bool {
        match event {
            ProviderStreamEvent::ContentDelta(text) => {
                self.content.push_str(&text);
                stream_cb(StreamChunk::ContentDelta(text));
            }
            ProviderStreamEvent::ReasoningDelta(text) => {
                self.reasoning.push_str(&text);
                stream_cb(StreamChunk::ReasoningDelta(text));
            }
            ProviderStreamEvent::ToolStart { id, name } => {
                self.flush_current_tool();
                self.current_tool_id = Some(id);
                self.current_tool_name = Some(name);
                self.current_tool_args.clear();
            }
            ProviderStreamEvent::ToolArgsDelta(args) => {
                self.current_tool_args.push_str(&args);
            }
            ProviderStreamEvent::ToolCallComplete(tc) => {
                self.tool_calls.push(tc);
            }
            ProviderStreamEvent::Finish(reason) => {
                self.flush_current_tool();
                self.finish_reason = Some(reason);
            }
            ProviderStreamEvent::Usage {
                input,
                output,
                cache_hit,
            } => {
                let u = self.usage.get_or_insert_with(Default::default);
                u.prompt_tokens = u.prompt_tokens.max(input);
                u.completion_tokens = u.completion_tokens.max(output);
                u.prompt_cache_hit_tokens = u.prompt_cache_hit_tokens.max(cache_hit);
            }
            ProviderStreamEvent::Done => {
                self.flush_current_tool();
                stream_cb(StreamChunk::Done { reason: None });
                return true;
            }
            ProviderStreamEvent::None => {}
        }
        false
    }

    fn flush_current_tool(&mut self) {
        if let (Some(id), Some(name)) = (self.current_tool_id.take(), self.current_tool_name.take())
            && !name.is_empty()
        {
            self.tool_calls.push(LlmToolCall {
                id,
                name,
                arguments: std::mem::take(&mut self.current_tool_args),
            });
        }
        self.current_tool_args.clear();
    }
}
