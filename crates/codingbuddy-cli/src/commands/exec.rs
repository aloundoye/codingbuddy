use anyhow::Result;
use codingbuddy_core::ToolHostBlockingExt;
use codingbuddy_core::{
    AppConfig, ApprovedToolCall, EventKind, ToolCall, ToolHost, ToolPermission,
};
use codingbuddy_policy::PolicyEngine;
use codingbuddy_tools::LocalToolHost;
use serde_json::json;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use crate::ExecArgs;
use crate::context::append_control_event;
use crate::output::*;

pub(crate) fn run_exec(cwd: &Path, args: ExecArgs, json_mode: bool) -> Result<()> {
    let config = AppConfig::load(cwd).unwrap_or_default();
    let policy = PolicyEngine::from_app_config(&config.policy);

    let tool_host = LocalToolHost::new(cwd, policy)?;

    let call = ToolCall {
        name: "bash.run".to_string(),
        args: json!({"cmd": args.command, "timeout": args.timeout}),
        requires_approval: false,
    };
    let (proposal, permission) = tool_host.propose_with_permission(call);
    match permission {
        ToolPermission::Deny { reason, .. } => {
            append_control_event(
                cwd,
                EventKind::ToolDenied {
                    invocation_id: proposal.invocation_id,
                    tool_name: proposal.call.name,
                    reason: reason.clone(),
                },
            )?;
            anyhow::bail!("{reason}");
        }
        ToolPermission::Ask { reason, .. } => {
            anyhow::ensure!(
                !json_mode && io::stdin().is_terminal(),
                "{reason}. Interactive approval required"
            );
            eprint!("{reason}. Run this command once? [y/N] ");
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            anyhow::ensure!(answer.trim().eq_ignore_ascii_case("y"), "command declined");
        }
        ToolPermission::Allow => {}
    }
    let result = tool_host.execute_blocking(ApprovedToolCall {
        invocation_id: proposal.invocation_id,
        call: proposal.call,
    });

    if json_mode {
        print_json(&json!({
            "command": args.command,
            "success": result.success,
            "output": result.output,
        }))?;
    } else {
        if let Some(stdout) = result
            .output
            .get("stdout")
            .and_then(|v: &serde_json::Value| v.as_str())
        {
            print!("{stdout}");
        }
        if let Some(stderr) = result
            .output
            .get("stderr")
            .and_then(|v: &serde_json::Value| v.as_str())
            && !stderr.is_empty()
        {
            eprint!("{stderr}");
        }
        if let Some(error) = result
            .output
            .get("error")
            .and_then(serde_json::Value::as_str)
        {
            anyhow::bail!("{error}");
        }
    }
    Ok(())
}
