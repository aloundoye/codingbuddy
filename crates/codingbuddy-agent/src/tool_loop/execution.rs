//! Shared tool preparation, execution, and result handling.
use super::*;
use std::time::Instant;

struct PreparedToolCall {
    llm_call: LlmToolCall,
    approved: codingbuddy_core::ApprovedToolCall,
    args_summary: String,
    modified_paths: Vec<std::path::PathBuf>,
    cache_args: serde_json::Value,
    start: Instant,
}

enum ToolPreparation {
    Ready(Box<PreparedToolCall>),
    Complete(Vec<ToolCallRecord>),
}

impl ToolUseLoop<'_> {
    pub(super) fn execute_tool_calls(
        &mut self,
        mut calls: &[LlmToolCall],
    ) -> Result<Vec<ToolCallRecord>> {
        let mut records = Vec::new();
        while let Some((first, rest)) = calls.split_first() {
            let group = calls
                .iter()
                .take_while(|call| {
                    let name = tool_bridge::repair_tool_name(&call.name, &self.tools)
                        .unwrap_or_else(|| call.name.clone());
                    let metadata = self.runtime_metadata_for_tool(&name);
                    name != "batch"
                        && metadata.read_only
                        && metadata.concurrency_safe
                        && !metadata.agent_level
                        && !metadata.approval_required
                })
                .count();
            if group < 2 {
                records.extend(self.execute_tool_call(first)?);
                calls = rest;
                continue;
            }
            let (reads, remaining) = calls.split_at(group);
            for chunk in reads.chunks(16) {
                let ready = chunk
                    .iter()
                    .map(|call| {
                        let start = self.messages.len();
                        let preparation = self.prepare_tool_call(call, false)?;
                        Ok((preparation, self.messages.split_off(start)))
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.check_cancellation()?;
                let host = &self.tool_host;
                let results = std::thread::scope(|scope| {
                    let workers = ready
                        .into_iter()
                        .map(|(preparation, messages)| {
                            let worker = match &preparation {
                                ToolPreparation::Ready(call) => {
                                    let approved = call.approved.clone();
                                    Some(scope.spawn(move || host.execute(approved)))
                                }
                                ToolPreparation::Complete(_) => None,
                            };
                            (preparation, worker, messages)
                        })
                        .collect::<Vec<_>>();
                    workers
                        .into_iter()
                        .map(|(preparation, worker, messages)| {
                            (preparation, worker.map(|worker| worker.join()), messages)
                        })
                        .collect::<Vec<_>>()
                });
                for (preparation, result, messages) in results {
                    self.messages.extend(messages);
                    match preparation {
                        ToolPreparation::Ready(call) => {
                            let result = result.expect("prepared tool worker").unwrap_or_else(
                                |_| codingbuddy_core::ToolResult {
                                    invocation_id: call.approved.invocation_id,
                                    success: false,
                                    output: serde_json::json!({"error": "tool worker panicked"}),
                                },
                            );
                            records.extend(self.finish_tool_call(*call, result, false)?);
                        }
                        ToolPreparation::Complete(completed) => records.extend(completed),
                    }
                }
            }
            calls = remaining;
        }
        Ok(records)
    }

    /// Execute a single call through the same preparation and result pipeline as parallel reads.
    fn execute_tool_call(&mut self, llm_call: &LlmToolCall) -> Result<Vec<ToolCallRecord>> {
        self.execute_tool_call_inner(llm_call, false)
    }

    fn execute_tool_call_inner(
        &mut self,
        llm_call: &LlmToolCall,
        batch_child: bool,
    ) -> Result<Vec<ToolCallRecord>> {
        self.check_cancellation()?;
        if tool_bridge::is_agent_level_tool(&llm_call.name) {
            return self.handle_agent_level_tool(llm_call);
        }
        let prepared = match self.prepare_tool_call(llm_call, batch_child)? {
            ToolPreparation::Ready(prepared) => prepared,
            ToolPreparation::Complete(records) => return Ok(records),
        };
        if !prepared.modified_paths.is_empty()
            && let Some(ref checkpoint) = self.checkpoint_cb
        {
            let _ = checkpoint("before tool execution", &prepared.modified_paths);
        }
        self.check_cancellation()?;
        let result = if prepared.approved.call.name == "batch" {
            self.execute_batch(&prepared.approved)?
        } else {
            self.tool_host.execute(prepared.approved.clone())
        };
        self.finish_tool_call(*prepared, result, batch_child)
    }

    fn prepare_tool_call(
        &mut self,
        llm_call: &LlmToolCall,
        batch_child: bool,
    ) -> Result<ToolPreparation> {
        self.check_cancellation()?;
        let mut records = Vec::new();
        let start = Instant::now();

        let mut llm_call = llm_call.clone();
        if let Some(name) = tool_bridge::repair_tool_name(&llm_call.name, &self.tools) {
            llm_call.name = name;
        }
        if let Some(name) = codingbuddy_core::ToolName::from_internal_name(&llm_call.name) {
            llm_call.name = name.as_api_name().to_owned();
        }

        // ── Circuit breaker ──
        if let Some(state) = self.circuit_breaker.get(&llm_call.name)
            && (state.permanently_disabled
                || (state.consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD
                    && state.cooldown_remaining > 0))
        {
            let duration = start.elapsed().as_millis() as u64;
            self.emit(StreamChunk::ToolCallEnd {
                tool_name: llm_call.name.clone(),
                duration_ms: duration,
                success: false,
                summary: "circuit-broken".to_string(),
            });
            self.messages.push(tool_bridge::tool_error_to_message(
                &llm_call.id,
                &format!(
                    "Tool '{}' is temporarily disabled after {} consecutive failures. \
                     It will be re-enabled in {} turn(s). Try a different approach or tool.",
                    llm_call.name, CIRCUIT_BREAKER_THRESHOLD, state.cooldown_remaining
                ),
            ));
            records.push(ToolCallRecord {
                tool_name: llm_call.name.clone(),
                tool_call_id: llm_call.id.clone(),
                args_summary: String::new(),
                success: false,
                duration_ms: duration,
                args_json: None,
                result_preview: None,
            });
            return Ok(ToolPreparation::Complete(records));
        }

        // Convert LLM call to internal format
        let metadata = self.runtime_metadata_for_tool(&llm_call.name);
        let mut tool_call =
            tool_bridge::llm_tool_call_to_internal_with_metadata(&llm_call, &metadata);
        // Save parsed args before tool_call is moved into propose()
        let mut parsed_args = tool_call.args.clone();

        let invalid = match serde_json::from_str::<serde_json::Value>(&llm_call.arguments) {
            Ok(args) if args.is_object() => self.validate_tool_input(&llm_call.name, &args),
            Ok(_) => Err("tool arguments must be an object".to_owned()),
            Err(err) => Err(format!("invalid tool argument JSON: {err}")),
        };
        if let Err(error) = invalid {
            return Ok(self.reject_tool_call(&llm_call, start, "invalid args", &error));
        }

        let args_summary = summarize_args(&tool_call.args);
        self.emit(StreamChunk::ToolCallStart {
            tool_name: llm_call.name.clone(),
            args_summary: args_summary.clone(),
        });

        // Fire PreToolUse hook
        if let Some(ref hooks) = self.hooks {
            let input = HookInput {
                event: HookEvent::PreToolUse.as_str().to_string(),
                tool_name: Some(llm_call.name.clone()),
                tool_input: Some(tool_call.args.clone()),
                tool_result: None,
                prompt: None,
                session_type: None,
                workspace: self.workspace_str().to_string(),
            };
            let hook_result = hooks.fire(HookEvent::PreToolUse, &input);
            if hook_result.blocked
                || hook_result.permission_decision
                    == Some(codingbuddy_hooks::PermissionDecision::Deny)
            {
                let extra = hook_result.block_reason.as_deref().unwrap_or("");
                let duration = start.elapsed().as_millis() as u64;
                self.emit(StreamChunk::ToolCallEnd {
                    tool_name: llm_call.name.clone(),
                    duration_ms: duration,
                    success: false,
                    summary: "blocked by hook".to_string(),
                });
                let msg = if extra.is_empty() {
                    "Tool call blocked by pre-tool-use hook. Try a different approach.".to_string()
                } else {
                    format!(
                        "Tool call blocked by pre-tool-use hook: {extra}. Try a different approach."
                    )
                };
                self.messages
                    .push(tool_bridge::tool_error_to_message(&llm_call.id, &msg));
                records.push(ToolCallRecord {
                    tool_name: llm_call.name.clone(),
                    tool_call_id: llm_call.id.clone(),
                    args_summary,
                    success: false,
                    duration_ms: duration,
                    args_json: args_json_for_record(&llm_call.name, &llm_call.arguments),
                    result_preview: None,
                });
                return Ok(ToolPreparation::Complete(records));
            }
            // Apply modified arguments from hook (input modification).
            if let Some(modified_args) = hook_result.updated_input {
                if let Err(error) = self.validate_tool_input(&llm_call.name, &modified_args) {
                    return Ok(self.reject_tool_call(
                        &llm_call,
                        start,
                        "invalid hook args",
                        &error,
                    ));
                }
                tool_call.args = modified_args;
                parsed_args = tool_call.args.clone();
            }
        }

        if let Err(error) = self.tool_host.validate(&tool_call) {
            return Ok(self.reject_tool_call(
                &llm_call,
                start,
                "tool precondition failed",
                &error.to_string(),
            ));
        }
        let args_summary = summarize_args(&parsed_args);
        llm_call.arguments = parsed_args.to_string();

        let proposal = self.tool_host.propose(tool_call);
        if let Some(ref cb) = self.event_cb {
            cb(EventKind::ToolProposed {
                proposal: proposal.clone(),
            });
        }

        if !proposal.approved {
            // Check PermissionRequest hooks — can auto-approve matching patterns
            let mut hook_decision = None;
            if let Some(ref hooks) = self.hooks {
                let perm_input = HookInput {
                    event: HookEvent::PermissionRequest.as_str().to_string(),
                    tool_name: Some(llm_call.name.clone()),
                    tool_input: Some(parsed_args.clone()),
                    tool_result: None,
                    prompt: None,
                    session_type: None,
                    workspace: self.workspace_str().to_string(),
                };
                let result = hooks.fire(HookEvent::PermissionRequest, &perm_input);
                hook_decision = if result.blocked {
                    Some(codingbuddy_hooks::PermissionDecision::Deny)
                } else {
                    result.permission_decision
                };
            }

            let approved = if hook_decision == Some(codingbuddy_hooks::PermissionDecision::Deny) {
                false
            } else if hook_decision == Some(codingbuddy_hooks::PermissionDecision::Allow) {
                true
            } else if let Some(ref cb) = self.approval_cb {
                match cb(&proposal.call) {
                    Ok(v) => v,
                    Err(e) => {
                        // Push a Tool message so the conversation chain stays valid
                        // (prevents HTTP 400 "insufficient tool messages" from the API)
                        let err_msg = format!("Approval error: {e}");
                        if let Some(ref cb) = self.event_cb {
                            cb(EventKind::ToolDenied {
                                invocation_id: proposal.invocation_id,
                                tool_name: llm_call.name.clone(),
                                reason: err_msg.clone(),
                            });
                        }
                        self.messages
                            .push(tool_bridge::tool_error_to_message(&llm_call.id, &err_msg));
                        let duration = start.elapsed().as_millis() as u64;
                        self.emit(StreamChunk::ToolCallEnd {
                            tool_name: llm_call.name.clone(),
                            duration_ms: duration,
                            success: false,
                            summary: err_msg,
                        });
                        records.push(ToolCallRecord {
                            tool_name: llm_call.name.clone(),
                            tool_call_id: llm_call.id.clone(),
                            args_summary,
                            success: false,
                            duration_ms: duration,
                            args_json: args_json_for_record(&llm_call.name, &llm_call.arguments),
                            result_preview: None,
                        });
                        return Ok(ToolPreparation::Complete(records));
                    }
                }
            } else {
                false
            };

            if !approved {
                if let Some(ref cb) = self.event_cb {
                    cb(EventKind::ToolDenied {
                        invocation_id: proposal.invocation_id,
                        tool_name: llm_call.name.clone(),
                        reason: if hook_decision
                            == Some(codingbuddy_hooks::PermissionDecision::Deny)
                        {
                            "permission hook denied the tool".to_owned()
                        } else {
                            "approval was not granted".to_owned()
                        },
                    });
                }
                let duration = start.elapsed().as_millis() as u64;
                self.emit(StreamChunk::ToolCallEnd {
                    tool_name: llm_call.name.clone(),
                    duration_ms: duration,
                    success: false,
                    summary: "denied by user".to_string(),
                });

                // Track denial for burst detection
                let threshold_hit = self.denial_tracker.record_denial(&llm_call.name);
                let denial_msg = if threshold_hit && !self.denial_tracker.guidance_injected {
                    self.denial_tracker.guidance_injected = true;
                    safety::DENIAL_GUIDANCE.to_string()
                } else {
                    permission_execution::denial_guidance_for_tool(&llm_call.name)
                };
                self.messages.push(tool_bridge::tool_error_to_message(
                    &llm_call.id,
                    &denial_msg,
                ));

                records.push(ToolCallRecord {
                    tool_name: llm_call.name.clone(),
                    tool_call_id: llm_call.id.clone(),
                    args_summary,
                    success: false,
                    duration_ms: duration,
                    args_json: args_json_for_record(&llm_call.name, &llm_call.arguments),
                    result_preview: None,
                });
                return Ok(ToolPreparation::Complete(records));
            }
            // Approved after previous denials — reset tracker
            if self.denial_tracker.denials_pending() {
                self.denial_tracker.reset();
            }
            if let Some(ref cb) = self.event_cb {
                cb(EventKind::ToolApproved {
                    invocation_id: proposal.invocation_id,
                });
            }
        }

        let cache_args = self.tool_host.cache_key(&proposal.call);
        // ── Cache lookup ──
        // For read-only tools, check if we have a recent cached result.
        // Reuse the already-parsed args from tool_call instead of re-parsing.
        if !batch_child
            && let Some(cached_result) = self.tool_cache.lookup(&llm_call.name, &cache_args)
        {
            if let Some(ref cb) = self.event_cb {
                cb(EventKind::ToolCacheHit {
                    tool_name: llm_call.name.clone(),
                });
            }
            let duration = start.elapsed().as_millis() as u64;
            let result = codingbuddy_core::ToolResult {
                invocation_id: proposal.invocation_id,
                success: true,
                output: cached_result,
            };
            if let Some(ref cb) = self.event_cb {
                cb(EventKind::ToolResult {
                    result: result.clone(),
                });
            }
            let (msg, injection_warnings) = tool_bridge::tool_result_to_message_with_limit(
                &llm_call.id,
                &llm_call.name,
                &result,
                Some(&self.output_scanner),
                self.runtime_metadata_for_tool(&llm_call.name)
                    .max_result_chars(),
            );
            self.messages.push(msg);
            self.emit_injection_warnings(&injection_warnings);
            self.emit(StreamChunk::ToolCallEnd {
                tool_name: llm_call.name.clone(),
                duration_ms: duration,
                success: true,
                summary: "cached".to_string(),
            });
            records.push(ToolCallRecord {
                tool_name: llm_call.name.clone(),
                tool_call_id: llm_call.id.clone(),
                args_summary,
                success: true,
                duration_ms: duration,
                args_json: args_json_for_record(&llm_call.name, &llm_call.arguments),
                result_preview: None,
            });
            return Ok(ToolPreparation::Complete(records));
        }

        let modified_paths =
            tool_bridge::extract_modified_paths(&llm_call.name, &parsed_args.to_string());
        Ok(ToolPreparation::Ready(Box::new(PreparedToolCall {
            llm_call,
            approved: permission_execution::approved_call_after_permission(proposal),
            args_summary,
            modified_paths,
            cache_args,
            start,
        })))
    }

    fn finish_tool_call(
        &mut self,
        prepared: PreparedToolCall,
        mut result: codingbuddy_core::ToolResult,
        batch_child: bool,
    ) -> Result<Vec<ToolCallRecord>> {
        let PreparedToolCall {
            llm_call,
            approved,
            args_summary,
            modified_paths,
            cache_args,
            start,
        } = prepared;
        let parsed_args = approved.call.args;
        let mut records = Vec::new();

        let duration = start.elapsed().as_millis() as u64;
        let success = result.success;

        // ── Circuit breaker update ──
        let cb_state = self
            .circuit_breaker
            .entry(llm_call.name.clone())
            .or_default();
        if success {
            cb_state.consecutive_failures = 0;
            cb_state.cooldown_remaining = 0;
        } else {
            cb_state.consecutive_failures += 1;
            cb_state.last_error = Some({
                // Extract error from string or JSON object {"error": "..."}
                let err_text = result
                    .output
                    .as_str()
                    .or_else(|| result.output.get("error").and_then(|e| e.as_str()))
                    .unwrap_or("unknown error");
                err_text.chars().take(200).collect::<String>()
            });
            if cb_state.consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
                cb_state.session_trip_count += 1;
                if cb_state.session_trip_count >= 2 {
                    cb_state.permanently_disabled = true;
                }
                cb_state.cooldown_remaining = CIRCUIT_BREAKER_COOLDOWN_TURNS;
                if let Some(ref cb) = self.event_cb {
                    cb(EventKind::CircuitBreakerTripped {
                        tool_name: llm_call.name.clone(),
                        consecutive_failures: CIRCUIT_BREAKER_THRESHOLD as u64,
                        cooldown_turns: CIRCUIT_BREAKER_COOLDOWN_TURNS as u64,
                    });
                }
                // Inject system message so the LLM knows why and can self-correct
                let last_err = cb_state.last_error.as_deref().unwrap_or("unknown error");
                let permanent = if cb_state.permanently_disabled {
                    " This tool is now PERMANENTLY disabled for this session."
                } else {
                    ""
                };
                self.messages.push(ChatMessage::System {
                    content: format!(
                        "Tool '{}' has been temporarily disabled after {} consecutive failures. \
                         Last error: {}. Try a different approach.{}",
                        llm_call.name, CIRCUIT_BREAKER_THRESHOLD, last_err, permanent
                    ),
                });
                cb_state.consecutive_failures = 0;
            }
        }

        if !self.runtime_metadata_for_tool(&llm_call.name).read_only {
            self.tool_cache.clear();
        }

        let lsp_diagnostics_text = if success && !modified_paths.is_empty() {
            verification::post_edit_diagnostics(
                self.config.edit_validator.as_ref(),
                &modified_paths,
            )
        } else {
            String::new()
        };

        // Scan tool output for evidence-driven budget escalation signals
        if let Some(output_str) = result.output.as_str() {
            self.escalation.scan_tool_output(&llm_call.name, output_str);
            if !success {
                self.track_error(output_str);
            }
        } else if let Ok(output_text) = serde_json::to_string(&result.output) {
            self.escalation
                .scan_tool_output(&llm_call.name, &output_text);
            if !success {
                self.track_error(&output_text);
            }
        }

        // Track file accesses for post-compaction re-injection
        if success && let Some(path) = parsed_args.get("path").and_then(|v| v.as_str()) {
            self.track_file_access(path);
        }

        // Fire PostToolUse hook
        if let Some(ref hooks) = self.hooks {
            let input = HookInput {
                event: HookEvent::PostToolUse.as_str().to_string(),
                tool_name: Some(llm_call.name.clone()),
                tool_input: Some(parsed_args.clone()),
                tool_result: Some(result.output.clone()),
                prompt: None,
                session_type: None,
                workspace: self.workspace_str().to_string(),
            };
            let post_hook_result = hooks.fire(HookEvent::PostToolUse, &input);
            for run in &post_hook_result.runs {
                if !run.success {
                    eprintln!(
                        "[hooks] PostToolUse hook failed: {}",
                        run.handler_description
                    );
                }
            }
            if let Some(modified) = post_hook_result.modified_result {
                result.output = serde_json::Value::String(modified);
            }
            if !success {
                let fail_input = HookInput {
                    event: HookEvent::PostToolUseFailure.as_str().to_string(),
                    ..input
                };
                Self::fire_hook_logged(hooks, HookEvent::PostToolUseFailure, &fail_input);
            }
        }

        // Log ToolResult event
        if let Some(ref cb) = self.event_cb {
            cb(EventKind::ToolResult {
                result: codingbuddy_core::ToolResult {
                    invocation_id: result.invocation_id,
                    success: result.success,
                    output: result.output.clone(),
                },
            });
        }

        self.emit(StreamChunk::ToolCallEnd {
            tool_name: llm_call.name.clone(),
            duration_ms: duration,
            success,
            summary: if success {
                "ok".to_string()
            } else {
                "error".to_string()
            },
        });

        // Persist large outputs to disk BEFORE truncation so the full content is saved
        let file_hint = result
            .output
            .as_str()
            .and_then(|s| self.persist_large_output(&llm_call.name, s));

        // Per-tool output truncation using metadata-driven max_result_chars
        let max_chars = self
            .runtime_metadata_for_tool(&llm_call.name)
            .max_result_chars();
        result_handling::truncate_result_output(&mut result, max_chars);

        // Preserve the child result's JSON shape inside the batch response.
        let message_result = if batch_child {
            codingbuddy_core::ToolResult {
                invocation_id: result.invocation_id,
                success: true,
                output: serde_json::Value::String(result.output.to_string()),
            }
        } else {
            result.clone()
        };
        // Convert result to ChatMessage::Tool and append, scanning for security issues
        let (msg, injection_warnings) = tool_bridge::tool_result_to_message_with_limit(
            &llm_call.id,
            &llm_call.name,
            &message_result,
            Some(&self.output_scanner),
            max_chars,
        );
        // Append file persistence hint to the truncated message content.
        let msg = if let Some(hint) = file_hint {
            map_tool_content(msg, |c| format!("{c}{hint}"))
        } else {
            msg
        };
        let msg = self.apply_privacy_to_message(msg);

        // ── Cache store (after privacy filtering) ──
        // Cache successful read-only results with privacy-filtered content.
        if success && !batch_child {
            if let ChatMessage::Tool { ref content, .. } = msg {
                self.tool_cache.store(
                    &llm_call.name,
                    &cache_args,
                    &serde_json::Value::String(content.clone()),
                );
            } else {
                self.tool_cache
                    .store(&llm_call.name, &cache_args, &result.output);
            }
        }

        // Append LSP diagnostics directly to the tool result message for provider compatibility
        let msg = if !lsp_diagnostics_text.is_empty() {
            map_tool_content(msg, |c| {
                format!(
                    "{c}\n\n⚠ Post-edit diagnostics:\n{lsp_diagnostics_text}Fix these before proceeding."
                )
            })
        } else {
            msg
        };
        self.messages.push(msg);

        self.emit_injection_warnings(&injection_warnings);

        // Build result preview for consistency checking (first ~200 chars)
        let result_preview = result.output.as_str().map(|s| {
            let end = s.floor_char_boundary(200.min(s.len()));
            s[..end].to_string()
        });

        records.push(ToolCallRecord {
            tool_name: llm_call.name.clone(),
            tool_call_id: llm_call.id.clone(),
            args_summary,
            success,
            duration_ms: duration,
            args_json: args_json_for_record(&llm_call.name, &llm_call.arguments),
            result_preview,
        });

        Ok(records)
    }

    fn validate_tool_input(&self, name: &str, args: &serde_json::Value) -> Result<(), String> {
        if !args.is_object() {
            return Err("tool arguments must be an object".to_owned());
        }
        if let Some(reason) = self.tool_visibility_error(name) {
            return Err(format!("tool '{name}' is unavailable in {reason}"));
        }
        if name == "batch" {
            codingbuddy_tools::batch_children(args).map_err(|error| error.to_string())?;
        }
        codingbuddy_tools::validate_tool_args_schema(name, args, &self.tools)
    }

    fn reject_tool_call(
        &mut self,
        call: &LlmToolCall,
        start: Instant,
        summary: &str,
        error: &str,
    ) -> ToolPreparation {
        let duration_ms = start.elapsed().as_millis() as u64;
        self.emit(StreamChunk::ToolCallEnd {
            tool_name: call.name.clone(),
            duration_ms,
            success: false,
            summary: summary.to_owned(),
        });
        self.messages
            .push(tool_bridge::tool_error_to_message(&call.id, error));
        ToolPreparation::Complete(vec![ToolCallRecord {
            tool_name: call.name.clone(),
            tool_call_id: call.id.clone(),
            args_summary: String::new(),
            success: false,
            duration_ms,
            args_json: None,
            result_preview: None,
        }])
    }

    fn execute_batch(
        &mut self,
        approved: &codingbuddy_core::ApprovedToolCall,
    ) -> Result<codingbuddy_core::ToolResult> {
        let children = codingbuddy_tools::batch_children(&approved.call.args)?;
        let mut results = Vec::with_capacity(children.len());
        let mut succeeded = 0;
        for (index, (name, args)) in children.into_iter().enumerate() {
            let child = LlmToolCall {
                id: format!("{}:{}", approved.invocation_id, index),
                name: codingbuddy_core::ToolName::from_internal_name(name)
                    .map_or(name, |tool| tool.as_api_name())
                    .to_owned(),
                arguments: args.to_string(),
            };
            let records = self.execute_tool_call_inner(&child, true)?;
            let message = self.messages.pop();
            let content = match message {
                Some(ChatMessage::Tool { content, .. }) => content,
                other => {
                    self.messages.extend(other);
                    String::new()
                }
            };
            let success = records.iter().all(|record| record.success);
            succeeded += usize::from(success);
            let output = serde_json::from_str(&content).unwrap_or_else(|_| {
                if success {
                    serde_json::Value::String(content)
                } else {
                    serde_json::json!({"error": content})
                }
            });
            results.push(serde_json::json!({"tool": name, "success": success, "output": output}));
        }
        let total = results.len();
        Ok(codingbuddy_core::ToolResult {
            invocation_id: approved.invocation_id,
            success: true,
            output: serde_json::json!({"results": results, "total": total, "succeeded": succeeded, "failed": total - succeeded}),
        })
    }
}
