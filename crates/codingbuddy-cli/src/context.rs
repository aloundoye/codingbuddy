use anyhow::{Result, anyhow};
use codingbuddy_agent::{AgentEngine, ChatMode, ChatOptions};
use codingbuddy_core::{
    AppConfig, EventEnvelope, EventKind, Session, SessionBudgets, SessionState,
    normalize_codingbuddy_model, normalize_codingbuddy_profile, runtime_dir,
};
use codingbuddy_store::Store;
use serde_json::json;
use std::fs;
use std::path::Path;
use uuid::Uuid;

use crate::Cli;

/// Apply CLI-level engine overrides (permission mode, verbose, budget limits).
pub(crate) fn apply_cli_flags(engine: &mut AgentEngine, cli: &Cli) {
    if cli.dangerously_skip_permissions && cli.allow_dangerously_skip_permissions {
        engine.set_permission_mode("bypassPermissions");
    } else if let Some(ref mode) = cli.permission_mode {
        engine.set_permission_mode(mode);
    }
    if cli.verbose {
        engine.set_verbose(true);
    }
    engine.set_max_turns(cli.max_turns);
    engine.set_max_budget_usd(cli.max_budget_usd);
    if cli.auto_lint {
        engine.enable_lint();
    }
    for entry in &cli.lint_cmd {
        if let Some((lang, cmd)) = entry.split_once(':') {
            engine.set_lint_command(lang.trim(), cmd.trim());
        }
    }
}

/// Subagent orchestration connects the parallel Worker execution to isolated engine scopes.
pub(crate) fn wire_subagent_worker(engine: &AgentEngine, cwd: &Path) {
    let workspace = cwd.to_path_buf();
    let worker = std::sync::Arc::new(
        move |task: &codingbuddy_subagent::SubagentTask| -> anyhow::Result<String> {
            let mut engine = codingbuddy_agent::AgentEngine::new(&workspace)?;
            let (mode, force_max_think, role_prompt) = match &task.role {
                codingbuddy_subagent::SubagentRole::Explore => (
                    codingbuddy_agent::ChatMode::Ask,
                    false,
                    "You are an exploration subagent. Read, search, and summarize. Do not edit files.",
                ),
                codingbuddy_subagent::SubagentRole::Plan => (
                    codingbuddy_agent::ChatMode::Ask,
                    true,
                    "You are a planning subagent. Explore the codebase, identify risks, and produce a concrete implementation plan. Do not edit files.",
                ),
                codingbuddy_subagent::SubagentRole::Bash => (
                    codingbuddy_agent::ChatMode::Code,
                    false,
                    "You are a bash-focused subagent. Prefer commands and verification steps, keep file edits minimal, and report command outcomes precisely.",
                ),
                codingbuddy_subagent::SubagentRole::Task => (
                    codingbuddy_agent::ChatMode::Code,
                    false,
                    "You are an execution subagent. Use the available tools to complete the delegated task and report what changed and what remains.",
                ),
                codingbuddy_subagent::SubagentRole::Custom(_) => (
                    codingbuddy_agent::ChatMode::Code,
                    false,
                    "You are a custom subagent. Follow the delegated objective and use tools deliberately.",
                ),
            };
            let mut options = codingbuddy_agent::ChatOptions {
                tools: true,
                force_max_think,
                mode,
                session_id: task.child_session_id,
                ..Default::default()
            };

            if let Some(max_turns) = task.max_turns {
                engine.set_max_turns(Some(max_turns as u64));
            }
            if let Some(model) = &task.model_override {
                engine.cfg_mut().llm.base_model = model.clone();
            }

            let system_prompt = match task.name.as_str() {
                "debugger" => {
                    "You are the Debugger subagent. Triage failing tests or build output, identify suspect files, and recommend the smallest credible fix."
                }
                "refactor-sheriff" => {
                    "You are the Refactor Sheriff subagent. Identify behavior-preserving refactors, call out risks, and propose the cleanest change sequence."
                }
                "security-sentinel" => {
                    "You are the Security Sentinel subagent. Review the requested goal for vulnerabilities, risky commands, and unsafe assumptions."
                }
                _ => role_prompt,
            };

            options.system_prompt_append = Some(format!(
                "{system_prompt}\n\nParent Session: {}\nChild Session: {}\nDelegated Goal:\n{}\n\nReturn a concise, structured result with findings, actions taken, and any remaining risks or follow-ups.",
                task.parent_session_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                task.child_session_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                task.goal
            ));
            engine.chat_with_options(&task.goal, options)
        },
    );
    engine.set_subagent_worker(worker);
}

/// Build ChatOptions from CLI flags.
pub(crate) fn chat_options_from_cli(cli: &Cli, tools: bool, mode: ChatMode) -> ChatOptions {
    // --system-prompt-file overrides --system-prompt
    let sys_override = if let Some(ref path) = cli.system_prompt_file {
        fs::read_to_string(path).ok()
    } else {
        cli.system_prompt.clone()
    };
    // --append-system-prompt-file overrides --append-system-prompt
    let sys_append = if let Some(ref path) = cli.append_system_prompt_file {
        fs::read_to_string(path).ok()
    } else {
        cli.append_system_prompt.clone()
    };
    let force_max_think = cli
        .model
        .as_deref()
        .is_some_and(super::commands::chat::is_max_think_selection);
    ChatOptions {
        tools,
        force_max_think,
        system_prompt_override: sys_override,
        system_prompt_append: sys_append,
        additional_dirs: cli.add_dir.clone(),
        repo_root_override: cli.repo.clone(),
        debug_context: cli.debug_context
            || std::env::var("CODINGBUDDY_DEBUG_CONTEXT")
                .map(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "1" | "true" | "yes" | "on"
                    )
                })
                .unwrap_or(false),
        mode,
        teammate_mode: cli.teammate_mode.clone(),
        disable_team_orchestration: false,
        detect_urls: cli.detect_urls,
        watch_files: cli.watch_files,
        images: vec![],
        chat_history: vec![],
        session_id: None,
    }
}

