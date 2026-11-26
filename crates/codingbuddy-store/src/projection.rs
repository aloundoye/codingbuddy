//! Event storage versioning and rebuilding projections from events.

use super::*;

pub(crate) const EVENT_STORAGE_VERSION: i64 = 14;

pub(crate) fn assign_legacy_sequence(
    sequences: &mut std::collections::HashMap<String, i64>,
    session: &str,
    recorded: i64,
) -> Result<i64> {
    let latest = sequences.entry(session.to_owned()).or_default();
    *latest = recorded.max(
        latest
            .checked_add(1)
            .context("legacy event sequence exhausted")?,
    );
    Ok(*latest)
}

#[derive(Debug, Default, Clone)]
pub struct RebuildProjection {
    pub transcript: Vec<String>,
    /// Structured chat messages for accurate session resume (from ChatTurn events).
    pub chat_messages: Vec<ChatMessage>,
    pub legacy_tool_history: bool,
    pub(crate) model_steps: Vec<(Uuid, usize)>,
    pub provider_selection: Option<(String, String, Option<String>)>,
    pub latest_plan: Option<Plan>,
    pub step_status: Vec<(Uuid, bool, String)>,
    pub staged_patches: Vec<Uuid>,
    pub applied_patches: Vec<Uuid>,
    pub tool_invocations: Vec<Uuid>,
    pub approved_invocations: Vec<Uuid>,
    pub denied_invocations: Vec<Uuid>,
    pub state: Option<SessionState>,
    pub plugin_events: Vec<String>,
    pub usage_input_tokens: u64,
    pub usage_cache_hit_tokens: u64,
    pub usage_cache_miss_tokens: u64,
    pub usage_output_tokens: u64,
    pub cost_input_tokens: u64,
    pub cost_output_tokens: u64,
    pub estimated_cost_usd: f64,
    pub compaction_events: usize,
    pub autopilot_runs: Vec<Uuid>,
    pub permission_mode: Option<String>,
    pub task_ids: Vec<Uuid>,
    pub review_ids: Vec<Uuid>,
    pub last_provider_model: Option<String>,
    pub last_provider_compatibility: Option<codingbuddy_core::AppliedCompatibility>,
}

pub(crate) fn portable_call_id(id: &str) -> &str {
    if let Some(rest) = id.strip_prefix("gemini:")
        && let Some((position, _)) = rest.split_once(':')
    {
        return position;
    }
    id
}

pub(crate) fn public_event(mut kind: EventKind) -> EventKind {
    if let EventKind::ChatTurn { message } = &mut kind {
        match message {
            ChatMessage::Assistant {
                reasoning_content,
                tool_calls,
                ..
            } => {
                *reasoning_content = None;
                for call in tool_calls {
                    call.id = portable_call_id(&call.id).to_owned();
                }
            }
            ChatMessage::Tool { tool_call_id, .. } => {
                *tool_call_id = portable_call_id(tool_call_id).to_owned();
            }
            _ => {}
        }
    }
    kind
}

