//! Running one chat turn through the tool-use loop: configuration, bootstrap context and streaming.

use super::*;

impl AgentEngine {
    /// Run the tool-use conversation loop.
    ///
    /// The model freely decides which tools to call at each step, enabling
    /// fluid think→act→observe behavior. Bootstrap context (project structure,
    /// repo map, manifests) is injected on the first turn so the model starts
    /// with project awareness instead of blind.
    pub(crate) async fn run_tool_use_loop(
        &self,
        prompt: &str,
        options: &ChatOptions,
    ) -> Result<String> {
        let cfg = self.effective_config(options);
        let routed_llm = self.routed_llm_for_options(&cfg, options)?;
        let llm = match routed_llm.as_ref() {
            Some(client) => client.as_ref(),
            None => self.llm.as_ref(),
        };
        let project_memory = codingbuddy_core::runtime::blocking_section(|| {
            codingbuddy_memory::MemoryManager::new(&self.workspace)
                .ok()
                .and_then(|mm| mm.read_combined_memory().ok())
        });

        // Build workspace context for environment section in system prompt
        let mut branch = tokio::process::Command::from(
            codingbuddy_core::process_env::git_inspection(&self.workspace),
        );
        branch
            .args(["branch", "--show-current"])
            .current_dir(&self.workspace)
            .kill_on_drop(true);
        let git_branch = branch
            .output()
            .await
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());
        let ws_context = codingbuddy_core::prompts::WorkspaceContext {
            cwd: self.workspace.display().to_string(),
            git_branch,
            os: std::env::consts::OS.to_string(),
        };

        // Classify complexity before building system prompt (planning injection depends on it).
        let complexity = codingbuddy_core::complexity::classify_complexity(prompt);

        // Build repo map for Complex tasks so the model knows the project structure
        let repo_map_summary =
            if complexity == codingbuddy_core::complexity::PromptComplexity::Complex {
                // Lists and reads files: off the async worker.
                Some(codingbuddy_core::runtime::blocking_section(|| {
                    shared::build_repo_map(
                        &self.workspace,
                        prompt,
                        40, // top 40 files
                        &options.additional_dirs,
                    )
                }))
            } else {
                None
            };

        // Select agent profile: manual override takes precedence over auto-selection.
        let profile = options
            .profile_override
            .as_deref()
            .and_then(agent_profiles::profile_by_name)
            .or_else(|| agent_profiles::select_profile(options.mode, prompt, complexity));

        let active_base_model = cfg.llm.active_base_model();
        let active_reasoner_model = cfg.llm.active_reasoner_model();
        let mut session = self.ensure_session_record_for(options.session_id)?;
        if session.status == SessionState::AwaitingApproval && prompt_grants_plan_approval(prompt) {
            session.status = SessionState::ExecutingStep;
            self.store.save_session(&session)?;
            self.append_event_best_effort_for_session(
                Some(session.session_id),
                EventKind::SessionStateChanged {
                    from: SessionState::AwaitingApproval,
                    to: SessionState::ExecutingStep,
                },
            );
            self.emit_stream_chunk_best_effort(StreamChunk::PhaseTransition {
                from: TaskPhase::Plan.as_str().to_string(),
                to: TaskPhase::Execute.as_str().to_string(),
            });
        }

        let mut system_prompt = codingbuddy_core::prompts::build_model_aware_system_prompt(
            project_memory.as_deref(),
            options.system_prompt_override.as_deref(),
            options.system_prompt_append.as_deref(),
            Some(&ws_context),
            complexity,
            repo_map_summary.as_deref(),
            &active_base_model,
        );

        // Inject output style preference
        match cfg.output_style.as_str() {
            "concise" => {
                system_prompt.push_str("\n\nIMPORTANT: Be extremely concise. One-sentence answers preferred. No preamble or trailing summaries.");
            }
            "verbose" => {
                system_prompt.push_str("\n\nExplain your reasoning in detail. Include context, alternatives considered, and trade-offs.");
            }
            _ => {} // "normal" or unrecognized — no injection
        }

