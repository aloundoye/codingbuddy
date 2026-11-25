//! Versioned fixture scope and local, route-bound live verification evidence.
use crate::{ApiClient, LlmClient};
use anyhow::{Context, Result, ensure};
use codingbuddy_core::{
    ChatMessage, ChatRequest, FunctionDefinition, LlmConfig, NativeParts, ToolChoice,
    ToolDefinition, TransportPart,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

pub const MANIFEST: &str = include_str!("../fixtures/provider-contracts.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureRoute {
    pub id: String,
    pub provider: String,
    pub protocol: String,
    pub model: String,
    pub path: String,
    pub auth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<codingbuddy_core::CloudDeployment>,
}

pub fn routes() -> Vec<FixtureRoute> {
    serde_json::from_str::<Value>(MANIFEST)
        .ok()
        .and_then(|manifest| serde_json::from_value(manifest["routes"].clone()).ok())
        .unwrap_or_default()
}

pub fn implementation_digest() -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for source in [
        MANIFEST,
        include_str!("../../../Cargo.lock"),
        include_str!("../../../Cargo.toml"),
        include_str!("../Cargo.toml"),
        include_str!("lib.rs"),
        include_str!("route.rs"),
        include_str!("cloud_identity.rs"),
        include_str!("extension.rs"),
        include_str!("providers/bedrock.rs"),
        include_str!("error.rs"),
        include_str!("protocol.rs"),
        include_str!("protocol/adapters.rs"),
        include_str!("native_parts.rs"),
        include_str!("providers/responses.rs"),
        include_str!("providers/anthropic.rs"),
        include_str!("providers/google.rs"),
        include_str!("chat_transport.rs"),
        include_str!("provider_transform.rs"),
        include_str!("certification.rs"),
        include_str!("../../codingbuddy-testkit/tests/provider_certification.rs"),
        include_str!("../../codingbuddy-agent/src/tool_loop/request_building.rs"),
        include_str!("../../codingbuddy-core/src/lib.rs"),
    ] {
        context.update(source.as_bytes());
    }
    context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Only an exact, operator-configured price has evidence for this route.
pub fn configured_price(cfg: &LlmConfig, model: &str) -> Option<codingbuddy_core::ModelCost> {
    let cost = cfg
        .model_catalog
        .overrides
        .iter()
        .rev()
        .find(|entry| entry.provider == cfg.provider && entry.id == model)?
        .cost;
    let rates = [cost.input_per_mtok_usd, cost.output_per_mtok_usd];
    (rates.iter().all(|rate| rate.is_finite() && *rate >= 0.0)
        && rates.iter().any(|rate| *rate > 0.0))
    .then_some(cost)
}

pub fn status(cfg: &LlmConfig, model: &str) -> Value {
    let provider = cfg.active_provider();
    let kind = cfg
        .active_provider_kind()
        .unwrap_or(codingbuddy_core::ProviderKind::OpenAiCompatible);
    let protocol = crate::protocol::select_chat_protocol(&provider, kind).as_key();
    let auth = crate::protocol::select_auth_strategy(
        &provider,
        kind,
        crate::protocol::select_chat_protocol(&provider, kind),
    );
    let cloud = serde_json::to_value(&provider.cloud).unwrap_or_default();
    let resolved = crate::route::ChatRoute::resolve(cfg, model, false).ok();
    let fixture = routes().into_iter().find(|route| {
        let case = serde_json::to_value(&route.cloud).unwrap_or_default();
        route.provider == cfg.provider
            && route.protocol == protocol
            && route.auth == auth.as_key()
            && case["type"] == cloud["type"]
            && case["api_version"] == cloud["api_version"]
            && (route.cloud.is_some()
                || resolved.as_ref().is_some_and(|resolved| {
                    reqwest::Url::parse(&resolved.endpoint)
                        .is_ok_and(|url| url.path() == route.path && url.query().is_none())
                }))
    });
    let evidence = std::env::current_exe()
        .ok()
        .and_then(|exe| std::fs::read(exe.with_file_name("provider-certification.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let verified = evidence.as_ref().is_some_and(|evidence| {
        evidence["schema_version"] == 1
            && evidence["suite"] == "delivery-order-4"
            && evidence["implementation_digest"] == implementation_digest()
            && evidence["platform"] == std::env::consts::OS
            && evidence["architecture"] == std::env::consts::ARCH
            && evidence["passed"] == true
            && fixture.as_ref().is_some_and(|route| {
                evidence["routes"].as_array().is_some_and(|rows| {
                    [false, true].into_iter().all(|streaming| {
                        rows.iter().any(|row| {
                            row["id"] == route.id
                                && row["model"] == model
                                && row["streaming"] == streaming
                                && row["passed"] == true
                        })
                    })
                })
            })
    });
    json!({"configured": crate::check_provider(cfg).is_ok(),"protocol":protocol,
        "fixture_profile":fixture,"fixture_verified":verified,"live_verified":false,
        "connectivity":Value::Null,
        "implementation_digest":implementation_digest(),"model":model,
        "auth":auth.as_key(), "deployment":cloud,
        "pricing":configured_price(cfg, model)})
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveEvidence {
    pub schema_version: u32,
    pub implementation_digest: String,
    pub provider: String,
    pub protocol: String,
    pub model: String,
    #[serde(default)]
    pub deployment: Option<codingbuddy_core::CloudDeployment>,
    #[serde(default)]
    pub auth: String,
    pub verified_at: String,
    pub streaming: bool,
    pub tool_rounds: usize,
    pub features: Vec<String>,
    pub usage: Vec<Value>,
    pub model_digest: Option<String>,
}

/// Bounded live probe. Only the built-in fs_read contract is offered and executed.
/// A fresh client with the same route continues the first completed tool step.
pub fn live_probe(cfg: &LlmConfig, streaming: bool) -> Result<LiveEvidence> {
    crate::check_provider(cfg)?;
    // Snapshot the credential once, including when it originally came from the keyring.
    let mut pinned = cfg.clone();
    pinned.api_key = crate::resolve_key_from_config(cfg);
    let cfg = &pinned;
    let workspace = tempfile::tempdir()?;
    for (path, text) in [
        ("first.txt", "first fixture"),
        ("second.txt", "second fixture"),
    ] {
        std::fs::write(workspace.path().join(path), text)?;
    }
    let model = cfg.active_base_model();
    let definition = ToolDefinition {
        tool_type: "function".into(),
        function: FunctionDefinition {
            name: "fs_read".into(),
            description: "Read first.txt or second.txt from the verification workspace.".into(),
            strict: Some(true),
            parameters: json!({"type":"object","properties":{"path":{"type":"string","enum":["first.txt","second.txt"]}},"required":["path"],"additionalProperties":false}),
        },
    };
    let mut messages = vec![ChatMessage::System {content:"Use fs_read for first.txt, wait for its result, then use fs_read for second.txt, then answer VERIFIED. Use exactly one call at a time.".into()},
        ChatMessage::User {content:"Run the verification workflow.".into()}];
    let mut usage = Vec::new();
    let mut rounds = 0;
    for step in 0..3 {
        let request = ChatRequest {
            model: model.clone(),
            messages: messages.clone(),
            tools: if step < 2 {
                vec![definition.clone()]
            } else {
                vec![]
            },
            tool_choice: if step < 2 {
                ToolChoice::required()
            } else {
                ToolChoice::none()
            },
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: None,
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let mut bounded = cfg.clone();
        bounded.max_retries = 0;
        bounded.timeout_seconds = bounded.timeout_seconds.clamp(1, 30);
        bounded.stream = streaming;
        let client = ApiClient::new(bounded)?;
        let response = if streaming {
            client.complete_chat_streaming(&request, std::sync::Arc::new(|_| {}))?
        } else {
            client.complete_chat(&request)?
        };
        usage.push(json!(response.usage));
        if step == 2 {
            ensure!(
                response.tool_calls.is_empty() && response.text.contains("VERIFIED"),
                "Provider did not complete verification"
            );
            break;
        }
        ensure!(
            response.tool_calls.len() == 1,
            "Expected exactly one verification tool call"
        );
        let call = &response.tool_calls[0];
        ensure!(call.name == "fs_read", "Unexpected verification tool");
        let args: Value = serde_json::from_str(&call.arguments)?;
        ensure!(
            args.as_object().is_some_and(|object| object.len() == 1),
            "Unexpected verification arguments"
        );
        let expected = if step == 0 { "first.txt" } else { "second.txt" };
        ensure!(args["path"] == expected, "Unexpected verification path");
        let native_parts: Option<NativeParts> =
            response.transport_parts.iter().find_map(|part| match part {
                TransportPart::Native(native) => Some(native.clone()),
                _ => None,
            });
        messages.push(ChatMessage::Assistant {
            content: Some(response.text),
            reasoning_content: None,
            native_parts,
            tool_calls: response.tool_calls.clone(),
        });
        messages.push(ChatMessage::Tool {
            tool_call_id: call.id.clone(),
            tool_name: Some(call.name.clone()),
            content: std::fs::read_to_string(workspace.path().join(expected))?,
        });
        rounds += 1;
    }
    let model_digest = if cfg.provider == "ollama" {
        let route = crate::route::ChatRoute::resolve(cfg, &model, false)?;
        let root = route
            .provider
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1");
        let client = crate::chat_transport::build_http_client(std::time::Duration::from_secs(10))?;
        let request = client
            .get(format!("{root}/api/tags"))
            .headers(route.headers(cfg.api_key.as_deref())?);
        let models: Value = codingbuddy_core::runtime::block_on(async move {
            request
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await
        })?;
        Some(
            models["models"]
                .as_array()
                .context("Missing installed Ollama models")?
                .iter()
                .find(|value| value["name"] == model || value["model"] == model)
                .and_then(|value| value["digest"].as_str())
                .context("Installed model digest unavailable")?
                .to_owned(),
        )
    } else {
        None
    };
    let route = crate::route::ChatRoute::resolve(cfg, &model, false)?;
    Ok(LiveEvidence {
        schema_version: 1,
        implementation_digest: implementation_digest(),
        provider: cfg.provider.clone(),
        protocol: route.protocol.as_key().into(),
        auth: route.auth.as_key().into(),
        deployment: route.provider.cloud,
        model,
        verified_at: chrono::Utc::now().to_rfc3339(),
        streaming,
        tool_rounds: rounds,
        features: vec![
            "text".into(),
            "two_sequential_function_rounds".into(),
            if streaming {
                "streaming".into()
            } else {
                "non_streaming".into()
            },
        ],
        usage,
        model_digest,
    })
}

/// Record an explicit successful connectivity probe without implying tool certification.
pub fn record_connectivity(cfg: &LlmConfig, workspace: &Path, local_key: &[u8]) -> Result<()> {
    let model = cfg.active_base_model();
    let route = crate::session_route(cfg, &model, local_key)?;
    let runtime = codingbuddy_core::runtime_dir(workspace);
    let file = runtime.join(format!("provider-connectivity-{route}.json"));
    let evidence = json!({"schema_version":1,"route":route,"implementation_digest":implementation_digest(),
        "checked_at":chrono::Utc::now().to_rfc3339()});
    let tmp = tempfile::NamedTempFile::new_in(&runtime)?;
    serde_json::to_writer_pretty(tmp.as_file(), &evidence)?;
    tmp.as_file().sync_all()?;
    tmp.persist(file)?;
    Ok(())
}

pub fn live_status(cfg: &LlmConfig, model: &str, workspace: &Path, local_key: &[u8]) -> Value {
    let mut result = status(cfg, model);
    let route = crate::session_route(cfg, model, local_key).ok();
    if let Some(route) = route.as_ref()
        && let Ok(bytes) = std::fs::read(
            codingbuddy_core::runtime_dir(workspace)
                .join(format!("provider-connectivity-{route}.json")),
        )
        && let Ok(evidence) = serde_json::from_slice::<Value>(&bytes)
        && evidence["schema_version"] == 1
        && evidence["route"] == route.as_str()
        && evidence["implementation_digest"] == implementation_digest()
    {
        result["connectivity"] = json!({"confirmed":true,"checked_at":evidence["checked_at"]});
    }
    let path = codingbuddy_core::runtime_dir(workspace).join("provider-verification.json");
    if let Ok(bytes) = std::fs::read(path)
        && let Ok(evidence) = serde_json::from_slice::<Value>(&bytes)
        && let Some(actual) = route.as_ref()
    {
        let entries = evidence["entries"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![evidence]);
        let matching: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry["route"] == actual.as_str()
                    && entry["evidence"]["schema_version"] == 1
                    && entry["evidence"]["implementation_digest"] == implementation_digest()
            })
            .map(|entry| entry["evidence"].clone())
            .collect();
        result["live_verified"] = json!(!matching.is_empty());
        if result["connectivity"].is_null()
            && let Some(evidence) = matching.last()
        {
            result["connectivity"] = json!({"confirmed":true,"checked_at":evidence["verified_at"]});
        }
        result["live_evidence"] = json!(matching);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_evidence_is_bound_to_route_model_account_and_implementation() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let mut cfg = LlmConfig::default();
        cfg.select_provider("ollama");
        let model = cfg.active_base_model();
        let local_key = b"local-only-evidence-key";
        let route = crate::session_route(&cfg, &model, local_key)?;
        let directory = codingbuddy_core::runtime_dir(workspace.path());
        std::fs::create_dir_all(&directory)?;
        let evidence = json!({"schema_version":1,"entries":[{"route":route,"evidence":{"schema_version":1,"implementation_digest":implementation_digest(),"features":["text","two_sequential_function_rounds"],"streaming":true}}]});
        std::fs::write(
            directory.join("provider-verification.json"),
            evidence.to_string(),
        )?;
        assert_eq!(
            live_status(&cfg, &model, workspace.path(), local_key)["live_verified"],
            true
        );
        assert_eq!(
            live_status(&cfg, "different-model", workspace.path(), local_key)["live_verified"],
            false
        );
        assert_eq!(
            live_status(&cfg, &model, workspace.path(), b"different-local-key")["live_verified"],
            false
        );
        cfg.endpoint = "http://127.0.0.1:1/different".into();
        assert_eq!(
            live_status(&cfg, &model, workspace.path(), local_key)["live_verified"],
            false
        );
        Ok(())
    }
}
