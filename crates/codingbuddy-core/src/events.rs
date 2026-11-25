//! Durable session events

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub seq_no: u64,
    pub at: DateTime<Utc>,
    pub session_id: Uuid,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Deserialize)]
struct RawEventEnvelope {
    seq_no: u64,
    at: DateTime<Utc>,
    session_id: Uuid,
    kind: serde_json::Value,
}

fn parse_event_kind_compat_value(kind_value: serde_json::Value) -> Result<EventKind> {
    if let Ok(kind) = serde_json::from_value::<EventKind>(kind_value.clone()) {
        return Ok(kind);
    }
    let kind_obj = kind_value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid event kind shape"))?;
    let kind_type = kind_obj
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let payload = kind_obj
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    // Handle well-known removed/renamed event types explicitly.
    match kind_type {
        "RouterDecisionV1" => {
            return Ok(EventKind::TelemetryEvent {
                name: "legacy.router_decision".to_string(),
                properties: payload,
            });
        }
        "RouterEscalationV1" => {
            return Ok(EventKind::TelemetryEvent {
                name: "legacy.router_escalation".to_string(),
                properties: payload,
            });
        }
        _ => {}
    }

    // For known event types whose schema evolved (e.g. added fields with
    // defaults), inject default values and retry deserialization.
    if let serde_json::Value::Object(mut payload_map) = payload.clone() {
        // UsageUpdatedV1: added cache_hit_tokens / cache_miss_tokens
        if kind_type == "UsageUpdatedV1" {
            payload_map
                .entry("cache_hit_tokens")
                .or_insert(serde_json::Value::Number(0.into()));
            payload_map
                .entry("cache_miss_tokens")
                .or_insert(serde_json::Value::Number(0.into()));
        }

        let patched = serde_json::json!({
            "type": kind_type,
            "payload": payload_map,
        });
        if let Ok(kind) = serde_json::from_value::<EventKind>(patched) {
            return Ok(kind);
        }
    }

    // Last resort: wrap in a telemetry event so sessions with unknown future
    // event types can still load rather than crashing.
    Ok(EventKind::TelemetryEvent {
        name: format!("unknown.{}", kind_type),
        properties: payload,
    })
}

/// Parse an `EventKind` payload with read compatibility for removed router events.
pub fn parse_event_kind_compat(raw: &str) -> Result<EventKind> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    parse_event_kind_compat_value(value)
}

