use anyhow::Result;
use clap::Subcommand;
use codingbuddy_store::Store;
use std::path::Path;
use uuid::Uuid;

#[derive(Subcommand)]
pub enum SessionCmd {
    /// Preview a redacted fork or create it using the preview fingerprint.
    PortableFork {
        id: Uuid,
        #[arg(long)]
        confirm: Option<String>,
    },
    /// Acknowledge an interrupted tool invocation with an unknown result.
    ResolveInvocation {
        session_id: Uuid,
        invocation_id: Uuid,
        #[arg(long, conflicts_with = "retry")]
        acknowledge: bool,
        /// Execute the recorded tool again under current permissions.
        #[arg(long)]
        retry: bool,
    },
    /// List all sessions
    List,
    /// Show details of a specific session
    Show {
        /// Session ID to show (defaults to latest)
        id: Option<String>,
    },
    /// List all runs for a specific session
    Runs {
        /// Session ID to list runs for (defaults to latest)
        id: Option<String>,
    },
    /// Show a specific run
    Run {
        /// Run ID to show
        run_id: String,
    },
}

pub(crate) fn portable_history(
    workspace: &Path,
    store: &Store,
    id: Uuid,
) -> Result<Vec<codingbuddy_core::ChatMessage>> {
    use codingbuddy_core::ChatMessage;
    let config = codingbuddy_core::AppConfig::load(workspace)?;
    let privacy = codingbuddy_policy::privacy::PrivacyRouter::from_settings(&config.privacy)?;
    let history = store
        .rebuild_from_events(id)?
        .chat_messages
        .into_iter()
        .filter_map(|message| match message {
            ChatMessage::User { content } => Some(ChatMessage::User {
                content: privacy.redact(&content),
            }),
            ChatMessage::Assistant {
                content: Some(content),
                ..
            } => Some(ChatMessage::Assistant {
                content: Some(privacy.redact(&content)),
                native_parts: None,
                reasoning_content: None,
                tool_calls: vec![],
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    Ok(history)
}

pub fn run_session_cmd(workspace: &Path, command: SessionCmd, _json: bool) -> Result<()> {
    let store = Store::new(workspace)?;

    match command {
        SessionCmd::ResolveInvocation {
            session_id,
            invocation_id,
            acknowledge,
            retry,
        } => {
            if retry {
                let (id, result) = codingbuddy_agent::AgentEngine::new(workspace)?
                    .retry_interrupted_tool(session_id, invocation_id)?;
                println!("Retry invocation {id}. Success: {}", result.success);
                return Ok(());
            }
            anyhow::ensure!(
                acknowledge,
                "Pass --acknowledge after checking for possible side effects"
            );
            store.resolve_invocation(session_id, invocation_id)?;
            println!("Invocation {invocation_id} settled with an unknown result.");
        }
        SessionCmd::PortableFork { id, confirm } => {
            use sha2::{Digest, Sha256};
            anyhow::ensure!(store.load_session(id)?.is_some(), "session is unavailable");
            let _lease = store.session_lease(id)?;
            let history = portable_history(workspace, &store, id)?;
            let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&history)?));
            if let Some(confirm) = confirm {
                anyhow::ensure!(
                    confirm == fingerprint,
                    "History changed. Preview the fork again."
                );
                println!(
                    "{}",
                    store.fork_session_with_messages(id, &history)?.session_id
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "source_session":id,"fingerprint":fingerprint,"history":history
                    }))?
                );
            }
        }
        SessionCmd::List => {
            let sessions = store.list_sessions()?;
            if sessions.is_empty() {
                println!("No sessions found.");
                return Ok(());
            }

            println!("{:<40} | {:<20} | WORKSPACE", "SESSION ID", "STATUS");
            println!("{:-<40}-+-{:-<20}-+-{:-<40}", "", "", "");
            for s in sessions {
                println!(
                    "{:<40} | {:<20} | {}",
                    s.session_id.to_string(),
                    format!("{:?}", s.status),
                    s.workspace_root
                );
            }
        }
        SessionCmd::Show { id } => {
            let session = match id {
                Some(sid) => store.load_session(Uuid::parse_str(&sid)?)?,
                None => store.load_latest_session()?,
            };

            match session {
                Some(s) => {
                    println!("Session ID: {}", s.session_id);
                    println!("Status: {:?}", s.status);
                    if store.rebuild_from_events(s.session_id)?.legacy_tool_history {
                        println!("Legacy tool history may lack provider continuation data.");
                    }
                    println!("Workspace: {}", s.workspace_root);
                    if let Some(c) = s.baseline_commit {
                        println!("Baseline Commit: {}", c);
                    }
                    if let Some(p) = s.active_plan_id {
                        println!("Active Plan ID: {}", p);
                    }
                }
                None => println!("Session not found."),
            }
        }
        SessionCmd::Runs { id } => {
            let session = match id {
                Some(sid) => store.load_session(Uuid::parse_str(&sid)?)?,
                None => store.load_latest_session()?,
            };

            match session {
                Some(s) => {
                    let runs = store.list_runs(s.session_id)?;
                    if runs.is_empty() {
                        println!("No runs found for session {}.", s.session_id);
                        return Ok(());
                    }

                    println!(
                        "{:<40} | {:<20} | {:<30} | PROMPT",
                        "RUN ID", "STATUS", "CREATED AT"
                    );
                    println!("{:-<40}-+-{:-<20}-+-{:-<30}-+-{:-<40}", "", "", "", "");
                    for r in runs {
                        let short_prompt = if r.prompt.len() > 37 {
                            format!("{}...", &r.prompt[0..37]).replace("\n", " ")
                        } else {
                            r.prompt.replace("\n", " ")
                        };
                        println!(
                            "{:<40} | {:<20} | {:<30} | {}",
                            r.run_id.to_string(),
                            format!("{:?}", r.status),
                            r.created_at,
                            short_prompt
                        );
                    }
                }
                None => println!("Session not found."),
            }
        }
        SessionCmd::Run { run_id } => {
            let run = store.load_run(Uuid::parse_str(&run_id)?)?;
            match run {
                Some(r) => {
                    println!("Run ID:     {}", r.run_id);
                    println!("Session ID: {}", r.session_id);
                    println!("Status:     {:?}", r.status);
                    println!("Created At: {}", r.created_at);
                    println!("Updated At: {}", r.updated_at);
                    println!("Prompt:\n{}", r.prompt);
                }
                None => println!("Run not found."),
            }
        }
    }

    Ok(())
}
