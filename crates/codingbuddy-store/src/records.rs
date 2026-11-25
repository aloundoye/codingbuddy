//! Row types stored in and returned by the session database.

use super::*;

#[derive(Debug, Clone)]
pub struct PluginStateRecord {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub path: String,
    pub enabled: bool,
    pub manifest_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageSummary {
    pub input_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub output_tokens: u64,
    pub records: u64,
}

/// Per-session usage breakdown for `/stats` history display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistoryEntry {
    pub session_id: String,
    pub started_at: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub turn_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageByUnitSummary {
    pub unit: String,
    pub input_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCompactionRecord {
    pub summary_id: Uuid,
    pub session_id: Uuid,
    pub from_turn: u64,
    pub to_turn: u64,
    pub token_delta_estimate: i64,
    pub replay_pointer: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutopilotRunRecord {
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub prompt: String,
    pub status: String,
    pub stop_reason: Option<String>,
    pub completed_iterations: u64,
    pub failed_iterations: u64,
    pub consecutive_failures: u64,
    pub last_error: Option<String>,
    pub stop_file: String,
    pub heartbeat_file: String,
    pub tools: bool,
    pub max_think: bool,
    pub started_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCatalogEntryRecord {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub source: String,
    pub signature: Option<String>,
    pub verified: bool,
    pub metadata_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub checkpoint_id: Uuid,
    pub reason: String,
    pub snapshot_path: String,
    pub files_count: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentApproval {
    pub tool_name: String,
    pub command_pattern: String,
    pub project_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptExportRecord {
    pub export_id: Uuid,
    pub session_id: Uuid,
    pub format: String,
    pub output_path: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerRecord {
    pub server_id: String,
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    pub enabled: bool,
    pub metadata_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCacheRecord {
    pub server_id: String,
    pub tool_name: String,
    pub description: String,
    pub schema_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRunRecord {
    pub run_id: Uuid,
    pub session_id: Option<Uuid>,
    pub task_id: Option<Uuid>,
    pub child_session_id: Option<Uuid>,
    pub background_job_id: Option<Uuid>,
    pub name: String,
    pub goal: String,
    pub status: String,
    pub output: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileRunRecord {
    pub profile_id: Uuid,
    pub session_id: Uuid,
    pub summary: String,
    pub elapsed_ms: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationRunRecord {
    pub command: String,
    pub success: bool,
    pub output: String,
    pub run_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundJobRecord {
    pub job_id: Uuid,
    pub kind: String,
    pub reference: String,
    pub status: String,
    pub metadata_json: String,
    pub started_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayCassetteRecord {
    pub cassette_id: Uuid,
    pub session_id: Uuid,
    pub deterministic: bool,
    pub events_count: u64,
    pub payload_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRegistryRecord {
    pub skill_id: String,
    pub name: String,
    pub path: String,
    pub enabled: bool,
    pub metadata_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderMetricRecord {
    pub provider: String,
    pub model: String,
    pub cache_key: Option<String>,
    pub cache_hit: bool,
    pub latency_ms: u64,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceCatalogRecord {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub metadata_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualArtifactRecord {
    pub artifact_id: Uuid,
    pub path: String,
    pub mime: String,
    pub metadata_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteEnvProfileRecord {
    pub profile_id: Uuid,
    pub name: String,
    pub endpoint: String,
    pub auth_mode: String,
    pub metadata_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskQueueRecord {
    pub task_id: Uuid,
    pub session_id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub priority: u32,
    pub status: String,
    pub outcome: Option<String>,
    pub artifact_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTodoRecord {
    pub todo_id: Uuid,
    pub session_id: Uuid,
    pub content: String,
    pub status: String,
    pub position: u32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchCacheRecord {
    pub query_hash: String,
    pub query: String,
    pub results_json: String,
    pub results_count: u64,
    pub cached_at: String,
    pub ttl_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLockRecord {
    pub session_id: Uuid,
    pub lock_holder: String,
    pub locked_at: String,
    pub heartbeat_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRunRecord {
    pub review_id: Uuid,
    pub session_id: Uuid,
    pub preset: String,
    pub target: String,
    pub findings_json: String,
    pub findings_count: u64,
    pub critical_count: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact_id: Uuid,
    pub task_id: Uuid,
    pub artifact_path: String,
    pub files_json: String,
    pub created_at: String,
}

/// An event to append. Retain the same value when retrying a write.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub event_id: Uuid,
    pub at: chrono::DateTime<Utc>,
    pub session_id: Uuid,
    pub kind: EventKind,
}

impl NewEvent {
    pub fn new(session_id: Uuid, kind: EventKind) -> Self {
        Self {
            event_id: Uuid::now_v7(),
            at: Utc::now(),
            session_id,
            kind,
        }
    }
}

/// Stored wire fields used by event polling and timelines.
pub struct StoredEvent {
    pub seq_no: u64,
    pub at: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// A session as shown in pickers and `session list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session: Session,
    /// The name set with `/rename`, else the first prompt (shortened).
    pub title: String,
    /// True when `title` was set explicitly.
    pub renamed: bool,
    pub updated_at: String,
}
