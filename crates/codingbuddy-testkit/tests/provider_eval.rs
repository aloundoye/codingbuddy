//! Fixed tasks for comparing routes without changing the workspace or tool policy.
#![allow(clippy::unwrap_used)] // integration test helpers may panic
use anyhow::{Context, Result, ensure};
use codingbuddy_agent::{AgentEngine, ChatOptions};
use codingbuddy_core::{
    AppConfig, CancellationToken, LlmConfig, LlmResponse, LlmToolCall, PermissionRuleConfig,
    StreamChunk,
};
use codingbuddy_store::Store;
use codingbuddy_testkit::{ScriptedLlm, scripted_text_response, scripted_tool_response};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

const ORIGINAL: &str = "{\"destination\":\"legacy\"}\n";
const FIXED: &str = "{\"destination\":\"current\"}\n";
const USER_EDIT: &str = "Uncommitted user note. Preserve byte for byte.\n";
const CASES: &[&str] = &[
    "exploration",
    "multi-file-fix",
    "failed-check-recovery",
    "cancellation",
    "long-context-resume",
];

fn task_call(tool: &str, args: Value) -> LlmResponse {
    scripted_tool_response(vec![LlmToolCall {
        id: uuid::Uuid::now_v7().to_string(),
        name: tool.into(),
        arguments: args.to_string(),
    }])
}

fn engine(
    workspace: &Path,
    mut cfg: AppConfig,
    scripted: Option<Vec<LlmResponse>>,
) -> Result<AgentEngine> {
    cfg.llm.max_retries = 0;
    cfg.llm.timeout_seconds = 30;
    cfg.workspace_trust = Some(false);
    cfg.agent_loop.phase_loop_enabled = false;
    cfg.policy.allowlist.clear();
    cfg.policy.block_paths.push("user-notes.txt".into());
    cfg.policy.permission_rules.push(PermissionRuleConfig {
        rule: "Edit(user-notes.txt)".into(),
        decision: "deny".into(),
    });
    let engine = if let Some(responses) = scripted {
        AgentEngine::new_with_config_and_llm(workspace, cfg, Box::new(ScriptedLlm::new(responses)))?
    } else {
        codingbuddy_llm::check_provider(&cfg.llm)?;
        AgentEngine::new_with_config(workspace, cfg)?
    };
    let workspace = workspace.canonicalize()?;
    engine.set_approval_handler(Box::new(move |call| {
        if !matches!(call.name.as_str(), "fs.write" | "fs.edit") {
            return Ok(false);
        }
        let Some(path) = call.args.get("path").and_then(Value::as_str) else {
            return Ok(false);
        };
        let path = Path::new(path);
        Ok(["router.json", "fallback.json"]
            .iter()
            .any(|name| path == Path::new(name) || path == workspace.join(name)))
    }));
    Ok(engine)
}

