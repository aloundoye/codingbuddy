//! Chat protocol selection for provider request/response handling.
//!
//! Providers and gateways often share an auth endpoint but speak different chat
//! wire protocols. Keeping protocol choice explicit prevents provider-specific
//! payload rules from spreading through the client.

use codingbuddy_core::{ProviderConfig, ProviderKind};

pub mod adapters;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatProtocol {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    GeminiGenerateContent,
    BedrockConverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStrategy {
    Bearer,
    XApiKey,
    QueryApiKey,
    AwsSigV4,
    None,
}

impl AuthStrategy {
    #[must_use]
    pub fn as_key(self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::XApiKey => "x-api-key",
            Self::QueryApiKey => "query-api-key",
            Self::AwsSigV4 => "aws-sigv4",
            Self::None => "none",
        }
    }
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
        .chat_protocol
        .as_deref()
        .and_then(parse_chat_protocol)
        .or_else(|| {
            provider
                .payload_options
                .get("chat_protocol")
                .or_else(|| provider.payload_options.get("protocol"))
                .and_then(|value| value.as_str())
                .and_then(parse_chat_protocol)
        })
        .unwrap_or_else(|| default_chat_protocol(kind))
}

#[must_use]
pub fn parse_auth_strategy(value: &str) -> Option<AuthStrategy> {
    match value.trim().to_ascii_lowercase().as_str() {
        "bearer" | "bearer-token" | "authorization-bearer" => Some(AuthStrategy::Bearer),
        "x-api-key" | "api-key" | "api_key" | "header-api-key" => Some(AuthStrategy::XApiKey),
        "query-api-key" | "query" | "url-api-key" | "gemini-key" => Some(AuthStrategy::QueryApiKey),
        "aws-sigv4" | "sigv4" | "aws" => Some(AuthStrategy::AwsSigV4),
        "none" | "no-auth" | "anonymous" => Some(AuthStrategy::None),
        _ => None,
    }
}

#[must_use]
pub fn select_auth_strategy(
    provider: &ProviderConfig,
    kind: ProviderKind,
    protocol: ChatProtocol,
) -> AuthStrategy {
    provider
        .auth_strategy
        .as_deref()
        .and_then(parse_auth_strategy)
        .unwrap_or_else(|| default_auth_strategy(kind, protocol))
}

#[must_use]
pub fn default_auth_strategy(kind: ProviderKind, protocol: ChatProtocol) -> AuthStrategy {
    match (kind, protocol) {
        (_, ChatProtocol::GeminiGenerateContent) => AuthStrategy::QueryApiKey,
        (_, ChatProtocol::BedrockConverse) | (ProviderKind::Bedrock, _) => AuthStrategy::AwsSigV4,
        (ProviderKind::Anthropic, _) | (_, ChatProtocol::AnthropicMessages) => {
            AuthStrategy::XApiKey
        }
        (ProviderKind::Ollama, _) => AuthStrategy::None,
        _ => AuthStrategy::Bearer,
    }
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
            chat_protocol: None,
            auth_strategy: None,
            headers: std::collections::BTreeMap::new(),
            discovery: codingbuddy_core::ProviderDiscoveryConfig::default(),
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

    #[test]
    fn explicit_provider_protocol_precedes_payload_options() {
        let mut provider = provider(json!({ "protocol": "openai-chat" }));
        provider.chat_protocol = Some("anthropic-messages".to_string());
        assert_eq!(
            select_chat_protocol(&provider, ProviderKind::OpenAiCompatible),
            ChatProtocol::AnthropicMessages
        );
    }

    #[test]
    fn auth_strategy_defaults_follow_protocol() {
        let provider = provider(json!({}));
        assert_eq!(
            select_auth_strategy(
                &provider,
                ProviderKind::Google,
                ChatProtocol::GeminiGenerateContent
            ),
            AuthStrategy::QueryApiKey
        );
        assert_eq!(
            select_auth_strategy(&provider, ProviderKind::Anthropic, ChatProtocol::OpenAiChat),
            AuthStrategy::XApiKey
        );
    }
}