pub(crate) fn apply_projection(proj: &mut RebuildProjection, event: &EventEnvelope) {
    match &event.kind {
        EventKind::ProviderSelected {
            provider,
            model,
            destination,
        } => {
            proj.provider_selection = Some((provider.clone(), model.clone(), destination.clone()));
        }

        EventKind::TurnAdded { role, content } => {
            proj.transcript.push(format!("{role}: {content}"))
        }
        EventKind::ChatTurn { message } => {
            if matches!(message, ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty())
            {
                proj.legacy_tool_history = true;
            }
            proj.chat_messages.push(message.clone());
        }
        EventKind::ModelStep { step_id, parts } => {
            proj.model_steps.push((*step_id, proj.chat_messages.len()));
            let mut text = String::new();
            let mut calls = Vec::new();
            for part in parts {
                match part {
                    codingbuddy_core::ModelPart::Text { text: value } => text.push_str(value),
                    codingbuddy_core::ModelPart::Call {
                        provider_call_id,
                        name,
                        arguments,
                        ..
                    } => calls.push(codingbuddy_core::LlmToolCall {
                        id: provider_call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.to_string(),
                    }),
                }
            }
            proj.chat_messages.push(ChatMessage::Assistant {
                content: (!text.is_empty()).then_some(text),
                native_parts: None,
                reasoning_content: None,
                tool_calls: calls,
            });
        }
        EventKind::ModelToolResult { message, .. } => proj.chat_messages.push(message.clone()),
        EventKind::TurnReverted { turns_dropped } => {
            let keep_chat = proj
                .chat_messages
                .len()
                .saturating_sub(*turns_dropped as usize);
            proj.chat_messages.truncate(keep_chat);
            proj.model_steps
                .retain(|(_, position)| *position < keep_chat);
            let keep_transcript = proj
                .transcript
                .len()
                .saturating_sub(*turns_dropped as usize);
            proj.transcript.truncate(keep_transcript);
        }
        EventKind::PlanCreated { plan } | EventKind::PlanRevised { plan } => {
            proj.latest_plan = Some(plan.clone())
        }
        EventKind::StepMarked {
            step_id,
            done,
            note,
        } => proj.step_status.push((*step_id, *done, note.clone())),
        EventKind::PatchStaged { patch_id, .. } => proj.staged_patches.push(*patch_id),
        EventKind::PatchApplied {
            patch_id, applied, ..
        } => {
            if *applied {
                proj.applied_patches.push(*patch_id)
            }
        }
        EventKind::ToolProposed { proposal } => {
            if proj.model_steps.is_empty()
                && proj
                    .chat_messages
                    .iter()
                    .any(|message| matches!(message, ChatMessage::User { .. }))
            {
                proj.legacy_tool_history = true;
            }
            proj.tool_invocations.push(proposal.invocation_id);
        }
        EventKind::ToolApproved { invocation_id } => proj.approved_invocations.push(*invocation_id),
        EventKind::ToolDenied { invocation_id, .. } => proj.denied_invocations.push(*invocation_id),
        EventKind::SessionStateChanged { to, .. } => proj.state = Some(to.clone()),
        EventKind::PluginInstalled { plugin_id, .. }
        | EventKind::PluginRemoved { plugin_id }
        | EventKind::PluginEnabled { plugin_id }
        | EventKind::PluginDisabled { plugin_id } => proj.plugin_events.push(plugin_id.clone()),
        EventKind::UsageUpdated {
            input_tokens,
            cache_hit_tokens,
            cache_miss_tokens,
            output_tokens,
            ..
        } => {
            proj.usage_input_tokens = proj.usage_input_tokens.saturating_add(*input_tokens);
            proj.usage_cache_hit_tokens = proj
                .usage_cache_hit_tokens
                .saturating_add(*cache_hit_tokens);
            proj.usage_cache_miss_tokens = proj
                .usage_cache_miss_tokens
                .saturating_add(*cache_miss_tokens);
            proj.usage_output_tokens = proj.usage_output_tokens.saturating_add(*output_tokens);
        }
        EventKind::CostUpdated {
            input_tokens,
            output_tokens,
            estimated_cost_usd,
        } => {
            proj.cost_input_tokens = proj.cost_input_tokens.saturating_add(*input_tokens);
            proj.cost_output_tokens = proj.cost_output_tokens.saturating_add(*output_tokens);
            proj.estimated_cost_usd += *estimated_cost_usd;
        }
        EventKind::ContextCompacted { .. } => {
            proj.compaction_events = proj.compaction_events.saturating_add(1)
        }
        EventKind::AutopilotRunStarted { run_id, .. } => proj.autopilot_runs.push(*run_id),
        EventKind::PermissionModeChanged { to, .. } => proj.permission_mode = Some(to.clone()),
        EventKind::TaskCreated { task_id, .. } => proj.task_ids.push(*task_id),
        EventKind::ReviewStarted { review_id, .. } => proj.review_ids.push(*review_id),
        EventKind::ProviderCompatibilityApplied {
            model,
            provider,
            transforms,
            degraded_inputs,
        } => {
            proj.last_provider_model = Some(model.clone());
            proj.last_provider_compatibility = Some(codingbuddy_core::AppliedCompatibility {
                provider: provider.clone(),
                family: String::new(),
                transforms: transforms.clone(),
                degraded_inputs: degraded_inputs.clone(),
            });
        }
        _ => {}
    }
}

