//! One resolved transport contract for a chat request.
use crate::{
    error::{FailureKind, ProviderError},
    protocol::{self, AuthStrategy, ChatProtocol},
};
use anyhow::{Context, Result};
use codingbuddy_core::{
    AzureApiVersion, CloudDeployment, LlmConfig, ModelCapabilities, ProviderConfig, ProviderKind,
};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

pub(crate) struct ChatRoute {
    pub provider: ProviderConfig,
    pub protocol: ChatProtocol,
    pub auth: AuthStrategy,
    pub endpoint: String,
    pub deployment: Option<String>,
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
        if (protocol == ChatProtocol::BedrockConverse || auth == AuthStrategy::AwsSigV4)
            && !matches!(provider.cloud, Some(CloudDeployment::Bedrock { .. }))
            || auth == AuthStrategy::QueryApiKey && protocol != ChatProtocol::GeminiGenerateContent
        {
            return Err(ProviderError::new(protocol, FailureKind::UnsupportedFeature).into());
        }
        let resolution = cfg
            .capability_resolution_for_model(model)
            .context("Unknown provider model capabilities")?;
        let capabilities = resolution.capabilities;
        let capability_rules = resolution.applied_rules;
        let custom_headers = custom_headers(&provider)?;
        if provider.gateway.is_some() {
            anyhow::ensure!(
                provider.cloud.is_none()
                    && cfg.endpoint.is_empty()
                    && matches!(
                        protocol,
                        ChatProtocol::OpenAiChat | ChatProtocol::OpenAiResponses
                    ),
                "Gateway paths require an OpenAI-compatible route and a single endpoint source"
            );
        }
        let deployment = if let Some(cloud) = &provider.cloud {
            anyhow::ensure!(
                cfg.endpoint.is_empty() && provider.auth_strategy.is_none(),
                "Cloud routes derive their endpoint and authentication from the deployment"
            );
            anyhow::ensure!(
                custom_headers.is_empty(),
                "Cloud routes use service-defined headers"
            );
            if auth.uses_cloud_identity() {
                anyhow::ensure!(
                    provider
                        .account_id
                        .as_ref()
                        .is_some_and(|id| !id.trim().is_empty()),
                    "Set account_id to a stable label for this cloud principal"
                );
            }
            if matches!(cloud, CloudDeployment::Azure { .. }) {
                anyhow::ensure!(
                    model == provider.models.chat,
                    "Azure deployment requires its configured model profile"
                );
            }
            Some(cloud_endpoint(cloud, kind, protocol, model, streaming)?)
        } else {
            None
        };
        let base = provider.base_url.trim_end_matches('/');
        if deployment.is_none() {
            crate::validate_base_url(base)?;
            if !cfg.endpoint.is_empty() {
                crate::validate_base_url(&cfg.endpoint)?;
            }
        }
        let endpoint = if let Some((endpoint, _)) = &deployment {
            endpoint.clone()
        } else if let Some(gateway) = &provider.gateway {
            gateway_endpoint(base, &gateway.chat_path, model)?
        } else if !cfg.endpoint.is_empty() {
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
            deployment: deployment.and_then(|(_, deployment)| deployment),
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
                AuthStrategy::Bearer | AuthStrategy::AzureEntra | AuthStrategy::GoogleAdc => {
                    headers.insert(
                        AUTHORIZATION,
                        HeaderValue::from_str(&format!("Bearer {key}"))
                            .context("Invalid provider credential header")?,
                    );
                }
                AuthStrategy::AzureApiKey => {
                    headers.insert(
                        "api-key",
                        HeaderValue::from_str(key).context("Invalid provider credential header")?,
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
        headers.extend(custom_headers(&self.provider)?);
        for name in [
            AUTHORIZATION.as_str(),
            "api-key",
            "x-api-key",
            "x-goog-api-key",
        ] {
            if let Some(value) = headers.get_mut(name) {
                value.set_sensitive(true);
            }
        }
        Ok(headers)
    }
}

pub(crate) fn custom_headers(provider: &ProviderConfig) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in &provider.headers {
        let name =
            HeaderName::from_bytes(name.as_bytes()).context("Invalid provider header name")?;
        anyhow::ensure!(
            !matches!(
                name.as_str(),
                "authorization"
                    | "proxy-authorization"
                    | "api-key"
                    | "x-api-key"
                    | "x-goog-api-key"
                    | "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "upgrade"
                    | "content-type"
                    | "cookie"
                    | "forwarded"
            ) && !name.as_str().starts_with("x-forwarded-")
                && !name.as_str().starts_with("x-amz-"),
            "Reserved provider header"
        );
        headers.insert(
            name,
            HeaderValue::from_str(value).context("Invalid provider header value")?,
        );
    }
    Ok(headers)
}

fn gateway_endpoint(base: &str, path: &str, model: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base)?;
    let segments = path
        .strip_prefix('/')
        .context("Gateway chat_path must begin with /")?;
    let mut path = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid gateway base"))?;
    path.clear();
    for segment in segments.split('/') {
        anyhow::ensure!(
            !segment.is_empty() && !matches!(segment, "." | ".."),
            "Invalid gateway path segment"
        );
        if segment == "{model}" {
            anyhow::ensure!(
                !matches!(model, "" | "." | ".."),
                "Invalid gateway model path"
            );
            path.push(model);
        } else {
            anyhow::ensure!(
                segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')),
                "Invalid gateway path segment"
            );
            path.push(segment);
        }
    }
    drop(path);
    Ok(url.into())
}

