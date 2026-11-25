//! Streaming request paths for ApiClient: chat, completion and FIM.

use super::*;

impl ApiClient {
    pub(crate) fn complete_fim_streaming_inner(
        &self,
        req: &FimRequest,
        api_key: Option<&str>,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let mut payload = self.build_fim_payload(req);
        payload["stream"] = json!(true);
        payload["stream_options"] = json!({"include_usage": true});

        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            let response = self
                .apply_auth(
                    self.client.post(self.resolved_endpoint(false, true, false)),
                    api_key,
                )
                .json(&payload)
                .send();

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());

                    if status.is_success() {
                        let mut content_out = String::new();
                        let mut finish_reason: Option<String> = None;
                        let mut usage: Option<codingbuddy_core::TokenUsage> = None;

                        let reader = std::io::BufReader::new(resp);
                        for line_result in std::io::BufRead::lines(reader) {
                            if let Some(ref ct) = self.cancel_token
                                && ct.is_cancelled()
                            {
                                cb(StreamChunk::Done {
                                    reason: Some("cancelled".to_string()),
                                });
                                break;
                            }
                            let line = match line_result {
                                Ok(l) => l,
                                Err(e) => {
                                    last_err = Some(anyhow!("stream read error: {e}"));
                                    break;
                                }
                            };
                            let trimmed = line.trim();
                            if !trimmed.starts_with("data:") {
                                continue;
                            }
                            let chunk = trimmed.trim_start_matches("data:").trim();
                            if chunk == "[DONE]" {
                                cb(StreamChunk::Done { reason: None });
                                break;
                            }
                            let value: Value = match serde_json::from_str(chunk) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            if let Some(usage_value) = value.get("usage")
                                && !usage_value.is_null()
                            {
                                usage = parse_usage_object(Some(usage_value));
                            }
                            let choice = value
                                .get("choices")
                                .and_then(|v| v.as_array())
                                .and_then(|arr| arr.first());
                            let Some(choice) = choice else {
                                continue;
                            };
                            if let Some(reason) =
                                choice.get("finish_reason").and_then(|v| v.as_str())
                            {
                                finish_reason = Some(reason.to_string());
                            }
                            if let Some(text) = choice.get("text").and_then(|v| v.as_str()) {
                                cb(StreamChunk::ContentDelta(text.to_string()));
                                content_out.push_str(text);
                            }
                        }
                        if last_err.is_none() {
                            return Ok(LlmResponse {
                                text: content_out,
                                finish_reason: finish_reason.unwrap_or_else(|| "stop".to_string()),
                                reasoning_content: String::new(),
                                tool_calls: vec![],
                                usage,
                                transport_parts: Vec::new(),
                                compatibility: None,
                            });
                        }
                    } else {
                        let body = resp.text().unwrap_or_default();
                        last_err = Some(format_api_error(
                            status,
                            &body,
                            attempt,
                            self.cfg.max_retries,
                        ));
                    }
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
                        std::thread::sleep(retry_delay_ms(
                            self.cfg.retry_base_ms,
                            attempt,
                            retry_after,
                        ));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(format_transport_error(&e));
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
                        std::thread::sleep(retry_delay_ms(NETWORK_RETRY_BASE_MS, attempt, None));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("fim stream request failed")))
    }

    pub(crate) fn complete_chat_streaming_inner(
        &self,
        req: &ChatRequest,
        api_key: Option<&str>,
        cb: StreamCallback,
        route: &route::ChatRoute,
    ) -> Result<LlmResponse> {
        if let Some(adapter) = extension::registered_adapter(route.protocol) {
            return self.complete_native_adapter(req, route, adapter, Some(cb));
        }
        let mut prepared = self.build_chat_payload_for_route(req, route)?;
        let capabilities = &route.capabilities;
        let chat_protocol = route.protocol;
        let is_responses = chat_protocol == protocol::ChatProtocol::OpenAiResponses;
        let is_native = chat_protocol.is_native();
        if chat_protocol != protocol::ChatProtocol::GeminiGenerateContent {
            prepared.payload["stream"] = json!(true);
        }
        if !is_native
            && !is_responses
            && !route
                .provider
                .gateway
                .as_ref()
                .is_some_and(|g| g.omit_stream_usage_option)
        {
            prepared.payload["stream_options"] = json!({"include_usage":true});
        }
        let stream_endpoint = &route.endpoint;
        let transport = chat_transport::ChatTransport::new(
            Duration::from_secs(self.cfg.timeout_seconds),
            self.cancel_token.clone(),
        )?;
        if let Some(deployment) = &route.deployment {
            prepared.payload["model"] = json!(deployment);
        }
        let cloud_token = match route.auth {
            protocol::AuthStrategy::AzureEntra | protocol::AuthStrategy::GoogleAdc => {
                Some(self.chat_bearer(route, &transport)?)
            }
            _ => None,
        };
        let headers = route.headers(cloud_token.as_deref().or(api_key))?;
        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            self.check_cancellation()?;
            let response = transport.post(stream_endpoint, headers.clone(), &prepared.payload)?;

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());

                    if status.is_success() {
                        let mut content_out = String::new();
                        let mut reasoning_out = String::new();
                        let mut finish_reason: Option<String> = None;
                        let mut tool_call_parts: BTreeMap<u64, StreamToolCall> = BTreeMap::new();
                        let mut completed_tool_calls: Vec<LlmToolCall> = Vec::new();
                        let mut order = Vec::new();
                        let mut usage: Option<codingbuddy_core::TokenUsage> = None;
                        let mut saw_anthropic_stop = false;
                        let mut native_state = providers::NativeStreamState::default();
                        let mut anthropic_blocks = native_parts::AnthropicBlocks::default();
                        let mut gemini_blocks = Vec::new();
                        let mut native_usage = None;
                        let mut anthropic_usage = json!({});
                        let mut responses = providers::responses::Stream::default();
                        let mut reader = std::io::BufReader::new(transport.reader(resp));
                        while let Some(frame) = chat_transport::next_frame(&mut reader)
                            .context("Provider stream read failed")?
                        {
                            let anthropic_event_type = frame.event;
                            let chunk = frame.data.as_str();
                            if chunk == "[DONE]" {
                                if !is_native && !is_responses {
                                    break;
                                }
                                return Err(anyhow!("Unexpected [DONE] in native provider stream"));
                            }
                            let value: Value = serde_json::from_str(chunk)
                                .context("Malformed provider stream event")?;
                            if anthropic_event_type == "error"
                                || value.get("type").and_then(Value::as_str) == Some("error")
                                || !value.get("error").is_none_or(Value::is_null)
                            {
                                return Err(error::ProviderError::http(
                                    chat_protocol,
                                    status.as_u16(),
                                    &value.to_string(),
                                )
                                .into());
                            }

                            if is_responses {
                                if let Some(mut response) = responses.frame(&value, &*cb)? {
                                    response.compatibility = Some(prepared.compatibility.clone());
                                    self.check_cancellation()?;
                                    cb(StreamChunk::Done {
                                        reason: Some(response.finish_reason.clone()),
                                    });
                                    return Ok(response);
                                }
                                continue;
                            }

                            // ── Native Anthropic streaming ──
                            if chat_protocol == protocol::ChatProtocol::AnthropicMessages {
                                let evt_type = if anthropic_event_type.is_empty() {
                                    value.get("type").and_then(|v| v.as_str()).unwrap_or("")
                                } else {
                                    &anthropic_event_type
                                };
                                let usage_part = if evt_type == "message_start" {
                                    value.pointer("/message/usage")
                                } else {
                                    value.get("usage")
                                };
                                if let Some(fields) = usage_part.and_then(Value::as_object) {
                                    anthropic_usage
                                        .as_object_mut()
                                        .expect("object")
                                        .extend(fields.clone());
                                    native_usage =
                                        Some(providers::anthropic::parse_anthropic_usage(
                                            &json!({"usage":anthropic_usage}),
                                        ));
                                }
                                if evt_type == "content_block_start"
                                    && value.pointer("/content_block/type").and_then(Value::as_str)
                                        == Some("text")
                                    && let Some(text) =
                                        value.pointer("/content_block/text").and_then(Value::as_str)
                                    && !text.is_empty()
                                {
                                    cb(StreamChunk::ContentDelta(text.into()));
                                }
                                anthropic_blocks.frame(evt_type, &value)?;
                                let raw =
                                    providers::anthropic::parse_streaming_event(evt_type, &value);
                                let evt = providers::from_anthropic_event(raw);
                                if matches!(evt, providers::ProviderStreamEvent::Done) {
                                    saw_anthropic_stop = true;
                                    break;
                                }
                                native_state.handle_event(evt, &*cb);
                                continue;
                            }

                            // ── Native Google streaming ──
                            if chat_protocol == protocol::ChatProtocol::GeminiGenerateContent {
                                anyhow::ensure!(
                                    value["candidates"].as_array().is_none_or(|c| c.len() <= 1),
                                    "Expected one Gemini candidate"
                                );
                                if value["usageMetadata"].is_object() {
                                    native_usage =
                                        Some(providers::google::parse_gemini_usage(&value));
                                }
                                anyhow::ensure!(
                                    native_state.finish_reason.is_none()
                                        || value.pointer("/candidates/0").is_none(),
                                    "Gemini candidate after terminal event"
                                );
                                if let Some(parts) = value
                                    .pointer("/candidates/0/content/parts")
                                    .and_then(Value::as_array)
                                {
                                    gemini_blocks.extend(parts.iter().cloned());
                                }
                                let raw = providers::google::parse_streaming_chunk(&value);
                                for evt in providers::from_google_event(raw) {
                                    native_state.handle_event(evt, &*cb);
                                }
                                continue;
                            }

                            // ── OpenAI-compatible streaming (default) ──
                            if let Some(usage_value) = value.get("usage")
                                && !usage_value.is_null()
                            {
                                usage = parse_usage_object(Some(usage_value));
                            }
                            let choice = value
                                .get("choices")
                                .and_then(|v| v.as_array())
                                .and_then(|arr| arr.first());
                            let Some(choice) = choice else {
                                continue;
                            };
                            if let Some(reason) =
                                choice.get("finish_reason").and_then(|v| v.as_str())
                            {
                                finish_reason = Some(reason.to_string());
                            }
                            if let Some(delta) = choice.get("delta") {
                                if let Some(content) = delta
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .or_else(|| delta.get("refusal").and_then(Value::as_str))
                                {
                                    content_out.push_str(content);
                                    order.push(Ok::<_, u64>(TransportPart::Text(
                                        content.to_owned(),
                                    )));
                                    cb(StreamChunk::ContentDelta(content.to_string()));
                                }
                                if let Some(reasoning) =
                                    delta.get("reasoning_content").and_then(|v| v.as_str())
                                {
                                    reasoning_out.push_str(reasoning);
                                }
                                if let Some(tool_calls) =
                                    delta.get("tool_calls").and_then(|v| v.as_array())
                                {
                                    for (position, item) in tool_calls.iter().enumerate() {
                                        let id = item["id"].as_str();
                                        let index = item["index"]
                                            .as_u64()
                                            .or_else(|| {
                                                id.and_then(|id| {
                                                    tool_call_parts.iter().find_map(
                                                        |(index, entry)| {
                                                            (entry.id.as_deref() == Some(id))
                                                                .then_some(*index)
                                                        },
                                                    )
                                                })
                                            })
                                            .unwrap_or(position as u64);
                                        let entry =
                                            tool_call_parts.entry(index).or_insert_with(|| {
                                                order.push(Err(index));
                                                StreamToolCall::default()
                                            });
                                        if let Some(id) = id {
                                            anyhow::ensure!(
                                                entry
                                                    .id
                                                    .as_deref()
                                                    .is_none_or(|previous| previous == id),
                                                "Provider changed a tool call ID"
                                            );
                                            entry.id = Some(id.into());
                                        }
                                        if let Some(name) =
                                            item.pointer("/function/name").and_then(Value::as_str)
                                        {
                                            anyhow::ensure!(
                                                entry.name.is_empty() || entry.name == name,
                                                "Provider changed a tool call name"
                                            );
                                            entry.name = name.into();
                                        }
                                        if let Some(args) = item
                                            .pointer("/function/arguments")
                                            .and_then(Value::as_str)
                                        {
                                            entry.arguments.push_str(args);
                                        }
                                    }
                                }
                            }
                            if let Some(message) = choice.get("message") {
                                if let Some(content) = message["content"]
                                    .as_str()
                                    .or_else(|| message["refusal"].as_str())
                                {
                                    content_out.push_str(content);
                                    order.push(Ok(TransportPart::Text(content.into())));
                                    cb(StreamChunk::ContentDelta(content.to_string()));
                                }
                                if let Some(reasoning) = message["reasoning_content"].as_str() {
                                    reasoning_out.push_str(reasoning);
                                }
                                let calls = parse_completed_calls(message.get("tool_calls"))?;
                                order.extend(
                                    calls
                                        .iter()
                                        .map(|call| Ok(TransportPart::Call(call.id.clone()))),
                                );
                                completed_tool_calls.extend(calls);
                            }
                        }

                        self.check_cancellation()?;
                        let finished = match chat_protocol {
                            protocol::ChatProtocol::AnthropicMessages => {
                                saw_anthropic_stop && native_state.finish_reason.is_some()
                            }
                            protocol::ChatProtocol::GeminiGenerateContent => {
                                native_state.finish_reason.is_some()
                            }
                            _ => finish_reason.is_some(),
                        };
                        anyhow::ensure!(
                            finished,
                            "Provider stream ended before a terminal response"
                        );

                        if chat_protocol == protocol::ChatProtocol::GeminiGenerateContent
                            && !matches!(
                                native_state.finish_reason.as_deref(),
                                Some("stop" | "tool_calls")
                            )
                        {
                            return Err(error::ProviderError::new(
                                chat_protocol,
                                error::FailureKind::IncompleteResponse,
                            )
                            .into());
                        }

                        let mut tool_calls = Vec::new();
                        let mut transport_parts = Vec::new();
                        for part in order {
                            match part {
                                Ok(part) => transport_parts.push(part),
                                Err(index) => {
                                    let call = tool_call_parts
                                        .remove(&index)
                                        .expect("reserved tool index");
                                    let id = call.id.unwrap_or_default();
                                    transport_parts.push(TransportPart::Call(id.clone()));
                                    tool_calls.push(LlmToolCall {
                                        id,
                                        name: call.name,
                                        arguments: call.arguments,
                                    });
                                }
                            }
                        }
                        tool_calls.extend(completed_tool_calls);
                        if is_native {
                            let (protocol, blocks) = match chat_protocol {
                                protocol::ChatProtocol::AnthropicMessages => {
                                    (native_parts::ANTHROPIC, anthropic_blocks.finish()?)
                                }
                                _ => (native_parts::GEMINI, gemini_blocks),
                            };
                            (transport_parts, tool_calls) =
                                native_parts::native_response(protocol, blocks)?;
                            content_out = transport_parts
                                .iter()
                                .filter_map(|part| match part {
                                    TransportPart::Text(text) => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect();
                            reasoning_out = native_state.reasoning;
                            if protocol == native_parts::GEMINI
                                && native_state.finish_reason.as_deref() == Some("stop")
                                && !tool_calls.is_empty()
                            {
                                native_state.finish_reason = Some("tool_calls".into());
                            }
                            finish_reason = native_state.finish_reason;
                            usage = native_usage.or(native_state.usage);
                        }
                        if chat_protocol == protocol::ChatProtocol::OpenAiChat
                            && !reasoning_out.is_empty()
                        {
                            transport_parts.push(TransportPart::Native(
                                codingbuddy_core::NativeParts {
                                    protocol: "openai-chat".into(),
                                    blocks: vec![json!({"reasoning_content":reasoning_out})],
                                },
                            ));
                        }
                        validate_completed_calls(&tool_calls)?;
                        let text = content_out;

                        let reason = finish_reason
                            .ok_or_else(|| anyhow!("Provider finish reason missing"))?;
                        cb(StreamChunk::Done {
                            reason: Some(reason.clone()),
                        });
                        let response = LlmResponse {
                            text,
                            finish_reason: reason,
                            reasoning_content: reasoning_out,
                            tool_calls,
                            usage,
                            transport_parts,
                            compatibility: Some(prepared.compatibility.clone()),
                        };
                        // Native providers handle their own format; OpenAI-compat needs postprocessing
                        if is_native {
                            return Ok(response);
                        }
                        return Ok(
                            provider_transform::postprocess_chat_response_with_compatibility(
                                response,
                                capabilities,
                                &req.tools,
                                prepared.compatibility.clone(),
                            ),
                        );
                    }

                    let body = transport.text(resp)?;
                    last_err = Some(
                        error::ProviderError::http(chat_protocol, status.as_u16(), &body).into(),
                    );
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
                        self.wait_before_retry(retry_delay_ms(
                            self.cfg.retry_base_ms,
                            attempt,
                            retry_after,
                        ))?;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(
                        error::ProviderError::new(chat_protocol, error::FailureKind::Transport)
                            .into(),
                    );
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
                        self.wait_before_retry(retry_delay_ms(
                            NETWORK_RETRY_BASE_MS,
                            attempt,
                            None,
                        ))?;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("chat streaming request failed")))
    }

    /// Streaming variant: reads the SSE response line-by-line, invoking `cb`
    /// for each content/reasoning delta, then returns the assembled response.
    pub(crate) fn complete_streaming_inner(
        &self,
        req: &LlmRequest,
        api_key: Option<&str>,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let mut prepared = self.build_payload(req);
        // Force streaming on for the HTTP request
        prepared.payload["stream"] = json!(true);
        prepared.payload["stream_options"] = json!({"include_usage": true});

        let mut last_err: Option<anyhow::Error> = None;
        let mut attempt: u8 = 0;
        while attempt <= self.cfg.max_retries {
            let response = self
                .apply_auth(
                    self.client
                        .post(self.resolved_endpoint(false, false, false)),
                    api_key,
                )
                .json(&prepared.payload)
                .send();

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    // Check retry-after-ms first (milliseconds), fall back to Retry-After (seconds)
                    let retry_after = parse_retry_after(resp.headers());

                    if status.is_success() {
                        // Read SSE line-by-line, invoking callback for each delta
                        let mut content_out = String::new();
                        let mut reasoning_out = String::new();
                        let mut finish_reason: Option<String> = None;
                        let mut tool_call_parts: BTreeMap<u64, StreamToolCall> = BTreeMap::new();
                        let mut completed_tool_calls = Vec::new();
                        let mut usage: Option<codingbuddy_core::TokenUsage> = None;

                        let reader = std::io::BufReader::new(resp);
                        for line_result in reader.lines() {
                            if let Some(ref ct) = self.cancel_token
                                && ct.is_cancelled()
                            {
                                cb(StreamChunk::Done {
                                    reason: Some("cancelled".to_string()),
                                });
                                break;
                            }
                            let line = match line_result {
                                Ok(l) => l,
                                Err(e) => {
                                    last_err = Some(anyhow!("stream read error: {e}"));
                                    break;
                                }
                            };
                            let trimmed = line.trim();
                            if !trimmed.starts_with("data:") {
                                continue;
                            }
                            let chunk = trimmed.trim_start_matches("data:").trim();
                            if chunk == "[DONE]" {
                                cb(StreamChunk::Done { reason: None });
                                break;
                            }
                            let value: Value = match serde_json::from_str(chunk) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            if let Some(usage_value) = value.get("usage")
                                && !usage_value.is_null()
                            {
                                usage = parse_usage_object(Some(usage_value));
                            }
                            let choice = value
                                .get("choices")
                                .and_then(|v| v.as_array())
                                .and_then(|arr| arr.first());
                            let Some(choice) = choice else {
                                continue;
                            };
                            if let Some(reason) =
                                choice.get("finish_reason").and_then(|v| v.as_str())
                            {
                                finish_reason = Some(reason.to_string());
                            }
                            if let Some(delta) = choice.get("delta") {
                                if let Some(content) = delta.get("content").and_then(|v| v.as_str())
                                {
                                    content_out.push_str(content);
                                    cb(StreamChunk::ContentDelta(content.to_string()));
                                }
                                if let Some(reasoning) =
                                    delta.get("reasoning_content").and_then(|v| v.as_str())
                                {
                                    reasoning_out.push_str(reasoning);
                                    cb(StreamChunk::ReasoningDelta(reasoning.to_string()));
                                }
                                if let Some(tool_calls) =
                                    delta.get("tool_calls").and_then(|v| v.as_array())
                                {
                                    merge_stream_tool_calls(tool_calls, &mut tool_call_parts);
                                }
                            }
                            if let Some(message) = choice.get("message") {
                                if let Some(content) =
                                    message.get("content").and_then(|v| v.as_str())
                                {
                                    content_out.push_str(content);
                                    cb(StreamChunk::ContentDelta(content.to_string()));
                                }
                                if let Some(reasoning) =
                                    message.get("reasoning_content").and_then(|v| v.as_str())
                                {
                                    reasoning_out.push_str(reasoning);
                                    cb(StreamChunk::ReasoningDelta(reasoning.to_string()));
                                }
                                if let Some(tool_calls) = message.get("tool_calls") {
                                    completed_tool_calls.extend(parse_tool_calls_array(tool_calls));
                                }
                            }
                        }

                        // If stream read failed, propagate the error
                        if let Some(err) = last_err.take() {
                            return Err(err);
                        }

                        let mut tool_calls: Vec<LlmToolCall> = tool_call_parts
                            .into_iter()
                            .filter_map(|(index, value)| {
                                if value.name.trim().is_empty() {
                                    return None;
                                }
                                Some(LlmToolCall {
                                    id: value
                                        .id
                                        .unwrap_or_else(|| format!("tool_call_{}", index + 1)),
                                    name: value.name,
                                    arguments: value.arguments,
                                })
                            })
                            .collect();
                        if !completed_tool_calls.is_empty() {
                            tool_calls.extend(completed_tool_calls);
                        }

                        let text = if !content_out.is_empty() {
                            content_out
                        } else {
                            reasoning_out.clone()
                        };
                        let mut response = LlmResponse {
                            text,
                            finish_reason: finish_reason.unwrap_or_else(|| "stop".to_string()),
                            reasoning_content: reasoning_out,
                            tool_calls,
                            usage,
                            transport_parts: Vec::new(),
                            compatibility: None,
                        };
                        if !prepared.compatibility.transforms.is_empty()
                            || !prepared.compatibility.degraded_inputs.is_empty()
                        {
                            response.compatibility = Some(prepared.compatibility.clone());
                        }
                        return Ok(response);
                    }

                    let body = resp.text().unwrap_or_default();
                    last_err = Some(with_compatibility_context(
                        format_api_error(
                            status,
                            &redact_key(&body, api_key),
                            attempt,
                            self.cfg.max_retries,
                        ),
                        &prepared.compatibility,
                    ));
                    if should_retry_status(status) && attempt < self.cfg.max_retries {
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format!("HTTP {status}"),
                        );
                        let delay = retry_delay_ms(self.cfg.retry_base_ms, attempt, retry_after);
                        if status == StatusCode::TOO_MANY_REQUESTS {
                            cb(StreamChunk::RateLimited {
                                wait_seconds: delay.as_secs().max(1),
                                attempt: attempt + 1,
                                max_attempts: self.cfg.max_retries,
                                provider: self.cfg.provider.clone(),
                            });
                        }
                        thread::sleep(delay);
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
                Err(e) => {
                    last_err = Some(with_compatibility_context(
                        format_transport_error(&e),
                        &prepared.compatibility,
                    ));
                    if should_retry_transport_error(&e) && attempt < self.cfg.max_retries {
                        // Never log `e` itself: its Display includes the request URL,
                        // which carries the API key for query-key providers (Gemini).
                        log_retry(
                            &self.cfg.provider,
                            attempt,
                            self.cfg.max_retries,
                            &format_transport_error(&e),
                        );
                        thread::sleep(retry_delay_ms(NETWORK_RETRY_BASE_MS, attempt, None));
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    break;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("streaming request failed without detailed error")))
    }
}
