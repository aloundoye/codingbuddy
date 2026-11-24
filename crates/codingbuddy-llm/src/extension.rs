//! Build-time provider codec registration. Dispatch and settlement remain in the host.
use crate::{protocol::ChatProtocol, providers::bedrock};
use anyhow::Result;
use codingbuddy_core::{ChatRequest, LlmResponse, StreamCallback};
use serde_json::Value;
use std::io::Read;

pub struct ProviderRequest<'a> {
    pub(crate) route: &'a crate::route::ChatRoute,
    pub(crate) transport: &'a crate::chat_transport::ChatTransport,
    pub(crate) timeout: std::time::Duration,
    #[cfg(feature = "provider-fixtures")]
    pub(crate) fixture: bool,
}
impl ProviderRequest<'_> {
    pub fn endpoint(&self) -> &str {
        &self.route.endpoint
    }
    pub fn profile(&self) -> &codingbuddy_core::ProviderConfig {
        &self.route.provider
    }
    pub fn wait<T>(&self, future: impl std::future::Future<Output = T>) -> Result<T> {
        self.transport
            .wait_with_timeout(self.timeout, future)?
            .map_err(|_| {
                crate::error::ProviderError::new(
                    self.route.protocol,
                    crate::error::FailureKind::Transport,
                )
                .into()
            })
    }
}

/// A registered native wire codec. Breaking changes require a new interface version.
pub trait ProviderAdapterV1: Send + Sync {
    fn protocol(&self) -> ChatProtocol;
    fn endpoint(
        &self,
        kind: codingbuddy_core::ProviderKind,
        deployment: &codingbuddy_core::CloudDeployment,
        model: &str,
        streaming: bool,
    ) -> Result<String>;
    fn authenticate(
        &self,
        request: &ProviderRequest<'_>,
        body: &[u8],
    ) -> Result<reqwest::header::HeaderMap>;
    fn encode(&self, request: &ChatRequest) -> Result<Value>;
    fn decode_response(&self, body: &Value) -> Result<LlmResponse>;
    fn decode_stream(&self, input: &mut dyn Read, callback: StreamCallback) -> Result<LlmResponse>;
}

struct Bedrock;
impl ProviderAdapterV1 for Bedrock {
    fn protocol(&self) -> ChatProtocol {
        ChatProtocol::BedrockConverse
    }
    fn endpoint(
        &self,
        kind: codingbuddy_core::ProviderKind,
        deployment: &codingbuddy_core::CloudDeployment,
        model: &str,
        streaming: bool,
    ) -> Result<String> {
        bedrock::endpoint(kind, deployment, model, streaming)
    }
    fn authenticate(
        &self,
        request: &ProviderRequest<'_>,
        body: &[u8],
    ) -> Result<reqwest::header::HeaderMap> {
        #[cfg(feature = "provider-fixtures")]
        if request.fixture {
            let Some(codingbuddy_core::CloudDeployment::Bedrock { region, .. }) =
                &request.profile().cloud
            else {
                anyhow::bail!("Missing fixture region");
            };
            return crate::cloud_identity::sign_bedrock(
                request.endpoint(),
                region,
                aws_credential_types::Credentials::new(
                    "SYNTHETICACCESS",
                    "synthetic-secret",
                    Some("synthetic-session".into()),
                    None,
                    "fixture",
                ),
                body,
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            );
        }
        crate::cloud_identity::bedrock_headers(
            request.route,
            request.transport,
            request.timeout,
            body,
        )
    }
    fn encode(&self, request: &ChatRequest) -> Result<Value> {
        bedrock::encode(request)
    }
    fn decode_response(&self, body: &Value) -> Result<LlmResponse> {
        bedrock::decode(body)
    }
    fn decode_stream(&self, input: &mut dyn Read, callback: StreamCallback) -> Result<LlmResponse> {
        bedrock::stream(input, callback)
    }
}

const ADAPTERS: &[&dyn ProviderAdapterV1] = &[&Bedrock];

pub fn registered_adapter(protocol: ChatProtocol) -> Option<&'static dyn ProviderAdapterV1> {
    ADAPTERS
        .iter()
        .copied()
        .find(|adapter| adapter.protocol() == protocol)
}