pub(crate) fn event_kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::TurnAdded { .. } => "TurnAdded@v1",
        EventKind::ChatTurn { .. } => "ChatTurn@v1",
        EventKind::ModelStep { .. } => "ModelStep@v1",
        EventKind::ModelToolResult { .. } => "ModelToolResult@v1",
        EventKind::TurnReverted { .. } => "TurnReverted@v1",
        EventKind::SessionStateChanged { .. } => "SessionStateChanged@v1",
        EventKind::RunStarted { .. } => "RunStarted@v1",
        EventKind::RunStateChanged { .. } => "RunStateChanged@v1",
        EventKind::RunCompleted { .. } => "RunCompleted@v1",
        EventKind::PlanCreated { .. } => "PlanCreated@v1",
        EventKind::PlanRevised { .. } => "PlanRevised@v1",
        EventKind::StepMarked { .. } => "StepMarked@v1",
        EventKind::ToolProposed { .. } => "ToolProposed@v1",
        EventKind::ToolApproved { .. } => "ToolApproved@v1",
        EventKind::ToolResult { .. } => "ToolResult@v1",
        EventKind::AgentStepStarted { .. } => "AgentStepStarted@v1",
        EventKind::AgentStepEnded { .. } => "AgentStepEnded@v1",
        EventKind::ToolInputStarted { .. } => "ToolInputStarted@v1",
        EventKind::ToolInputEnded { .. } => "ToolInputEnded@v1",
        EventKind::ToolExecutionStarted { .. } => "ToolExecutionStarted@v1",
        EventKind::ToolExecutionEnded { .. } => "ToolExecutionEnded@v1",
        EventKind::ToolRetryScheduled { .. } => "ToolRetryScheduled@v1",
        EventKind::PatchStaged { .. } => "PatchStaged@v1",
        EventKind::PatchApplied { .. } => "PatchApplied@v1",
        EventKind::VerificationRun { .. } => "VerificationRun@v1",
        EventKind::CommitProposal { .. } => "CommitProposal@v1",
        EventKind::PluginInstalled { .. } => "PluginInstalled@v1",
        EventKind::PluginRemoved { .. } => "PluginRemoved@v1",
        EventKind::PluginEnabled { .. } => "PluginEnabled@v1",
        EventKind::PluginDisabled { .. } => "PluginDisabled@v1",
        EventKind::UsageUpdated { .. } => "UsageUpdated@v1",
        EventKind::ContextCompacted { .. } => "ContextCompacted@v1",
        EventKind::AutopilotRunStarted { .. } => "AutopilotRunStarted@v1",
        EventKind::AutopilotRunHeartbeat { .. } => "AutopilotRunHeartbeat@v1",
        EventKind::AutopilotRunStopped { .. } => "AutopilotRunStopped@v1",
        EventKind::PluginCatalogSynced { .. } => "PluginCatalogSynced@v1",
        EventKind::PluginVerified { .. } => "PluginVerified@v1",
        EventKind::HookExecuted { .. } => "HookExecuted@v1",
        EventKind::SessionForked { .. } => "SessionForked@v1",
        EventKind::CheckpointCreated { .. } => "CheckpointCreated@v1",
        EventKind::CheckpointRewound { .. } => "CheckpointRewound@v1",
        EventKind::TranscriptExported { .. } => "TranscriptExported@v1",
        EventKind::McpServerAdded { .. } => "McpServerAdded@v1",
        EventKind::McpServerRemoved { .. } => "McpServerRemoved@v1",
        EventKind::McpToolDiscovered { .. } => "McpToolDiscovered@v1",
        EventKind::SubagentSpawned { .. } => "SubagentSpawned@v1",
        EventKind::SubagentCompleted { .. } => "SubagentCompleted@v1",
        EventKind::SubagentFailed { .. } => "SubagentFailed@v1",
        EventKind::CostUpdated { .. } => "CostUpdated@v1",
        EventKind::EffortChanged { .. } => "EffortChanged@v1",
        EventKind::ProfileCaptured { .. } => "ProfileCaptured@v1",
        EventKind::MemorySynced { .. } => "MemorySynced@v1",
        EventKind::BackgroundJobStarted { .. } => "BackgroundJobStarted@v1",
        EventKind::BackgroundJobResumed { .. } => "BackgroundJobResumed@v1",
        EventKind::BackgroundJobStopped { .. } => "BackgroundJobStopped@v1",
        EventKind::SkillLoaded { .. } => "SkillLoaded@v1",
        EventKind::ReplayExecuted { .. } => "ReplayExecuted@v1",
        EventKind::PromptCacheHit { .. } => "PromptCacheHit@v1",
        EventKind::OffPeakScheduled { .. } => "OffPeakScheduled@v1",
        EventKind::VisualArtifactCaptured { .. } => "VisualArtifactCaptured@v1",
        EventKind::RemoteEnvConfigured { .. } => "RemoteEnvConfigured@v1",
        EventKind::RemoteEnvExecutionStarted { .. } => "RemoteEnvExecutionStarted@v1",
        EventKind::RemoteEnvExecutionCompleted { .. } => "RemoteEnvExecutionCompleted@v1",
        EventKind::TeleportBundleCreated { .. } => "TeleportBundleCreated@v1",
        EventKind::TeleportHandoffLinkCreated { .. } => "TeleportHandoffLinkCreated@v1",
        EventKind::TeleportHandoffLinkConsumed { .. } => "TeleportHandoffLinkConsumed@v1",
        EventKind::TelemetryEvent { .. } => "TelemetryEvent@v1",
        EventKind::PermissionModeChanged { .. } => "PermissionModeChanged@v1",
        EventKind::WebSearchExecuted { .. } => "WebSearchExecuted@v1",
        EventKind::ReviewStarted { .. } => "ReviewStarted@v1",
        EventKind::ReviewCompleted { .. } => "ReviewCompleted@v1",
        EventKind::ReviewPublished { .. } => "ReviewPublished@v1",
        EventKind::TaskCreated { .. } => "TaskCreated@v1",
        EventKind::TaskCompleted { .. } => "TaskCompleted@v1",
        EventKind::ArtifactBundled { .. } => "ArtifactBundled@v1",
        EventKind::SessionStarted { .. } => "SessionStarted@v1",
        EventKind::SessionResumed { .. } => "SessionResumed@v1",
        EventKind::ToolDenied { .. } => "ToolDenied@v1",
        EventKind::PermissionDecisionRecorded { .. } => "PermissionDecisionRecorded@v1",
        EventKind::AgentInterrupted { .. } => "AgentInterrupted@v1",
        EventKind::NotebookEdited { .. } => "NotebookEdited@v1",
        EventKind::PdfTextExtracted { .. } => "PdfTextExtracted@v1",
        EventKind::IdeSessionStarted { .. } => "IdeSessionStarted@v1",
        EventKind::TurnLimitExceeded { .. } => "TurnLimitExceeded@v1",
        EventKind::BudgetExceeded { .. } => "BudgetExceeded@v1",
        EventKind::TaskUpdated { .. } => "TaskUpdated@v1",
        EventKind::TaskDeleted { .. } => "TaskDeleted@v1",
        EventKind::EnterPlanMode { .. } => "EnterPlanMode@v1",
        EventKind::ExitPlanMode { .. } => "ExitPlanMode@v1",
        EventKind::ProviderSelected { .. } => "ProviderSelected@v1",
        EventKind::ProviderCompatibilityApplied { .. } => "ProviderCompatibilityApplied@v1",
        EventKind::IndexBuild { .. } => "IndexBuild@v1",
        EventKind::IndexUpdate { .. } => "IndexUpdate@v1",
        EventKind::IndexQueryEvent { .. } => "IndexQuery@v1",
        EventKind::Retrieval { .. } => "Retrieval@v1",
        EventKind::PrivacyRedaction { .. } => "PrivacyRedaction@v1",
        EventKind::PrivacyBlock { .. } => "PrivacyBlock@v1",
        EventKind::Autocomplete { .. } => "Autocomplete@v1",
        EventKind::CompactionTriggered { .. } => "CompactionTriggered@v1",
        EventKind::CompactionSnapshotLinked { .. } => "CompactionSnapshotLinked@v1",
        EventKind::DoomLoopDetected { .. } => "DoomLoopDetected@v1",
        EventKind::CircuitBreakerTripped { .. } => "CircuitBreakerTripped@v1",
        EventKind::HallucinationNudgeFired { .. } => "HallucinationNudgeFired@v1",
        EventKind::ToolCacheHit { .. } => "ToolCacheHit@v1",
        EventKind::ErrorRecoveryTriggered { .. } => "ErrorRecoveryTriggered@v1",
        EventKind::ModelRoutingChanged { .. } => "ModelRoutingChanged@v1",
        EventKind::MemoryObservations { .. } => "MemoryObservations@v1",
    }
}
