use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[test]
fn plan_command_emits_json_shape() {
    let workspace = TempDir::new().expect("workspace");
    let out = run_json(
        workspace.path(),
        &["--json", "plan", "Refactor planner state"],
    );
    // The plan command now routes through chat_with_options and wraps
    // the LLM response text in a {"plan": "..."} envelope.
    assert!(
        out.get("plan").is_some(),
        "plan command JSON should contain 'plan' key, got: {out}"
    );
}

#[test]
fn ask_json_fails_fast_without_api_key_when_api_key_missing() {
    let workspace = TempDir::new().expect("workspace");
    let home = TempDir::new().expect("home");
    let mock = start_mock_llm_server();
    configure_runtime_for_mock_llm(workspace.path(), &mock.endpoint);
    let output = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"))
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path())
        .env_remove("DEEPSEEK_API_KEY")
        .args(["--json", "ask", "hello"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8_lossy(&output);
    assert!(stderr.contains("DEEPSEEK_API_KEY"), "{stderr}");
    assert_eq!(mock.requests.load(Ordering::Relaxed), 0);
}

#[test]
fn ask_json_uses_workspace_local_api_key_when_env_missing() {
    let workspace = TempDir::new().expect("workspace");
    let mock = start_mock_llm_server();
    configure_runtime_for_mock_llm(workspace.path(), &mock.endpoint);
    fs::write(
        workspace.path().join(".codingbuddy/settings.local.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "llm": {
                "provider": "deepseek",
                "profile": "v3_2",
                "endpoint": mock.endpoint,
                "api_key_env": "DEEPSEEK_API_KEY",
                "api_key": "workspace-local-key"
            }
        }))
        .expect("serialize settings"),
    )
    .expect("settings");

    let output = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"))
        .current_dir(workspace.path())
        .env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path())
        .env_remove("DEEPSEEK_API_KEY")
        .args(["--json", "ask", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let payload: Value = serde_json::from_slice(&output).expect("json payload");
    assert!(
        payload["output"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
}

#[test]
fn config_show_redacts_api_key_in_json_and_text_modes() {
    let workspace = TempDir::new().expect("workspace");
    let runtime = workspace.path().join(".codingbuddy");
    fs::create_dir_all(&runtime).expect("runtime dir");
    fs::write(
        runtime.join("settings.local.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "llm": {
                "api_key": "dsk-secret-value"
            }
        }))
        .expect("serialize settings"),
    )
    .expect("settings file");

    let json_out = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"))
        .current_dir(workspace.path())
        .env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path())
        .args(["--json", "config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let payload: Value = serde_json::from_slice(&json_out).expect("json");
    assert_eq!(payload["llm"]["api_key"], "***REDACTED***");

    let text_out = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"))
        .current_dir(workspace.path())
        .env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path())
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&text_out);
    assert!(stdout.contains("***REDACTED***"));
    assert!(!stdout.contains("dsk-secret-value"));
}

#[test]
fn plugins_commands_round_trip_json() {
    let workspace = TempDir::new().expect("workspace");
    let plugin_root = workspace.path().join("demo-plugin");
    fs::create_dir_all(plugin_root.join(".codingbuddy-plugin")).expect("plugin manifest dir");
    fs::create_dir_all(plugin_root.join("commands")).expect("plugin commands dir");
    fs::write(
        plugin_root.join(".codingbuddy-plugin/plugin.json"),
        r#"{"id":"demo","name":"Demo","version":"0.1.0"}"#,
    )
    .expect("plugin manifest");
    fs::write(plugin_root.join("commands/build.md"), "# build").expect("plugin command");

    let install = run_json(
        workspace.path(),
        &[
            "--json",
            "plugins",
            "install",
            plugin_root.to_string_lossy().as_ref(),
        ],
    );
    assert_eq!(install["manifest"]["id"], "demo");

    let listed = run_json(workspace.path(), &["--json", "plugins", "list"]);
    assert!(listed.as_array().is_some_and(|l| !l.is_empty()));

    let disabled = run_json(workspace.path(), &["--json", "plugins", "disable", "demo"]);
    assert_eq!(disabled["disabled"], "demo");

    let inspected = run_json(workspace.path(), &["--json", "plugins", "inspect", "demo"]);
    assert_eq!(inspected["manifest"]["version"], "0.1.0");
    assert_eq!(inspected["enabled"], false);

    let removed = run_json(workspace.path(), &["--json", "plugins", "remove", "demo"]);
    assert_eq!(removed["removed"], "demo");
}