pub(crate) fn ensure_llm_ready(cwd: &Path, json_mode: bool) -> Result<()> {
    let cfg = AppConfig::ensure(cwd)?;
    ensure_llm_ready_with_cfg(Some(cwd), &cfg, json_mode)
}

pub(crate) fn ensure_llm_ready_with_cfg(
    cwd: Option<&Path>,
    cfg: &AppConfig,
    json_mode: bool,
) -> Result<()> {
    use std::io::IsTerminal;

    let provider = cfg.llm.provider.trim().to_ascii_lowercase();
    if provider != "deepseek" {
        return Err(anyhow!(
            "unsupported llm.provider='{}' (supported: deepseek)",
            cfg.llm.provider
        ));
    }
    let _profile = normalize_codingbuddy_profile(&cfg.llm.profile).ok_or_else(|| {
        anyhow!(
            "unsupported llm.profile='{}' (supported: v3_2)",
            cfg.llm.profile
        )
    })?;
    if normalize_codingbuddy_model(&cfg.llm.base_model).is_none() {
        return Err(anyhow!(
            "unsupported llm.base_model='{}' (supported aliases: deepseek-chat, deepseek-reasoner)",
            cfg.llm.base_model
        ));
    }
    if normalize_codingbuddy_model(&cfg.llm.max_think_model).is_none() {
        return Err(anyhow!(
            "unsupported llm.max_think_model='{}' (supported aliases: deepseek-chat, deepseek-reasoner)",
            cfg.llm.max_think_model
        ));
    }
    let env_key = cfg.llm.api_key_env.trim();
    if env_key.is_empty() {
        return Err(anyhow!(
            "llm.api_key_env is empty; set it in .codingbuddy/settings.json"
        ));
    }

    if std::env::var(env_key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok(());
    }

    if let Some(configured_key) = cfg
        .llm
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        // SAFETY: We set process-local environment for this CLI process before worker threads start.
        unsafe {
            std::env::set_var(env_key, configured_key);
        }
        return Ok(());
    }

    let interactive_tty = std::io::stderr().is_terminal();
    if json_mode || !interactive_tty {
        return Err(anyhow!("{} is required. Set it and retry.", env_key));
    }

    eprintln!(
        "API key is required to use provider '{}'.",
        cfg.llm.provider
    );
    let prompt = format!("Enter {}: ", env_key);
    let key = rpassword::prompt_password(prompt)?;
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("received empty API key"));
    }
    // SAFETY: We set process-local environment for this CLI process before worker threads start.
    unsafe {
        std::env::set_var(env_key, trimmed);
    }
    if let Some(cwd) = cwd {
        maybe_persist_api_key(cwd, env_key, trimmed)?;
    }
    Ok(())
}

fn maybe_persist_api_key(cwd: &Path, env_key: &str, api_key: &str) -> Result<()> {
    use std::io::{IsTerminal, Write};

    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Ok(());
    }
    eprint!(
        "Save API key to {} for this workspace? [Y/n]: ",
        AppConfig::project_local_settings_path(cwd).display()
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let normalized = answer.trim().to_ascii_lowercase();
    if matches!(normalized.as_str(), "n" | "no") {
        return Ok(());
    }

    let local_path = AppConfig::project_local_settings_path(cwd);
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut root = if local_path.exists() {
        let raw = fs::read_to_string(&local_path)?;
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
        llm.insert("api_key".to_string(), json!(api_key));
        llm.insert("api_key_env".to_string(), json!(env_key));
    }
    fs::write(&local_path, serde_json::to_vec_pretty(&root)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&local_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&local_path, perms)?;
    }
    eprintln!("saved API key in {}", local_path.display());
    Ok(())
}

pub(crate) fn read_session_events(cwd: &Path, session_id: Uuid) -> Result<Vec<EventEnvelope>> {
    let path = runtime_dir(cwd).join("events.jsonl");
    let Ok(raw) = fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in raw.lines() {
        let Ok(event) = codingbuddy_core::parse_event_envelope_compat(line) else {
            continue;
        };
        if event.session_id == session_id {
            out.push(event);
        }
    }
    Ok(out)
}

pub(crate) fn append_control_event(cwd: &Path, kind: EventKind) -> Result<()> {
    let store = Store::new(cwd)?;
    let session = ensure_session_record(cwd, &store)?;
    let event = EventEnvelope {
        seq_no: store.next_seq_no(session.session_id)?,
        at: chrono::Utc::now(),
        session_id: session.session_id,
        kind,
    };
    store.append_event(&event)?;
    Ok(())
}

pub(crate) fn ensure_session_record(cwd: &Path, store: &Store) -> Result<Session> {
    if let Some(existing) = store.load_latest_session()? {
        return Ok(existing);
    }
    let cfg = AppConfig::load(cwd).unwrap_or_default();
    let session = Session {
        session_id: Uuid::now_v7(),
        workspace_root: cwd.to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: cfg.budgets.max_turn_duration_secs,
            max_think_tokens: cfg.budgets.max_reasoner_tokens_per_session as u32,
        },
        active_plan_id: None,
    };
    store.save_session(&session)?;
    Ok(session)
}