        if let Some(plan_addendum) = self.active_plan_prompt_addendum(&session)? {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&plan_addendum);
        }

        if let Some(p) = profile {
            system_prompt.push_str(p.system_prompt_addendum);
        }

        // Append skill catalog with when_to_use hints so the LLM knows available skills
        if let Ok(skill_mgr) = crate::skills::SkillManager::new(&self.workspace)
            && let Ok(skills) = skill_mgr.list(&cfg.skills.paths)
        {
            let prompt_lower = prompt.to_lowercase();
            let mut recommended: Vec<String> = Vec::new();
            let mut available: Vec<String> = Vec::new();

            for s in &skills {
                let Some(hint) = s.when_to_use.as_ref() else {
                    continue;
                };
                let effort_tag = s.effort.as_deref().unwrap_or("medium");
                let line = format!("- `{}` [{}]: {}", s.id, effort_tag, hint);

                // Check if prompt matches the when_to_use keywords
                let hint_words: Vec<&str> = hint.split_whitespace().collect();
                let matches = hint_words
                    .iter()
                    .filter(|w| w.len() > 3)
                    .any(|w| prompt_lower.contains(&w.to_lowercase()));
                if matches {
                    recommended.push(line);
                } else {
                    available.push(line);
                }
            }

            if !recommended.is_empty() {
                system_prompt.push_str("\n\n## Recommended Skills (match this task)\n");
                system_prompt.push_str("Consider invoking these with the `skill` tool:\n");
                system_prompt.push_str(&recommended.join("\n"));
            }
            if !available.is_empty() {
                system_prompt.push_str("\n\n## Other Available Skills\n");
                system_prompt.push_str(&available.join("\n"));
            }
        }

        let read_only = matches!(options.mode, ChatMode::Ask | ChatMode::Context);

        // Complexity-driven initial budget, with evidence-driven escalation in the loop.
        // force_max_think is an explicit user override to maximum.
        let think_budget = if options.force_max_think {
            codingbuddy_core::complexity::MAX_THINK_BUDGET // 64K
        } else {
            codingbuddy_core::complexity::thinking_budget_for(complexity) // Simple 8K / Medium 16K / Complex 32K
        };

        // Gather lightweight bootstrap context so the model starts with project awareness.
        // Budget: ~15% of context window to leave room for conversation.
        let initial_context = codingbuddy_core::runtime::blocking_section(|| {
            self.build_bootstrap_context(prompt, options, cfg.llm.context_window_tokens)
        });
        let initial_phase = match session.status {
            SessionState::Planning | SessionState::AwaitingApproval => Some(TaskPhase::Plan),
            SessionState::ExecutingStep => Some(TaskPhase::Execute),
            SessionState::Verifying => Some(TaskPhase::Verify),
            _ => None,
        };

        let provider_kind = cfg
            .llm
            .active_provider_kind()
            .unwrap_or(codingbuddy_core::ProviderKind::Deepseek);
        let config = tool_loop::ToolLoopConfig {
            model: active_base_model.clone(),
            max_tokens: codingbuddy_core::max_output_tokens_for_model(
                provider_kind,
                &active_base_model,
                true,
            ),
            provider_kind,
            temperature: None, // Incompatible with thinking mode
            context_window_tokens: cfg.llm.context_window_tokens,
            reserved_overhead_tokens: cfg.context.reserved_overhead_tokens,
            response_budget_tokens: cfg.context.response_budget_tokens,
            max_turns: self
                .max_turns
                .or(cfg.budgets.max_turns)
                .unwrap_or(cfg.agent_loop.tool_loop_max_turns)
                .min(cfg.agent_loop.tool_loop_max_turns) as usize,
            read_only,
            thinking: Some(codingbuddy_core::ThinkingConfig::enabled(think_budget)),
            extended_thinking_model: active_reasoner_model,
            complexity,
            subagent_worker: self.build_subagent_worker(),
            skill_runner: self.build_skill_runner(),
            workspace: Some(self.workspace.clone()),
            trusted_workspace: self.cfg.workspace_trust == Some(true),
            privacy_router: build_privacy_router(&cfg),
            images: options.images.clone(),
            initial_context,
            profile_name: profile.map(|p| p.name.to_string()),
            phase_loop_enabled: cfg.agent_loop.phase_loop_enabled,
            initial_phase,
            session_id: Some(session.session_id),
            edit_validator: if self.cfg.workspace_trust == Some(true) && cfg.lsp.enabled {
                Some(std::sync::Arc::new(codingbuddy_lsp::EditValidator::new(
                    self.workspace.clone(),
                    codingbuddy_lsp::LspConfig {
                        enabled: cfg.lsp.enabled,
                        languages: cfg.lsp.languages.clone(),
                        timeout_seconds: cfg.lsp.timeout_seconds,
                    },
                )))
            } else {
                None
            },
            heuristics: cfg
                .agent_loop
                .heuristics
                .resolve(&cfg.llm.provider, &active_base_model),
        };
        tracing::debug!(
            model = %active_base_model,
            provider = %cfg.llm.provider,
            heuristics = ?config.heuristics,
            "tool loop configured"
        );

        // Build tool list: built-in tools + contextual filtering + MCP-discovered tools.
        let builtin_tools =
            runtime_tool_definitions(&self.workspace, self.cfg.workspace_trust == Some(true));
        let signals = detect_signals(prompt, &self.workspace);
        let (tiered_tools, mut discoverable_tools) =
            tiered_tool_definitions(builtin_tools, &signals);
        let mut runtime_tool_metadata = HashMap::new();

        // Move deferred tools to discoverable (discovered via tool_search, not sent by default)
        let mut tools = Vec::with_capacity(tiered_tools.len());
        for tool in tiered_tools {
            let is_deferred = codingbuddy_core::ToolName::from_api_name(&tool.function.name)
                .map(|tn| tn.metadata().deferred)
                .unwrap_or(false);
            if is_deferred {
                discoverable_tools.push(tool);
            } else {
                tools.push(tool);
            }
        }
        register_tool_definitions_metadata(&mut runtime_tool_metadata, &tools);
        register_tool_definitions_metadata(&mut runtime_tool_metadata, &discoverable_tools);
        if let Some(ref mcp) = self.mcp
            && let Ok(mcp_tools) = mcp.discover_tools().await
        {
            for mt in mcp_tools {
                let tool_name = format!("mcp__{}__{}", mt.server_id, mt.name);
                runtime_tool_metadata.insert(tool_name.clone(), mcp_tool_metadata(&tool_name, &mt));
                tools.push(codingbuddy_core::ToolDefinition {
                    tool_type: "function".to_string(),
                    function: codingbuddy_core::FunctionDefinition {
                        name: tool_name,
                        description: truncate_mcp_description(&mt.description),
                        parameters: mcp_tool_parameters(&mt.input_schema),
                        strict: None,
                    },
                });
            }
        }

        let visible = |tool: &codingbuddy_core::ToolDefinition| {
            let name = &tool.function.name;
            let internal = codingbuddy_tools::map_tool_name(name);
            // Resource-specific rules are checked against each actual tool call.
            self.policy
                .denial(&codingbuddy_core::ToolCall {
                    name: internal.to_owned(),
                    args: serde_json::Value::Null,
                    requires_approval: false,
                })
                .is_none()
        };
        tools.retain(visible);
        discoverable_tools.retain(visible);

        // Apply agent profile tool filtering
        if let Some(p) = profile {
            tools =
                agent_profiles::filter_by_profile_with_metadata(tools, p, &runtime_tool_metadata);
            discoverable_tools = agent_profiles::filter_by_profile_with_metadata(
                discoverable_tools,
                p,
                &runtime_tool_metadata,
            );
        }
        if let Some(capabilities) = cfg.llm.capabilities_for_model(&active_base_model) {
            (tools, discoverable_tools) =
                shape_tool_surface(tools, discoverable_tools, capabilities);
        }

        if !discoverable_tools.is_empty() {
            let tool_search = tool_search_definition();
            runtime_tool_metadata.insert(
                tool_search.function.name.clone(),
                RuntimeToolMetadata::for_api_name(&tool_search.function.name),
            );
            tools.push(tool_search);
        }

        let mut loop_ =
            tool_loop::ToolUseLoop::new(llm, self.tool_host.clone(), config, system_prompt, tools);
        loop_.set_budget_limit(self.max_budget_usd.or(cfg.budgets.max_budget_usd));
        loop_.set_discoverable_tools(discoverable_tools);
        loop_.extend_tool_metadata(runtime_tool_metadata);

        if let Some(cb) = self.prompt_stream_callback(options.prompt_id) {
            loop_.set_stream_callback(cb.clone());
            self.tool_host
                .set_progress_callback(Arc::new(move |data: &str| {
                    cb(StreamChunk::ToolProgress {
                        tool_name: "bash_run".to_string(),
                        data: data.to_string(),
                    });
                }));
        }

        // Wire approval handler (clone Arc — engine stays reusable across run() calls)
        {
            let guard = self
                .approval_handler
                .lock()
                .map_err(|_| anyhow!("approval handler mutex poisoned"))?;
            if let Some(ref handler) = *guard {
                loop_.set_approval_callback(handler.clone());
            }
        }

        // Wire user question handler (clone Arc — already Arc<dyn Fn>)
        {
            let guard = self
                .user_question_handler
                .lock()
                .map_err(|_| anyhow!("user question handler mutex poisoned"))?;
            if let Some(ref handler) = *guard {
                loop_.set_user_question_callback(handler.clone());
            }
        }

        loop_.durable_session = Some((self.store.clone(), session.session_id));
        loop_.durable_prompt = options.prompt_id;
        loop_.set_event_callback(Arc::new({
            let store = self.store.clone();
            let session_id = session.session_id;
            move |kind| {
                if let Err(error) =
                    store.append_event(&codingbuddy_store::NewEvent::new(session_id, kind))
                {
                    eprintln!("[agent] failed to persist tool event: {error}");
                }
            }
        }));

        loop_.set_hooks(HookRuntime::from_app_config(&self.workspace, &self.cfg));

        loop_.cancellation = options.cancellation.clone();
        loop_.deadline = options.deadline;
        loop_.parent_run_id = options.parent_run_id;

        // Wire config watcher for hot-reload polling between tool-use turns
        if let Some(ref watcher) = self.config_watcher {
            loop_.set_config_watcher(watcher);
        }

        let ws = self.workspace.clone();
        let checkpoint_session = session.session_id;
        loop_.set_checkpoint_callback(Arc::new(move |reason, paths| {
            codingbuddy_memory::MemoryManager::new(&ws)?.create_checkpoint_linked(
                reason,
                paths,
                Some(checkpoint_session),
            )?;
            Ok(())
        }));

        if !options.chat_history.is_empty() {
            loop_ = loop_.with_history(options.chat_history.clone());
        }

        let result = loop_.run(prompt).await?;

        if !result.tool_calls_made.is_empty() {
            self.observer.verbose_log(&format!(
                "tool-use loop: {} turns, {} tool calls, {} prompt tokens",
                result.turns,
                result.tool_calls_made.len(),
                result.usage.prompt_tokens,
            ));
        }

        Ok(result.response)
    }

    /// Build bootstrap context messages from gather_context.
    ///
    /// Returns a vec of System messages containing project structure, repo map, etc.
    /// Budgeted to ~15% of context window to leave room for conversation.
    pub(crate) fn build_bootstrap_context(
        &self,
        prompt: &str,
        options: &ChatOptions,
        context_window_tokens: u64,
    ) -> Vec<ChatMessage> {
        let bootstrap = gather_context::gather_for_prompt(
            &self.workspace,
            &self.cfg,
            prompt,
            options.mode,
            &options.additional_dirs,
            options.repo_root_override.as_deref(),
        );

        if !bootstrap.enabled || bootstrap.packet.is_empty() {
            return vec![];
        }

        // Enrich with dependency-analysis hub files
        let mut packet = bootstrap.packet;
        if let Ok(mut ctx_mgr) = crate::context::ContextManager::new(&self.workspace)
            && ctx_mgr.analyze_workspace().is_ok()
        {
            let suggestions = ctx_mgr.suggest_relevant_files(prompt, 10);
            if !suggestions.is_empty() {
                packet.push_str("\nKey files (by dependency centrality):\n");
                for s in &suggestions {
                    let reasons = s.reasons.join(", ");
                    packet.push_str(&format!(
                        "  - {} (score: {:.1}, {})\n",
                        s.path.display(),
                        s.score,
                        reasons
                    ));
                }
            }
        }

        // Budget: ~10% of context window (128K × 0.10 ≈ 12.8K tokens)
        // Reduced from /7 to leave more room for conversation context retention.
        let budget_tokens = context_window_tokens / 10;
        let truncated = truncate_to_token_budget(&packet, budget_tokens);

        vec![ChatMessage::System {
            content: format!(
                "The following is background context about the project. Use it to inform your tool calls and analysis. NEVER reference this context directly in your response — the user already knows their project. Do not mention section headers like \"directory structure\" or \"build manifests\". Just act on the information.\n\n{truncated}"
            ),
        }]
    }

    pub(crate) fn prompt_stream_callback(
        &self,
        prompt_id: Option<Uuid>,
    ) -> Option<codingbuddy_core::StreamCallback> {
        let callback = self
            .stream_callback
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        if callback.is_none() && prompt_id.is_none() {
            return None;
        }
        let store = self.store.clone();
        Some(Arc::new(move |chunk: StreamChunk| {
            if matches!(
                chunk,
                StreamChunk::ReasoningDelta(_) | StreamChunk::Done { .. }
            ) {
                return;
            }
            if let Some(id) = prompt_id
                && matches!(
                    chunk,
                    StreamChunk::ContentDelta(_) | StreamChunk::ClearStreamingText
                )
                && let Err(error) = store
                    .append_prompt_event(id, &codingbuddy_core::stream_chunk_to_event_json(&chunk))
            {
                eprintln!("[agent] failed to persist prompt event: {error}");
            }
            if let Some(callback) = &callback {
                callback(chunk);
            }
        }))
    }
}
