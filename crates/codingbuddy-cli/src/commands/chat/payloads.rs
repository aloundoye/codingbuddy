use super::*;

pub(super) fn agents_payload(cwd: &Path, limit: usize) -> Result<serde_json::Value> {
    let store = Store::new(cwd)?;
    let session_id = store
        .load_latest_session()?
        .map(|session| session.session_id);
    let runs = store.list_subagent_runs(session_id, limit)?;
    Ok(json!({
        "schema": "deepseek.chat.agents.v1",
        "session_id": session_id.map(|id| id.to_string()),
        "count": runs.len(),
        "agents": runs,
    }))
}

pub(super) fn render_agents_payload(payload: &serde_json::Value) -> String {
    let runs = payload
        .get("agents")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if runs.is_empty() {
        return "No subagent runs recorded in this session.".to_string();
    }
    let parsed_runs = runs
        .into_iter()
        .filter_map(|row| serde_json::from_value::<SubagentRunRecord>(row).ok())
        .collect::<Vec<_>>();
    if parsed_runs.is_empty() {
        return "No subagent runs recorded in this session.".to_string();
    }
    let mut lines = vec![format!("Subagents ({} total):", parsed_runs.len())];
    for run in parsed_runs {
        let detail = run
            .output
            .as_deref()
            .or(run.error.as_deref())
            .unwrap_or_default()
            .replace('\n', " ");
        let detail = truncate_inline(&detail, 120);
        lines.push(format!(
            "- {} [{}] {} — {}",
            run.name, run.status, run.run_id, detail
        ));
    }
    lines.join("\n")
}

pub(super) fn render_todos_payload(payload: &serde_json::Value) -> String {
    let count = payload
        .get("count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let session_id = payload
        .get("session_id")
        .and_then(|value| value.as_str())
        .unwrap_or("pending");
    let workflow_phase = payload
        .get("workflow_phase")
        .and_then(|value| value.as_str())
        .unwrap_or("idle");
    let plan_state = payload
        .get("plan_state")
        .and_then(|value| value.as_str())
        .unwrap_or("none");
    let summary = payload.get("summary").cloned().unwrap_or_else(|| json!({}));
    let mut lines = vec![
        format!(
            "Session todos: {count} item(s) — session={session_id} phase={workflow_phase} plan={plan_state}"
        ),
        format!(
            "Active={} In progress={} Completed={}",
            summary["active"].as_u64().unwrap_or(0),
            summary["in_progress"].as_u64().unwrap_or(0),
            summary["completed"].as_u64().unwrap_or(0),
        ),
    ];
    if let Some(current) = summary
        .get("current")
        .and_then(|value| value.as_object())
        .map(|obj| {
            format!(
                "{} [{}]",
                obj.get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
                obj.get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending")
            )
        })
    {
        lines.push(format!("Current todo: {current}"));
    }
    if let Some(step) = payload
        .get("current_step")
        .and_then(|value| value.as_object())
    {
        lines.push(format!(
            "Current plan step: {}",
            step.get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("none")
        ));
    }
    if let Some(rows) = payload.get("items").and_then(|value| value.as_array()) {
        for row in rows.iter().take(30) {
            lines.push(format!(
                "- [{}] {} ({})",
                row["status"].as_str().unwrap_or("pending"),
                row["content"].as_str().unwrap_or_default(),
                row["todo_id"].as_str().unwrap_or_default()
            ));
        }
    }
    lines.push("Use /comment-todos to scan TODO/FIXME comments in source files.".to_string());
    lines.join("\n")
}

pub(super) fn render_comment_todos_payload(payload: &serde_json::Value) -> String {
    let count = payload
        .get("count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let mut lines = vec![
        format!("Workspace comment scan: {count} result(s)"),
        "This is source-comment scanning only. Use /todos for session-native agent checklist tracking."
            .to_string(),
    ];
    if let Some(rows) = payload.get("items").and_then(|value| value.as_array()) {
        for row in rows.iter().take(20) {
            lines.push(format!(
                "- {}:{} {}",
                row["path"].as_str().unwrap_or_default(),
                row["line"].as_u64().unwrap_or(0),
                row["text"].as_str().unwrap_or_default()
            ));
        }
    }
    lines.join("\n")
}

pub(super) fn session_focus_payload(cwd: &Path, session_id: Uuid) -> Result<serde_json::Value> {
    let store = Store::new(cwd)?;
    let session = store
        .load_session(session_id)?
        .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
    let projection = store.rebuild_from_events(session.session_id)?;
    Ok(json!({
        "schema": "deepseek.chat.resume.v1",
        "session_id": session.session_id.to_string(),
        "state": format!("{:?}", session.status),
        "turns": projection.transcript.len(),
        "steps": projection.step_status.len(),
        "message": format!(
            "switched active chat session to {} ({} turns, state={:?})",
            session.session_id,
            projection.transcript.len(),
            session.status
        ),
    }))
}

pub(super) type SessionLifecycleNotice = chat_lifecycle::SessionLifecycleNotice;

pub(super) fn poll_session_lifecycle_notices(
    cwd: &Path,
    session_override: Option<Uuid>,
    watermarks: &mut HashMap<Uuid, u64>,
) -> Result<Vec<SessionLifecycleNotice>> {
    chat_lifecycle::poll_session_lifecycle_notices(cwd, session_override, watermarks)
}

pub(super) fn pr_comments_payload(
    cwd: &Path,
    pr_number: &str,
    output_path: Option<&str>,
) -> Result<serde_json::Value> {
    let gh_available = std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !gh_available {
        return Err(anyhow!(
            "GitHub CLI ('gh') is required for /pr_comments. Install gh and authenticate first."
        ));
    }

    let output = std::process::Command::new("gh")
        .current_dir(cwd)
        .args([
            "pr",
            "view",
            pr_number,
            "--json",
            "number,title,url,author,comments",
        ])
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to fetch PR comments: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let comments_count = parsed["comments"]
        .as_array()
        .map(|rows| rows.len())
        .unwrap_or(0);

    let mut saved_to = None;
    if let Some(path) = output_path {
        let destination = resolve_additional_dir(cwd, path);
        if let Some(parent) = destination.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&destination, serde_json::to_vec_pretty(&parsed)?)?;
        saved_to = Some(destination.to_string_lossy().to_string());
    }

    Ok(json!({
        "schema": "deepseek.pr_comments.v1",
        "ok": true,
        "pr": pr_number,
        "summary": format!("Fetched {} comment(s) for PR #{}", comments_count, pr_number),
        "saved_to": saved_to,
        "data": parsed,
    }))
}