fn check_routes(root: &Path, paths: &[&str]) -> Result<bool> {
    for path in paths {
        let document: Value = serde_json::from_slice(&fs::read(root.join(path))?)?;
        if document != json!({"destination":"current"}) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn run_case(case: &str, llm: Option<&LlmConfig>) -> Result<Value> {
    let workspace = tempfile::tempdir()?;
    let root = workspace.path();
    fs::write(root.join("user-notes.txt"), USER_EDIT)?;
    fs::write(root.join("router.json"), ORIGINAL)?;
    fs::write(root.join("fallback.json"), ORIGINAL)?;
    fs::write(
        root.join("README.md"),
        "Read router.json for the primary destination.\n",
    )?;
    let cfg = AppConfig {
        llm: llm.cloned().unwrap_or_default(),
        ..Default::default()
    };
    let scripted = llm.is_none();
    let paths: &[&str] = if case == "multi-file-fix" {
        &["router.json", "fallback.json"]
    } else {
        &["router.json"]
    };
    let known_failure = !check_routes(root, paths)?;
    let (prompt, responses): (String, Vec<LlmResponse>) = match case {
        "exploration" => (
            "Use fs_read to inspect router.json and report the exact destination and filename."
                .into(),
            vec![
                task_call("fs_read", json!({"path":"router.json"})),
                scripted_text_response("router.json uses legacy."),
            ],
        ),
        "multi-file-fix" | "failed-check-recovery" => {
            let mut responses = Vec::new();
            for path in paths {
                responses.push(task_call("fs_write", json!({"path":path,"content":FIXED})));
            }
            responses.push(scripted_text_response("Updated the fixture."));
            let prompt = if case == "failed-check-recovery" {
                "The route schema check failed: router.json expected destination current, got legacy. Fix only router.json and preserve user-notes.txt.".into()
            } else {
                "Set destination to current in router.json and fallback.json. Preserve user-notes.txt and all other files.".into()
            };
            (prompt, responses)
        }
        "cancellation" => (
            "Generate a long explanation of how a routing table works.".into(),
            Vec::new(),
        ),
        "long-context-resume" => (
            "Repeat the decision code exactly, then read router.json.".into(),
            vec![
                task_call("fs_read", json!({"path":"router.json"})),
                scripted_text_response("ROUTE-731"),
            ],
        ),
        _ => anyhow::bail!("Unknown evaluation case"),
    };
    let store = Store::new(root)?;
    let session_id = if case == "long-context-resume" {
        let first = engine(
            root,
            cfg.clone(),
            scripted.then(|| vec![scripted_text_response("Recorded ROUTE-731.")]),
        )?;
        first.chat_with_options(
            &format!(
                "Remember decision code ROUTE-731. Reply with an acknowledgement. {}",
                "Routing documentation. ".repeat(512)
            ),
            ChatOptions {
                tools: true,
                ..Default::default()
            },
        )?;
        Some(
            store
                .load_latest_session()?
                .context("Missing initial session")?
                .session_id,
        )
    } else {
        None
    };
    let engine = engine(root, cfg, scripted.then_some(responses))?;
    let started = Instant::now();
    let first_event = Arc::new(OnceLock::new());
    let first_event_observed = first_event.clone();
    let cancellation = CancellationToken::new();
    let observed_token = cancellation.clone();
    let cancelling = case == "cancellation";
    engine.set_stream_callback(Arc::new(move |chunk| {
        if matches!(
            chunk,
            StreamChunk::ContentDelta(_) | StreamChunk::ToolCallStart { .. }
        ) {
            first_event_observed.get_or_init(|| started.elapsed().as_millis());
            if cancelling {
                observed_token.cancel();
            }
        }
    }));
    if cancelling && scripted {
        cancellation.cancel();
    }
    let result = engine.chat_with_options(
        &prompt,
        ChatOptions {
            tools: true,
            session_id,
            cancellation: Some(cancellation.clone()),
            deadline: Some(started + Duration::from_secs(90)),
            disable_team_orchestration: true,
            ..Default::default()
        },
    );

    let session = store.load_latest_session()?;
    let history = session
        .as_ref()
        .map(|s| store.rebuild_from_events(s.session_id))
        .transpose()?;
    let text = result.as_ref().map(String::as_str).unwrap_or_default();
    let patch_ok = match case {
        "multi-file-fix" | "failed-check-recovery" => {
            known_failure
                && check_routes(root, paths)?
                && (case != "failed-check-recovery"
                    || fs::read_to_string(root.join("fallback.json"))? == ORIGINAL)
        }
        _ => {
            fs::read_to_string(root.join("router.json"))? == ORIGINAL
                && fs::read_to_string(root.join("fallback.json"))? == ORIGINAL
        }
    };
    let user_edits_preserved = fs::read_to_string(root.join("user-notes.txt"))? == USER_EDIT;
    let permission_denials = history.as_ref().map_or(0, |p| p.denied_invocations.len());
    let tool_invocations = history.as_ref().map_or(0, |p| p.tool_invocations.len());
    let passed = patch_ok
        && user_edits_preserved
        && match case {
            "exploration" => {
                text.contains("router.json") && text.contains("legacy") && tool_invocations > 0
            }
            "long-context-resume" => {
                text.contains("ROUTE-731") && session_id == session.as_ref().map(|x| x.session_id)
            }
            "cancellation" => {
                result.is_err() && cancellation.is_cancelled() && tool_invocations == 0
            }
            _ => result.is_ok(),
        };
    let usage = session
        .as_ref()
        .map(|s| store.usage_summary(Some(s.session_id), None))
        .transpose()?;
    let price = llm.and_then(|cfg| {
        codingbuddy_llm::certification::configured_price(cfg, &cfg.active_base_model())
    });
    let estimate = usage
        .as_ref()
        .filter(|u| u.records > 0)
        .zip(price)
        .map(|(usage, cost)| {
            (usage.input_tokens as f64 * cost.input_per_mtok_usd
                + usage.output_tokens as f64 * cost.output_per_mtok_usd)
                / 1_000_000.0
        });
    Ok(json!({
        "case":case, "passed":passed, "patch_correct":patch_ok, "user_edits_preserved":user_edits_preserved,
        "permission_denials":permission_denials, "tool_invocations":tool_invocations,
        "transport_retries":0, "first_event_ms":first_event.get(), "duration_ms":started.elapsed().as_millis(),
        "usage":usage.filter(|u| u.records > 0), "estimated_cost_usd":estimate,
    }))
}

#[test]
fn provider_neutral_fixture_scores() -> Result<()> {
    for case in CASES {
        let report = run_case(case, None)?;
        ensure!(report["passed"] == true, "{report}");
    }
    Ok(())
}

#[test]
#[ignore = "provides an explicit billable live evaluation"]
fn provider_neutral_live_eval() -> Result<()> {
    let root = std::env::var_os("CODINGBUDDY_EVAL_CONFIG_WORKSPACE").context(
        "Set CODINGBUDDY_EVAL_CONFIG_WORKSPACE to the workspace holding your provider settings",
    )?;
    let provider =
        std::env::var("CODINGBUDDY_EVAL_PROVIDER").context("Set CODINGBUDDY_EVAL_PROVIDER")?;
    let model = std::env::var("CODINGBUDDY_EVAL_MODEL").context("Set CODINGBUDDY_EVAL_MODEL")?;
    let mut cfg = AppConfig::load(Path::new(&root))?.llm;
    cfg.select_provider(&provider);
    let profile = cfg
        .providers
        .get_mut(&provider)
        .context("Configure the named provider profile")?;
    profile.models.chat.clone_from(&model);
    let cases = CASES
        .iter()
        .map(|case| run_case(case, Some(&cfg)))
        .collect::<Result<Vec<_>>>()?;
    let status = codingbuddy_llm::certification::status(&cfg, &model);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "suite":"provider-neutral-v1","provider":provider,"model":model,
            "auth":status["auth"],"protocol":status["protocol"],"deployment":status["deployment"],
            "implementation_digest":status["implementation_digest"],
            "evaluated_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
            "cases":cases
        }))?
    );
    Ok(())
}
