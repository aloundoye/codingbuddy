//! Timeline summaries, handoff descriptor files and JSON-RPC parameter helpers.

use super::*;

pub(crate) fn timeline_category(kind: &str) -> &'static str {
    if kind.contains("Patch") || kind.contains("Snapshot") {
        "patch"
    } else if kind.contains("Tool") {
        "tool"
    } else if kind.contains("Background") || kind.contains("Subagent") || kind.contains("Run") {
        "background"
    } else if kind.contains("Task") || kind.contains("Step") || kind.contains("Plan") {
        "plan"
    } else if kind.contains("Model") || kind.contains("Provider") {
        "model"
    } else if kind.contains("Session") || kind.contains("Phase") {
        "session"
    } else if kind.contains("Error") || kind.contains("Warning") {
        "diagnostic"
    } else {
        "message"
    }
}

pub(crate) fn timeline_summary(kind: &str, payload: &Value) -> String {
    let kind = kind.split('@').next().unwrap_or(kind);
    let body = payload.get("payload").unwrap_or(payload);
    match kind {
        "ChatTurn" => body
            .get("message")
            .map(chat_turn_summary)
            .unwrap_or_else(|| "chat turn".to_string()),
        "ToolProposed" => body
            .pointer("/proposal/call/name")
            .and_then(Value::as_str)
            .map(|name| format!("tool proposed: {name}"))
            .unwrap_or_else(|| "tool proposed".to_string()),
        "ToolApproved" => body
            .get("invocation_id")
            .and_then(Value::as_str)
            .map(|id| format!("tool approved: {id}"))
            .unwrap_or_else(|| "tool approved".to_string()),
        "ToolResult" => body
            .pointer("/result/name")
            .and_then(Value::as_str)
            .map(|name| format!("tool result: {name}"))
            .unwrap_or_else(|| "tool result".to_string()),
        "PatchStaged" => body
            .get("patch_id")
            .and_then(Value::as_str)
            .map(|id| format!("patch staged: {id}"))
            .unwrap_or_else(|| "patch staged".to_string()),
        "PatchApplied" => {
            let applied = body
                .get("applied")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            body.get("patch_id")
                .and_then(Value::as_str)
                .map(|id| format!("patch applied={applied}: {id}"))
                .unwrap_or_else(|| format!("patch applied={applied}"))
        }
        "ModelChanged" => body
            .get("model")
            .and_then(Value::as_str)
            .map(|model| format!("model changed: {model}"))
            .unwrap_or_else(|| "model changed".to_string()),
        "ProviderSelected" => {
            let provider = body.get("provider").and_then(Value::as_str).unwrap_or("");
            let model = body.get("model").and_then(Value::as_str).unwrap_or("");
            format!("model selected: {provider}/{model}")
        }
        "PhaseTransition" => {
            let from = body.get("from").and_then(Value::as_str).unwrap_or("");
            let to = body.get("to").and_then(Value::as_str).unwrap_or("");
            format!("phase: {from} -> {to}")
        }
        "SessionStateChanged" => {
            let from = body.get("from").and_then(Value::as_str).unwrap_or("");
            let to = body.get("to").and_then(Value::as_str).unwrap_or("");
            format!("session state: {from} -> {to}")
        }
        _ => kind.to_string(),
    }
}

pub(crate) fn chat_turn_summary(message: &Value) -> String {
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        return format!("message: {}", truncate_timeline(content));
    }
    if message.get("tool_calls").is_some() {
        return "assistant tool call turn".to_string();
    }
    "chat turn".to_string()
}

pub(crate) fn truncate_timeline(value: &str) -> String {
    const MAX_CHARS: usize = 120;
    if value.chars().count() <= MAX_CHARS {
        return value.replace('\n', " ");
    }
    let mut out: String = value.chars().take(MAX_CHARS - 1).collect();
    out.push('…');
    out.replace('\n', " ")
}

pub(crate) fn handoff_dir(store_root: &Path) -> PathBuf {
    store_root.join("teleport").join("handoffs")
}

pub(crate) fn handoff_descriptor_path(store_root: &Path, handoff_id: Uuid) -> PathBuf {
    handoff_dir(store_root).join(format!("{handoff_id}.json"))
}

pub(crate) fn write_handoff_descriptor(
    store_root: &Path,
    descriptor: &HandoffDescriptor,
) -> Result<()> {
    let path = handoff_descriptor_path(store_root, descriptor.handoff_id);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(descriptor)?)?;
    Ok(())
}

pub(crate) fn read_handoff_descriptor(
    store_root: &Path,
    handoff_id: Uuid,
) -> Result<HandoffDescriptor> {
    let path = handoff_descriptor_path(store_root, handoff_id);
    let raw = fs::read_to_string(&path)?;
    let descriptor = serde_json::from_str::<HandoffDescriptor>(&raw)?;
    Ok(descriptor)
}

pub(crate) fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Extract a UUID from a JSON params object.
pub(crate) fn public_chat_history(messages: Vec<ChatMessage>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            ChatMessage::User { content } => {
                Some(serde_json::json!({"role":"user","content":content}))
            }
            ChatMessage::Assistant {
                content: Some(content),
                ..
            } => Some(serde_json::json!({"role":"assistant","content":content})),
            _ => None,
        })
        .collect()
}

pub(crate) fn require_uuid(params: &Value, field: &str) -> Result<Uuid> {
    let s = params
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing '{field}' parameter"))?;
    Uuid::parse_str(s).map_err(|e| anyhow!("invalid UUID for '{field}': {e}"))
}

pub(crate) fn optional_uuid(params: &Value, field: &str) -> Result<Option<Uuid>> {
    let Some(raw) = params.get(field).and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    Ok(Some(
        Uuid::parse_str(raw).map_err(|e| anyhow!("invalid UUID for '{field}': {e}"))?,
    ))
}

pub(crate) fn env_flag_enabled(key: &str) -> bool {
    match std::env::var(key) {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            !normalized.is_empty() && !matches!(normalized.as_str(), "0" | "false" | "off" | "no")
        }
        Err(_) => false,
    }
}

pub(crate) fn parse_chat_mode(value: &str) -> ChatMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "ask" => ChatMode::Ask,
        "context" => ChatMode::Context,
        _ => ChatMode::Code,
    }
}