#[test]
fn plugin_run_executes_command_prompt() {
    let workspace = TempDir::new().expect("workspace");
    let plugin_root = workspace.path().join("demo-plugin");
    fs::create_dir_all(plugin_root.join(".codingbuddy-plugin")).expect("plugin manifest dir");
    fs::create_dir_all(plugin_root.join("commands")).expect("plugin commands dir");
    fs::write(
        plugin_root.join(".codingbuddy-plugin/plugin.json"),
        r#"{"id":"demo","name":"Demo","version":"0.1.0"}"#,
    )
    .expect("plugin manifest");
    fs::write(
        plugin_root.join("commands/review.md"),
        "Please review: {{input}}",
    )
    .expect("plugin command");

    let _ = run_json(
        workspace.path(),
        &[
            "--json",
            "plugins",
            "install",
            plugin_root.to_string_lossy().as_ref(),
        ],
    );

    let out = run_json(
        workspace.path(),
        &[
            "--json",
            "plugins",
            "run",
            "demo",
            "review",
            "--input",
            "router thresholds",
            "--tools",
            "false",
            "--max-think",
            "true",
        ],
    );
    assert_eq!(out["plugin_id"], "demo");
    assert_eq!(out["command_name"], "review");
    assert_eq!(out["tools"], false);
    assert!(out["output"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
fn autopilot_runs_bounded_iteration_in_json_mode() {
    let workspace = TempDir::new().expect("workspace");
    let out = run_json(
        workspace.path(),
        &[
            "--json",
            "autopilot",
            "Stabilize project",
            "--max-iterations",
            "1",
            "--duration-seconds",
            "30",
            "--tools",
            "false",
            "--max-think",
            "true",
        ],
    );

    assert_eq!(out["stop_reason"], "max_iterations_reached");
    assert_eq!(out["completed_iterations"], 1);
    assert_eq!(out["failed_iterations"], 0);
}

#[test]
fn autopilot_respects_stop_file_and_writes_heartbeat() {
    let workspace = TempDir::new().expect("workspace");
    let stop_file = workspace.path().join(".codingbuddy/autopilot.stop");
    let heartbeat_file = workspace
        .path()
        .join(".codingbuddy/autopilot.heartbeat.json");
    fs::create_dir_all(
        stop_file
            .parent()
            .expect("stop file must have parent directory"),
    )
    .expect("create stop dir");
    fs::write(&stop_file, "stop").expect("write stop file");

    let out = run_json(
        workspace.path(),
        &[
            "--json",
            "autopilot",
            "Stabilize project",
            "--duration-seconds",
            "30",
            "--max-iterations",
            "5",
            "--stop-file",
            stop_file.to_string_lossy().as_ref(),
            "--heartbeat-file",
            heartbeat_file.to_string_lossy().as_ref(),
        ],
    );

    assert_eq!(out["stop_reason"], "stop_file_detected");
    assert_eq!(out["completed_iterations"], 0);

    let heartbeat: Value = serde_json::from_slice(
        &fs::read(heartbeat_file).expect("heartbeat file should be written"),
    )
    .expect("heartbeat json");
    assert_eq!(heartbeat["status"], "stopped");
}

#[test]
fn status_usage_compact_and_doctor_emit_json() {
    let workspace = TempDir::new().expect("workspace");
    let _ = run_json(
        workspace.path(),
        &[
            "--json",
            "autopilot",
            "Create baseline session for status checks",
            "--max-iterations",
            "1",
            "--duration-seconds",
            "30",
            "--tools",
            "false",
        ],
    );

    let status = run_json(workspace.path(), &["--json", "status"]);
    assert!(status.get("session_id").is_some());
    assert!(status["model"]["base"].as_str().is_some());
    assert!(status["model"]["compatibility"]["base"]["summary"].is_string());
    assert!(status["privacy"]["enabled"].is_boolean());

    let usage = run_json(workspace.path(), &["--json", "usage", "--session"]);
    assert!(usage["records"].as_u64().is_some());

    let compact = run_json(workspace.path(), &["--json", "compact", "--yes"]);
    if compact["persisted"].as_bool().unwrap_or(false) {
        assert!(compact["result"]["summary_id"].as_str().is_some());
    } else {
        assert_eq!(compact["status"], "no_op");
    }

    let doctor = run_json(workspace.path(), &["--json", "doctor"]);
    assert!(doctor["os"].as_str().is_some());
    assert!(doctor["checks"]["cargo"].as_bool().is_some());
}

#[test]
fn autopilot_status_stop_resume_commands_work() {
    let workspace = TempDir::new().expect("workspace");
    let run = run_json(
        workspace.path(),
        &[
            "--json",
            "autopilot",
            "Control command smoke test",
            "--max-iterations",
            "1",
            "--duration-seconds",
            "30",
            "--tools",
            "false",
        ],
    );
    let run_id = run["run_id"].as_str().expect("run id").to_string();

    let status = run_json(
        workspace.path(),
        &["--json", "autopilot", "status", "--run-id", &run_id],
    );
    assert_eq!(status["run_id"], run_id);
    assert!(status["pause_file"].as_str().is_some());

    let follow = run_json(
        workspace.path(),
        &[
            "--json",
            "autopilot",
            "status",
            "--run-id",
            &run_id,
            "--follow",
            "--samples",
            "2",
            "--interval-seconds",
            "1",
        ],
    );
    assert_eq!(follow["follow"], true);
    assert!(follow["samples_collected"].as_u64().unwrap_or(0) >= 1);
    assert!(
        follow["samples"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );

    let stop = run_json(
        workspace.path(),
        &["--json", "autopilot", "stop", "--run-id", &run_id],
    );
    assert_eq!(stop["stop_requested"], true);

    let pause = run_json(
        workspace.path(),
        &["--json", "autopilot", "pause", "--run-id", &run_id],
    );
    assert_eq!(pause["pause_requested"], true);

    let resumed = run_json(
        workspace.path(),
        &["--json", "autopilot", "resume", "--run-id", &run_id],
    );
    assert!(resumed["run_id"].as_str().is_some());
    if resumed["resumed_live"].is_null() {
        assert!(resumed["stop_reason"].as_str().is_some());
    } else {
        assert_eq!(resumed["resumed_live"], true);
    }
}

#[test]
fn permissions_show_set_and_status_emit_json() {
    let workspace = TempDir::new().expect("workspace");
    let shown = run_json(workspace.path(), &["--json", "permissions", "show"]);
    assert!(shown["policy"]["approve_bash"].as_str().is_some());
    assert!(shown["policy"]["allowlist"].is_array());

    let updated = run_json(
        workspace.path(),
        &[
            "--json",
            "permissions",
            "set",
            "--approve-bash",
            "always",
            "--approve-edits",
            "never",
            "--sandbox-mode",
            "workspace-write",
            "--clear-allowlist",
            "--allow",
            "npm *",
        ],
    );
    assert_eq!(updated["updated"], true);
    assert_eq!(updated["policy"]["approve_bash"], "always");
    assert_eq!(updated["policy"]["approve_edits"], "never");
    assert_eq!(updated["policy"]["sandbox_mode"], "workspace-write");
    assert!(
        updated["policy"]["allowlist"]
            .as_array()
            .is_some_and(|items| items.iter().any(|entry| entry == "npm *"))
    );

    let status = run_json(workspace.path(), &["--json", "status"]);
    assert_eq!(status["permissions"]["approve_bash"], "always");
    assert_eq!(status["permissions"]["approve_edits"], "never");
    assert_eq!(status["permissions"]["sandbox_mode"], "workspace-write");
}

#[test]
fn permissions_set_respects_team_policy_locks() {
    let workspace = TempDir::new().expect("workspace");
    let team_policy_path = workspace.path().join(".codingbuddy/team-policy.json");
    fs::create_dir_all(
        team_policy_path
            .parent()
            .expect("team policy parent directory"),
    )
    .expect("team policy dir");
    fs::write(
        &team_policy_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "approve_bash": "never",
            "allowlist": ["git status"],
            "sandbox_mode": "read-only"
        }))
        .expect("serialize team policy"),
    )
    .expect("write team policy");

    let shown = run_json_with_env(
        workspace.path(),
        &["--json", "permissions", "show"],
        &[(
            "CODINGBUDDY_TEAM_POLICY_PATH",
            team_policy_path.to_string_lossy().as_ref(),
        )],
    );
    assert_eq!(shown["team_policy"]["active"], true);
    assert_eq!(shown["team_policy"]["approve_bash_locked"], true);
    assert_eq!(shown["team_policy"]["allowlist_locked"], true);
    assert_eq!(shown["team_policy"]["sandbox_mode_locked"], true);

    let output = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"))
        .current_dir(workspace.path())
        .env(
            "CODINGBUDDY_TEAM_POLICY_PATH",
            team_policy_path.to_string_lossy().as_ref(),
        )
        .args([
            "--json",
            "permissions",
            "set",
            "--approve-bash",
            "always",
            "--allow",
            "npm *",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8_lossy(&output);
    assert!(stderr.contains("locks permissions fields"));
}

#[test]
fn plugins_catalog_search_and_verify_emit_json() {
    let workspace = TempDir::new().expect("workspace");
    let catalog_root = workspace.path().join(".codingbuddy/plugins");
    fs::create_dir_all(&catalog_root).expect("catalog dir");
    fs::write(
        catalog_root.join("catalog.json"),
        r#"{
  "plugins": [
    {
      "id": "demo-plugin",
      "name": "Demo Plugin",
      "version": "0.1.0",
      "description": "catalog fixture",
      "source": "https://example.com/demo"
    }
  ]
}"#,
    )
    .expect("catalog file");

    let catalog = run_json(workspace.path(), &["--json", "plugins", "catalog"]);
    assert_eq!(catalog.as_array().map(|a| a.len()).unwrap_or_default(), 1);

    let search = run_json(
        workspace.path(),
        &["--json", "plugins", "search", "demo-plugin"],
    );
    assert_eq!(search.as_array().map(|a| a.len()).unwrap_or_default(), 1);

    let verify = run_json(
        workspace.path(),
        &["--json", "plugins", "verify", "demo-plugin"],
    );
    assert_eq!(verify["plugin_id"], "demo-plugin");
    assert_eq!(verify["verified"], false);
}

#[test]
fn mcp_memory_and_export_emit_json() {
    let workspace = TempDir::new().expect("workspace");

    let memory = run_json(workspace.path(), &["--json", "memory", "show"]);
    assert!(
        memory["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("CODINGBUDDY.md"))
    );

    let sync = run_json(
        workspace.path(),
        &["--json", "memory", "sync", "--note", "test-sync"],
    );
    assert_eq!(sync["synced"], true);
    assert_eq!(sync["note"], "test-sync");

    let added = run_json(
        workspace.path(),
        &[
            "--json",
            "mcp",
            "add",
            "local",
            "--name",
            "Local MCP",
            "--transport",
            "stdio",
            "--command",
            "echo",
            "--arg",
            "ok",
            "--metadata",
            r#"{"tools":[{"name":"ping","description":"health"}]}"#,
        ],
    );
    assert_eq!(added["added"]["id"], "local");

    let listed = run_json(workspace.path(), &["--json", "mcp", "list"]);
    assert!(listed.as_array().is_some_and(|servers| !servers.is_empty()));

    let got = run_json(workspace.path(), &["--json", "mcp", "get", "local"]);
    assert_eq!(got["id"], "local");

    let _ = run_json(
        workspace.path(),
        &["--json", "ask", "Generate transcript for export command"],
    );
    let exported = run_json(workspace.path(), &["--json", "export", "--format", "md"]);
    assert_eq!(exported["format"], "md");
    assert!(exported["output_path"].as_str().is_some());

    let removed = run_json(workspace.path(), &["--json", "mcp", "remove", "local"]);
    assert_eq!(removed["removed"], true);
}

#[test]
fn rewind_uses_checkpoint_id_from_apply() {
    let workspace = TempDir::new().expect("workspace");
    let _ = run_json(
        workspace.path(),
        &["--json", "ask", "Create patch so apply can checkpoint"],
    );

    // Stage a patch directly so `apply --yes` can create a checkpoint.
    // The chat-with-tools path doesn't auto-stage patches like the legacy
    // plan-and-execute path did.
    let store = codingbuddy_diff::PatchStore::new(workspace.path()).expect("patch store");
    let diff = "diff --git a/.codingbuddy/notes.txt b/.codingbuddy/notes.txt\nnew file mode 100644\nindex 0000000..2b9d865\n--- /dev/null\n+++ b/.codingbuddy/notes.txt\n@@ -0,0 +1 @@\n+rewind checkpoint test\n";
    let _ = store.stage(diff, &[]).expect("stage patch");

    let applied = run_json(workspace.path(), &["--json", "apply", "--yes"]);
    let checkpoint_id = applied["checkpoint_id"]
        .as_str()
        .expect("checkpoint id from apply")
        .to_string();

    fs::write(workspace.path().join("scratch.txt"), "modified").expect("write");

    let rewound = run_json(
        workspace.path(),
        &[
            "--json",
            "rewind",
            "--to-checkpoint",
            &checkpoint_id,
            "--yes",
        ],
    );
    assert_eq!(rewound["rewound"], true);
    assert_eq!(rewound["checkpoint_id"], checkpoint_id);
}

#[test]
fn git_resolve_all_and_stage_clears_conflicts() {
    if StdCommand::new("git").arg("--version").output().is_err() {
        return;
    }

    let workspace = TempDir::new().expect("workspace");
    let init_status = StdCommand::new("git")
        .current_dir(workspace.path())
        .arg("init")
        .status()
        .expect("git init");
    assert!(init_status.success());
    let initial_branch = String::from_utf8_lossy(
        &StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["branch", "--show-current"])
            .output()
            .expect("git branch --show-current")
            .stdout,
    )
    .trim()
    .to_string();
    assert!(!initial_branch.is_empty());
    assert!(
        StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("git config email")
            .success()
    );
    assert!(
        StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["config", "user.name", "Test User"])
            .status()
            .expect("git config name")
            .success()
    );

    let conflicted = workspace.path().join("conflicted.txt");
    fs::write(&conflicted, "base\n").expect("write base");
    assert!(
        StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["add", "conflicted.txt"])
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["commit", "-m", "base"])
            .status()
            .expect("git commit base")
            .success()
    );

    assert!(
        StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["checkout", "-b", "feature"])
            .status()
            .expect("git checkout feature")
            .success()
    );
    fs::write(&conflicted, "feature\n").expect("write feature");
    assert!(
        StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["commit", "-am", "feature"])
            .status()
            .expect("git commit feature")
            .success()
    );

    assert!(
        StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["checkout", &initial_branch])
            .status()
            .expect("git checkout initial branch")
            .success()
    );
    fs::write(&conflicted, "main\n").expect("write main");
    assert!(
        StdCommand::new("git")
            .current_dir(workspace.path())
            .args(["commit", "-am", "main"])
            .status()
            .expect("git commit main")
            .success()
    );

    let merge_status = StdCommand::new("git")
        .current_dir(workspace.path())
        .args(["merge", "feature"])
        .status()
        .expect("git merge");
    assert!(!merge_status.success());

    let resolved = run_json(
        workspace.path(),
        &[
            "--json",
            "git",
            "resolve",
            "--strategy",
            "ours",
            "--all",
            "--stage",
        ],
    );
    assert!(resolved["count"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(resolved["stage"], true);
    assert!(
        resolved["resolved_files"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row == "conflicted.txt"))
    );

    let remaining = StdCommand::new("git")
        .current_dir(workspace.path())
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .expect("git diff conflicts");
    assert!(remaining.status.success());
    let remaining_stdout = String::from_utf8_lossy(&remaining.stdout);
    assert!(remaining_stdout.trim().is_empty());
}

