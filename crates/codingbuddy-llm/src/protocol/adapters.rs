//! Typed protocol adapter metadata.
//!
//! This module is intentionally data-first. The request execution path can ask
//! an adapter what it supports and how it expects auth, endpoints, payloads, and
//! stream events to be shaped without branching on provider names.

use super::{AuthStrategy, ChatProtocol};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSchemaDialect {
    OpenAiTools,
    AnthropicTools,
    GeminiFunctionDeclarations,
    BedrockToolConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEventDialect {
    OpenAiDataDelta,
    AnthropicSse,
    GeminiSse,
    BedrockEventStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadShape {
    ChatCompletions,
    Responses,
    AnthropicMessages,
    GeminiGenerateContent,
    BedrockConverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolFeature {
    ToolCalls,
    StreamingToolDeltas,
    Reasoning,
    CacheHints,
    ImageInput,
    PdfInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolSupport {
    pub tool_calls: bool,
    pub streaming_tool_deltas: bool,
    pub reasoning: bool,
    pub cache_hints: bool,
    pub image_input: bool,
    pub pdf_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolAdapter {
    pub protocol: ChatProtocol,
    pub payload_shape: PayloadShape,
    pub tool_schema: ToolSchemaDialect,
    pub stream_events: StreamEventDialect,
    pub default_auth: AuthStrategy,
    pub support: ProtocolSupport,
}

impl ProtocolAdapter {
    #[must_use]
    pub fn supports(self, feature: ProtocolFeature) -> bool {
        match feature {
            ProtocolFeature::ToolCalls => self.support.tool_calls,
            ProtocolFeature::StreamingToolDeltas => self.support.streaming_tool_deltas,
            ProtocolFeature::Reasoning => self.support.reasoning,
            ProtocolFeature::CacheHints => self.support.cache_hints,
            ProtocolFeature::ImageInput => self.support.image_input,
            ProtocolFeature::PdfInput => self.support.pdf_input,
        }
    }

    #[must_use]
    pub fn unsupported_feature_error(self, feature: ProtocolFeature) -> Option<String> {
        if self.supports(feature) {
            None
        } else {
            Some(format!(
                "chat protocol '{}' does not support {:?}; choose a compatible model/provider or disable that mode",
                self.protocol.as_key(),
                feature
            ))
        }
    }
}

#[must_use]
pub fn adapter_for(protocol: ChatProtocol) -> ProtocolAdapter {
    match protocol {
        ChatProtocol::OpenAiChat => ProtocolAdapter {
            protocol,
            payload_shape: PayloadShape::ChatCompletions,
            tool_schema: ToolSchemaDialect::OpenAiTools,
            stream_events: StreamEventDialect::OpenAiDataDelta,
            default_auth: AuthStrategy::Bearer,
            support: ProtocolSupport {
                tool_calls: true,
                streaming_tool_deltas: true,
                reasoning: true,
                cache_hints: true,
                image_input: true,
                pdf_input: false,
            },
        },
        ChatProtocol::OpenAiResponses => ProtocolAdapter {
            protocol,
            payload_shape: PayloadShape::Responses,
            tool_schema: ToolSchemaDialect::OpenAiTools,
            stream_events: StreamEventDialect::OpenAiDataDelta,
            default_auth: AuthStrategy::Bearer,
            support: ProtocolSupport {
                tool_calls: true,
                streaming_tool_deltas: true,
                reasoning: true,
                cache_hints: true,
                image_input: true,
                pdf_input: true,
            },
        },
        ChatProtocol::AnthropicMessages => ProtocolAdapter {
            protocol,
            payload_shape: PayloadShape::AnthropicMessages,
            tool_schema: ToolSchemaDialect::AnthropicTools,
            stream_events: StreamEventDialect::AnthropicSse,
            default_auth: AuthStrategy::XApiKey,
            support: ProtocolSupport {
                tool_calls: true,
                streaming_tool_deltas: true,
                reasoning: true,
                cache_hints: true,
                image_input: true,
                pdf_input: true,
            },
        },
        ChatProtocol::GeminiGenerateContent => ProtocolAdapter {
            protocol,
            payload_shape: PayloadShape::GeminiGenerateContent,
            tool_schema: ToolSchemaDialect::GeminiFunctionDeclarations,
            stream_events: StreamEventDialect::GeminiSse,
            default_auth: AuthStrategy::QueryApiKey,
            support: ProtocolSupport {
                tool_calls: true,
                streaming_tool_deltas: true,
                reasoning: true,
                cache_hints: false,
                image_input: true,
                pdf_input: true,
            },
        },
        ChatProtocol::BedrockConverse => ProtocolAdapter {
            protocol,
            payload_shape: PayloadShape::BedrockConverse,
            tool_schema: ToolSchemaDialect::BedrockToolConfig,
            stream_events: StreamEventDialect::BedrockEventStream,
            default_auth: AuthStrategy::AwsSigV4,
            support: ProtocolSupport {
                tool_calls: true,
                streaming_tool_deltas: true,
                reasoning: true,
                cache_hints: false,
                image_input: true,
                pdf_input: true,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_expose_protocol_capabilities() {
        let openai = adapter_for(ChatProtocol::OpenAiResponses);
        assert_eq!(openai.payload_shape, PayloadShape::Responses);
        assert!(openai.supports(ProtocolFeature::PdfInput));

        let gemini = adapter_for(ChatProtocol::GeminiGenerateContent);
        assert_eq!(gemini.default_auth, AuthStrategy::QueryApiKey);
        assert!(!gemini.supports(ProtocolFeature::CacheHints));
        assert!(
            gemini
                .unsupported_feature_error(ProtocolFeature::CacheHints)
                .is_some()
        );
    }
}
