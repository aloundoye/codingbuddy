//! One resolved transport contract for a chat request.
use crate::{
    error::{FailureKind, ProviderError},
    protocol::{self, AuthStrategy, ChatProtocol},
};
use anyhow::{Context, Result};
use codingbuddy_core::{LlmConfig, ModelCapabilities, ProviderConfig};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

pub(crate) struct ChatRoute {
    pub provider: ProviderConfig,
    pub protocol: ChatProtocol,
    pub auth: AuthStrategy,
    pub endpoint: String,
    pub capabilities: ModelCapabilities,
    pub capability_rules: Vec<String>,
}

impl ChatRoute {
    pub fn resolve(cfg: &LlmConfig, model: &str, streaming: bool) -> Result<Self> {
        let provider = cfg.active_provider();
        let kind = cfg.active_provider_kind().context("Unknown provider")?;
        let protocol = protocol::select_chat_protocol(&provider, kind);
        let auth = protocol::select_auth_strategy(&provider, kind, protocol);
        let explicit_protocol = provider.chat_protocol.as_deref().or_else(|| {
            provider
                .payload_options
                .get("chat_protocol")
                .or_else(|| provider.payload_options.get("protocol"))
                .and_then(serde_json::Value::as_str)
        });
        if explicit_protocol.is_some_and(|value| protocol::parse_chat_protocol(value).is_none())
            || provider
                .auth_strategy
                .as_deref()
                .is_some_and(|value| protocol::parse_auth_strategy(value).is_none())
        {
            return Err(ProviderError::new(protocol, FailureKind::UnsupportedFeature).into());
        }
        if protocol == ChatProtocol::BedrockConverse
            || auth == AuthStrategy::AwsSigV4
            || auth == AuthStrategy::QueryApiKey && protocol != ChatProtocol::GeminiGenerateContent
        {
            return Err(ProviderError::new(protocol, FailureKind::UnsupportedFeature).into());
        }
        let resolution = cfg
            .capability_resolution_for_model(model)
            .context("Unknown provider model capabilities")?;
        let capabilities = resolution.capabilities;
        let capability_rules = resolution.applied_rules;
        let base = provider.base_url.trim_end_matches('/');
        crate::validate_base_url(base)?;
        if !cfg.endpoint.is_empty() {
            crate::validate_base_url(&cfg.endpoint)?;
        }
        let endpoint = if !cfg.endpoint.is_empty() {
            cfg.endpoint.clone()
        } else {
            match protocol {
                ChatProtocol::OpenAiResponses => format!(
                    "{}/responses",
                    if base.ends_with("/v1") {
                        base.to_owned()
                    } else {
                        format!("{base}/v1")
                    }
                ),
                ChatProtocol::AnthropicMessages => format!(
                    "{}/messages",
                    if base.ends_with("/v1") {
                        base.to_owned()
                    } else {
                        format!("{base}/v1")
                    }
                ),
                ChatProtocol::GeminiGenerateContent => {
                    crate::providers::google::endpoint(base, model, streaming)
                }
                _ => {
                    let prefix = if provider.openai_compat_prefix && !base.ends_with("/v1") {
                        "/v1"
                    } else {
                        ""
                    };
                    format!("{base}{prefix}/chat/completions")
                }
            }
        };
        Ok(Self {
            provider,
            protocol,
            auth,
            endpoint,
            capabilities,
            capability_rules,
        })
    }

    pub fn headers(&self, key: Option<&str>) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        if self.protocol == ChatProtocol::AnthropicMessages {
            headers.insert(
                "anthropic-version",
                HeaderValue::from_static(crate::providers::anthropic::ANTHROPIC_VERSION),
            );
        }
        let credential = key.filter(|key| !key.trim().is_empty());
        if self.auth != AuthStrategy::None && credential.is_none() {
            return Err(ProviderError::new(self.protocol, FailureKind::Authentication).into());
        }
        if let Some(key) = credential {
            match self.auth {
                AuthStrategy::Bearer => {
                    headers.insert(
                        AUTHORIZATION,
                        HeaderValue::from_str(&format!("Bearer {key}"))
                            .context("Invalid provider credential header")?,
                    );
                }
                AuthStrategy::XApiKey => {
                    headers.insert(
                        "x-api-key",
                        HeaderValue::from_str(key).context("Invalid provider credential header")?,
                    );
                }
                AuthStrategy::QueryApiKey => {
                    headers.insert(
                        "x-goog-api-key",
                        HeaderValue::from_str(key).context("Invalid provider credential header")?,
                    );
                }
                _ => {}
            }
        }
        for (name, value) in &self.provider.headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).context("Invalid provider header name")?,
                HeaderValue::from_str(value).context("Invalid provider header value")?,
            );
        }
        Ok(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_respects_endpoint_override_and_one_v1() {
        for base in ["https://api.openai.com", "https://api.openai.com/v1/"] {
            let mut cfg = LlmConfig::default();
            cfg.select_provider("openai-compatible");
            let provider = cfg.providers.get_mut("openai-compatible").unwrap();
            provider.base_url = base.into();
            provider.chat_protocol = Some("openai-responses".into());
            assert_eq!(
                ChatRoute::resolve(&cfg, "gpt-4.1-mini", true)
                    .unwrap()
                    .endpoint,
                "https://api.openai.com/v1/responses"
            );
            cfg.endpoint = "https://gateway.example/custom-responses".into();
            assert_eq!(
                ChatRoute::resolve(&cfg, "gpt-4.1-mini", true)
                    .unwrap()
                    .endpoint,
                cfg.endpoint
            );
        }
    }

    #[test]
    fn unsupported_contracts_fail_during_resolution() {
        for (protocol, auth) in [
            ("bedrock-converse", "bearer"),
            ("openai-responses", "aws-sigv4"),
            ("openai-chat", "query-api-key"),
            ("unknown", "bearer"),
            ("openai-chat", "unknown"),
        ] {
            let mut cfg = LlmConfig::default();
            cfg.select_provider("openai-compatible");
            let provider = cfg.providers.get_mut("openai-compatible").unwrap();
            provider.chat_protocol = Some(protocol.into());
            provider.auth_strategy = Some(auth.into());
            assert!(ChatRoute::resolve(&cfg, "gpt-4.1-mini", false).is_err());
        }
    }
}