#[test]
fn delivery_three_background_headless_denial_is_durable() {
    let workspace = TempDir::new().unwrap();
    let started = run_json(
        workspace.path(),
        &["--json", "background", "run-shell", "echo", "must-not-run"],
    );
    let id = uuid::Uuid::parse_str(started["job_id"].as_str().unwrap()).unwrap();
    let store = codingbuddy_store::Store::new(workspace.path()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let job = store.reconcile_background_job(id).unwrap().unwrap();
        if !matches!(job.status.as_str(), "prepared" | "running" | "cancelling") {
            assert_eq!(job.status, "failed", "{job:?}");
            assert!(
                job.metadata_json.contains("denied"),
                "{}",
                job.metadata_json
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Background worker did not settle"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        fs::read_to_string(started["stdout_log"].as_str().unwrap())
            .unwrap_or_default()
            .is_empty()
    );
    let stopped = run_json(
        workspace.path(),
        &["--json", "background", "stop", &id.to_string()],
    );
    assert_eq!(stopped["stopped"], true);
}

#[test]
fn delivery_three_background_private_bootstrap_survives_parent_exit() {
    let workspace = TempDir::new().unwrap();
    let ws = workspace.path();
    let mock = start_mock_llm_server();
    configure_runtime_for_mock_llm(ws, &mock.endpoint);
    fs::write(ws.join("seed.txt"), "parent input").unwrap();
    fs::write(ws.join(".gitignore"), ".codingbuddy/\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "--", "seed.txt", ".gitignore"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    ] {
        let output = StdCommand::new("git")
            .current_dir(ws)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let index = fs::read(ws.join(".git/index")).unwrap();
    let output = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"))
        .current_dir(ws)
        .env("CODINGBUDDY_TRUST_WORKSPACE", ws)
        .env("DEEPSEEK_API_KEY", "synthetic-provider-key")
        .env(
            "ARBITRARY_WORKER_SECRET",
            "synthetic-bootstrap-must-stay-private",
        )
        .args([
            "--json",
            "background",
            "run-agent",
            "Describe the supplied repository",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let started: Value = serde_json::from_slice(&output).unwrap();
    let id = uuid::Uuid::parse_str(started["job_id"].as_str().unwrap()).unwrap();
    let store = codingbuddy_store::Store::new(ws).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let job = loop {
        let job = store.reconcile_background_job(id).unwrap().unwrap();
        if !matches!(job.status.as_str(), "prepared" | "running" | "cancelling") {
            break job;
        }
        assert!(
            Instant::now() < deadline,
            "Worker did not finish after parent exit"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(job.status, "completed", "{}", job.metadata_json);
    assert!(
        job.metadata_json.contains("synthetic credential accepted"),
        "Provider did not receive the private bootstrap key: {}",
        job.metadata_json
    );
    assert!(mock.requests.load(Ordering::SeqCst) > 0);
    assert_eq!(fs::read(ws.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read_to_string(ws.join("seed.txt")).unwrap(),
        "parent input"
    );
    let mut pending = vec![ws.join(".codingbuddy")];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            pending.extend(
                fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        } else if metadata.is_file() {
            let bytes = fs::read(&path).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains("synthetic-provider-key")
                    && !text.contains("synthetic-bootstrap-must-stay-private"),
                "Credential leaked into {}",
                path.display()
            );
        }
    }
}

fn run_json(workspace: &Path, args: &[&str]) -> Value {
    run_json_with_env(workspace, args, &[])
}

fn run_json_with_env(workspace: &Path, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let mock = start_mock_llm_server();
    configure_runtime_for_mock_llm(workspace, &mock.endpoint);
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"));
    command
        .current_dir(workspace)
        .env("CODINGBUDDY_TRUST_WORKSPACE", workspace)
        .env("DEEPSEEK_API_KEY", "test-api-key")
        .args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&output).expect("json output")
}

struct MockLlmServer {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for MockLlmServer {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn start_mock_llm_server() -> MockLlmServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock llm server");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking listener");
    let addr = listener.local_addr().expect("mock addr");
    let (tx, rx) = mpsc::channel::<()>();
    let requests = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        loop {
            if rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    accepted.fetch_add(1, Ordering::Relaxed);
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = handle_mock_llm_connection(&mut stream);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    MockLlmServer {
        endpoint: format!("http://{addr}/chat/completions"),
        requests,
        stop_tx: Some(tx),
        handle: Some(handle),
    }
}

fn configure_runtime_for_mock_llm(workspace: &Path, endpoint: &str) {
    let runtime = workspace.join(".codingbuddy");
    fs::create_dir_all(&runtime).expect("runtime dir");
    fs::write(
        runtime.join("settings.local.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "llm": {
                "provider": "deepseek",
                "endpoint": endpoint,
                "api_key_env": "DEEPSEEK_API_KEY"
            }
        }))
        .expect("serialize settings"),
    )
    .expect("settings override");
}

fn handle_mock_llm_connection(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let mut header_end = None;
    while header_end.is_none() {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        header_end = find_subsequence(&buffer, b"\r\n\r\n").map(|idx| idx + 4);
        if buffer.len() > 1_048_576 {
            break;
        }
    }

    let header_len = header_end.unwrap_or(buffer.len());
    let content_length = parse_content_length(&buffer[..header_len]);
    let fixture_auth = String::from_utf8_lossy(&buffer[..header_len])
        .lines()
        .any(|line| line.eq_ignore_ascii_case("authorization: Bearer synthetic-provider-key"));
    let mut body = if header_len <= buffer.len() {
        buffer[header_len..].to_vec()
    } else {
        Vec::new()
    };
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    let prompt =
        extract_prompt_from_request_body(&body).unwrap_or_else(|| "mock prompt".to_string());
    let content = if fixture_auth {
        format!("Mock response: {prompt} (synthetic credential accepted)")
    } else {
        format!("Mock response: {prompt}")
    };
    let streaming =
        serde_json::from_slice::<Value>(&body).is_ok_and(|request| request["stream"] == true);
    let responses =
        serde_json::from_slice::<Value>(&body).is_ok_and(|request| request.get("input").is_some());
    let (media_type, payload) = if responses {
        let reasoning = serde_json::json!({"type":"reasoning","id":"reason","summary":[],"encrypted_content":"private-client-fixture"});
        let message = serde_json::json!({"type":"message","id":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":content,"annotations":[]}]});
        let response =
            serde_json::json!({"id":"response","status":"completed","output":[reasoning,message]});
        if streaming {
            let events = [
                serde_json::json!({"type":"response.output_item.added","output_index":0,"item":reasoning}),
                serde_json::json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
                serde_json::json!({"type":"response.output_item.added","output_index":1,"item":message}),
                serde_json::json!({"type":"response.output_text.delta","output_index":1,"item_id":"message","content_index":0,"delta":content}),
                serde_json::json!({"type":"response.output_item.done","output_index":1,"item":message}),
                serde_json::json!({"type":"response.completed","response":response}),
            ];
            (
                "text/event-stream",
                events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect::<String>(),
            )
        } else {
            ("application/json", response.to_string())
        }
    } else if streaming {
        let event =
            serde_json::json!({"choices":[{"delta":{"content":content},"finish_reason":"stop"}]});
        (
            "text/event-stream",
            format!("data: {event}\n\ndata: [DONE]\n\n"),
        )
    } else {
        (
            "application/json",
            serde_json::json!({"choices":[{"message":{"content":content}}]}).to_string(),
        )
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len(),
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn parse_content_length(headers: &[u8]) -> usize {
    let raw = String::from_utf8_lossy(headers);
    for line in raw.lines() {
        let mut parts = line.splitn(2, ':');
        let key = parts.next().unwrap_or_default().trim();
        if key.eq_ignore_ascii_case("content-length")
            && let Some(value) = parts.next()
            && let Ok(parsed) = value.trim().parse::<usize>()
        {
            return parsed;
        }
    }
    0
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn extract_prompt_from_request_body(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get("messages")
        .and_then(|v| v.as_array())
        .and_then(|rows| rows.last())
        .and_then(|row| row.get("content"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

#[test]
fn explicit_model_checks_and_uses_only_the_selected_provider() {
    let workspace = TempDir::new().expect("workspace");
    let user = TempDir::new().expect("isolated user settings");
    let target = start_mock_llm_server();
    configure_runtime_for_mock_llm(workspace.path(), "http://127.0.0.1:1/chat/completions");
    let settings = workspace.path().join(".codingbuddy/settings.local.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    value["llm"]["providers"]["my-gateway"] = serde_json::json!({
        "kind": "openai-compatible",
        "base_url": target.endpoint.strip_suffix("/chat/completions").unwrap(),
        "api_key_env": "CODINGBUDDY_TEST_GATEWAY_KEY",
        "models": { "chat": "example/model-high" }
    });
    value["llm"]["timeout_seconds"] = 2.into();
    value["llm"]["max_retries"] = 0.into();
    fs::write(&settings, serde_json::to_vec(&value).unwrap()).unwrap();
    let output = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"))
        .current_dir(workspace.path())
        .env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path())
        .env("HOME", user.path())
        .env("USERPROFILE", user.path())
        .env_remove("DEEPSEEK_API_KEY")
        .env("CODINGBUDDY_TEST_GATEWAY_KEY", "mock-credential")
        .args([
            "--json",
            "--model",
            "my-gateway/example/model-high",
            "ask",
            "hello",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert!(result["output"].as_str().unwrap().contains("Mock response"));
}

#[test]
fn headless_resume_rejects_a_different_endpoint_before_reading_a_prompt() {
    let workspace = TempDir::new().expect("workspace");
    let user = TempDir::new().expect("isolated user settings");
    let original = start_mock_llm_server();
    configure_runtime_for_mock_llm(workspace.path(), &original.endpoint);
    let command = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"));
        cmd.current_dir(workspace.path())
            .env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path())
            .env("HOME", user.path())
            .env("USERPROFILE", user.path())
            .env("DEEPSEEK_API_KEY", "fixture-credential");
        cmd
    };
    command()
        .args(["--json", "ask", "remember this private message"])
        .assert()
        .success();
    let replacement = start_mock_llm_server();
    configure_runtime_for_mock_llm(workspace.path(), &replacement.endpoint);
    let result = command()
        .args(["--json", "--continue"])
        .write_stdin("hello\n")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&result).contains("different provider or endpoint"));
}

struct P0Server {
    child: std::process::Child,
    address: String,
    token: String,
}

fn p0_binary() -> PathBuf {
    std::env::var_os("CODINGBUDDY_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| assert_cmd::cargo::cargo_bin!("codingbuddy").to_path_buf())
}

impl Drop for P0Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl P0Server {
    fn launch(workspace: &Path, home: &Path) -> Self {
        Self::launch_exposed(workspace, home, None)
    }

    fn launch_exposed(workspace: &Path, home: &Path, exposed: Option<&str>) -> Self {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        let mut command = StdCommand::new(p0_binary());
        command
            .current_dir(workspace)
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env_remove("CODINGBUDDY_TRUST_WORKSPACE")
            .env("P0_FIXTURE_KEY", "synthetic-provider-key")
            .args(["--json", "serve", "--transport", "http", "--port", "0"])
            .stdout(Stdio::piped());
        if let Some(id) = exposed {
            command.args(["--expose-session", id]);
        }
        let mut child = command.spawn().expect("start RPC server");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("startup stdout"))
            .read_line(&mut line)
            .expect("read startup response");
        let startup: Value = serde_json::from_str(&line).expect("startup JSON");
        Self {
            child,
            address: startup["address"].as_str().expect("address").to_owned(),
            token: startup["token"].as_str().expect("token").to_owned(),
        }
    }

    fn request(&self, headers: &str, method: &str, params: Value) -> (u16, Value) {
        let body =
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
        let mut stream = TcpStream::connect(&self.address).expect("connect to RPC server");
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        write!(stream, "POST /rpc HTTP/1.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").expect("HTTP response");
        assert!(
            !head
                .to_ascii_lowercase()
                .contains("access-control-allow-origin")
        );
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    fn authorized(&self) -> String {
        format!(
            "Host: {}\r\nOrigin: http://{}\r\nAuthorization: Bearer {}\r\n",
            self.address, self.address, self.token
        )
    }
}

fn p0_provider_settings(home: &Path, mock: &MockLlmServer, extra: Value) {
    fs::create_dir_all(home.join(".codingbuddy")).unwrap();
    let mut settings = serde_json::json!({"llm": {
        "provider":"deepseek", "endpoint":mock.endpoint,
        "api_key_env":"P0_FIXTURE_KEY", "stream":false, "max_retries":0, "timeout_seconds":3
    }});
    if let Some(extra) = extra.as_object() {
        settings.as_object_mut().unwrap().extend(extra.clone());
    }
    fs::write(
        home.join(".codingbuddy/settings.json"),
        settings.to_string(),
    )
    .unwrap();
}

#[test]
fn delivery_one_http_rpc_reauthorizes_replay_after_restart() {
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let mock = start_mock_llm_server();
    p0_provider_settings(home.path(), &mock, Value::Null);
    let server = P0Server::launch(workspace.path(), home.path());
    let auth = server.authorized();
    let (_, opened) = server.request(&auth, "session/open", Value::Null);
    let session = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let request = serde_json::json!({"session_id":session,"prompt":"hello","request_id":uuid::Uuid::now_v7()});
    let (_, first) = server.request(&auth, "prompt/start", request.clone());
    let id = first["result"]["prompt_id"].as_str().unwrap().to_owned();
    let (_, second) = server.request(&auth, "prompt/start", request);
    assert_eq!(second["result"]["prompt_id"], id);
    let limit = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let (_, page) = server.request(&auth, "prompt/events", serde_json::json!({"prompt_id":id}));
        if page["result"]["done"] == true {
            assert_eq!(page["result"]["status"], "completed", "{page}");
            break;
        }
        assert!(std::time::Instant::now() < limit, "{page}");
        std::thread::sleep(Duration::from_millis(30));
    }
    let old = server.token.clone();
    drop(server);
    let restarted = P0Server::launch_exposed(workspace.path(), home.path(), Some(&session));
    let auth = restarted.authorized();
    assert_eq!(
        restarted
            .request(
                &auth.replace(&restarted.token, &old),
                "session/open",
                Value::Null
            )
            .0,
        401
    );
    let (_, resumed) = restarted.request(&auth, "session/open", Value::Null);
    assert_eq!(resumed["result"]["session_id"], session);
    assert_eq!(resumed["result"]["latest_prompt"]["prompt_id"], id);
    assert_eq!(mock.requests.load(Ordering::Relaxed), 1);
}

#[test]
fn p0_http_rpc_auth_and_workspace_scope() {
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let mock = start_mock_llm_server();
    p0_provider_settings(home.path(), &mock, Value::Null);
    let server = P0Server::launch(workspace.path(), home.path());
    let auth = server.authorized();
    let empty = serde_json::json!({});
    for (headers, status) in [
        (format!("Host: {}\r\n", server.address), 401),
        (auth.replace(&server.token, "stale-token"), 401),
        (
            format!("{auth}Authorization: Bearer {}\r\n", server.token),
            401,
        ),
        (format!("{auth}Host: {}\r\n", server.address), 403),
        (format!("{auth}Origin: http://{}\r\n", server.address), 403),
        (
            auth.replace(
                &format!("Origin: http://{}", server.address),
                "Origin: https://foreign.invalid",
            ),
            403,
        ),
        (
            auth.replacen(
                &format!("Host: {}", server.address),
                "Host: spoof.invalid",
                1,
            ),
            403,
        ),
    ] {
        assert_eq!(
            server.request(&headers, "session/open", empty.clone()).0,
            status
        );
    }

    for (method, params) in [
        ("config/get", serde_json::json!({"key":"llm.api_key"})),
        (
            "config/set",
            serde_json::json!({"key":"llm.api_key","value":"overwrite"}),
        ),
        (
            "session/open",
            serde_json::json!({"workspace_root":home.path()}),
        ),
        (
            "prompt/stream_next",
            serde_json::json!({"prompt_id":uuid::Uuid::now_v7()}),
        ),
    ] {
        let (_, result) = server.request(&auth, method, params);
        assert!(result["error"].is_object(), "{result}");
    }
    assert!(
        !workspace
            .path()
            .join(".codingbuddy/settings.local.json")
            .exists()
    );

    let (_, opened) = server.request(&auth, "session/open", empty);
    let session = opened["result"]["session_id"].clone();
    assert!(session.is_string(), "{opened}");
    let (_, rejected) = server.request(
        &auth,
        "prompt/execute",
        serde_json::json!({"session_id":session,"prompt":"hello","additional_dirs":[home.path()]}),
    );
    assert!(rejected["error"].is_object(), "{rejected}");
    assert_eq!(mock.requests.load(Ordering::Relaxed), 0);
    let (_, executed) = server.request(
        &auth,
        "prompt/execute",
        serde_json::json!({"session_id":session,"prompt":"hello"}),
    );
    assert_eq!(executed["result"]["status"], "completed", "{executed}");
    assert!(
        executed["result"]["output"]
            .as_str()
            .is_some_and(|output| output.contains("synthetic credential accepted"))
    );
    let prompt = &executed["result"]["prompt_id"];
    let (_, stream) = server.request(
        &auth,
        "prompt/stream_next",
        serde_json::json!({"prompt_id":prompt}),
    );
    assert!(stream["result"]["chunks"].is_array(), "{stream}");

    let restarted = P0Server::launch(workspace.path(), home.path());
    let stale = restarted
        .authorized()
        .replace(&restarted.token, &server.token);
    assert_eq!(
        restarted.request(&stale, "session/open", Value::Null).0,
        401
    );
    let (_, rejected) = restarted.request(
        &restarted.authorized(),
        "prompt/execute",
        serde_json::json!({"session_id":session,"prompt":"hello"}),
    );
    assert!(rejected["error"].is_object(), "{rejected}");

    Command::new(p0_binary())
        .current_dir(workspace.path())
        .args(["serve", "--transport", "http", "--host", "0.0.0.0"])
        .assert()
        .failure();
}

#[test]
fn p0_workspace_hooks_and_process_env_grants() {
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let mock = start_mock_llm_server();
    let unwanted = TcpListener::bind("127.0.0.1:0").unwrap();
    unwanted.set_nonblocking(true).unwrap();
    let command = if cfg!(windows) {
        r#"if defined P0_PRIVATE_VALUE (echo present>hook-value) else (echo absent>hook-value)"#
    } else {
        r#"if [ -n "$P0_PRIVATE_VALUE" ]; then printf present >hook-value; else printf absent >hook-value; fi"#
    };
    fs::create_dir_all(workspace.path().join(".codingbuddy")).unwrap();
    fs::write(workspace.path().join(".codingbuddy/settings.json"), serde_json::json!({
        "llm": {"endpoint":format!("http://{}/chat/completions", unwanted.local_addr().unwrap())},
        "hooks":{"SessionStart":[{"hooks":[{"type":"command","command":command}]}]}
    }).to_string()).unwrap();
    let mcp_marker = workspace.path().join("mcp-marker");
    let (program, args) = if cfg!(windows) {
        (
            "cmd",
            vec![
                "/D".to_owned(),
                "/C".to_owned(),
                "copy NUL mcp-marker".to_owned(),
            ],
        )
    } else {
        (
            "sh",
            vec!["-c".to_owned(), format!("touch '{}'", mcp_marker.display())],
        )
    };
    fs::write(
        workspace.path().join(".mcp.json"),
        serde_json::json!({
            "servers":[{"id":"p0-project","command":program,"args":args,"enabled":true}]
        })
        .to_string(),
    )
    .unwrap();
    let marker = workspace.path().join("hook-value");
    p0_provider_settings(home.path(), &mock, Value::Null);
    let run = |trusted, managed: Option<&Path>| {
        let mut command = Command::new(p0_binary());
        command
            .current_dir(workspace.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("P0_FIXTURE_KEY", "synthetic-provider-key")
            .env("P0_PRIVATE_VALUE", "arbitrary-synthetic-secret")
            .env_remove("CODINGBUDDY_TRUST_WORKSPACE")
            .args(["-p", "--no-input", "fixture"]);
        if trusted {
            command.env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path());
        }
        if let Some(managed) = managed {
            command.env("CODINGBUDDY_MANAGED_SETTINGS_PATH", managed);
        }
        command.assert().success();
    };
    run(false, None);
    assert!(!marker.exists());
    assert!(!mcp_marker.exists());
    assert_eq!(
        unwanted.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    let path = workspace.path().join(".codingbuddy/settings.json");
    let mut project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    project.as_object_mut().unwrap().remove("llm");
    project["process_env"] = serde_json::json!({"grants":[{
        "consumer":"hook:SessionStart:0:0","source":"project","workspace":workspace.path(),
        "variables":["P0_PRIVATE_VALUE"]
    }]});
    fs::write(&path, project.to_string()).unwrap();
    let managed = home.path().join("managed.json");
    fs::write(&managed, r#"{"denied_mcp_servers":["p0-project"]}"#).unwrap();
    run(true, Some(&managed));
    assert!(!mcp_marker.exists());
    run(true, None);
    assert!(mcp_marker.exists());
    assert_eq!(fs::read_to_string(&marker).unwrap().trim(), "absent");
    fs::remove_file(workspace.path().join(".mcp.json")).unwrap();

    p0_provider_settings(
        home.path(),
        &mock,
        serde_json::json!({"process_env":project["process_env"]}),
    );
    run(true, None);
    assert_eq!(fs::read_to_string(&marker).unwrap().trim(), "present");

    fs::write(&managed, r#"{"process_env":{"grants":[]}}"#).unwrap();
    run(true, Some(&managed));
    assert_eq!(fs::read_to_string(&marker).unwrap().trim(), "absent");
    fs::remove_file(&marker).unwrap();
    fs::write(&managed, r#"{"allow_managed_hooks_only":true}"#).unwrap();
    run(true, Some(&managed));
    assert!(!marker.exists());
}

#[test]
fn delivery_zero_user_deny_survives_trusted_project_overlay() {
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    for (root, decision) in [(home.path(), "deny"), (workspace.path(), "allow")] {
        let config = root.join(".codingbuddy");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("settings.json"),
            serde_json::json!({"policy":{"permission_rules":[{"rule":"Bash(git status)","decision":decision}]}}).to_string(),
        ).unwrap();
    }
    let output = Command::new(p0_binary())
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path())
        .args([
            "--json",
            "permissions",
            "dry-run",
            "bash_run",
            "--args-json",
            r#"{"command":"git status"}"#,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).expect("permission output");
    assert_eq!(result["dry_run"]["permission"]["decision"], "deny");
    assert_eq!(result["dry_run"]["permission"]["source"], "user settings");
}

#[test]
fn delivery_two_responses_share_cli_web_and_legacy_ide_session() {
    use std::process::Stdio;
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let mock = start_mock_llm_server();
    p0_provider_settings(
        home.path(),
        &mock,
        serde_json::json!({
            "llm":{"provider":"openai-compatible","endpoint":mock.endpoint,"api_key":"synthetic-provider-key","stream":true,"max_retries":0,"timeout_seconds":10,
                "providers":{"openai-compatible":{"chat_protocol":"openai-responses","models":{"chat":"gpt-4.1-mini"}}}},
            "agent_loop":{"phase_loop_enabled":false}
        }),
    );
    let status = StdCommand::new(p0_binary())
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("CODINGBUDDY_TRUST_WORKSPACE")
        .args(["provider", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["configured"], true);
    assert_eq!(status["live_verified"], false);
    assert_eq!(mock.requests.load(Ordering::Relaxed), 0);
    let server = P0Server::launch(workspace.path(), home.path());
    let auth = server.authorized();
    let (_, opened) = server.request(&auth, "session/open", Value::Null);
    let session = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let output = StdCommand::new(p0_binary())
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("CODINGBUDDY_TRUST_WORKSPACE")
        .args([
            "--json",
            "ask",
            "hello",
            "--session-id",
            &session,
            "--tools=false",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(cli["output"].as_str().unwrap().contains("Mock response"));
    let (_, started) = server.request(&auth,"prompt/start",serde_json::json!({"session_id":session,"prompt":"continue","request_id":uuid::Uuid::now_v7()}));
    let id = &started["result"]["prompt_id"];
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut cursor = 0;
    loop {
        let (_, page) = server.request(
            &auth,
            "prompt/events",
            serde_json::json!({"prompt_id":id,"cursor":cursor}),
        );
        assert!(!page.to_string().contains("private-client-fixture"));
        cursor = page["result"]["next_cursor"].as_i64().unwrap();
        if page["result"]["done"] == true {
            assert_eq!(page["result"]["status"], "completed", "{page}");
            assert!(page["result"]["provider_error"].is_null());
            break;
        }
        assert!(Instant::now() < deadline, "{page}");
        thread::sleep(Duration::from_millis(20));
    }
    drop(server);
    let mut ide = StdCommand::new(p0_binary())
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env_remove("CODINGBUDDY_TRUST_WORKSPACE")
        .args(["serve", "--transport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = ide.stdin.take().unwrap();
    writeln!(input,"{}",serde_json::json!({"jsonrpc":"2.0","id":1,"method":"prompt/execute","params":{"session_id":session,"prompt":"legacy continuation","tools":false}})).unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"shutdown"})
    )
    .unwrap();
    drop(input);
    let mut line = String::new();
    ide.stdout
        .take()
        .unwrap()
        .read_to_string(&mut line)
        .unwrap();
    let reply: Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(reply["result"]["session_id"], session);
    assert_eq!(reply["result"]["status"], "completed", "{reply}");
    assert!(!line.contains("private-client-fixture"));
    assert!(ide.wait().unwrap().success());
    assert_eq!(mock.requests.load(Ordering::Relaxed), 3);
    let store = codingbuddy_store::Store::new(workspace.path()).unwrap();
    let events = store
        .session_events(uuid::Uuid::parse_str(&session).unwrap())
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.kind, codingbuddy_core::EventKind::ModelStep { .. }))
            .count(),
        3
    );
}

#[test]
fn delivery_four_provider_check_records_connectivity_without_certification() {
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let mock = start_mock_llm_server();
    configure_runtime_for_mock_llm(workspace.path(), &mock.endpoint);
    let run = |action: &str| -> Value {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("codingbuddy"))
            .current_dir(workspace.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("CODINGBUDDY_TRUST_WORKSPACE", workspace.path())
            .env("DEEPSEEK_API_KEY", "synthetic-provider-key")
            .args(["--json", "provider", action])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).expect("provider status JSON")
    };
    assert!(run("status")["connectivity"].is_null());
    assert_eq!(mock.requests.load(Ordering::Relaxed), 0);
    let checked = run("check");
    assert_eq!(checked["connectivity"]["confirmed"], true);
    assert_eq!(checked["live_verified"], false);
    assert_eq!(run("status")["connectivity"], checked["connectivity"]);
    assert_eq!(mock.requests.load(Ordering::Relaxed), 1);
}