pub(super) fn release_notes_payload(
    cwd: &Path,
    range: &str,
    output_path: Option<&str>,
) -> Result<serde_json::Value> {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(["log", "--no-merges", "--pretty=format:%h %s", range])
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to generate release notes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    let mut saved_to = None;
    if let Some(path) = output_path {
        let destination = resolve_additional_dir(cwd, path);
        if let Some(parent) = destination.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut rendered = format!("# Release Notes ({range})\n\n");
        for line in &lines {
            rendered.push_str("- ");
            rendered.push_str(line);
            rendered.push('\n');
        }
        std::fs::write(&destination, rendered)?;
        saved_to = Some(destination.to_string_lossy().to_string());
    }

    Ok(json!({
        "schema": "deepseek.release_notes.v1",
        "ok": true,
        "range": range,
        "count": lines.len(),
        "lines": lines,
        "saved_to": saved_to,
    }))
}

pub(super) fn login_payload(cwd: &Path) -> Result<serde_json::Value> {
    let cfg = AppConfig::ensure(cwd)?;
    let env_key = if cfg.llm.api_key_env.trim().is_empty() {
        "DEEPSEEK_API_KEY".to_string()
    } else {
        cfg.llm.api_key_env.clone()
    };
    let token = std::env::var(&env_key).unwrap_or_default();
    if token.trim().is_empty() {
        return Err(anyhow!(
            "missing {}. export the key first, then run /login",
            env_key
        ));
    }

    let runtime_auth = runtime_dir(cwd).join("auth").join("session.json");
    if let Some(parent) = runtime_auth.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mask = format!(
        "***{}",
        token
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );
    let session_payload = json!({
        "provider": "deepseek",
        "api_key_env": env_key,
        "masked": mask,
        "created_at": Utc::now().to_rfc3339(),
    });
    std::fs::write(&runtime_auth, serde_json::to_vec_pretty(&session_payload)?)?;

    let local_path = AppConfig::project_local_settings_path(cwd);
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut root = if local_path.exists() {
        let raw = std::fs::read_to_string(&local_path)?;
        serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }
    let map = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.local.json root must be an object"))?;
    let llm_value = map.entry("llm".to_string()).or_insert_with(|| json!({}));
    if !llm_value.is_object() {
        *llm_value = json!({});
    }
    if let Some(llm) = llm_value.as_object_mut() {
        llm.insert("api_key".to_string(), json!(token));
        llm.insert("api_key_env".to_string(), json!(env_key));
    }
    std::fs::write(&local_path, serde_json::to_vec_pretty(&root)?)?;

    Ok(json!({
        "schema": "deepseek.auth.v1",
        "logged_in": true,
        "session_path": runtime_auth.to_string_lossy().to_string(),
        "settings_path": local_path.to_string_lossy().to_string(),
        "message": "Login successful. Workspace auth session and settings.local.json updated.",
    }))
}

