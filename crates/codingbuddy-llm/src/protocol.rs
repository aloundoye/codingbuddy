//! Chat protocol selection for provider request/response handling.
//!
//! Providers and gateways often share an auth endpoint but speak different chat
//! wire protocols. Keeping protocol choice explicit prevents provider-specific
//! payload rules from spreading through the client.

use codingbuddy_core::{ProviderConfig, ProviderKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatProtocol {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    GeminiGenerateContent,
    BedrockConverse,
}

impl ChatProtocol {
    #[must_use]
    pub fn as_key(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GeminiGenerateContent => "gemini-generate-content",
            Self::BedrockConverse => "bedrock-converse",
        }
    }

    #[must_use]
    pub fn is_native(self) -> bool {
        matches!(
            self,
            Self::AnthropicMessages | Self::GeminiGenerateContent | Self::BedrockConverse
        )
    }

    #[must_use]
    pub fn uses_api_key_query(self) -> bool {
        matches!(self, Self::GeminiGenerateContent)
    }
}

#[must_use]
pub fn parse_chat_protocol(value: &str) -> Option<ChatProtocol> {
    match value.trim().to_ascii_lowercase().as_str() {
        "openai-chat" | "openai" | "chat-completions" | "openai-chat-completions" => {
            Some(ChatProtocol::OpenAiChat)
        }
        "openai-responses" | "responses" => Some(ChatProtocol::OpenAiResponses),
        "anthropic" | "anthropic-messages" | "messages" => Some(ChatProtocol::AnthropicMessages),
        "google" | "gemini" | "gemini-generate-content" | "generate-content" => {
            Some(ChatProtocol::GeminiGenerateContent)
        }
        "bedrock" | "bedrock-converse" | "converse" => Some(ChatProtocol::BedrockConverse),
        _ => None,
    }
}

#[must_use]
pub fn select_chat_protocol(provider: &ProviderConfig, kind: ProviderKind) -> ChatProtocol {
    provider
        .payload_options
        .get("chat_protocol")
        .or_else(|| provider.payload_options.get("protocol"))
        .and_then(|value| value.as_str())
        .and_then(parse_chat_protocol)
        .unwrap_or_else(|| default_chat_protocol(kind))
}

#[must_use]
pub fn default_chat_protocol(kind: ProviderKind) -> ChatProtocol {
    match kind {
        ProviderKind::Anthropic => ChatProtocol::AnthropicMessages,
        ProviderKind::Google => ChatProtocol::GeminiGenerateContent,
        // Native Bedrock Converse support is represented in the protocol enum,
        // but the current default remains OpenAI-compatible for existing users
        // who route Bedrock-compatible gateways through /chat/completions.
        ProviderKind::Bedrock => ChatProtocol::OpenAiChat,
        _ => ChatProtocol::OpenAiChat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codingbuddy_core::ProviderModels;
    use serde_json::json;

    fn provider(payload_options: serde_json::Value) -> ProviderConfig {
        ProviderConfig {
            kind: "openai-compatible".to_string(),
            base_url: "https://example.invalid".to_string(),
            api_key_env: "EXAMPLE_API_KEY".to_string(),
            openai_compat_prefix: true,
            payload_options,
            models: ProviderModels {
                chat: "example-model".to_string(),
                reasoner: None,
            },
        }
    }

    #[test]
    fn defaults_to_native_protocol_for_native_providers() {
        assert_eq!(
            default_chat_protocol(ProviderKind::Anthropic),
            ChatProtocol::AnthropicMessages
        );
        assert_eq!(
            default_chat_protocol(ProviderKind::Google),
            ChatProtocol::GeminiGenerateContent
        );
        assert_eq!(
            default_chat_protocol(ProviderKind::OpenAiCompatible),
            ChatProtocol::OpenAiChat
        );
    }

    #[test]
    fn payload_options_can_override_protocol() {
        let provider = provider(json!({ "chat_protocol": "openai-responses" }));
        assert_eq!(
            select_chat_protocol(&provider, ProviderKind::OpenAiCompatible),
            ChatProtocol::OpenAiResponses
        );
    }
}
