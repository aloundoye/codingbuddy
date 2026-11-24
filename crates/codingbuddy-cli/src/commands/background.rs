use anyhow::{Result, anyhow};
use chrono::Utc;
use codingbuddy_core::{AppConfig, EventKind};
use codingbuddy_store::Store;
use serde_json::json;
use std::fs::{self};
use std::path::Path;
use uuid::Uuid;

use crate::context::*;
use crate::output::*;
use crate::{
    BackgroundAttachArgs, BackgroundCmd, BackgroundRunAgentArgs, BackgroundRunShellArgs,
    BackgroundStopArgs,
};

pub(crate) fn parse_background_cmd(args: Vec<String>) -> Result<BackgroundCmd> {
    if args.is_empty() || args[0].eq_ignore_ascii_case("list") {
        return Ok(BackgroundCmd::List);
    }
    let first = args[0].to_ascii_lowercase();
    match first.as_str() {
        "attach" => {
            let job_id = args
                .get(1)
                .cloned()
                .ok_or_else(|| anyhow!("usage: /background attach <job_id>"))?;
            Ok(BackgroundCmd::Attach(BackgroundAttachArgs {
                job_id,
                tail_lines: 40,
            }))
        }
        "stop" => {
            let job_id = args
                .get(1)
                .cloned()
                .ok_or_else(|| anyhow!("usage: /background stop <job_id>"))?;
            Ok(BackgroundCmd::Stop(BackgroundStopArgs { job_id }))
        }
        "run-agent" | "agent" => {
            let prompt = args[1..].to_vec();
            if prompt.is_empty() {
                return Err(anyhow!("usage: /background run-agent <prompt>"));
            }
            Ok(BackgroundCmd::RunAgent(BackgroundRunAgentArgs {
                prompt,
                tools: true,
            }))
        }
        "run-shell" | "shell" => {
            let command = args[1..].to_vec();
            if command.is_empty() {
                return Err(anyhow!("usage: /background run-shell <command>"));
            }
            Ok(BackgroundCmd::RunShell(BackgroundRunShellArgs { command }))
        }
        _ => Err(anyhow!(
            "use /background list|attach|stop|run-agent|run-shell"
        )),
    }
}

pub(crate) fn run_background(cwd: &Path, cmd: BackgroundCmd, json_mode: bool) -> Result<()> {
    let payload = background_payload(cwd, cmd)?;
    if json_mode {
        print_json(&payload)?;
        return Ok(());
    }

    if let Some(rows) = payload.as_array() {
        if rows.is_empty() {
            println!("no background jobs");
            return Ok(());
        }
        for row in rows {
            println!(
                "{} {} {} {}",
                row["job_id"].as_str().unwrap_or_default(),
                row["kind"].as_str().unwrap_or_default(),
                row["status"].as_str().unwrap_or_default(),
                row["reference"].as_str().unwrap_or_default(),
            );
        }
        return Ok(());
    }

    if payload
        .get("stopped")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        println!(
            "stopped background job {}",
            payload["job_id"].as_str().unwrap_or_default(),
        );
        return Ok(());
    }

    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

pub(crate) fn background_payload(cwd: &Path, cmd: BackgroundCmd) -> Result<serde_json::Value> {
    let store = Store::new(cwd)?;
    match cmd {
        BackgroundCmd::List => {
            let jobs = store
                .list_background_jobs()?
                .into_iter()
                .map(|job| {
                    store
                        .reconcile_background_job(job.job_id)
                        .map(|updated| updated.unwrap_or(job))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(serde_json::to_value(jobs)?)
        }
        BackgroundCmd::Attach(args) => {
            let job_id = Uuid::parse_str(&args.job_id)?;
            let job = store
                .reconcile_background_job(job_id)?
                .ok_or_else(|| anyhow!("background job not found: {}", args.job_id))?;
            append_control_event(
                cwd,
                EventKind::BackgroundJobResumed {
                    job_id,
                    reference: job.reference.clone(),
                },
            )?;
            let metadata =
                serde_json::from_str::<serde_json::Value>(&job.metadata_json).unwrap_or_default();
            let stdout_tail = metadata
                .get("stdout_log")
                .and_then(|v| v.as_str())
                .and_then(|path| tail_file_lines(Path::new(path), args.tail_lines));
            let stderr_tail = metadata
                .get("stderr_log")
                .and_then(|v| v.as_str())
                .and_then(|path| tail_file_lines(Path::new(path), args.tail_lines));
            Ok(json!({
                "job_id": job_id,
                "kind": job.kind,
                "status": job.status,
                "reference": job.reference,
                "metadata": metadata,
                "log_tail": {
                    "stdout": stdout_tail,
                    "stderr": stderr_tail
                }
            }))
        }
        BackgroundCmd::Stop(args) => {
            let job_id = Uuid::parse_str(&args.job_id)?;
            let mut job = store
                .load_background_job(job_id)?
                .ok_or_else(|| anyhow!("background job not found: {}", args.job_id))?;
            let cancelled = store.cancel_operation(job_id)?;
            anyhow::ensure!(
                cancelled > 0,
                "Legacy background job has no owned cancellation channel; inspect it locally before terminating any process"
            );
            let operation = store.operation(job_id)?;
            let active = matches!(operation.state.as_str(), "prepared" | "running");
            if active {
                job.status = "cancelling".into();
            }
            job.updated_at = Utc::now().to_rfc3339();
            store.upsert_background_job(&job)?;
            Ok(json!({"job_id":job_id,"stopped":!active,"status":job.status}))
        }

        BackgroundCmd::RunAgent(args) => {
            let prompt = args.prompt.join(" ").trim().to_string();
            if prompt.is_empty() {
                return Err(anyhow!("background run-agent prompt is empty"));
            }
            let cfg = AppConfig::ensure(cwd)?;
            codingbuddy_llm::check_provider(&cfg.llm)?;
            let session = ensure_session_record(cwd, &store)?;
            crate::worker::spawn(
                cwd,
                session.session_id,
                crate::worker::WorkerJob::Agent {
                    prompt,
                    tools: args.tools,
                    session: session.session_id,
                    task: None,
                    max_turns: None,
                },
                cfg,
                None,
            )
        }

        BackgroundCmd::RunShell(args) => {
            let command_line = args.command.join(" ").trim().to_string();
            if command_line.is_empty() {
                return Err(anyhow!("background run-shell command is empty"));
            }
            let session = ensure_session_record(cwd, &store)?;
            crate::worker::spawn(
                cwd,
                session.session_id,
                crate::worker::WorkerJob::Shell {
                    command: command_line,
                },
                AppConfig::ensure(cwd)?,
                None,
            )
        }
    }
}

pub(crate) fn tail_file_lines(path: &Path, max_lines: usize) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return Some(String::new());
    }
    let keep = max_lines.max(1);
    let start = lines.len().saturating_sub(keep);
    Some(lines[start..].join("\n"))
}