pub(super) fn logout_payload(cwd: &Path) -> Result<serde_json::Value> {
    let _cfg = AppConfig::ensure(cwd)?;
    let runtime_auth = runtime_dir(cwd).join("auth").join("session.json");
    let session_removed = if runtime_auth.exists() {
        std::fs::remove_file(&runtime_auth)?;
        true
    } else {
        false
    };

    let local_path = AppConfig::project_local_settings_path(cwd);
    let mut settings_updated = false;
    if local_path.exists() {
        let raw = std::fs::read_to_string(&local_path)?;
        let mut root =
            serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_else(|_| json!({}));
        if let Some(llm) = root.get_mut("llm").and_then(|entry| entry.as_object_mut())
            && llm.remove("api_key").is_some()
        {
            settings_updated = true;
        }
        std::fs::write(&local_path, serde_json::to_vec_pretty(&root)?)?;
    }

    // NOTE: We no longer mutate the process environment (unsafe data race).
    // The ApiClient caches the key at construction, so clearing the env var
    // would not affect the running session anyway. The settings file update
    // above ensures the key is gone on next launch.

    Ok(json!({
        "schema": "deepseek.auth.v1",
        "logged_in": false,
        "session_removed": session_removed,
        "settings_updated": settings_updated,
        "message": "Logged out. Restart the session to complete logout.",
    }))
}

pub(super) fn desktop_payload(cwd: &Path, _args: &[String]) -> Result<serde_json::Value> {
    let session_id = Store::new(cwd)?
        .load_latest_session()?
        .map(|session| session.session_id.to_string());
    Ok(json!({
        "schema": "deepseek.desktop_handoff.v2",
        "session_id": session_id,
        "resume_command": session_id.map(|id| format!("deepseek --resume {id}")),
    }))
}

/// Generate 2-3 context-aware follow-up prompt suggestions from the assistant response.
pub(super) fn generate_prompt_suggestions(response: &str) -> Vec<String> {
    let lower = response.to_ascii_lowercase();
    let mut suggestions = Vec::new();

    // Detect edits → suggest test/review/commit
    if lower.contains("applied") || lower.contains("modified") || lower.contains("created") {
        suggestions.push("run tests".to_string());
        suggestions.push("/diff".to_string());
        if lower.contains("created") {
            suggestions.push("document this change".to_string());
        }
    }

    // Detect errors → suggest debug/fix
    if lower.contains("error") || lower.contains("failed") || lower.contains("panic") {
        suggestions.push("fix the error".to_string());
        suggestions.push("show the full stack trace".to_string());
    }

    // Detect test results → suggest coverage
    if lower.contains("test") && (lower.contains("passed") || lower.contains("ok")) {
        suggestions.push("check test coverage".to_string());
    }

    // Detect refactoring → suggest verification
    if lower.contains("refactor") || lower.contains("renamed") || lower.contains("moved") {
        suggestions.push("verify no regressions".to_string());
    }

    // Detect explanations → suggest deeper dives
    if lower.contains("because") || lower.contains("reason") || lower.contains("architecture") {
        suggestions.push("explain in more detail".to_string());
    }

    // Always cap at 3 suggestions
    suggestions.truncate(3);

    // Fallback if nothing triggered
    if suggestions.is_empty() {
        suggestions.push("/compact".to_string());
        suggestions.push("/cost".to_string());
    }

    suggestions
}
pub(super) fn todo_summary_payload(items: &[SessionTodoRecord]) -> serde_json::Value {
    let completed = items
        .iter()
        .filter(|item| item.status.eq_ignore_ascii_case("completed"))
        .count();
    let in_progress = items
        .iter()
        .filter(|item| item.status.eq_ignore_ascii_case("in_progress"))
        .count();
    let active = items.len().saturating_sub(completed);
    let current = items
        .iter()
        .find(|item| item.status.eq_ignore_ascii_case("in_progress"))
        .or_else(|| {
            items
                .iter()
                .find(|item| item.status.eq_ignore_ascii_case("pending"))
        })
        .map(|item| {
            json!({
                "todo_id": item.todo_id.to_string(),
                "content": item.content,
                "status": item.status,
                "position": item.position,
            })
        });
    json!({
        "total": items.len(),
        "active": active,
        "completed": completed,
        "in_progress": in_progress,
        "current": current,
    })
}