/// Parse an `EventEnvelope` line with read compatibility for removed router events.
pub fn parse_event_envelope_compat(raw: &str) -> Result<EventEnvelope> {
    let envelope: RawEventEnvelope = serde_json::from_str(raw)?;
    Ok(EventEnvelope {
        seq_no: envelope.seq_no,
        at: envelope.at,
        session_id: envelope.session_id,
        kind: parse_event_kind_compat_value(envelope.kind)?,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum EventKind {
    #[serde(alias = "TurnAddedV1")]
    TurnAdded { role: String, content: String },
    /// Structured chat turn with full tool_call data for accurate session resume.
    #[serde(alias = "ChatTurnV1")]
    ChatTurn { message: ChatMessage },
    ModelStep {
        step_id: Uuid,
        parts: Vec<ModelPart>,
    },
    ModelToolResult {
        invocation_id: Uuid,
        message: ChatMessage,
    },
    /// Reverts the session to a prior state by dropping N turns.
    #[serde(alias = "TurnRevertedV1")]
    TurnReverted { turns_dropped: u32 },
    #[serde(alias = "SessionStateChangedV1")]
    SessionStateChanged {
        from: SessionState,
        to: SessionState,
    },
    #[serde(alias = "PlanCreatedV1")]
    PlanCreated { plan: Plan },
    #[serde(alias = "PlanRevisedV1")]
    PlanRevised { plan: Plan },
    #[serde(alias = "RunStartedV1")]
    RunStarted { run_id: Uuid, prompt: String },
    #[serde(alias = "RunStateChangedV1")]
    RunStateChanged {
        run_id: Uuid,
        from: RunState,
        to: RunState,
    },
    #[serde(alias = "RunCompletedV1")]
    RunCompleted { run_id: Uuid, success: bool },
    #[serde(alias = "StepMarkedV1")]
    StepMarked {
        step_id: Uuid,
        done: bool,
        note: String,
    },
    #[serde(alias = "ToolProposedV1")]
    ToolProposed { proposal: ToolProposal },
    #[serde(alias = "ToolApprovedV1")]
    ToolApproved { invocation_id: Uuid },
    #[serde(alias = "ToolResultV1")]
    ToolResult { result: ToolResult },
    #[serde(alias = "AgentStepStartedV1")]
    AgentStepStarted {
        step_id: Uuid,
        run_id: Option<Uuid>,
        provider: String,
        model: String,
        phase: Option<String>,
    },
    #[serde(alias = "AgentStepEndedV1")]
    AgentStepEnded {
        step_id: Uuid,
        success: bool,
        finish_reason: String,
        duration_ms: u64,
    },
    #[serde(alias = "ToolInputStartedV1")]
    ToolInputStarted {
        invocation_id: Uuid,
        tool_name: String,
    },
    #[serde(alias = "ToolInputEndedV1")]
    ToolInputEnded {
        invocation_id: Uuid,
        tool_name: String,
        input_bytes: u64,
        validation_status: String,
    },
    #[serde(alias = "ToolExecutionStartedV1")]
    ToolExecutionStarted {
        invocation_id: Uuid,
        tool_name: String,
    },
    #[serde(alias = "ToolExecutionEndedV1")]
    ToolExecutionEnded {
        invocation_id: Uuid,
        tool_name: String,
        success: bool,
        error: Option<String>,
        output_bytes: u64,
        truncated: bool,
        duration_ms: u64,
    },
    #[serde(alias = "ToolRetryScheduledV1")]
    ToolRetryScheduled {
        invocation_id: Uuid,
        tool_name: String,
        attempt: u64,
        reason: String,
    },
    #[serde(alias = "PatchStagedV1")]
    PatchStaged { patch_id: Uuid, base_sha256: String },
    #[serde(alias = "PatchAppliedV1")]
    PatchApplied {
        patch_id: Uuid,
        applied: bool,
        conflicts: Vec<String>,
    },
    #[serde(alias = "VerificationRunV1")]
    VerificationRun {
        command: String,
        success: bool,
        output: String,
    },
    #[serde(alias = "CommitProposalV1")]
    CommitProposal {
        files: Vec<String>,
        touched_files: u64,
        loc_delta: u64,
        verify_commands: Vec<String>,
        verify_status: String,
        suggested_message: String,
    },
    #[serde(alias = "PluginInstalledV1")]
    PluginInstalled { plugin_id: String, version: String },
    #[serde(alias = "PluginRemovedV1")]
    PluginRemoved { plugin_id: String },
    #[serde(alias = "PluginEnabledV1")]
    PluginEnabled { plugin_id: String },
    #[serde(alias = "PluginDisabledV1")]
    PluginDisabled { plugin_id: String },
    #[serde(alias = "UsageUpdatedV1")]
    UsageUpdated {
        unit: LlmUnit,
        model: String,
        input_tokens: u64,
        cache_hit_tokens: u64,
        cache_miss_tokens: u64,
        output_tokens: u64,
    },
    #[serde(alias = "ContextCompactedV1")]
    ContextCompacted {
        summary_id: Uuid,
        from_turn: u64,
        to_turn: u64,
        token_delta_estimate: i64,
        replay_pointer: String,
    },
    #[serde(alias = "AutopilotRunStartedV1")]
    AutopilotRunStarted { run_id: Uuid, prompt: String },
    #[serde(alias = "AutopilotRunHeartbeatV1")]
    AutopilotRunHeartbeat {
        run_id: Uuid,
        completed_iterations: u64,
        failed_iterations: u64,
        consecutive_failures: u64,
        last_error: Option<String>,
    },
    #[serde(alias = "AutopilotRunStoppedV1")]
    AutopilotRunStopped {
        run_id: Uuid,
        stop_reason: String,
        completed_iterations: u64,
        failed_iterations: u64,
    },
    #[serde(alias = "PluginCatalogSyncedV1")]
    PluginCatalogSynced {
        source: String,
        total: usize,
        verified_count: usize,
    },
    #[serde(alias = "PluginVerifiedV1")]
    PluginVerified {
        plugin_id: String,
        verified: bool,
        reason: String,
    },
    #[serde(alias = "CheckpointCreatedV1")]
    CheckpointCreated {
        checkpoint_id: Uuid,
        reason: String,
        files_count: u64,
        snapshot_path: String,
    },
    #[serde(alias = "CheckpointRewoundV1")]
    CheckpointRewound { checkpoint_id: Uuid, reason: String },
    #[serde(alias = "TranscriptExportedV1")]
    TranscriptExported {
        export_id: Uuid,
        format: String,
        output_path: String,
    },
    #[serde(alias = "McpServerAddedV1")]
    McpServerAdded {
        server_id: String,
        transport: String,
        endpoint: String,
    },
    #[serde(alias = "McpServerRemovedV1")]
    McpServerRemoved { server_id: String },
    #[serde(alias = "McpToolDiscoveredV1")]
    McpToolDiscovered {
        server_id: String,
        tool_name: String,
    },
    #[serde(alias = "SubagentSpawnedV1")]
    SubagentSpawned {
        run_id: Uuid,
        name: String,
        goal: String,
    },
    #[serde(alias = "SubagentCompletedV1")]
    SubagentCompleted { run_id: Uuid, output: String },
    #[serde(alias = "SubagentFailedV1")]
    SubagentFailed { run_id: Uuid, error: String },
    #[serde(alias = "CostUpdatedV1")]
    CostUpdated {
        input_tokens: u64,
        output_tokens: u64,
        estimated_cost_usd: f64,
    },
    #[serde(alias = "EffortChangedV1")]
    EffortChanged { level: String },
    #[serde(alias = "ProfileCapturedV1")]
    ProfileCaptured {
        profile_id: Uuid,
        summary: String,
        elapsed_ms: u64,
    },
    #[serde(alias = "MemorySyncedV1")]
    MemorySynced {
        version_id: Uuid,
        path: String,
        note: String,
    },
    #[serde(alias = "HookExecutedV1")]
    HookExecuted {
        phase: String,
        hook_path: String,
        success: bool,
        timed_out: bool,
        exit_code: Option<i32>,
    },
    #[serde(alias = "SessionForkedV1")]
    SessionForked {
        from_session_id: Uuid,
        to_session_id: Uuid,
    },
    #[serde(alias = "PermissionModeChangedV1")]
    PermissionModeChanged { from: String, to: String },
    #[serde(alias = "WebSearchExecutedV1")]
    WebSearchExecuted {
        query: String,
        results_count: u64,
        cached: bool,
    },
    #[serde(alias = "ReviewStartedV1")]
    ReviewStarted {
        review_id: Uuid,
        preset: String,
        target: String,
    },
    #[serde(alias = "ReviewCompletedV1")]
    ReviewCompleted {
        review_id: Uuid,
        findings_count: u64,
        critical_count: u64,
    },
    #[serde(alias = "ReviewPublishedV1")]
    ReviewPublished {
        review_id: Uuid,
        pr_number: u64,
        comments_published: u64,
        dry_run: bool,
    },
    #[serde(alias = "TaskCreatedV1")]
    TaskCreated {
        task_id: Uuid,
        title: String,
        priority: u32,
    },
    #[serde(alias = "TaskCompletedV1")]
    TaskCompleted { task_id: Uuid, outcome: String },
    #[serde(alias = "ArtifactBundledV1")]
    ArtifactBundled {
        task_id: Uuid,
        artifact_path: String,
        files: Vec<String>,
    },
    #[serde(alias = "TelemetryEventV1")]
    TelemetryEvent {
        name: String,
        properties: serde_json::Value,
    },
    #[serde(alias = "BackgroundJobStartedV1")]
    BackgroundJobStarted {
        job_id: Uuid,
        kind: String,
        reference: String,
    },
    #[serde(alias = "BackgroundJobResumedV1")]
    BackgroundJobResumed { job_id: Uuid, reference: String },
    #[serde(alias = "BackgroundJobStoppedV1")]
    BackgroundJobStopped { job_id: Uuid, reason: String },
    #[serde(alias = "SkillLoadedV1")]
    SkillLoaded {
        skill_id: String,
        source_path: String,
    },
    #[serde(alias = "ReplayExecutedV1")]
    ReplayExecuted {
        session_id: Uuid,
        deterministic: bool,
        events_replayed: u64,
    },
    #[serde(alias = "PromptCacheHitV1")]
    PromptCacheHit { cache_key: String, model: String },
    #[serde(alias = "OffPeakScheduledV1")]
    OffPeakScheduled {
        reason: String,
        resume_after: String,
    },
    #[serde(alias = "VisualArtifactCapturedV1")]
    VisualArtifactCaptured {
        artifact_id: Uuid,
        path: String,
        mime: String,
    },
    #[serde(alias = "RemoteEnvConfiguredV1")]
    RemoteEnvConfigured {
        profile_id: Uuid,
        name: String,
        endpoint: String,
    },
    #[serde(alias = "RemoteEnvExecutionStartedV1")]
    RemoteEnvExecutionStarted {
        execution_id: Uuid,
        profile_id: Uuid,
        mode: String,
        background: bool,
        reference: String,
    },
    #[serde(alias = "RemoteEnvExecutionCompletedV1")]
    RemoteEnvExecutionCompleted {
        execution_id: Uuid,
        profile_id: Uuid,
        mode: String,
        success: bool,
        duration_ms: u64,
        background: bool,
        reference: String,
    },
    #[serde(alias = "TeleportBundleCreatedV1")]
    TeleportBundleCreated { bundle_id: Uuid, path: String },
    #[serde(alias = "TeleportHandoffLinkCreatedV1")]
    TeleportHandoffLinkCreated {
        handoff_id: Uuid,
        session_id: Uuid,
        expires_at: String,
    },
    #[serde(alias = "TeleportHandoffLinkConsumedV1")]
    TeleportHandoffLinkConsumed {
        handoff_id: Uuid,
        session_id: Uuid,
        success: bool,
        reason: String,
    },
    #[serde(alias = "SessionStartedV1")]
    SessionStarted { session_id: Uuid, workspace: String },
    #[serde(alias = "SessionResumedV1")]
    SessionResumed {
        session_id: Uuid,
        events_replayed: u64,
    },
    #[serde(alias = "ToolDeniedV1")]
    ToolDenied {
        invocation_id: Uuid,
        tool_name: String,
        reason: String,
    },
    #[serde(alias = "PermissionDecisionRecordedV1")]
    PermissionDecisionRecorded {
        invocation_id: Uuid,
        tool_name: String,
        decision: String,
        reason: String,
        remembered_approval_source: Option<String>,
        managed_setting_lock_source: Option<String>,
        high_risk_category: Option<String>,
    },
    #[serde(alias = "AgentInterruptedV1")]
    AgentInterrupted {
        reason: String,
        partial_output_chars: u64,
        last_consistent_event_id: Option<Uuid>,
    },
    #[serde(alias = "NotebookEditedV1")]
    NotebookEdited {
        path: String,
        operation: String,
        cell_index: u64,
        cell_type: Option<String>,
    },
    #[serde(alias = "PdfTextExtractedV1")]
    PdfTextExtracted {
        path: String,
        pages: String,
        text_length: u64,
    },
    #[serde(alias = "IdeSessionStartedV1")]
    IdeSessionStarted {
        transport: String,
        client_info: String,
    },
    #[serde(alias = "TurnLimitExceededV1")]
    TurnLimitExceeded { limit: u64, actual: u64 },
    #[serde(alias = "BudgetExceededV1")]
    BudgetExceeded { limit_usd: f64, actual_usd: f64 },
    #[serde(alias = "TaskUpdatedV1")]
    TaskUpdated { task_id: String, status: String },
    #[serde(alias = "TaskDeletedV1")]
    TaskDeleted { task_id: String },
    #[serde(alias = "EnterPlanModeV1")]
    EnterPlanMode { session_id: Uuid },
    #[serde(alias = "ExitPlanModeV1")]
    ExitPlanMode { session_id: Uuid },
    #[serde(alias = "ProviderSelectedV1")]
    ProviderSelected {
        provider: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
    },
    #[serde(alias = "ProviderCompatibilityAppliedV1")]
    ProviderCompatibilityApplied {
        model: String,
        provider: String,
        transforms: Vec<String>,
        degraded_inputs: Vec<String>,
    },
    /// Code index was built from scratch.
    #[serde(alias = "IndexBuildV1")]
    IndexBuild {
        chunks_indexed: u64,
        files_processed: u64,
        duration_ms: u64,
    },
    /// Code index was incrementally updated.
    #[serde(alias = "IndexUpdateV1")]
    IndexUpdate {
        chunks_added: u64,
        chunks_removed: u64,
        files_changed: u64,
        duration_ms: u64,
    },
    /// A retrieval query was executed against the code index.
    #[serde(alias = "IndexQueryV1")]
    IndexQueryEvent {
        query: String,
        results_count: u64,
        duration_ms: u64,
    },
    /// Legacy event retained to replay sessions with automatic retrieval.
    #[serde(alias = "RetrievalV1")]
    Retrieval {
        query: String,
        chunks_injected: u64,
        token_estimate: u64,
    },
    /// Sensitive content was redacted by the privacy router.
    #[serde(alias = "PrivacyRedactionV1")]
    PrivacyRedaction {
        path: String,
        patterns_matched: u64,
        policy: String,
    },
    /// Content was blocked from cloud by the privacy router.
    #[serde(alias = "PrivacyBlockV1")]
    PrivacyBlock { path: String, reason: String },
    /// Legacy event retained to replay sessions with model-generated autocomplete.
    #[serde(alias = "AutocompleteV1")]
    Autocomplete {
        model_id: String,
        tokens_generated: u64,
        latency_ms: u64,
        accepted: bool,
    },

    // ── Tool loop intelligence events ──
    /// Context compaction was triggered in the tool loop.
    #[serde(alias = "CompactionTriggeredV1")]
    CompactionTriggered {
        phase: String,
        pre_tokens: u64,
        post_tokens: u64,
        messages_before: u64,
        messages_after: u64,
    },
    #[serde(alias = "CompactionSnapshotLinkedV1")]
    CompactionSnapshotLinked {
        summary_id: Uuid,
        snapshot_id: String,
        active_files: Vec<String>,
        directives_preserved: u64,
    },
    /// Doom loop detected — model repeated identical tool calls.
    #[serde(alias = "DoomLoopDetectedV1")]
    DoomLoopDetected {
        tool_name: String,
        repeat_count: u64,
    },
    /// Circuit breaker tripped — tool disabled after consecutive failures.
    #[serde(alias = "CircuitBreakerTrippedV1")]
    CircuitBreakerTripped {
        tool_name: String,
        consecutive_failures: u64,
        cooldown_turns: u64,
    },
    /// Anti-hallucination nudge fired — model responded without using tools.
    #[serde(alias = "HallucinationNudgeFiredV1")]
    HallucinationNudgeFired { nudge_count: u64, trigger: String },
    /// Tool result cache hit — returned cached result instead of re-executing.
    #[serde(alias = "ToolCacheHitV1")]
    ToolCacheHit { tool_name: String },
    /// Error recovery guidance injected after tool failures.
    #[serde(alias = "ErrorRecoveryTriggeredV1")]
    ErrorRecoveryTriggered {
        level: String,
        repeated_error_count: u64,
    },
    /// Model routing changed (e.g. escalation to reasoner or de-escalation).
    #[serde(alias = "ModelRoutingChangedV1")]
    ModelRoutingChanged {
        from_model: String,
        to_model: String,
        reason: String,
    },
    /// Auto-extracted memory observations from compacted conversation.
    #[serde(alias = "MemoryObservationsV1")]
    MemoryObservations { observations: Vec<String> },
}

impl EventKind {
    /// Logical category for this event kind.
    #[must_use]
    pub fn category(&self) -> &'static str {
        match self {
            // Session lifecycle
            Self::SessionStarted { .. }
            | Self::SessionResumed { .. }
            | Self::SessionStateChanged { .. }
            | Self::SessionForked { .. } => "session",

            // Run lifecycle
            Self::RunStarted { .. } | Self::RunStateChanged { .. } | Self::RunCompleted { .. } => {
                "run"
            }

            // Chat / transcript
            Self::TurnAdded { .. }
            | Self::ChatTurn { .. }
            | Self::ModelStep { .. }
            | Self::ModelToolResult { .. }
            | Self::TurnReverted { .. }
            | Self::ContextCompacted { .. }
            | Self::AgentInterrupted { .. }
            | Self::EffortChanged { .. }
            | Self::PermissionModeChanged { .. }
            | Self::TurnLimitExceeded { .. }
            | Self::BudgetExceeded { .. } => "chat",

            // Tool invocations
            Self::ToolProposed { .. }
            | Self::ToolApproved { .. }
            | Self::ToolResult { .. }
            | Self::ToolDenied { .. }
            | Self::PermissionDecisionRecorded { .. }
            | Self::ToolInputStarted { .. }
            | Self::ToolInputEnded { .. }
            | Self::ToolExecutionStarted { .. }
            | Self::ToolExecutionEnded { .. }
            | Self::ToolRetryScheduled { .. } => "tool",

            // Plans
            Self::PlanCreated { .. }
            | Self::PlanRevised { .. }
            | Self::StepMarked { .. }
            | Self::EnterPlanMode { .. }
            | Self::ExitPlanMode { .. } => "plan",

            // Tasks
            Self::TaskCreated { .. }
            | Self::TaskCompleted { .. }
            | Self::TaskUpdated { .. }
            | Self::TaskDeleted { .. } => "task",

            // Model/provider selection
            Self::ProviderSelected { .. } | Self::ProviderCompatibilityApplied { .. } => "model",

            // Patches (diff/apply)
            Self::PatchStaged { .. } | Self::PatchApplied { .. } => "patch",

            // Plugins
            Self::PluginInstalled { .. }
            | Self::PluginRemoved { .. }
            | Self::PluginEnabled { .. }
            | Self::PluginDisabled { .. }
            | Self::PluginCatalogSynced { .. }
            | Self::PluginVerified { .. } => "plugin",

            // Usage / cost tracking
            Self::UsageUpdated { .. } | Self::CostUpdated { .. } | Self::PromptCacheHit { .. } => {
                "usage"
            }

            // Autopilot
            Self::AutopilotRunStarted { .. }
            | Self::AutopilotRunHeartbeat { .. }
            | Self::AutopilotRunStopped { .. } => "autopilot",

            // Subagent orchestration
            Self::SubagentSpawned { .. }
            | Self::SubagentCompleted { .. }
            | Self::SubagentFailed { .. } => "subagent",

            // Background jobs
            Self::BackgroundJobStarted { .. }
            | Self::BackgroundJobResumed { .. }
            | Self::BackgroundJobStopped { .. } => "background",

            // MCP
            Self::McpServerAdded { .. }
            | Self::McpServerRemoved { .. }
            | Self::McpToolDiscovered { .. } => "mcp",

            // Skills
            Self::SkillLoaded { .. } => "skill",

            // Hooks
            Self::HookExecuted { .. } => "hook",

            // Memory
            Self::MemorySynced { .. } => "memory",

            // Profile / benchmark
            Self::ProfileCaptured { .. } => "profile",

            // Verification
            Self::VerificationRun { .. } => "verification",

            // Git workflow guidance
            Self::CommitProposal { .. } => "git",

            // Checkpoint / rewind
            Self::CheckpointCreated { .. } | Self::CheckpointRewound { .. } => "checkpoint",

            // Export
            Self::TranscriptExported { .. } => "export",

            // Web search
            Self::WebSearchExecuted { .. } => "web",

            // Review
            Self::ReviewStarted { .. }
            | Self::ReviewCompleted { .. }
            | Self::ReviewPublished { .. } => "review",

            // Artifact bundling
            Self::ArtifactBundled { .. } => "artifact",

            // Telemetry
            Self::TelemetryEvent { .. } => "telemetry",

            // Replay
            Self::ReplayExecuted { .. } => "replay",

            // Scheduling
            Self::OffPeakScheduled { .. } => "scheduling",

            // Visual testing
            Self::VisualArtifactCaptured { .. } => "visual",

            // Remote environments
            Self::RemoteEnvConfigured { .. }
            | Self::RemoteEnvExecutionStarted { .. }
            | Self::RemoteEnvExecutionCompleted { .. } => "remote_env",

            // Teleport
            Self::TeleportBundleCreated { .. }
            | Self::TeleportHandoffLinkCreated { .. }
            | Self::TeleportHandoffLinkConsumed { .. } => "teleport",

            // Notebook
            Self::NotebookEdited { .. } => "notebook",

            // PDF
            Self::PdfTextExtracted { .. } => "pdf",

            // IDE
            Self::IdeSessionStarted { .. } => "ide",

            // Code index and legacy retrieval
            Self::IndexBuild { .. }
            | Self::IndexUpdate { .. }
            | Self::IndexQueryEvent { .. }
            | Self::Retrieval { .. } => "index",

            Self::PrivacyRedaction { .. } | Self::PrivacyBlock { .. } => "privacy",

            Self::Autocomplete { .. } => "autocomplete",

            // Tool loop intelligence
            Self::CompactionTriggered { .. }
            | Self::CompactionSnapshotLinked { .. }
            | Self::AgentStepStarted { .. }
            | Self::AgentStepEnded { .. }
            | Self::DoomLoopDetected { .. }
            | Self::CircuitBreakerTripped { .. }
            | Self::HallucinationNudgeFired { .. }
            | Self::ToolCacheHit { .. }
            | Self::ErrorRecoveryTriggered { .. }
            | Self::ModelRoutingChanged { .. } => "tool_loop",

            Self::MemoryObservations { .. } => "memory",
        }
    }

    /// Whether this event is a tool-related event (proposal, approval, result, denial).
    #[must_use]
    pub fn is_tool_event(&self) -> bool {
        self.category() == "tool"
    }

    /// Whether this event is a session lifecycle event.
    #[must_use]
    pub fn is_session_event(&self) -> bool {
        self.category() == "session"
    }
}
