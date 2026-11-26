//! Durable session prompts: chat entry point, interrupted-tool retry, invocation reconciliation.
use super::*;

impl AgentEngine {
    /// Run one prompt to completion.
    ///
    /// Returns a boxed `Send` future: team mode runs lanes that call back
    /// into this method on other tasks, and naming the type breaks the
    /// cycle the compiler would otherwise have to resolve to prove `Send`.
    pub fn chat_with_options<'a>(
        &'a self,
        prompt: &'a str,
        options: ChatOptions,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(self.chat_with_options_inner(prompt, options))
    }

    async fn chat_with_options_inner(
        &self,
        prompt: &str,
        mut options: ChatOptions,
    ) -> Result<String> {
        let session = self.ensure_session_record_for(options.session_id)?;
        options.session_id = Some(session.session_id);
        if options.deadline.is_none() && self.cfg.budgets.max_turn_duration_secs > 0 {
            options.deadline = Some(
                std::time::Instant::now()
                    + std::time::Duration::from_secs(self.cfg.budgets.max_turn_duration_secs),
            );
        }
        options
            .cancellation
            .get_or_insert_with(codingbuddy_core::CancellationToken::new);
        let owned_prompt = options.session_lease.is_none();
        let lease = match options.session_lease.take() {
            Some(lease) => {
                anyhow::ensure!(
                    lease.matches(&self.store, session.session_id)?,
                    "invalid session lease"
                );
                lease
            }
            None => Arc::new(self.store.session_lease(session.session_id)?),
        };
        if !owned_prompt {
            let output = self
                .execute_session_prompt(prompt, options, session, &lease)
                .await?;
            self.stream(StreamChunk::Done { reason: None });
            return Ok(output);
        }
        let id = self.store.start_prompt(
            &lease,
            session.session_id,
            Uuid::now_v7(),
            &serde_json::json!({"prompt": prompt}),
        )?;
        options.prompt_id = Some(id);
        if let Some(parent) = options.parent_run_id {
            self.store.attach_operation_parent(id, parent)?;
        }
        let cancellation = options.cancellation.clone();
        let result = self
            .execute_session_prompt(prompt, options, session, &lease)
            .await;
        let output = result.as_ref().ok().map(String::as_str);
        if let Some(provider_error) = result
            .as_ref()
            .err()
            .and_then(codingbuddy_llm::error::diagnostic)
        {
            self.store.append_prompt_event(
                id,
                &serde_json::json!({"type":"provider_error","provider_error":provider_error}),
            )?;
        }
        let error = result.as_ref().err().map(|error| format!("{error:#}"));
        let status = if result.is_ok() {
            "completed"
        } else if cancellation.is_some_and(|token| token.is_cancelled()) {
            "cancelled"
        } else {
            "failed"
        };
        self.store
            .finish_prompt(id, status, output, error.as_deref())?;
        if result.is_ok() {
            self.stream(StreamChunk::Done { reason: None });
        }
        result
    }

    pub async fn retry_interrupted_tool(
        &self,
        session: Uuid,
        previous: Uuid,
    ) -> Result<(Uuid, codingbuddy_core::ToolResult)> {
        use codingbuddy_core::{ApprovedToolCall, ToolExecutionContext, ToolHost, ToolPermission};
        self.ensure_session_record_for(Some(session))?;
        let lease = self.store.session_lease(session)?;
        let call = self.store.interrupted_tool_request(session, previous)?;
        anyhow::ensure!(
            call.name != "batch",
            "Retry the individual tool operations from a new prompt"
        );
        let (proposal, permission) = self.tool_host.propose_with_permission(call);
        if let ToolPermission::Deny { reason, .. } = permission {
            self.tool_host.discard_proposal(proposal.invocation_id);
            self.store.append_event(&codingbuddy_store::NewEvent::new(
                session,
                EventKind::ToolDenied {
                    invocation_id: proposal.invocation_id,
                    tool_name: proposal.call.name,
                    reason: reason.clone(),
                },
            ))?;
            anyhow::bail!("{reason}");
        }
        let invocation = self
            .store
            .retry_invocation(&lease, session, previous, &proposal.call)?;
        let result = self
            .tool_host
            .execute_with_context(
                ApprovedToolCall {
                    invocation_id: proposal.invocation_id,
                    call: proposal.call,
                },
                ToolExecutionContext {
                    session_id: Some(session),
                    invocation_id: Some(invocation.id),
                    cancellation: None,
                    ..Default::default()
                },
            )
            .await;
        anyhow::ensure!(
            result.output["recovery_status"] != "ambiguous",
            "Retry invocation {} may have changed the workspace. Resolve it locally before continuing.",
            invocation.id
        );
        let (message, _) = tool_bridge::tool_result_to_message(
            &invocation.provider_call_id,
            &invocation.name,
            &result,
            Some(&codingbuddy_policy::output_scanner::OutputScanner::new()),
        );
        self.store
            .settle_invocation(session, invocation.id, message, "running")?;
        Ok((invocation.id, result))
    }

    pub(crate) fn reconcile_session_invocations(
        &self,
        lease: &codingbuddy_store::SessionLease,
        session: Uuid,
    ) -> Result<()> {
        use codingbuddy_store::InvocationReceipt;
        use sha2::{Digest, Sha256};
        let files = codingbuddy_core::workspace_fs::WorkspaceFs::open(&self.workspace)?;
        for invocation in self.store.interrupted_invocations(lease, session)? {
            let result = match self.store.invocation_receipt(invocation.id)? {
                Some(InvocationReceipt::File {
                    path,
                    postimage_sha256,
                    execution_root,
                }) if execution_root.as_ref().is_none_or(|root|root.canonicalize().is_ok_and(|root|root==files.root())) => files
                    .resolve(&path, false)
                    .ok()
                    .and_then(|path| files.read(&path).ok())
                    .filter(|bytes| format!("{:x}", Sha256::digest(bytes)) == postimage_sha256)
                    .map(
                        |_| serde_json::json!({"path":path,"recovery_status":"verified_postimage"}),
                    ),
                Some(InvocationReceipt::File {..})=>None,
                Some(InvocationReceipt::Patch { patch_id }) => {
                    let patches = codingbuddy_diff::PatchStore::new(&self.workspace)?;
                    if patches.verify_postimage(patch_id).unwrap_or(false) {
                        self.store
                            .record_patch_application(session, patch_id, true, Vec::new())?;
                        Some(
                            serde_json::json!({"patch_id":patch_id,"applied":true,"recovery_status":"verified_postimage"}),
                        )
                    } else {
                        None
                    }
                }
                None => None,
            };
            if let Some(content) = result {
                self.store.settle_invocation(
                    session,
                    invocation.id,
                    ChatMessage::Tool {
                        tool_call_id: invocation.provider_call_id,
                        tool_name: Some(invocation.name),
                        content: content.to_string(),
                    },
                    "running",
                )?;
            } else {
                anyhow::bail!(
                    "Session paused. Invocation {} ({}) may have had side effects. Resolve it locally before continuing.",
                    invocation.id,
                    invocation.name
                );
            }
        }
        // Earlier ambiguous invocations can delay settlement of later, never-dispatched calls.
        self.store.interrupted_invocations(lease, session)?;
        Ok(())
    }

    pub(crate) async fn execute_session_prompt(
        &self,
        prompt: &str,
        mut options: ChatOptions,
        session: Session,
        lease: &codingbuddy_store::SessionLease,
    ) -> Result<String> {
        let execution = codingbuddy_core::execution::ExecutionContext {
            cancellation: options.cancellation.clone().unwrap_or_default(),
            deadline: options.deadline,
            ..Default::default()
        };
        let _deadline_guard = execution.watch_deadline()?;
        let _cancellation_monitor = options
            .prompt_id
            .map(|id| {
                self.store
                    .monitor_operation(id, options.cancellation.clone().unwrap_or_default())
            })
            .transpose()?;
        self.reconcile_session_invocations(lease, session.session_id)?;
        let mut effective = self.effective_config(&options);
        let uses_routed_client = options.llm_config.is_some() || options.model_spec.is_some();
        if uses_routed_client {
            effective.llm.api_key = codingbuddy_llm::resolve_key_from_config(&effective.llm);
            options.llm_config = Some(effective.llm.clone());
        }
        let model = if !options.tools && options.force_max_think {
            effective.llm.active_reasoner_model()
        } else {
            effective.llm.active_base_model()
        };
        let destination = effective.llm.connection_id();
        let projection = self.store.rebuild_from_events(session.session_id)?;
        if !projection.chat_messages.is_empty()
            && self.store.pinned_route(session.session_id)?.is_none()
        {
            let changed =
                projection
                    .provider_selection
                    .as_ref()
                    .is_some_and(|(provider, _, previous)| {
                        provider != &effective.llm.provider
                            || previous
                                .as_ref()
                                .is_some_and(|previous| previous != &destination)
                    });
            if changed {
                return Err(anyhow!(
                    "Conversation belongs to a different provider or endpoint. Use session portable-fork to preview and transfer history."
                ));
            }
        }
        let local_key = self.store.route_key()?;
        let route = if uses_routed_client {
            codingbuddy_llm::session_route(&effective.llm, &model, &local_key)?
        } else {
            self.llm
                .pinned_session_route(&model, &local_key)
                .unwrap_or_else(|| {
                    codingbuddy_llm::session_route(&effective.llm, &model, &local_key)
                })?
        };
        if projection.legacy_tool_history
            && effective.llm.active_provider_kind().is_some_and(|kind| {
                matches!(
                    codingbuddy_llm::protocol::select_chat_protocol(
                        &effective.llm.active_provider(),
                        kind
                    ),
                    codingbuddy_llm::protocol::ChatProtocol::AnthropicMessages
                        | codingbuddy_llm::protocol::ChatProtocol::GeminiGenerateContent
                )
            })
        {
            anyhow::bail!(
                "Legacy session lacks provider continuation data. Use session portable-fork to preview and continue with its redacted text history."
            );
        }
        let projection = self.store.provider_history(session.session_id, &route)?;
        // Fire SessionStart hook
        let ws = self.workspace.display().to_string();
        let session_start_input =
            codingbuddy_hooks::HookInput::new(codingbuddy_hooks::HookEvent::SessionStart, &ws)
                .with_prompt(prompt)
                .with_session_type("startup");
        let _ = self.hooks.fire_with_context(
            codingbuddy_hooks::HookEvent::SessionStart,
            &session_start_input,
            &execution,
        );

        // Fire Setup hook once per session (not per turn)
        if !self
            .setup_fired
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            let setup_input =
                codingbuddy_hooks::HookInput::new(codingbuddy_hooks::HookEvent::Setup, &ws);
            let _ = self.hooks.fire_with_context(
                codingbuddy_hooks::HookEvent::Setup,
                &setup_input,
                &execution,
            );
        }

        // Fire UserPromptSubmit hook (blocking — can modify prompt)
        let user_submit_input =
            codingbuddy_hooks::HookInput::new(codingbuddy_hooks::HookEvent::UserPromptSubmit, &ws)
                .with_prompt(prompt);
        let submit_result = self.hooks.fire_with_context(
            codingbuddy_hooks::HookEvent::UserPromptSubmit,
            &user_submit_input,
            &execution,
        );
        if submit_result.blocked {
            return Err(anyhow!("prompt blocked by UserPromptSubmit hook"));
        }

        let prompt_enriched =
            enrich_prompt_with_urls(prompt, options.detect_urls, Some(&execution.cancellation));
        let mut history = projection.chat_messages;
        // A prompt that failed, was cancelled, or whose process died leaves its
        // user turn (and possibly tool results) without a final answer. Close it
        // so the model keeps the context but does not answer the stale request
        // alongside the new one.
        if previous_turn_unfinished(&history) {
            let marker = ChatMessage::Assistant {
                content: Some(INTERRUPTED_TURN_MARKER.to_string()),
                native_parts: None,
                reasoning_content: None,
                tool_calls: vec![],
            };
            self.store
                .append_event(&codingbuddy_store::NewEvent::new(
                    session.session_id,
                    EventKind::ChatTurn {
                        message: marker.clone(),
                    },
                ))
                .context("persist interrupted turn marker")?;
            history.push(marker);
        }
        options.chat_history = history;
        self.store.append_event(&codingbuddy_store::NewEvent {
            event_id: uuid::Uuid::now_v7(),
            at: Utc::now(),
            session_id: session.session_id,
            kind: EventKind::ProviderSelected {
                provider: effective.llm.provider.clone(),
                model,
                destination: Some(destination),
            },
        })?;

        self.store
            .append_event(&codingbuddy_store::NewEvent::new(
                session.session_id,
                EventKind::ChatTurn {
                    message: ChatMessage::User {
                        content: prompt_enriched.clone(),
                    },
                },
            ))
            .context("persist user turn")?;

        let analyze = !options.tools;
        let tool_loop = options.tools
            && (options.cancellation.is_some()
                || !should_run_team_orchestration(self, &prompt_enriched, &options));
        let result = if !options.tools {
            self.analyze_prompt(&prompt_enriched, options).await
        } else if !tool_loop {
            team::run(self, &prompt_enriched, &options).await
        } else {
            self.run_tool_use_loop(&prompt_enriched, &options).await
        };

        let result = result.and_then(|text| {
            if analyze || tool_loop {
                return Ok(text);
            }
            if self.store.rebuild_from_events(session.session_id)?.chat_messages.last().is_some_and(
                |message| matches!(message, ChatMessage::Assistant { content: Some(content), tool_calls, .. } if content == &text && tool_calls.is_empty())
            ) {
                return Ok(text);
            }
            self.store
                .append_event(&codingbuddy_store::NewEvent::new(
                    session.session_id,
                    EventKind::ChatTurn {
                        message: ChatMessage::Assistant {
                            content: Some(text.clone()),
                            native_parts: None,
                            reasoning_content: None,
                            tool_calls: vec![],
                        },
                    },
                ))
                .context("persist assistant turn")?;
            Ok(text)
        });

        // Fire SessionEnd hook
        let ws = self.workspace.display().to_string();
        let mut session_end_input =
            codingbuddy_hooks::HookInput::new(codingbuddy_hooks::HookEvent::SessionEnd, &ws);
        session_end_input.tool_result = result
            .as_ref()
            .ok()
            .map(|s| serde_json::Value::String(s.clone()));
        let _ = self.hooks.fire_with_context(
            codingbuddy_hooks::HookEvent::SessionEnd,
            &session_end_input,
            &execution,
        );

        let stop_input = codingbuddy_hooks::HookInput::new(codingbuddy_hooks::HookEvent::Stop, &ws);
        self.hooks
            .fire_with_context(codingbuddy_hooks::HookEvent::Stop, &stop_input, &execution);

        result
    }
}
