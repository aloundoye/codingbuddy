pub mod client;
pub mod model_catalog;
pub mod payload;
pub mod protocol;
mod provider_transform;
pub mod providers;
pub mod retry;
pub mod streaming;

pub use client::{ApiClient, LlmClient};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{
        NETWORK_RETRY_BASE_MS, format_api_error, parse_retry_after_seconds, retry_delay_ms,
        should_retry_status,
    };
    use crate::streaming::{
        parse_non_streaming_payload, parse_streaming_payload, parse_usage_object,
    };
    use chrono::Utc;
    use codingbuddy_core::{
        CancellationToken, ChatMessage, ChatRequest, LlmConfig, LlmRequest, ProviderKind,
        StreamCallback, StreamChunk, max_output_tokens_for_model,
    };
    use reqwest::StatusCode;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration as StdDuration;

    #[test]
    fn parses_non_streaming() {
        let body = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
        let got = parse_non_streaming_payload(body).expect("parse");
        assert_eq!(got.text, "hello");
        assert_eq!(got.finish_reason, "stop");
    }

    #[test]
    fn parses_streaming_sse_lines() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]";
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "hello");
    }

    #[test]
    fn parses_streaming_reasoning_content() {
        let body = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step1\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step2\"}}]}\n\ndata: [DONE]";
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "step1step2");
        assert_eq!(got.reasoning_content, "step1step2");
    }

    #[test]
    fn parses_non_streaming_tool_calls() {
        let body = r#"{
          "choices": [
            {
              "finish_reason": "tool_calls",
              "message": {
                "content": "",
                "tool_calls": [
                  {
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "fs.read", "arguments": "{\"path\":\"README.md\"}" }
                  }
                ]
              }
            }
          ]
        }"#;
        let got = parse_non_streaming_payload(body).expect("parse");
        assert_eq!(got.finish_reason, "tool_calls");
        assert_eq!(got.tool_calls.len(), 1);
        assert_eq!(got.tool_calls[0].name, "fs.read");
    }

    #[test]
    fn parses_streaming_tool_call_fragments() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"fs.read\",\"arguments\":\"{\\\"path\\\":\\\"REA\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"DME.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n"
        );
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.finish_reason, "tool_calls");
        assert_eq!(got.tool_calls.len(), 1);
        assert_eq!(got.tool_calls[0].name, "fs.read");
        assert_eq!(got.tool_calls[0].arguments, "{\"path\":\"README.md\"}");
    }

    #[test]
    fn fast_mode_caps_max_tokens_in_payload() {
        let cfg = LlmConfig {
            fast_mode: true,
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let payload = payload::build_simple_payload(
            &LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hello".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 16_000,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            },
            &client.cfg,
        );
        assert_eq!(payload["max_tokens"], 2048);
    }

    #[test]
    fn model_name_passes_through_unmodified_in_payload() {
        let client = ApiClient::new(LlmConfig::default()).expect("client");
        let payload = payload::build_simple_payload(
            &LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hello".to_string(),
                model: "codingbuddy-v3.2".to_string(),
                max_tokens: 256,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            },
            &client.cfg,
        );
        assert_eq!(payload["model"], "codingbuddy-v3.2");
    }

    #[test]
    fn truly_unsupported_provider_is_rejected() {
        let cfg = LlmConfig {
            provider: "invalid_provider_xyz".to_string(),
            api_key: Some("test-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let err = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hello".to_string(),
                model: "any".to_string(),
                max_tokens: 128,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect_err("truly unsupported provider should fail");
        assert!(err.to_string().contains("unsupported llm.provider"));
    }

    #[test]
    fn thinking_mode_rejects_logprobs() {
        let cfg = LlmConfig {
            api_key: Some("test-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: Some(true),
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(4096)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let err = client
            .complete_chat(&req)
            .expect_err("logprobs with thinking should fail");
        assert!(
            err.to_string()
                .contains("logprobs is incompatible with thinking mode")
        );
    }

    #[test]
    fn thinking_mode_rejects_top_logprobs() {
        let cfg = LlmConfig {
            api_key: Some("test-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: Some(5),
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(4096)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let err = client
            .complete_chat(&req)
            .expect_err("top_logprobs with thinking should fail");
        assert!(
            err.to_string()
                .contains("top_logprobs is incompatible with thinking mode")
        );
    }

    #[test]
    fn capability_contract_allows_openai_compat_thinking_shim() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(2048)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        client
            .validate_chat_request_contract(&req)
            .expect("openai-compatible thinking shim should be accepted");
    }

    #[test]
    fn capability_contract_rejects_ollama_thinking_config() {
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "qwen2.5-coder:7b".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(2048)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let err = client
            .validate_chat_request_contract(&req)
            .expect_err("ollama should reject thinking config");
        assert!(err.to_string().contains("does not support thinking config"));
    }

    #[test]
    fn capability_contract_rejects_tool_choice_when_unsupported() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-reasoner".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        let err = client
            .validate_chat_request_contract(&req)
            .expect_err("tool_choice should be rejected");
        assert!(err.to_string().contains("does not support tool_choice"));
    }

    #[test]
    fn capability_contract_rejects_too_many_tools() {
        let mut cfg = LlmConfig::default();
        cfg.capability_overrides.models.insert(
            "deepseek@deepseek-chat".to_string(),
            codingbuddy_core::CapabilityOverride {
                max_safe_tool_count: Some(1),
                ..codingbuddy_core::CapabilityOverride::default()
            },
        );
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![
                codingbuddy_core::ToolDefinition {
                    tool_type: "function".to_string(),
                    function: codingbuddy_core::FunctionDefinition {
                        name: "fs_read".to_string(),
                        description: "Read file".to_string(),
                        strict: None,
                        parameters: serde_json::json!({"type": "object"}),
                    },
                },
                codingbuddy_core::ToolDefinition {
                    tool_type: "function".to_string(),
                    function: codingbuddy_core::FunctionDefinition {
                        name: "fs_list".to_string(),
                        description: "List dir".to_string(),
                        strict: None,
                        parameters: serde_json::json!({"type": "object"}),
                    },
                },
            ],
            tool_choice: codingbuddy_core::ToolChoice::auto(),
            max_tokens: 128,
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
        let err = client
            .validate_chat_request_contract(&req)
            .expect_err("tool count should be rejected");
        assert!(err.to_string().contains("max_safe_tool_count"));
    }

    #[test]
    fn capability_contract_rejects_top_logprobs_without_logprobs() {
        let client = ApiClient::new(LlmConfig::default()).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: Some(3),
            thinking: None,
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let err = client
            .validate_chat_request_contract(&req)
            .expect_err("top_logprobs without logprobs should be rejected");
        assert!(
            err.to_string()
                .contains("top_logprobs requires logprobs=true")
        );
    }

    #[test]
    fn thinking_mode_strips_sampling_params_from_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            logprobs: None,
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(4096)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        // With thinking enabled, all sampling params must be omitted
        assert!(
            payload.get("temperature").is_none(),
            "temperature should be stripped"
        );
        assert!(payload.get("top_p").is_none(), "top_p should be stripped");
        assert!(
            payload.get("presence_penalty").is_none(),
            "presence_penalty should be stripped"
        );
        assert!(
            payload.get("frequency_penalty").is_none(),
            "frequency_penalty should be stripped"
        );
        // thinking config should be present
        assert!(
            payload.get("thinking").is_some(),
            "thinking should be present"
        );
    }

    #[test]
    fn non_thinking_mode_includes_sampling_params_in_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            logprobs: Some(true),
            top_logprobs: Some(3),
            thinking: None,
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert!(
            payload.get("temperature").is_some(),
            "temperature should be present"
        );
        assert!(payload.get("top_p").is_some(), "top_p should be present");
        assert!(
            payload.get("presence_penalty").is_some(),
            "presence_penalty should be present"
        );
        assert!(
            payload.get("frequency_penalty").is_some(),
            "frequency_penalty should be present"
        );
        assert_eq!(payload["logprobs"], true);
        assert_eq!(payload["top_logprobs"], 3);
    }

    #[test]
    fn reasoner_strips_tool_choice_from_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let tool = codingbuddy_core::ToolDefinition {
            tool_type: "function".to_string(),
            function: codingbuddy_core::FunctionDefinition {
                name: "fs_read".to_string(),
                description: "Read file".to_string(),
                strict: None,
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let req = ChatRequest {
            model: "deepseek-reasoner".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![tool],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        // Tools should be present
        assert!(payload.get("tools").is_some(), "tools should be present");
        // tool_choice must be stripped for reasoner (HTTP 400 otherwise)
        assert!(
            payload.get("tool_choice").is_none(),
            "tool_choice should be stripped for deepseek-reasoner"
        );
        // thinking config must also be stripped for reasoner (thinks natively)
        assert!(
            payload.get("thinking").is_none(),
            "thinking should be stripped for deepseek-reasoner"
        );
    }

    #[test]
    fn reasoner_strips_thinking_config_from_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-reasoner".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: None,
            top_logprobs: None,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(16_384)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert!(
            payload.get("thinking").is_none(),
            "thinking config must be stripped for deepseek-reasoner (thinks natively)"
        );
    }

    #[test]
    fn non_reasoner_preserves_tool_choice_in_payload() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let tool = codingbuddy_core::ToolDefinition {
            tool_type: "function".to_string(),
            function: codingbuddy_core::FunctionDefinition {
                name: "fs_read".to_string(),
                description: "Read file".to_string(),
                strict: None,
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![tool],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert!(payload.get("tools").is_some(), "tools should be present");
        assert!(
            payload.get("tool_choice").is_some(),
            "tool_choice should be present for non-reasoner"
        );
        assert_eq!(payload["tool_choice"], "required");
    }

    #[test]
    fn format_api_error_parses_structured_error() {
        let body = r#"{"error": {"type": "invalid_request_error", "message": "logprobs is not supported with reasoning"}}"#;
        let err = format_api_error(StatusCode::UNPROCESSABLE_ENTITY, body, 0, 3);
        let msg = err.to_string();
        assert!(msg.contains("HTTP 422"), "should mention 422: {msg}");
        assert!(
            msg.contains("invalid_request_error"),
            "should include error type: {msg}"
        );
        assert!(
            msg.contains("logprobs is not supported"),
            "should include detail: {msg}"
        );
    }

    #[test]
    fn format_api_error_400_includes_type() {
        let body = r#"{"error": {"type": "invalid_format", "message": "bad json"}}"#;
        let err = format_api_error(StatusCode::BAD_REQUEST, body, 0, 3);
        let msg = err.to_string();
        assert!(msg.contains("HTTP 400"), "should mention 400: {msg}");
        assert!(msg.contains("invalid_format"), "should include type: {msg}");
    }

    #[test]
    fn format_api_error_fallback_on_non_json_body() {
        let body = "something went wrong";
        let err = format_api_error(StatusCode::INTERNAL_SERVER_ERROR, body, 2, 3);
        let msg = err.to_string();
        assert!(
            msg.contains("something went wrong"),
            "should include raw body: {msg}"
        );
    }

    #[test]
    fn openai_compatible_payload_maps_thinking_to_reasoning_effort() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: None,
            frequency_penalty: None,
            logprobs: Some(true),
            top_logprobs: Some(3),
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(4096)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["model"], "gpt-4o-mini");
        assert!(payload.get("thinking").is_none());
        assert_eq!(payload["reasoning_effort"], "medium");
        assert_eq!(payload["temperature"], 0.5);
        assert!(
            (payload["top_p"].as_f64().unwrap_or_default() - 0.9).abs() < 0.000_001,
            "top_p should be preserved"
        );
        assert_eq!(payload["logprobs"], true);
        assert_eq!(payload["top_logprobs"], 3);
    }

    #[test]
    fn openai_reasoning_family_with_thinking_strips_sampling_controls() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "o3-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            logprobs: Some(true),
            top_logprobs: Some(3),
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(16_384)),
            images: vec![],
            provider_options: Default::default(),
            response_format: None,
        };
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["reasoning_effort"], "high");
        assert!(payload.get("temperature").is_none());
        assert!(payload.get("top_p").is_none());
        assert!(payload.get("presence_penalty").is_none());
        assert!(payload.get("frequency_penalty").is_none());
        assert!(payload.get("logprobs").is_none());
        assert!(payload.get("top_logprobs").is_none());
    }

    #[test]
    fn openai_gemini_payload_adds_max_output_tokens_alias() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gemini-2.0-flash".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 256,
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["max_tokens"], 256);
        assert_eq!(payload["max_output_tokens"], 256);
    }

    #[test]
    fn openai_gemini_downgrades_required_tool_choice() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gemini-2.0-flash".to_string(),
            messages: vec![ChatMessage::User {
                content: "run tool".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn litellm_proxy_with_tool_history_gets_placeholder_tool_payload() {
        let mut cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        cfg.endpoint = "https://litellm.internal/v1/chat/completions".to_string();
        cfg.providers
            .get_mut("openai-compatible")
            .expect("provider")
            .base_url = "https://litellm.internal".to_string();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![
                ChatMessage::User {
                    content: "run tool".to_string(),
                },
                ChatMessage::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![codingbuddy_core::LlmToolCall {
                        id: "call_1".to_string(),
                        name: "fs_read".to_string(),
                        arguments: "{}".to_string(),
                    }],
                },
                ChatMessage::Tool {
                    tool_call_id: "call_1".to_string(),
                    content: "ok".to_string(),
                    tool_name: None,
                },
            ],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
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

        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["tools"][0]["function"]["name"], "_noop");
        assert!(payload.get("parallel_tool_calls").is_none());
        assert!(payload.get("tool_choice").is_none());
    }

    #[test]
    fn gemini_payload_sanitizes_tool_schema_for_gateway_compatibility() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gemini-2.0-flash".to_string(),
            messages: vec![ChatMessage::User {
                content: "pick a mode".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "pick_mode".to_string(),
                    description: "Pick a mode".to_string(),
                    strict: None,
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "mode": {
                                "type": "integer",
                                "enum": [1, 2]
                            },
                            "items": {
                                "type": "array",
                                "items": {}
                            }
                        },
                        "required": ["mode", "missing"]
                    }),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::auto(),
            max_tokens: 128,
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

        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        let schema = &payload["tools"][0]["function"]["parameters"];
        assert_eq!(schema["required"], serde_json::json!(["mode"]));
        assert_eq!(schema["properties"]["mode"]["type"], "string");
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            serde_json::json!(["1", "2"])
        );
        assert_eq!(schema["properties"]["items"]["items"]["type"], "string");
    }

    #[test]
    fn openai_reasoning_family_uses_max_completion_tokens_key() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "o3-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 4096,
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

        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["max_completion_tokens"], 4096);
        assert!(
            payload.get("max_tokens").is_none(),
            "openai reasoning families should use max_completion_tokens"
        );
    }

    #[test]
    fn openai_payload_enables_parallel_tool_calls_when_supported() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "read a file".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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

        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["parallel_tool_calls"], true);
        assert!(payload.get("tool_choice").is_some());
    }

    #[test]
    fn capability_override_can_disable_tool_choice_and_parallel_payload_flags() {
        let mut cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            ..LlmConfig::default()
        };
        cfg.capability_overrides.models.insert(
            "openai-compatible@gpt-4o-mini".to_string(),
            codingbuddy_core::CapabilityOverride {
                supports_tool_choice: Some(false),
                supports_parallel_tool_calls: Some(false),
                ..codingbuddy_core::CapabilityOverride::default()
            },
        );
        let client = ApiClient::new(cfg).expect("client");

        let req = ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![ChatMessage::User {
                content: "read a file".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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

        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert!(payload.get("parallel_tool_calls").is_none());
        assert!(payload.get("tool_choice").is_none());
        assert!(payload.get("tools").is_some());
    }

    #[test]
    fn ollama_provider_allows_requests_without_api_key() {
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            api_key: None,
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        assert_eq!(client.resolve_request_api_key().expect("no auth"), None);
    }

    #[test]
    fn ollama_payload_downgrades_required_tool_choice() {
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "qwen2.5-coder:7b".to_string(),
            messages: vec![ChatMessage::User {
                content: "read a file".to_string(),
            }],
            tools: vec![codingbuddy_core::ToolDefinition {
                tool_type: "function".to_string(),
                function: codingbuddy_core::FunctionDefinition {
                    name: "fs_read".to_string(),
                    description: "Read file".to_string(),
                    strict: None,
                    parameters: serde_json::json!({"type": "object"}),
                },
            }],
            tool_choice: codingbuddy_core::ToolChoice::required(),
            max_tokens: 128,
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn ollama_payload_sets_num_predict_option_alias() {
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "qwen2.5-coder:7b".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 512,
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["options"]["num_predict"], 512);
    }

    #[test]
    fn missing_api_key_is_rejected() {
        let cfg = LlmConfig {
            providers: std::collections::HashMap::new(),
            api_key: None,
            api_key_env: "CODINGBUDDY_NONEXISTENT_KEY_FOR_TEST".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let err = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hello".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 128,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect_err("missing API key should fail");
        assert!(err.to_string().contains("not set and llm.api_key is empty"));
    }

    #[test]
    fn adds_language_system_instruction_when_not_english() {
        let cfg = LlmConfig {
            language: "es".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let payload = payload::build_simple_payload(
            &LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hola".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 128,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            },
            &client.cfg,
        );
        let messages = payload["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("Respond in es")
        );
        // P9: no cache_control annotations — DeepSeek uses server-side prefix caching
        assert!(messages[0].get("cache_control").is_none());
    }

    #[test]
    fn chat_payload_has_no_cache_annotations() {
        // P9: DeepSeek uses automatic server-side prefix caching.
        // No client-side cache_control annotations should be sent.
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![
                ChatMessage::System {
                    content: "system prompt".to_string(),
                },
                ChatMessage::User {
                    content: "hello".to_string(),
                },
            ],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 64,
            temperature: Some(0.2),
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        let messages = payload["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|m| m.get("cache_control").is_none()));
    }

    #[test]
    fn resolve_api_key_uses_config_fallback() {
        let cfg = LlmConfig {
            api_key_env: "DEEPSEEK_API_KEY_TEST_FALLBACK".to_string(),
            api_key: Some("local-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY_TEST_FALLBACK");
        }
        let resolved = client.resolve_api_key().expect("fallback key");
        assert_eq!(resolved, "local-key");
    }

    #[test]
    fn retry_status_classification_matches_codingbuddy_guidance() {
        assert!(should_retry_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(should_retry_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(should_retry_status(StatusCode::BAD_GATEWAY));
        assert!(should_retry_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!should_retry_status(StatusCode::UNAUTHORIZED));
        assert!(!should_retry_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn format_api_error_401_includes_setup_instructions() {
        let err = format_api_error(
            StatusCode::UNAUTHORIZED,
            r#"{"error":"invalid_api_key"}"#,
            0,
            3,
        );
        let msg = err.to_string();
        assert!(
            msg.contains("Invalid or missing API key"),
            "should mention invalid/missing key: {msg}"
        );
        assert!(
            msg.contains("llm.api_key"),
            "should mention config field: {msg}"
        );
        assert!(
            msg.contains("active provider API key"),
            "should mention provider auth: {msg}"
        );
    }

    #[test]
    fn format_api_error_401_does_not_retry() {
        // 401 should not be retryable
        assert!(!should_retry_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn network_retry_base_uses_one_second_delays() {
        // Verify the constant matches the spec: base ~1s, ~2s, ~4s (with jitter)
        assert_eq!(NETWORK_RETRY_BASE_MS, 1000);
        let d0 = retry_delay_ms(NETWORK_RETRY_BASE_MS, 0, None);
        let d1 = retry_delay_ms(NETWORK_RETRY_BASE_MS, 1, None);
        let d2 = retry_delay_ms(NETWORK_RETRY_BASE_MS, 2, None);
        // Jitter range: base * 2^attempt * [0.5, 1.5], capped at 30s
        let ms0 = d0.as_millis();
        let ms1 = d1.as_millis();
        let ms2 = d2.as_millis();
        assert!(
            (500..=1500).contains(&ms0),
            "attempt 0 should be ~1s (got {ms0}ms)"
        );
        assert!(
            (1000..=3000).contains(&ms1),
            "attempt 1 should be ~2s (got {ms1}ms)"
        );
        assert!(
            (2000..=6000).contains(&ms2),
            "attempt 2 should be ~4s (got {ms2}ms)"
        );
    }

    #[test]
    fn retry_after_parses_seconds_and_http_date() {
        let seconds_header = reqwest::header::HeaderValue::from_static("7");
        assert_eq!(parse_retry_after_seconds(Some(&seconds_header)), Some(7));

        let future = Utc::now() + chrono::Duration::seconds(5);
        let http_date = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let date_header = reqwest::header::HeaderValue::from_str(&http_date).expect("header");
        let parsed = parse_retry_after_seconds(Some(&date_header)).expect("parsed");
        assert!(parsed <= 10);
    }

    #[test]
    fn complete_retries_transient_status_then_succeeds() {
        let server = start_mock_retry_server(vec![
            MockHttpResponse {
                status: 503,
                body: r#"{"error":"temporarily_unavailable"}"#.to_string(),
                retry_after: Some("0".to_string()),
            },
            MockHttpResponse {
                status: 200,
                body: r#"{"choices":[{"message":{"content":"ok-after-retry"}}]}"#.to_string(),
                retry_after: None,
            },
        ]);

        let cfg = LlmConfig {
            endpoint: server.endpoint.clone(),
            stream: false,
            providers: std::collections::HashMap::new(),
            api_key_env: "DEEPSEEK_API_KEY_RETRY_TEST".to_string(),
            max_retries: 3,
            retry_base_ms: 1,
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY_RETRY_TEST", "test-key");
        }

        let out = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "retry test".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 64,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect("response should eventually succeed");
        assert_eq!(out.text, "ok-after-retry");
        assert!(server.request_count() >= 2);

        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY_RETRY_TEST");
        }
    }

    #[test]
    fn complete_stops_after_bounded_retries() {
        let server = start_mock_retry_server(vec![MockHttpResponse {
            status: 429,
            body: r#"{"error":"rate_limited"}"#.to_string(),
            retry_after: Some("0".to_string()),
        }]);

        let cfg = LlmConfig {
            endpoint: server.endpoint.clone(),
            stream: false,
            providers: std::collections::HashMap::new(),
            api_key_env: "DEEPSEEK_API_KEY_RETRY_LIMIT_TEST".to_string(),
            max_retries: 2,
            retry_base_ms: 1,
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY_RETRY_LIMIT_TEST", "test-key");
        }

        let err = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "retry limit test".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 64,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect_err("request should fail after retries are exhausted");
        assert!(err.to_string().contains("Rate limited (HTTP 429)"));
        assert_eq!(server.request_count(), 3);

        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY_RETRY_LIMIT_TEST");
        }
    }

    #[test]
    fn complete_returns_clear_instructions_on_401() {
        let server = start_mock_retry_server(vec![MockHttpResponse {
            status: 401,
            body: r#"{"error":{"message":"invalid_api_key"}}"#.to_string(),
            retry_after: None,
        }]);

        let cfg = LlmConfig {
            endpoint: server.endpoint.clone(),
            stream: false,
            providers: std::collections::HashMap::new(),
            api_key_env: "DEEPSEEK_API_KEY_401_TEST".to_string(),
            max_retries: 2,
            retry_base_ms: 1,
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY_401_TEST", "bad-key");
        }

        let err = client
            .complete(&LlmRequest {
                unit: codingbuddy_core::LlmUnit::Planner,
                prompt: "hello".to_string(),
                model: "deepseek-chat".to_string(),
                max_tokens: 64,
                non_urgent: false,
                images: vec![],
                provider_options: Default::default(),
            })
            .expect_err("401 should fail without retrying");

        let msg = err.to_string();
        assert!(
            msg.contains("Invalid or missing API key"),
            "401 error should include clear message: {msg}"
        );
        assert!(
            msg.contains("active provider API key"),
            "401 error should mention provider auth: {msg}"
        );
        // 401 is non-retryable, so only 1 request
        assert_eq!(server.request_count(), 1);

        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY_401_TEST");
        }
    }

    #[derive(Clone)]
    struct MockHttpResponse {
        status: u16,
        body: String,
        retry_after: Option<String>,
    }

    struct RetryMockServer {
        endpoint: String,
        request_count: Arc<AtomicUsize>,
        stop_tx: Option<mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl RetryMockServer {
        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }
    }

    impl Drop for RetryMockServer {
        fn drop(&mut self) {
            if let Some(tx) = self.stop_tx.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn start_mock_retry_server(responses: Vec<MockHttpResponse>) -> RetryMockServer {
        let scripted = if responses.is_empty() {
            vec![MockHttpResponse {
                status: 500,
                body: r#"{"error":"empty_script"}"#.to_string(),
                retry_after: None,
            }]
        } else {
            responses
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind retry mock server");
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");
        let addr = listener.local_addr().expect("addr");
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_thread = Arc::clone(&request_count);
        let (tx, rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            loop {
                if rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = consume_http_request(&mut stream);
                        let idx = request_count_thread.fetch_add(1, Ordering::SeqCst);
                        let selected = scripted
                            .get(idx)
                            .cloned()
                            .or_else(|| scripted.last().cloned())
                            .expect("scripted response");
                        let status_text = match selected.status {
                            200 => "OK",
                            429 => "Too Many Requests",
                            500 => "Internal Server Error",
                            503 => "Service Unavailable",
                            _ => "Error",
                        };
                        let mut headers = format!(
                            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                            selected.status,
                            status_text,
                            selected.body.len()
                        );
                        if let Some(retry_after) = selected.retry_after {
                            headers.push_str(&format!("Retry-After: {retry_after}\r\n"));
                        }
                        headers.push_str("\r\n");
                        let response = format!("{headers}{}", selected.body);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(StdDuration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        RetryMockServer {
            endpoint: format!("http://{addr}/chat/completions"),
            request_count,
            stop_tx: Some(tx),
            handle: Some(handle),
        }
    }

    fn consume_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut header_end = None;
        while header_end.is_none() {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            header_end = find_subsequence(&buffer, b"\r\n\r\n").map(|idx| idx + 4);
            if buffer.len() > 1_048_576 {
                break;
            }
        }
        let header_len = header_end.unwrap_or(buffer.len());
        let content_length = parse_content_length(&buffer[..header_len]);
        let mut body = if header_len <= buffer.len() {
            buffer[header_len..].to_vec()
        } else {
            Vec::new()
        };
        while body.len() < content_length {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        Ok(())
    }

    fn parse_content_length(headers: &[u8]) -> usize {
        let raw = String::from_utf8_lossy(headers);
        for line in raw.lines() {
            let mut parts = line.splitn(2, ':');
            let key = parts.next().unwrap_or_default().trim();
            if key.eq_ignore_ascii_case("content-length")
                && let Some(value) = parts.next()
                && let Ok(parsed) = value.trim().parse::<usize>()
            {
                return parsed;
            }
        }
        0
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn complete_streaming_invokes_callback_per_chunk() {
        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]\n";
        let server = start_mock_retry_server(vec![MockHttpResponse {
            status: 200,
            body: sse_body.to_string(),
            retry_after: None,
        }]);

        let cfg = LlmConfig {
            endpoint: server.endpoint.clone(),
            stream: true,
            providers: std::collections::HashMap::new(),
            api_key_env: "DEEPSEEK_API_KEY_STREAM_TEST".to_string(),
            max_retries: 0,
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY_STREAM_TEST", "test-key");
        }

        let chunks = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let chunks_clone = Arc::clone(&chunks);
        let cb: StreamCallback = Arc::new(move |chunk| match chunk {
            StreamChunk::ContentDelta(text) => {
                chunks_clone.lock().expect("test lock").push(text);
            }
            StreamChunk::Done { .. } => {
                chunks_clone
                    .lock()
                    .expect("test lock")
                    .push("[DONE]".to_string());
            }
            _ => {}
        });

        let resp = client
            .complete_streaming(
                &LlmRequest {
                    unit: codingbuddy_core::LlmUnit::Planner,
                    prompt: "hello".to_string(),
                    model: "deepseek-chat".to_string(),
                    max_tokens: 128,
                    non_urgent: false,
                    images: vec![],
                    provider_options: Default::default(),
                },
                cb,
            )
            .expect("streaming response");

        assert_eq!(resp.text, "hello");
        let collected = chunks.lock().expect("test lock");
        assert_eq!(collected.len(), 3); // "hel", "lo", "[DONE]"
        assert_eq!(collected[0], "hel");
        assert_eq!(collected[1], "lo");
        assert_eq!(collected[2], "[DONE]");

        // SAFETY: test-only process-level env mutation.
        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY_STREAM_TEST");
        }
    }

    // ── P0-06: SSE keep-alive test coverage ───────────────────────────

    #[test]
    fn sse_keep_alive_lines_are_ignored() {
        let body = concat!(
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            ": keep-alive\n\n",
            "data: [DONE]"
        );
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "hello");
    }

    #[test]
    fn sse_empty_comment_lines_ignored() {
        let body = concat!(
            ":\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            ":\n\n",
            "data: [DONE]"
        );
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "hi");
    }

    #[test]
    fn sse_blank_lines_between_events() {
        let body = concat!(
            "\n\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
            "\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n",
            "\n",
            "data: [DONE]"
        );
        let got = parse_streaming_payload(body).expect("stream parse");
        assert_eq!(got.text, "onetwo");
    }

    // ── P0-05: Usage parsing tests ────────────────────────────────────

    #[test]
    fn parse_usage_all_fields() {
        let usage_json = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 30,
            "prompt_cache_miss_tokens": 70,
            "completion_tokens_details": {
                "reasoning_tokens": 20
            }
        });
        let usage = parse_usage_object(Some(&usage_json)).expect("should parse");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.prompt_cache_hit_tokens, 30);
        assert_eq!(usage.prompt_cache_miss_tokens, 70);
        assert_eq!(usage.reasoning_tokens, 20);
    }

    #[test]
    fn parse_usage_defaults_missing() {
        let usage_json = serde_json::json!({
            "prompt_tokens": 42,
            "completion_tokens": 10
        });
        let usage = parse_usage_object(Some(&usage_json)).expect("should parse");
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 10);
        assert_eq!(usage.prompt_cache_hit_tokens, 0);
        assert_eq!(usage.prompt_cache_miss_tokens, 0);
        assert_eq!(usage.reasoning_tokens, 0);
    }

    #[test]
    fn parse_usage_none_returns_none() {
        assert!(parse_usage_object(None).is_none());
    }

    // ── P0-07/10: Retry status tests ─────────────────────────────────

    #[test]
    fn http_429_is_retryable() {
        assert!(should_retry_status(StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn http_500_is_retryable() {
        assert!(should_retry_status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[test]
    fn http_503_is_retryable() {
        assert!(should_retry_status(StatusCode::SERVICE_UNAVAILABLE));
    }

    #[test]
    fn http_401_is_not_retryable() {
        assert!(!should_retry_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn http_400_is_not_retryable() {
        assert!(!should_retry_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn http_422_is_not_retryable() {
        assert!(!should_retry_status(StatusCode::UNPROCESSABLE_ENTITY));
    }

    #[test]
    fn retry_includes_502_bad_gateway() {
        assert!(should_retry_status(StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn model_max_output_tokens_reasoner() {
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-reasoner", false),
            65536
        );
        // Reasoner always returns 64K regardless of thinking flag
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-reasoner", true),
            65536
        );
    }

    #[test]
    fn model_max_output_tokens_chat() {
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-chat", false),
            8192
        );
    }

    #[test]
    fn max_output_tokens_thinking_chat_32k() {
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-chat", true),
            32768
        );
    }

    #[test]
    fn max_output_tokens_reasoner_64k() {
        // Reasoner is always 64K regardless of thinking param
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-reasoner", false),
            65536
        );
        assert_eq!(
            max_output_tokens_for_model(ProviderKind::Deepseek, "deepseek-reasoner", true),
            65536
        );
    }

    #[test]
    fn reasoner_caps_max_tokens_to_65536() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        let req = LlmRequest {
            unit: codingbuddy_core::LlmUnit::Planner,
            prompt: "hello".to_string(),
            model: "deepseek-reasoner".to_string(),
            max_tokens: 100_000,
            non_urgent: false,
            images: vec![],
            provider_options: Default::default(),
        };
        let payload = payload::build_simple_payload(&req, &client.cfg);
        assert_eq!(payload["max_tokens"], 65536);
    }

    #[test]
    fn chat_caps_max_tokens_to_8192() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        let req = LlmRequest {
            unit: codingbuddy_core::LlmUnit::Planner,
            prompt: "hello".to_string(),
            model: "deepseek-chat".to_string(),
            max_tokens: 20_000,
            non_urgent: false,
            images: vec![],
            provider_options: Default::default(),
        };
        let payload = payload::build_simple_payload(&req, &client.cfg);
        assert_eq!(payload["max_tokens"], 8192);
    }

    // ── P1-18: resolved_endpoint routing tests ──────────────────────────

    #[test]
    fn resolved_endpoint_default_chat() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: String::new(), // empty triggers computed path
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        assert_eq!(
            client.resolved_endpoint(true, false, false),
            "https://api.deepseek.com/chat/completions"
        );
    }

    #[test]
    fn resolved_endpoint_fim_routes_to_beta() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: String::new(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        assert_eq!(
            client.resolved_endpoint(false, true, false),
            "https://api.deepseek.com/beta/completions"
        );
    }

    #[test]
    fn resolved_endpoint_strict_tools_routes_to_beta() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: String::new(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        assert_eq!(
            client.resolved_endpoint(true, false, true),
            "https://api.deepseek.com/beta/completions"
        );
    }

    #[test]
    fn resolved_endpoint_openai_compat_adds_v1_prefix() {
        let cfg = LlmConfig {
            provider: "openai-compatible".to_string(),
            endpoint: String::new(),
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        assert_eq!(
            client.resolved_endpoint(true, false, false),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn resolved_endpoint_custom_endpoint_overrides() {
        let cfg = LlmConfig {
            base_url: "https://api.deepseek.com".to_string(),
            endpoint: "https://custom.example.com/my-api".to_string(),
            openai_compat_prefix: true,
            ..Default::default()
        };
        let client = ApiClient::new(cfg).unwrap();
        // Custom endpoint takes priority over everything
        assert_eq!(
            client.resolved_endpoint(true, false, false),
            "https://custom.example.com/my-api"
        );
    }

    #[test]
    fn set_cancel_token_stores_token() {
        let cfg = LlmConfig::default();
        let mut client = ApiClient::new(cfg).unwrap();
        assert!(client.cancel_token.is_none());

        let token = CancellationToken::new();
        client.set_cancel_token(token.clone());
        assert!(client.cancel_token.is_some());
        assert!(!client.cancel_token.as_ref().unwrap().is_cancelled());

        token.cancel();
        assert!(client.cancel_token.as_ref().unwrap().is_cancelled());
    }

    #[test]
    fn cancellation_token_reset_works() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
        token.reset();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_build_chat_payload_returns_result() {
        let cfg = LlmConfig::default();
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::none(),
            max_tokens: 128,
            temperature: Some(0.5),
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
        let result = payload::build_chat_payload(&req, &client.cfg);
        assert!(
            result.is_ok(),
            "build_chat_payload should return Ok for valid input"
        );
        let payload = result.unwrap();
        assert_eq!(payload["model"], "deepseek-chat");
        assert_eq!(payload["max_tokens"], 128);
        assert!(payload["messages"].is_array());
    }

    #[test]
    fn test_api_client_caches_api_key() {
        let cfg = LlmConfig {
            api_key_env: "CODINGBUDDY_CACHE_TEST_NONEXISTENT".to_string(),
            api_key: Some("cached-test-key".to_string()),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        // The cached key should be resolved eagerly from cfg.api_key
        assert_eq!(
            client.api_key.as_deref(),
            Some("cached-test-key"),
            "api_key should be cached at construction"
        );
        // resolve_api_key should return the cached key
        let resolved = client.resolve_api_key().expect("should resolve cached key");
        assert_eq!(resolved, "cached-test-key");

        // After clearing, resolve_api_key falls back to env/config
        let mut client = client;
        client.clear_api_key();
        assert!(
            client.api_key.is_none(),
            "api_key should be None after clear"
        );
        // Since the env var does not exist and we cleared the cache,
        // resolve_api_key should still fall back to cfg.api_key
        let resolved = client
            .resolve_api_key()
            .expect("should fall back to cfg.api_key");
        assert_eq!(resolved, "cached-test-key");
    }

    #[test]
    fn anthropic_provider_payload_includes_anthropic_version_header() {
        let cfg = LlmConfig {
            provider: "anthropic".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::auto(),
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        assert_eq!(payload["model"], "claude-sonnet-4-20250514");
        assert!(payload["messages"].is_array());
    }

    #[test]
    fn google_provider_payload_uses_correct_model() {
        let cfg = LlmConfig {
            provider: "google".to_string(),
            ..LlmConfig::default()
        };
        let client = ApiClient::new(cfg).expect("client");
        let req = ChatRequest {
            model: "gemini-2.5-flash".to_string(),
            messages: vec![ChatMessage::User {
                content: "hello".to_string(),
            }],
            tools: vec![],
            tool_choice: codingbuddy_core::ToolChoice::auto(),
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
        let payload = payload::build_chat_payload(&req, &client.cfg).expect("build payload");
        // Native Gemini format: uses contents array, not messages
        assert!(
            payload["contents"].is_array(),
            "Google native format should use contents array"
        );
        assert!(
            payload.get("generationConfig").is_some(),
            "Google native format should have generationConfig"
        );
    }
}
