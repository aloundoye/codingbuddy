//! Resume, continue and one-shot print mode (-p) entry points.

use super::*;

pub(crate) fn run_resume(cwd: &Path, args: RunArgs, json_mode: bool) -> Result<String> {
    if let Some(session_id) = args.session_id {
        let session_id = Uuid::parse_str(&session_id)?;
        let store = Store::new(cwd)?;
        let projection = store.rebuild_from_events(session_id)?;
        return Ok(format!(
            "resumed session={} turns={} steps={}",
            session_id,
            projection.transcript.len(),
            projection.step_status.len()
        ));
    }
    ensure_llm_ready(cwd, json_mode)?;
    #[allow(deprecated)]
    {
        AgentEngine::new(cwd)?.resume()
    }
}

pub(crate) fn run_print_mode(cwd: &Path, cli: &Cli) -> Result<()> {
    use codingbuddy_core::StreamChunk;
    use std::io::{Read, stdin};

    let prompt = if !cli.prompt_args.is_empty() {
        cli.prompt_args.join(" ")
    } else if !stdin().is_terminal() {
        let mut buf = String::new();
        stdin().read_to_string(&mut buf)?;
        buf.trim().to_string()
    } else {
        return Err(anyhow!(
            "-p/--print requires a prompt argument or stdin input"
        ));
    };

    if prompt.is_empty() {
        return Err(anyhow!("empty prompt"));
    }

    let json_mode = cli.json || cli.output_format == "json" || cli.output_format == "stream-json";
    let is_stream_json = cli.output_format == "stream-json";
    let is_text = !json_mode;
    let cfg = selected_config(cwd, cli.model.as_deref())?;
    codingbuddy_llm::check_provider(&cfg.llm)?;
    let mut engine = AgentEngine::new_with_config(cwd, cfg)?;
    apply_cli_flags(&mut engine, cli);
    wire_subagent_worker(&engine, cwd);

    // Handle --no-input: auto-deny all approval prompts
    if cli.no_input {
        engine.set_approval_handler(Box::new(|_call| Ok(false)));
    }

    // Handle --from-pr: fetch PR diff and prepend to prompt
    let prompt = if let Some(pr_number) = cli.from_pr {
        let diff = std::process::Command::new("gh")
            .args(["pr", "diff", &pr_number.to_string()])
            .output()?;
        let pr_context = String::from_utf8_lossy(&diff.stdout);
        format!("PR #{pr_number} diff:\n```\n{pr_context}\n```\n\n{prompt}")
    } else {
        prompt
    };

    let plain_stream = PlainStream::default();
    if is_stream_json {
        engine.set_stream_callback(Arc::new(|chunk| {
            let mut handle = std::io::stdout().lock();
            let _ = serde_json::to_writer(&mut handle, &stream_chunk_to_event_json(&chunk));
            let _ = writeln!(handle);
            let _ = handle.flush();
        }));
    } else if is_text {
        let stream = plain_stream.clone();
        engine.set_stream_callback(Arc::new(move |chunk| stream.event(chunk)));
    }

    let options = chat_options_from_cli(cli, true, ChatMode::Code);
    let output = engine.chat_with_options(&prompt, options);
    plain_stream.event(StreamChunk::Done { reason: None });
    let output = output?;

    match cli.output_format.as_str() {
        "json" => {
            let session_id = Store::new(cwd)?
                .load_latest_session()?
                .map(|s| s.session_id.to_string())
                .unwrap_or_default();
            print_json(&json!({
                "output": output,
                "session_id": session_id,
                "model": AppConfig::load(cwd)
                    .unwrap_or_default()
                    .llm
                    .active_base_model(),
            }))?;
        }
        "stream-json" => {
            // Streaming was already output via callback; emit final summary
            let session_id = Store::new(cwd)?
                .load_latest_session()?
                .map(|s| s.session_id.to_string())
                .unwrap_or_default();
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "type": "result",
                    "output": output,
                    "session_id": session_id,
                    "model": AppConfig::load(cwd)
                        .unwrap_or_default()
                        .llm
                        .active_base_model(),
                }))?
            );
        }
        _ => {
            // Text was already streamed to stdout via callback; output is the session summary
            // (only print if there was no streaming, e.g., from cache hit)
        }
    }
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct PlainStream(Arc<std::sync::Mutex<bool>>);

impl PlainStream {
    pub(crate) fn event(&self, chunk: StreamChunk) {
        if let Ok(mut line_open) = self.0.lock() {
            let mut writer = std::io::stdout().lock();
            Self::write_event(&mut writer, &mut line_open, chunk);
            let _ = writer.flush();
        }
    }

