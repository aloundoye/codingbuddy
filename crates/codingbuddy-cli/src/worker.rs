//! Own-binary durable handoff. Credentials travel only over the private stdin pipe.
use anyhow::{Context, Result, ensure};
use codingbuddy_core::ToolHostBlockingExt;
use codingbuddy_core::{AppConfig, CancellationToken, process_env};
use codingbuddy_store::{BackgroundJobRecord, Store};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use uuid::Uuid;
#[derive(Serialize, Deserialize)]
pub(crate) enum WorkerJob {
    Agent {
        prompt: String,
        tools: bool,
        session: Uuid,
        task: Option<Box<codingbuddy_subagent::SubagentTask>>,
        max_turns: Option<u64>,
    },
    Shell {
        command: String,
    },
}
#[derive(Serialize, Deserialize)]
struct Bootstrap {
    version: u32,
    id: Uuid,
    workspace: PathBuf,
    session: Uuid,
    cfg: AppConfig,
    trusted: bool,
    hook_sources: std::collections::BTreeMap<String, process_env::Source>,
    policy_denials: std::collections::BTreeMap<String, process_env::Source>,
    deadline_unix_ms: Option<i64>,
    job: WorkerJob,
}
pub(crate) fn spawn(
    workspace: &Path,
    session: Uuid,
    job: WorkerJob,
    mut cfg: AppConfig,
    parent: Option<Uuid>,
) -> Result<serde_json::Value> {
    let workspace = workspace.canonicalize()?;
    let store = Store::new(&workspace)?;
    let id = Uuid::now_v7();
    cfg.workspace_trust = Some(
        cfg.workspace_trust
            .unwrap_or_else(|| codingbuddy_core::workspace_is_trusted(&workspace)),
    );
    cfg.llm.api_key = codingbuddy_llm::resolve_key_from_config(&cfg.llm);
    let private = codingbuddy_core::workspace_fs::WorkspaceFs::open(&workspace)?
        .subdir(Path::new(".codingbuddy/background/owned"), true)?;
    let root = private.root().to_owned();
    let stdout = root.join(format!("{id}.stdout.log"));
    let stderr = root.join(format!("{id}.stderr.log"));
    let deadline_unix_ms = match &job {
        WorkerJob::Agent {
            task: Some(task), ..
        } => task.execution.deadline.map(|deadline| {
            chrono::Utc::now().timestamp_millis()
                + deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_millis() as i64
        }),
        _ => None,
    };
    let payload = json!({"version":1,"workspace":workspace,"stdout_log":stdout,"stderr_log":stderr,"created_at":chrono::Utc::now().to_rfc3339(),"deadline_unix_ms":deadline_unix_ms});
    store.begin_operation(id, Some(session), parent, "worker", &payload)?;
    store.transition_operation(id, "running", "prepared", &payload)?;
    let record = BackgroundJobRecord {
        job_id: id,
        kind: "owned-worker".into(),
        reference: format!("worker:{id}"),
        status: "prepared".into(),
        metadata_json: payload.to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    };
    store.upsert_background_job(&record)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--internal-owned-worker")
        .current_dir(&workspace)
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create(&stdout)?)
        .stderr(std::fs::File::create(&stderr)?);
    process_env::apply_for(
        &mut command,
        &workspace,
        "codingbuddy_worker",
        process_env::Source::User,
    );
    let handoff = Bootstrap {
        version: 1,
        id,
        workspace: workspace.clone(),
        session,
        trusted: cfg.workspace_trust == Some(true),
        hook_sources: cfg.hook_sources.clone(),
        policy_denials: cfg.policy.configured_denials.clone(),
        deadline_unix_ms,
        cfg,
        job,
    };
    let bytes = serde_json::to_vec(&handoff)?;
    if bytes.len() > 4 * 1024 * 1024 {
        store.transition_operation(id, "prepared", "handoff_failed", &payload)?;
        anyhow::bail!("Worker bootstrap too large");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            store.transition_operation(id, "prepared", "handoff_failed", &payload)?;
            return Err(error.into());
        }
    };
    if let Err(error) = child
        .stdin
        .take()
        .context("Worker bootstrap pipe unavailable")?
        .write_all(&bytes)
    {
        let _ = child.kill();
        let _ = child.wait();
        store.transition_operation(id, "prepared", "failed", &payload)?;
        return Err(error.into());
    }
    // No parent-owned child/job handle: the durable worker owns all project children.
    // Waiting/reaping the supervisor must not cancel the accepted durable handoff.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(
        json!({"job_id":id,"kind":"owned-worker","status":"prepared","reference":format!("worker:{id}"),"stdout_log":stdout,"stderr_log":stderr}),
    )
}
pub(crate) fn run() -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "Worker bootstrap too large");
    let mut bootstrap: Bootstrap = serde_json::from_slice(&bytes)?;
    bootstrap.cfg.workspace_trust = Some(bootstrap.trusted);
    bootstrap.cfg.hook_sources = bootstrap.hook_sources.clone();
    bootstrap.cfg.policy.configured_denials = bootstrap.policy_denials.clone();
    if let Some(managed) = codingbuddy_policy::load_managed_settings() {
        managed.apply_to_config(&mut bootstrap.cfg);
    }
    drop(bytes);
    ensure!(bootstrap.version == 1, "Unsupported worker bootstrap");
    let store = Store::new(&bootstrap.workspace)?;
    let _ownership = store.session_lease(bootstrap.id)?;
    let operation = store.operation(bootstrap.id)?;
    ensure!(
        operation.kind == "worker"
            && operation.session_id == Some(bootstrap.session)
            && operation.payload["workspace"] == json!(bootstrap.workspace.canonicalize()?),
        "Worker ownership mismatch"
    );
    let delegated = match &bootstrap.job {
        WorkerJob::Agent {
            session,
            task: Some(task),
            ..
        } => Some((
            *session,
            task.run_id,
            task.task_id.context("Missing task identity")?,
        )),
        _ => None,
    };
    if operation.cancel_requested {
        store.transition_operation(bootstrap.id, "prepared", "cancelled", &operation.payload)?;
        if let Some(mut job) = store.load_background_job(bootstrap.id)? {
            job.status = "cancelled".into();
            store.upsert_background_job(&job)?;
        }
        if let Some((session, run, task)) = delegated {
            crate::context::finalize_background_subagent_run_with_ids(
                &bootstrap.workspace,
                bootstrap.session,
                session,
                run,
                task,
                Err("Cancelled before dispatch"),
            )?;
        }
        return Ok(());
    }
    store.transition_operation(bootstrap.id, "prepared", "running", &operation.payload)?;
    let token = CancellationToken::new();
    let _monitor = store.monitor_operation(bootstrap.id, token.clone())?;
    let deadline = bootstrap.deadline_unix_ms.map(|deadline| {
        std::time::Instant::now()
            + std::time::Duration::from_millis(
                (deadline - chrono::Utc::now().timestamp_millis()).max(0) as u64,
            )
    });
    let _deadline = codingbuddy_core::execution::ExecutionContext {
        cancellation: token.clone(),
        deadline,
        ..Default::default()
    }
    .watch_deadline()?;
    let outcome = (|| -> Result<String> {
        match bootstrap.job {
            WorkerJob::Shell { command } => {
                let policy =
                    codingbuddy_policy::PolicyEngine::from_app_config(&bootstrap.cfg.policy);
                let host = codingbuddy_tools::LocalToolHost::with_config(
                    &bootstrap.workspace,
                    policy,
                    std::sync::Arc::new(codingbuddy_tools::PtyShellRunner),
                    &bootstrap.cfg,
                )?;
                let result = host.verify_command_blocking(
                    &command,
                    &bootstrap.workspace,
                    120,
                    codingbuddy_core::ToolExecutionContext {
                        session_id: Some(bootstrap.session),
                        parent_run_id: Some(bootstrap.id),
                        cancellation: Some(&token),
                        deadline,
                        ..Default::default()
                    },
                );
                ensure!(
                    result["success"] == true,
                    "Background command did not pass: {}",
                    result["status"]
                );
                Ok(result.to_string())
            }
            WorkerJob::Agent {
                prompt,
                tools,
                session,
                task,
                max_turns,
            } => {
                let wt = codingbuddy_subagent::WorktreeIsolation::create_with_trust(
                    &bootstrap.workspace,
                    "background",
                    &[],
                    bootstrap.cfg.workspace_trust == Some(true),
                )?;
                let mut cfg = bootstrap.cfg.clone();
                if let Some(model) = task.as_ref().and_then(|task| task.model_override.as_ref()) {
                    cfg.llm.base_model = model.clone();
                    cfg.llm.max_think_model = model.clone();
                    if let Some(provider) = cfg.llm.providers.get_mut(&cfg.llm.provider) {
                        provider.models.chat = model.clone();
                        provider.models.reasoner = Some(model.clone());
                    }
                }
                let mut engine = codingbuddy_agent::AgentEngine::new_isolated(
                    wt.path(),
                    &bootstrap.workspace,
                    cfg,
                )?;
                let ceiling = bootstrap
                    .cfg
                    .budgets
                    .max_turns
                    .unwrap_or(bootstrap.cfg.agent_loop.tool_loop_max_turns);
                engine.set_max_turns(Some(max_turns.unwrap_or(ceiling).min(ceiling)));
                let result = codingbuddy_core::runtime::block_on(engine.chat_with_options(
                    &prompt,
                    codingbuddy_agent::ChatOptions {
                        tools,
                        session_id: Some(session),
                        cancellation: Some(token.clone()),
                        deadline,
                        parent_run_id: Some(bootstrap.id),
                        mode: if task.as_ref().is_some_and(|task| {
                            task.read_only_fallback
                                || matches!(
                                    task.role,
                                    codingbuddy_subagent::SubagentRole::Explore
                                        | codingbuddy_subagent::SubagentRole::Plan
                                )
                        }) {
                            codingbuddy_agent::ChatMode::Ask
                        } else {
                            codingbuddy_agent::ChatMode::Code
                        },
                        ..Default::default()
                    },
                ));
                let diff = wt.collect_for_parent()?;
                let patch = if diff.is_empty() {
                    None
                } else {
                    Some(
                        codingbuddy_diff::PatchStore::new(&bootstrap.workspace)?
                            .stage(&diff, &[])?
                            .patch_id,
                    )
                };
                result.map(|text| {
                    format!(
                        "{text}\nWorktree retained: {}. Staged patch: {}",
                        wt.path().display(),
                        patch
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "none".into())
                    )
                })
            }
        }
    })();
    if token.is_cancelled() {
        store.cancel_operation(bootstrap.id)?;
    }
    let status = if token.is_cancelled() {
        "cancelled"
    } else if outcome.is_ok() {
        "completed"
    } else {
        "failed"
    };
    let payload = json!({"workspace":bootstrap.workspace,"stdout_log":operation.payload["stdout_log"],"stderr_log":operation.payload["stderr_log"],"result":outcome.as_ref().ok(),"error":outcome.as_ref().err().map(|e|e.to_string())});
    store.transition_operation(bootstrap.id, "running", status, &payload)?;
    if let Some(mut job) = store.load_background_job(bootstrap.id)? {
        job.status = status.into();
        job.metadata_json = payload.to_string();
        job.updated_at = chrono::Utc::now().to_rfc3339();
        store.upsert_background_job(&job)?;
    }
    if let Some((session, run, task)) = delegated {
        crate::context::finalize_background_subagent_run_with_ids(
            &bootstrap.workspace,
            bootstrap.session,
            session,
            run,
            task,
            outcome
                .as_deref()
                .map_err(|error| error.to_string())
                .as_ref()
                .copied()
                .map_err(String::as_str),
        )?;
    }
    if let Ok(output) = &outcome {
        println!("{output}");
    }
    outcome.map(|_| ())
}