pub(super) fn current_plan_step_payload(
    plan_payload: &serde_json::Value,
) -> Option<serde_json::Value> {
    let steps = plan_payload.get("steps")?.as_array()?;
    for (index, step) in steps.iter().enumerate() {
        if !step
            .get("done")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            return Some(json!({
                "index": index + 1,
                "step_id": step.get("step_id").and_then(|value| value.as_str()),
                "title": step.get("title").and_then(|value| value.as_str()).unwrap_or_default(),
                "intent": step.get("intent").and_then(|value| value.as_str()).unwrap_or_default(),
            }));
        }
    }
    None
}

pub(super) fn todos_payload(
    cwd: &Path,
    session_override: Option<Uuid>,
    args: &[String],
) -> Result<serde_json::Value> {
    let mut max_results = 200usize;
    let mut query_parts = Vec::new();
    for arg in args {
        if query_parts.is_empty()
            && let Ok(parsed) = arg.parse::<usize>()
        {
            max_results = parsed.clamp(1, 2000);
            continue;
        }
        query_parts.push(arg.clone());
    }
    let query = (!query_parts.is_empty()).then(|| query_parts.join(" "));
    let query_lower = query.as_deref().map(|value| value.to_ascii_lowercase());

    let store = Store::new(cwd)?;
    let session = if let Some(session_id) = session_override {
        Some(
            store
                .load_session(session_id)?
                .ok_or_else(|| anyhow!("session not found: {session_id}"))?,
        )
    } else {
        store.load_latest_session()?
    };
    let Some(session) = session else {
        return Ok(json!({
            "schema": "deepseek.session_todos.v1",
            "session_id": serde_json::Value::Null,
            "workflow_phase": "idle",
            "plan_state": "none",
            "current_step": serde_json::Value::Null,
            "query": query,
            "count": 0,
            "summary": {
                "total": 0,
                "active": 0,
                "completed": 0,
                "in_progress": 0,
                "current": serde_json::Value::Null,
            },
            "items": [],
        }));
    };

    let all_items = store.list_session_todos(session.session_id)?;
    let mut filtered = Vec::new();
    for item in all_items.iter() {
        if let Some(filter) = query_lower.as_deref()
            && !item.content.to_ascii_lowercase().contains(filter)
        {
            continue;
        }
        filtered.push(item.clone());
        if filtered.len() >= max_results {
            break;
        }
    }
    let summary = todo_summary_payload(&all_items);
    let active_plan = current_plan_payload(&store, Some(&session))?;
    let current_step = active_plan
        .as_ref()
        .and_then(current_plan_step_payload)
        .unwrap_or(serde_json::Value::Null);

    Ok(json!({
        "schema": "deepseek.session_todos.v1",
        "session_id": session.session_id.to_string(),
        "workflow_phase": workflow_phase_label(&session.status),
        "plan_state": plan_state_label(Some(&session)),
        "current_step": current_step,
        "query": query,
        "count": filtered.len(),
        "summary": summary,
        "items": filtered,
    }))
}

pub(super) fn comment_todos_payload(cwd: &Path, args: &[String]) -> Result<serde_json::Value> {
    let mut max_results = 100usize;
    let mut query = None;
    if let Some(first) = args.first() {
        if let Ok(parsed) = first.parse::<usize>() {
            max_results = parsed.clamp(1, 2000);
            query = args.get(1).cloned();
        } else {
            query = Some(first.clone());
        }
    }
    let query_lower = query.as_deref().map(|value| value.to_ascii_lowercase());

    let output = std::process::Command::new("rg")
        .current_dir(cwd)
        .args([
            "--line-number",
            "--no-heading",
            "--hidden",
            "--glob",
            "!.git/*",
            "--glob",
            "!target/*",
            "--glob",
            "!node_modules/*",
            "TODO|FIXME",
            ".",
        ])
        .output();

    let mut items = Vec::new();
    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            let mut parts = line.splitn(3, ':');
            let path = parts.next().unwrap_or_default();
            let line_no = parts
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            let text = parts.next().unwrap_or_default().trim().to_string();
            if let Some(filter) = query_lower.as_deref()
                && !text.to_ascii_lowercase().contains(filter)
            {
                continue;
            }
            items.push(json!({
                "path": path,
                "line": line_no,
                "text": text,
            }));
            if items.len() >= max_results {
                break;
            }
        }
    }

    Ok(json!({
        "schema": "deepseek.comment_todos.v1",
        "count": items.len(),
        "query": query,
        "items": items,
    }))
}

// Chrome payload functions removed — browser automation deleted in crate consolidation.

pub(super) fn parse_debug_analysis_args(_args: &[String]) -> Result<Option<DoctorArgs>> {
    Ok(None)
}