    pub(crate) fn write_event(writer: &mut impl Write, line_open: &mut bool, chunk: StreamChunk) {
        let status = match chunk {
            StreamChunk::ContentDelta(text) => {
                if !text.is_empty() {
                    let _ = write!(writer, "{text}");
                    *line_open = !text.ends_with('\n');
                }
                return;
            }
            StreamChunk::ToolCallStart {
                tool_name,
                args_summary,
            } => format!("[tool: {tool_name}] {args_summary}"),
            StreamChunk::ToolCallEnd {
                tool_name,
                duration_ms,
                success,
                summary,
            } => {
                let status = if success { "ok" } else { "error" };
                format!("[tool: {tool_name}] {status} ({duration_ms}ms) {summary}")
            }
            StreamChunk::ModeTransition { from, to, reason } => {
                format!("[mode: {from} -> {to}: {reason}]")
            }
            StreamChunk::SubagentSpawned { run_id, name, goal } => {
                format!("[subagent:spawned] {name} ({run_id}) {goal}")
            }
            StreamChunk::SubagentCompleted {
                run_id,
                name,
                summary,
            } => format!(
                "[subagent:completed] {name} ({run_id}) {}",
                summary.replace('\n', " ")
            ),
            StreamChunk::SubagentFailed {
                run_id,
                name,
                error,
            } => format!(
                "[subagent:failed] {name} ({run_id}) {}",
                error.replace('\n', " ")
            ),
            StreamChunk::ImageData { label, .. } => format!("[image: {label}]"),
            StreamChunk::WatchTriggered { comment_count, .. } => {
                format!("[watch: {comment_count} comment(s) detected]")
            }
            StreamChunk::SecurityWarning { message } => format!("[security warning: {message}]"),
            StreamChunk::ContextWarning {
                used_pct,
                compact_at_pct,
            } => format!("[context {used_pct}% full; compacting at {compact_at_pct}%]"),
            StreamChunk::Error { message, .. } => format!("[error: {message}]"),
            StreamChunk::PhaseTransition { from, to } => format!("[phase: {from} -> {to}]"),
            StreamChunk::ModelChanged { model } => format!("[model changed: {model}]"),
            StreamChunk::ConfigReloaded { path } => {
                eprintln!("Config reloaded: {path}");
                return;
            }
            StreamChunk::RateLimited {
                wait_seconds,
                provider,
                attempt,
                max_attempts,
            } => format!(
                "[rate limited by {provider}: retrying in {wait_seconds}s ({attempt}/{max_attempts})]"
            ),
            StreamChunk::ToolProgress { tool_name, data } => {
                if !*line_open {
                    let _ = write!(writer, "[{tool_name}] ");
                }
                let _ = write!(writer, "{data}");
                *line_open = !data.ends_with('\n');
                return;
            }
            StreamChunk::ClearStreamingText | StreamChunk::Done { .. } => {
                if std::mem::take(line_open) {
                    let _ = writeln!(writer);
                }
                return;
            }
            StreamChunk::ReasoningDelta(_)
            | StreamChunk::UsageUpdate { .. }
            | StreamChunk::SnapshotRecorded { .. }
            | StreamChunk::ToolCallReady { .. } => return,
        };
        if std::mem::take(line_open) {
            let _ = writeln!(writer);
        }
        let _ = writeln!(writer, "{status}");
    }
}

pub(crate) fn print_resume_banner(session: &codingbuddy_core::Session, turn_count: usize) {
    let id_prefix = &session.session_id.to_string()[..8];
    eprintln!(
        "Resuming session {id_prefix} ({turn_count} messages, state={:?})",
        session.status
    );
    if session.status == codingbuddy_core::SessionState::ExecutingStep {
        eprintln!(
            "Warning: Previous session was interrupted during tool execution. Results may be incomplete."
        );
    }
}

pub(crate) fn run_continue_session(cwd: &Path, json_mode: bool, cli: &crate::Cli) -> Result<()> {
    let store = Store::new(cwd)?;
    let session = store
        .load_latest_session()?
        .ok_or_else(|| anyhow!("no previous session to continue"))?;
    let projection = store.rebuild_from_events(session.session_id)?;
    if !json_mode && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        print_resume_banner(&session, projection.transcript.len());
    }
    run_chat(
        cwd,
        json_mode,
        false,
        true,
        false,
        Some(cli),
        Some(session.session_id),
    )
}

pub(crate) fn run_resume_specific(
    cwd: &Path,
    session_id: &str,
    json_mode: bool,
    cli: &crate::Cli,
) -> Result<()> {
    let store = Store::new(cwd)?;
    let uuid =
        Uuid::parse_str(session_id).map_err(|_| anyhow!("invalid session ID: {session_id}"))?;
    let session = store
        .load_session(uuid)?
        .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
    let projection = store.rebuild_from_events(session.session_id)?;
    if !json_mode && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        print_resume_banner(&session, projection.transcript.len());
    }
    run_chat(
        cwd,
        json_mode,
        false,
        true,
        false,
        Some(cli),
        Some(session.session_id),
    )
}
