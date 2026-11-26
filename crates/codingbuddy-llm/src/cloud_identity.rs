use crate::{
    chat_transport::ChatTransport,
    error::{FailureKind, ProviderError},
    protocol::AuthStrategy,
    route::ChatRoute,
};
use anyhow::{Context, Result};
use azure_core::credentials::{Secret, TokenCredential};
use std::time::Duration;

pub(crate) async fn bearer(
    route: &ChatRoute,
    transport: &ChatTransport,
    timeout: Duration,
) -> Result<String> {
    let token = transport
        .wait_with_timeout(timeout, async {
            match route.auth {
                AuthStrategy::AzureEntra => azure_token().await,
                AuthStrategy::GoogleAdc => {
                    anyhow::ensure!(
                        std::env::var_os("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES").as_deref()
                            != Some(std::ffi::OsStr::new("1")),
                        "Executable Google credential sources are disabled"
                    );
                    let credentials = google_cloud_auth::credentials::Builder::default()
                        .with_scopes(["https://www.googleapis.com/auth/cloud-platform"])
                        .build_access_token_credentials()?;
                    Ok(credentials.access_token().await?.token)
                }
                _ => anyhow::bail!("Route does not use a cloud bearer credential"),
            }
        })
        .await?;
    match token {
        Ok(Ok(token)) if !token.trim().is_empty() => Ok(token),
        Err(_) => Err(ProviderError::new(route.protocol, FailureKind::Transport).into()),
        _ => Err(ProviderError::new(route.protocol, FailureKind::Authentication).into()),
    }
}

/// Stable identifiers supplied by the local credential source, when it has them.
pub(crate) fn principal_hint(auth: AuthStrategy) -> Vec<Option<String>> {
    let env = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
    match auth {
        AuthStrategy::AzureEntra => vec![env("AZURE_TENANT_ID"), env("AZURE_CLIENT_ID")],
        AuthStrategy::AwsSigV4 => vec![env("AWS_PROFILE"), env("AWS_ROLE_ARN")],
        AuthStrategy::GoogleAdc => vec![env("GOOGLE_APPLICATION_CREDENTIALS")],
        _ => Vec::new(),
    }
}

async fn azure_token() -> Result<String> {
    let secret = std::env::var("AZURE_CLIENT_SECRET");
    let credential: std::sync::Arc<dyn TokenCredential> = if let Ok(secret) = secret {
        azure_identity::ClientSecretCredential::new(
            &std::env::var("AZURE_TENANT_ID").context("Set AZURE_TENANT_ID")?,
            std::env::var("AZURE_CLIENT_ID").context("Set AZURE_CLIENT_ID")?,
            Secret::new(secret),
            None,
        )?
    } else {
        azure_identity::ManagedIdentityCredential::new(None)?
    };
    let token = credential
        .get_token(&["https://cognitiveservices.azure.com/.default"], None)
        .await?;
    Ok(token.token.secret().to_owned())
}

pub(crate) async fn bedrock_headers(
    route: &ChatRoute,
    transport: &ChatTransport,
    timeout: Duration,
    bytes: &[u8],
) -> Result<reqwest::header::HeaderMap> {
    use aws_credential_types::provider::ProvideCredentials;
    let Some(codingbuddy_core::CloudDeployment::Bedrock { region, profile }) =
        &route.provider.cloud
    else {
        anyhow::bail!("Missing Bedrock deployment");
    };
    let provider = aws_config::default_provider::credentials::DefaultCredentialsChain::builder()
        .region(aws_config::Region::new(region.clone()));
    let provider = if let Some(profile) = profile {
        provider.profile_name(profile)
    } else {
        provider
    };
    let credentials = transport
        .wait_with_timeout(timeout, async {
            provider.build().await.provide_credentials().await
        })
        .await?
        .map_err(|_| ProviderError::new(route.protocol, FailureKind::Transport))?
        .map_err(|_| ProviderError::new(route.protocol, FailureKind::Authentication))?;
    sign_bedrock(
        &route.endpoint,
        region,
        credentials,
        bytes,
        std::time::SystemTime::now(),
    )
}

pub(crate) fn sign_bedrock(
    endpoint: &str,
    region: &str,
    credentials: aws_credential_types::Credentials,
    bytes: &[u8],
    at: std::time::SystemTime,
) -> Result<reqwest::header::HeaderMap> {
    use aws_sigv4::{
        http_request::{SignableBody, SignableRequest, SigningSettings, sign},
        sign::v4,
    };
    use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
    let identity = credentials.into();
    let settings = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("bedrock")
        .time(at)
        .settings(SigningSettings::default())
        .build()?
        .into();
    let request = SignableRequest::new(
        "POST",
        endpoint,
        [("content-type", "application/json")].into_iter(),
        SignableBody::Bytes(bytes),
    )?;
    let (instructions, _) = sign(request, &settings)?.into_parts();
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    for (name, value) in instructions.headers() {
        let mut value = HeaderValue::from_str(value)?;
        value.set_sensitive(true);
        headers.insert(HeaderName::from_bytes(name.as_bytes())?, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_wait_can_be_cancelled_or_time_out() -> Result<()> {
        let token = codingbuddy_core::CancellationToken::new();
        let transport = ChatTransport::new(Duration::from_secs(2), Some(token.clone()))?;
        assert!(
            codingbuddy_core::runtime::block_on(
                transport
                    .wait_with_timeout(Duration::from_millis(10), std::future::pending::<()>())
            )?
            .is_err()
        );
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            token.cancel();
        });
        let result = codingbuddy_core::runtime::block_on(
            transport.wait_with_timeout(Duration::from_secs(2), std::future::pending::<()>()),
        );
        thread.join().expect("cancellation thread");
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn bedrock_signature_binds_endpoint_body_and_session() -> Result<()> {
        let sign = |endpoint, body: &[u8]| {
            sign_bedrock(
                endpoint,
                "us-east-1",
                aws_credential_types::Credentials::new(
                    "SYNTHETIC",
                    "synthetic-secret",
                    Some("session-fixture".into()),
                    None,
                    "fixture",
                ),
                body,
                std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )
        };
        let endpoint = "https://bedrock-runtime.us-east-1.amazonaws.com/model/fixture/converse";
        let headers = sign(endpoint, b"{}")?;
        assert!(
            headers["authorization"]
                .to_str()?
                .contains("/us-east-1/bedrock/aws4_request")
        );
        assert_eq!(headers["x-amz-security-token"], "session-fixture");
        assert!(headers["authorization"].is_sensitive());
        assert_ne!(
            headers["authorization"],
            sign(endpoint, b"{ }")?["authorization"]
        );
        assert_ne!(
            headers["authorization"],
            sign(
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/other/converse",
                b"{}"
            )?["authorization"]
        );
        Ok(())
    }
}