pub(crate) fn label(value: &str) -> Result<&str> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid cloud deployment identifier"
    );
    Ok(value)
}

fn cloud_endpoint(
    cloud: &CloudDeployment,
    kind: ProviderKind,
    protocol: ChatProtocol,
    model: &str,
    streaming: bool,
) -> Result<(String, Option<String>)> {
    if let Some(adapter) = crate::extension::registered_adapter(protocol) {
        return Ok((adapter.endpoint(kind, cloud, model, streaming)?, None));
    }
    match (cloud, kind, protocol) {
        (
            CloudDeployment::Azure {
                resource,
                deployment,
                api_version,
                ..
            },
            ProviderKind::Azure,
            ChatProtocol::OpenAiChat,
        ) => {
            let base = format!("https://{}.openai.azure.com", label(resource)?);
            anyhow::ensure!(
                !deployment.is_empty()
                    && deployment.len() <= 128
                    && deployment
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                    && !matches!(deployment.as_str(), "." | ".."),
                "Invalid Azure deployment name"
            );
            let endpoint = match api_version {
                AzureApiVersion::V1 => format!("{base}/openai/v1/chat/completions"),
                AzureApiVersion::Dated20241021 => format!(
                    "{base}/openai/deployments/{deployment}/chat/completions?api-version=2024-10-21"
                ),
            };
            Ok((endpoint, Some(deployment.clone())))
        }
        (
            CloudDeployment::Vertex { project, location },
            ProviderKind::Vertex,
            ChatProtocol::GeminiGenerateContent,
        ) => {
            let project = label(project)?;
            let location = label(location)?;
            anyhow::ensure!(
                !model.is_empty()
                    && model.len() <= 128
                    && model
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && model
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_')),
                "Invalid Vertex publisher model"
            );
            let host = if location == "global" {
                "aiplatform.googleapis.com".into()
            } else {
                format!("{location}-aiplatform.googleapis.com")
            };
            let method = if streaming {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            Ok((
                format!(
                    "https://{host}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:{method}"
                ),
                None,
            ))
        }
        _ => Err(ProviderError::new(protocol, FailureKind::UnsupportedFeature).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_rejects_override_and_pins_deployment_and_account() -> Result<()> {
        let mut cfg = LlmConfig::default();
        cfg.select_provider("azure");
        let profile = cfg.providers.get_mut("azure").unwrap();
        profile.account_id = Some("local-principal".into());
        profile.cloud = Some(CloudDeployment::Azure {
            resource: "safe-resource".into(),
            deployment: "deployment-one".into(),
            api_version: AzureApiVersion::V1,
            credential: codingbuddy_core::AzureCredential::Entra,
        });
        let first = crate::session_route(&cfg, "gpt-4o", b"synthetic fixture key")?;
        cfg.endpoint = "http://127.0.0.1:1/unauthorized".into();
        assert!(ChatRoute::resolve(&cfg, "gpt-4o", false).is_err());
        cfg.endpoint.clear();
        cfg.providers.get_mut("azure").unwrap().account_id = Some("other-principal".into());
        assert_ne!(
            first,
            crate::session_route(&cfg, "gpt-4o", b"synthetic fixture key")?
        );
        let profile = cfg.providers.get_mut("azure").unwrap();
        profile.account_id = Some("local-principal".into());
        if let Some(CloudDeployment::Azure { deployment, .. }) = &mut profile.cloud {
            *deployment = "deployment-two".into();
        }
        assert_ne!(
            first,
            crate::session_route(&cfg, "gpt-4o", b"synthetic fixture key")?
        );
        cfg.providers.get_mut("azure").unwrap().auth_strategy = Some("bearer".into());
        assert!(ChatRoute::resolve(&cfg, "gpt-4o", false).is_err());
        Ok(())
    }

    #[test]
    fn gateway_path_keeps_model_inside_one_segment_and_blocks_credential_headers() -> Result<()> {
        let mut cfg = LlmConfig::default();
        cfg.select_provider("openai-compatible");
        let profile = cfg.providers.get_mut("openai-compatible").unwrap();
        profile.base_url = "https://gateway.example/original".into();
        profile.gateway = Some(codingbuddy_core::GatewayProfile {
            chat_path: "/models/{model}/chat".into(),
            omit_stream_usage_option: true,
        });
        assert_eq!(
            ChatRoute::resolve(&cfg, "vendor/model", false)?.endpoint,
            "https://gateway.example/models/vendor%2Fmodel/chat"
        );
        let profile = cfg.providers.get_mut("openai-compatible").unwrap();
        profile
            .headers
            .insert("aUtHoRiZaTiOn".into(), "synthetic fixture value".into());
        assert!(ChatRoute::resolve(&cfg, "vendor/model", false).is_err());
        Ok(())
    }

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
