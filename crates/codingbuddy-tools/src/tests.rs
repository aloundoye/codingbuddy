use super::*;
use codingbuddy_core::{
    AppConfig, ApprovedToolCall, Session, SessionBudgets, SessionState, ToolCall, ToolHost,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn temp_host() -> (PathBuf, LocalToolHost) {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-tools-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    let host = LocalToolHost::new(&workspace, PolicyEngine::default()).expect("tool host");
    (workspace, host)
}

#[cfg(not(target_os = "windows"))]
#[test]
fn hooks_receive_phase_and_tool_context() {
    let workspace =
        std::env::temp_dir().join(format!("codingbuddy-tools-hook-test-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");

    let mut cfg = AppConfig {
        workspace_trust: Some(true),
        ..Default::default()
    };
    cfg.plugins.enable_hooks = true;

    let plugin_src = workspace.join("plugin-src");
    fs::create_dir_all(plugin_src.join(".codingbuddy-plugin")).expect("plugin dir");
    fs::create_dir_all(plugin_src.join("hooks")).expect("hooks dir");
    fs::write(
        plugin_src.join(".codingbuddy-plugin/plugin.json"),
        r#"{"id":"hookdemo","name":"Hook Demo","version":"0.1.0"}"#,
    )
    .expect("manifest");
    fs::write(
            plugin_src.join("hooks/pretooluse.sh"),
            "#!/bin/sh\nprintf \"%s|%s\" \"$CODINGBUDDY_HOOK_PHASE\" \"$CODINGBUDDY_TOOL_NAME\" > \"$CODINGBUDDY_WORKSPACE/hook.out\"\n",
        )
        .expect("hook script");

    let manager = PluginManager::new(&workspace).expect("plugin manager");
    manager.install(&plugin_src).expect("install plugin");

    let host = LocalToolHost::with_config(
        &workspace,
        PolicyEngine::default(),
        Arc::new(PlatformShellRunner),
        &cfg,
    )
    .expect("trusted tool host");
    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.list".to_string(),
            args: json!({"dir":"."}),
            requires_approval: false,
        },
    });
    assert!(result.success);

    let hook_out = fs::read_to_string(workspace.join("hook.out")).expect("hook output");
    assert!(hook_out.contains("pretooluse|fs.list"));
}

#[test]
fn plugin_tool_definitions_include_installed_commands() {
    let workspace = std::env::temp_dir().join(format!(
        "codingbuddy-tools-plugin-defs-test-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&workspace).expect("workspace");

    let plugin_src = workspace.join("plugin-src");
    fs::create_dir_all(plugin_src.join(".codingbuddy-plugin")).expect("plugin dir");
    fs::create_dir_all(plugin_src.join("commands/security")).expect("commands dir");
    fs::write(
        plugin_src.join(".codingbuddy-plugin/plugin.json"),
        r#"{"id":"phase-c-plugin","name":"Phase C Plugin","version":"0.1.0"}"#,
    )
    .expect("manifest");
    fs::write(plugin_src.join("commands/security/review.md"), "# review").expect("command");

    let manager = PluginManager::new(&workspace).expect("plugin manager");
    manager.install(&plugin_src).expect("install plugin");

    let defs = plugin_tool_definitions(&workspace);
    assert!(
        defs.iter()
            .any(|def| def.function.name == "plugin__phase_c_plugin__security_review"),
        "expected nested plugin command in generated tool definitions"
    );
}

#[test]
fn plugin_tool_renders_prompt() {
    let workspace = std::env::temp_dir().join(format!(
        "codingbuddy-tools-plugin-runtime-test-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&workspace).expect("workspace");

    let plugin_src = workspace.join("plugin-src");
    fs::create_dir_all(plugin_src.join(".codingbuddy-plugin")).expect("plugin dir");
    fs::create_dir_all(plugin_src.join("commands")).expect("commands dir");
    fs::write(
        plugin_src.join(".codingbuddy-plugin/plugin.json"),
        r#"{"id":"phase-c-plugin","name":"Phase C Plugin","version":"0.1.0"}"#,
    )
    .expect("manifest");
    fs::write(
        plugin_src.join("commands/review.md"),
        "Plugin {{plugin_id}} / {{command_name}}\nInput={{input}}",
    )
    .expect("command");

    let manager = PluginManager::new(&workspace).expect("plugin manager");
    manager.install(&plugin_src).expect("install plugin");

    let mut cfg = AppConfig {
        workspace_trust: Some(false),
        ..Default::default()
    };
    let call = ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "plugin__phase_c_plugin__review".to_string(),
            args: json!({"arguments":"focus auth module"}),
            requires_approval: false,
        },
    };
    let untrusted = LocalToolHost::with_config(
        &workspace,
        PolicyEngine::default(),
        Arc::new(PlatformShellRunner),
        &cfg,
    )
    .expect("untrusted tool host");
    assert!(!untrusted.execute(call.clone()).success);
    cfg.workspace_trust = Some(true);
    let host = LocalToolHost::with_config(
        &workspace,
        PolicyEngine::default(),
        Arc::new(PlatformShellRunner),
        &cfg,
    )
    .expect("trusted tool host");
    let result = host.execute(call);
    assert!(result.success, "plugin tool execution should succeed");
    assert_eq!(result.output["plugin_id"], "phase-c-plugin");
    assert_eq!(result.output["command_name"], "review");
    assert!(
        result.output["prompt"]
            .as_str()
            .is_some_and(|p| p.contains("focus auth module")),
        "rendered prompt should include provided input"
    );
}

#[test]
fn fs_read_supports_line_ranges_and_mime_metadata() {
    let (workspace, host) = temp_host();
    fs::write(workspace.join("note.txt"), "a\nb\nc\n").expect("seed");

    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.read".to_string(),
            args: json!({"path":"note.txt","start_line":2,"end_line":3}),
            requires_approval: false,
        },
    });
    assert!(result.success);
    assert_eq!(result.output["mime"], "text/plain");
    assert_eq!(result.output["binary"], false);
    let lines = result.output["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["line"], 2);
    assert_eq!(lines[0]["text"], "b");
}

#[test]
fn fs_glob_grep_and_edit_work() {
    let (workspace, host) = temp_host();
    fs::create_dir_all(workspace.join("src")).expect("src");
    fs::write(workspace.join("src/main.rs"), "fn old_name() {}\n").expect("seed");
    fs::write(workspace.join("src/lib.rs"), "pub fn helper() {}\n").expect("seed");

    let globbed = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.glob".to_string(),
            args: json!({"pattern":"src/*.rs"}),
            requires_approval: false,
        },
    });
    assert!(globbed.success);
    assert!(
        globbed.output["matches"]
            .as_array()
            .is_some_and(|items| items.len() >= 2)
    );

    let grepped = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.grep".to_string(),
            args: json!({"pattern":"old_name","glob":"src/*.rs"}),
            requires_approval: false,
        },
    });
    assert!(grepped.success);
    assert_eq!(
        grepped.output["matches"].as_array().expect("matches").len(),
        1
    );

    let edited = host.execute(ApprovedToolCall {
            invocation_id: Uuid::now_v7(),
            call: ToolCall {
                name: "fs.edit".to_string(),
                args: json!({"path":"src/main.rs","search":"old_name","replace":"new_name","all":false}),
                requires_approval: false,
            },
        });
    assert!(edited.success);
    assert_eq!(edited.output["edited"], true);
    let content = fs::read_to_string(workspace.join("src/main.rs")).expect("updated");
    assert!(content.contains("new_name"));
}

#[test]
fn fs_glob_and_grep_skip_denied_secret_paths() {
    let (workspace, host) = temp_host();
    fs::create_dir_all(workspace.join("src")).expect("src");
    fs::write(workspace.join(".env"), "TOKEN=secret\n").expect("secret");
    fs::write(
        workspace.join("src/main.rs"),
        "const TOKEN: &str = \"public\";\n",
    )
    .expect("seed");

    let globbed = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.glob".to_string(),
            args: json!({"pattern":"**/*", "respectGitignore": false}),
            requires_approval: false,
        },
    });
    assert!(globbed.success);
    let glob_paths: Vec<_> = globbed.output["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(!glob_paths.contains(&".env"));
    assert!(glob_paths.contains(&"src/main.rs"));

    let grepped = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.grep".to_string(),
            args: json!({"pattern":"TOKEN", "respectGitignore": false}),
            requires_approval: false,
        },
    });
    assert!(grepped.success);
    let grep_paths: Vec<_> = grepped.output["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|item| item["path"].as_str())
        .collect();
    assert!(!grep_paths.contains(&".env"));
    assert!(grep_paths.contains(&"src/main.rs"));
}

#[test]
fn fs_edit_includes_unified_diff_in_result() {
    let (workspace, host) = temp_host();
    fs::write(workspace.join("demo.rs"), "fn old() {}\n").expect("seed");

    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.edit".to_string(),
            args: json!({"path":"demo.rs","search":"old","replace":"new","all":false}),
            requires_approval: false,
        },
    });
    assert!(result.success);
    assert_eq!(result.output["edited"], true);
    let diff = result.output["diff"].as_str().expect("diff field");
    assert!(diff.contains("--- a/demo.rs"));
    assert!(diff.contains("+++ b/demo.rs"));
    assert!(diff.contains("-fn old() {}"));
    assert!(diff.contains("+fn new() {}"));
}

#[test]
fn fs_glob_respects_gitignore_rules() {
    let (workspace, host) = temp_host();
    fs::create_dir_all(workspace.join("ignored")).expect("ignored dir");
    fs::create_dir_all(workspace.join("src")).expect("src");
    fs::write(workspace.join(".gitignore"), "ignored/\n").expect("gitignore");
    fs::write(workspace.join("ignored/secret.txt"), "secret\n").expect("secret");
    fs::write(workspace.join("src/main.rs"), "fn main() {}\n").expect("main");

    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.glob".to_string(),
            args: json!({"pattern":"**/*","respectGitignore":true}),
            requires_approval: false,
        },
    });
    assert!(result.success);
    let paths = result
        .output
        .get("matches")
        .and_then(|items| items.as_array())
        .expect("matches")
        .iter()
        .filter_map(|item| item.get("path").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(paths.iter().any(|path| path.ends_with("src/main.rs")));
    assert!(
        !paths
            .iter()
            .any(|path| path.ends_with("ignored/secret.txt"))
    );
}

#[test]
fn fs_read_emits_visual_artifact_event_when_enabled() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-tools-vis-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");

    let mut cfg = AppConfig {
        workspace_trust: Some(true),
        ..Default::default()
    };
    cfg.experiments.visual_verification = true;

    let store = Store::new(&workspace).expect("store");
    store
        .save_session(&Session {
            session_id: Uuid::now_v7(),
            workspace_root: workspace.to_string_lossy().to_string(),
            baseline_commit: None,
            status: SessionState::Idle,
            budgets: SessionBudgets {
                per_turn_seconds: 30,
                max_think_tokens: 1024,
            },
            active_plan_id: None,
        })
        .expect("session");

    fs::write(workspace.join("image.png"), [0x89, b'P', b'N', b'G', 0, 1]).expect("image");
    let host = LocalToolHost::with_config(
        &workspace,
        PolicyEngine::default(),
        Arc::new(PlatformShellRunner),
        &cfg,
    )
    .expect("trusted tool host");
    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "fs.read".to_string(),
            args: json!({"path":"image.png"}),
            requires_approval: false,
        },
    });
    assert!(result.success);
    assert_eq!(result.output["binary"], true);

    let session = store.load_latest_session().unwrap().unwrap();
    let events = store.session_events(session.session_id).unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::VisualArtifactCaptured { .. }))
    );
}

#[test]
fn multi_edit_modifies_multiple_files() {
    let (workspace, host) = temp_host();
    fs::write(workspace.join("a.txt"), "hello world\n").expect("seed a");
    fs::write(workspace.join("b.txt"), "foo bar\n").expect("seed b");

    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "multi_edit".to_string(),
            args: json!({
                "files": [
                    {"path": "a.txt", "edits": [{"search": "hello", "replace": "hi"}]},
                    {"path": "b.txt", "edits": [{"search": "foo", "replace": "baz"}]}
                ]
            }),
            requires_approval: false,
        },
    });
    assert!(result.success);
    assert_eq!(result.output["total_files"], 2);
    assert!(result.output["total_replacements"].as_u64().unwrap() >= 2);
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).expect("a"),
        "hi world\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("b.txt")).expect("b"),
        "baz bar\n"
    );
}

#[test]
fn multi_edit_returns_diffs_and_shas() {
    let (workspace, host) = temp_host();
    fs::write(workspace.join("c.txt"), "old value\n").expect("seed c");

    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "multi_edit".to_string(),
            args: json!({
                "files": [
                    {"path": "c.txt", "edits": [{"search": "old", "replace": "new"}]}
                ]
            }),
            requires_approval: false,
        },
    });
    assert!(result.success);
    let results = result.output["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    let entry = &results[0];
    assert_eq!(entry["edited"], true);
    assert!(entry["diff"].as_str().unwrap().contains("--- a/c.txt"));
    assert!(entry["before_sha256"].as_str().is_some());
    assert!(entry["after_sha256"].as_str().is_some());
}

#[test]
fn multi_edit_blocked_in_review_mode() {
    let (workspace, mut host) = temp_host();
    host.set_review_mode(true);
    fs::write(workspace.join("d.txt"), "data\n").expect("seed d");

    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "multi_edit".to_string(),
            args: json!({
                "files": [
                    {"path": "d.txt", "edits": [{"search": "data", "replace": "new"}]}
                ]
            }),
            requires_approval: false,
        },
    });
    assert!(!result.success);
    assert!(
        result.output["error"]
            .as_str()
            .unwrap_or_default()
            .contains("review mode")
    );
}

#[test]
fn multi_edit_skips_unmodified_files() {
    let (workspace, host) = temp_host();
    fs::write(workspace.join("e.txt"), "keep me\n").expect("seed e");
    fs::write(workspace.join("f.txt"), "change me\n").expect("seed f");

    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "multi_edit".to_string(),
            args: json!({
                "files": [
                    {"path": "e.txt", "edits": [{"search": "missing", "replace": "gone"}]},
                    {"path": "f.txt", "edits": [{"search": "change", "replace": "changed"}]}
                ]
            }),
            requires_approval: false,
        },
    });
    assert!(result.success);
    let results = result.output["results"].as_array().expect("results");
    // e.txt had an error (search not found), f.txt was edited
    let f_entry = results
        .iter()
        .find(|r| r["path"] == "f.txt")
        .expect("f.txt");
    assert_eq!(f_entry["edited"], true);
    assert_eq!(
        fs::read_to_string(workspace.join("e.txt")).expect("e"),
        "keep me\n"
    );
}

#[test]
fn diagnostics_check_detects_rust_project() {
    let workspace = std::env::temp_dir().join(format!("codingbuddy-tools-diag-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(workspace.join("Cargo.toml"), "[package]\nname = \"test\"\n").expect("cargo");

    let (cmd, source) = detect_diagnostics_command(&workspace, None).expect("detect");
    assert_eq!(source, "rustc");
    assert!(cmd.contains("cargo check"));
}

#[test]
fn diagnostics_check_is_read_only() {
    let policy = PolicyEngine::new(codingbuddy_policy::PolicyConfig {
        permission_mode: codingbuddy_policy::PermissionMode::Plan,
        ..codingbuddy_policy::PolicyConfig::default()
    });
    let call = ToolCall {
        name: "diagnostics.check".to_string(),
        args: json!({}),
        requires_approval: false,
    };
    assert!(!policy.requires_approval(&call));
}

#[test]
fn parse_cargo_check_json_extracts_errors() {
    let output = r#"{"reason":"compiler-message","message":{"level":"error","message":"unused variable","spans":[{"file_name":"src/main.rs","line_start":10,"column_start":5}]}}"#;
    let diagnostics = parse_cargo_check_json(output);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["level"], "error");
    assert_eq!(diagnostics[0]["file"], "src/main.rs");
    assert_eq!(diagnostics[0]["line"], 10);
}

#[test]
fn parse_tsc_output_extracts_errors() {
    let output = "src/app.ts(42,13): error TS2304: Cannot find name 'foo'.";
    let diagnostics = parse_tsc_output(output);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["level"], "error");
    assert_eq!(diagnostics[0]["file"], "src/app.ts");
    assert_eq!(diagnostics[0]["line"], 42);
    assert_eq!(diagnostics[0]["column"], 13);
    assert_eq!(diagnostics[0]["code"], "TS2304");
}

#[test]
fn tool_definitions_include_all_tools() {
    let defs = tool_definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();

    // Core tools
    for expected in [
        "fs_read",
        "fs_write",
        "fs_edit",
        "fs_list",
        "fs_glob",
        "fs_grep",
        "bash_run",
        "multi_edit",
        "git_status",
        "git_diff",
        "git_show",
        "web_fetch",
        "web_search",
        "notebook_read",
        "notebook_edit",
        "index_query",
        "patch_stage",
        "patch_apply",
        "patch_direct",
        "diagnostics_check",
        "batch",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool definition: {expected}"
        );
    }
    // Agent-level tools
    for expected in [
        "user_question",
        "task_create",
        "task_update",
        "todo_read",
        "todo_write",
        "task_output",
        "task_stop",
        "spawn_task",
        "enter_plan_mode",
        "exit_plan_mode",
        "skill",
        "extended_thinking",
    ] {
        assert!(
            names.contains(&expected),
            "missing agent tool definition: {expected}"
        );
    }
}

#[test]
fn map_tool_name_covers_all_definitions() {
    let defs = tool_definitions();
    for def in &defs {
        let fn_name = &def.function.name;
        let mapped = map_tool_name(fn_name);
        // Every underscored name should map to something
        assert!(
            !mapped.is_empty(),
            "map_tool_name returned empty for {fn_name}"
        );
    }
    // Agent-level tool mappings
    assert_eq!(map_tool_name("user_question"), "user_question");
    assert_eq!(map_tool_name("task_create"), "task_create");
    assert_eq!(map_tool_name("task_update"), "task_update");
    assert_eq!(map_tool_name("todo_read"), "todo_read");
    assert_eq!(map_tool_name("todo_write"), "todo_write");
    assert_eq!(map_tool_name("task_output"), "task_output");
    assert_eq!(map_tool_name("task_stop"), "task_stop");
    assert_eq!(map_tool_name("spawn_task"), "spawn_task");
    assert_eq!(map_tool_name("extended_thinking"), "extended_thinking");
}

#[test]
fn agent_level_tools_constant_matches_definitions() {
    let defs = tool_definitions();
    let def_names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    for tool_name in AGENT_LEVEL_TOOLS.iter() {
        assert!(
            def_names.contains(tool_name),
            "AGENT_LEVEL_TOOLS contains '{tool_name}' but no definition exists"
        );
    }
}

#[test]
fn filter_tool_definitions_allowed() {
    let defs = tool_definitions();
    let allowed = vec!["fs_read".to_string(), "fs_write".to_string()];
    let filtered = filter_tool_definitions(defs, Some(&allowed), None);
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().any(|t| t.function.name == "fs_read"));
    assert!(filtered.iter().any(|t| t.function.name == "fs_write"));
}

#[test]
fn filter_tool_definitions_disallowed() {
    let defs = tool_definitions();
    let original_count = defs.len();
    let disallowed = vec!["bash_run".to_string()];
    let filtered = filter_tool_definitions(defs, None, Some(&disallowed));
    assert_eq!(filtered.len(), original_count - 1);
    assert!(!filtered.iter().any(|t| t.function.name == "bash_run"));
}

#[test]
fn filter_tool_definitions_no_filters() {
    let defs = tool_definitions();
    let original_count = defs.len();
    let filtered = filter_tool_definitions(defs, None, None);
    assert_eq!(filtered.len(), original_count);
}

// ── Parameter mapping tests (Phase 16.2) ────────────────────────────

#[test]
fn tool_definitions_required_fields_exist_in_properties() {
    let defs = tool_definitions();
    for def in &defs {
        let params = &def.function.parameters;
        let required = params
            .get("required")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let properties = params.get("properties").and_then(|v| v.as_object());
        for req in &required {
            let field_name = req.as_str().unwrap_or("(non-string)");
            if let Some(props) = properties {
                assert!(
                    props.contains_key(field_name),
                    "tool '{}': required field '{}' is missing from properties",
                    def.function.name,
                    field_name
                );
            }
        }
    }
}

#[test]
fn plan_mode_tools_all_exist_in_definitions() {
    let defs = tool_definitions();
    let def_names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    for tool_name in PLAN_MODE_TOOLS.iter() {
        assert!(
            def_names.contains(tool_name),
            "PLAN_MODE_TOOLS contains '{}' which is not in tool_definitions()",
            tool_name
        );
    }
}

#[test]
fn agent_level_tools_excluded_from_plan_mode() {
    for tool_name in AGENT_LEVEL_TOOLS.iter() {
        // Agent-level tools that ARE in plan mode are: user_question, task_*, todo_*,
        // task_output, spawn_task, exit_plan_mode, extended_thinking
        // The write-oriented agent-level tools should not be in plan mode:
        // enter_plan_mode makes no sense inside plan mode, kill_shell is mutating, skill is execution
        let expected_in_plan = matches!(
            *tool_name,
            "user_question"
                | "task_create"
                | "task_update"
                | "todo_read"
                | "todo_write"
                | "task_get"
                | "task_list"
                | "task_output"
                | "task_stop"
                | "spawn_task"
                | "send_message"
                | "extended_thinking"
                | "exit_plan_mode"
        );
        let in_plan = PLAN_MODE_TOOLS.contains(tool_name);
        if !expected_in_plan {
            assert!(
                !in_plan,
                "agent-level tool '{}' should not be in PLAN_MODE_TOOLS",
                tool_name
            );
        }
    }
}

#[test]
fn fuzz_random_json_args_do_not_panic() {
    let (workspace, host) = temp_host();
    let fuzz_args = vec![
        serde_json::json!(null),
        serde_json::json!({}),
        serde_json::json!({"wrong_field": 42}),
        serde_json::json!({"path": 123}),
        serde_json::json!([]),
    ];
    let tool_names = ["fs.read", "fs.glob", "fs.grep"];
    for tool_name in tool_names {
        for args in &fuzz_args {
            let approved = ApprovedToolCall {
                invocation_id: Uuid::now_v7(),
                call: ToolCall {
                    name: tool_name.to_string(),
                    args: args.clone(),
                    requires_approval: false,
                },
            };
            // Should not panic — Ok or Err are both fine
            let _result = host.execute(approved);
        }
    }
    let _ = fs::remove_dir_all(&workspace);
}

// ── Batch 5: Tool description enrichment tests ──────────────────────

#[test]
fn tool_descriptions_are_sufficiently_detailed() {
    let tools = tool_definitions();
    let core_tools = [
        "fs_read",
        "fs_write",
        "fs_edit",
        "fs_list",
        "fs_glob",
        "fs_grep",
        "bash_run",
        "multi_edit",
    ];
    for name in &core_tools {
        let tool = tools
            .iter()
            .find(|t| t.function.name == *name)
            .unwrap_or_else(|| panic!("tool {name} not found"));
        assert!(
            tool.function.description.len() >= 200,
            "tool {name} description is only {} chars (expected ≥200): {}",
            tool.function.description.len(),
            &tool.function.description[..tool.function.description.len().min(80)]
        );
    }
}

#[test]
fn fs_read_mentions_read_before_edit() {
    let tools = tool_definitions();
    let fs_read = tools
        .iter()
        .find(|t| t.function.name == "fs_read")
        .expect("fs_read tool");
    let desc = &fs_read.function.description;
    assert!(
        desc.contains("fs_edit"),
        "fs_read description should cross-reference fs_edit"
    );
    assert!(
        desc.contains("MUST") && desc.contains("BEFORE"),
        "fs_read description should emphasize reading before editing"
    );
}

#[test]
fn bash_run_cross_references_dedicated_tools() {
    let tools = tool_definitions();
    let bash = tools
        .iter()
        .find(|t| t.function.name == "bash_run")
        .expect("bash_run tool");
    let desc = &bash.function.description;
    assert!(
        desc.contains("fs_read"),
        "bash_run should cross-reference fs_read"
    );
    assert!(
        desc.contains("fs_grep"),
        "bash_run should cross-reference fs_grep"
    );
    assert!(
        desc.contains("fs_glob"),
        "bash_run should cross-reference fs_glob"
    );
    assert!(
        desc.contains("fs_edit"),
        "bash_run should cross-reference fs_edit"
    );
    assert!(
        desc.contains("DO NOT"),
        "bash_run should say DO NOT use for dedicated tool operations"
    );
}

// ── P6-06: Container sandbox detection tests ────────────────────────

// ── T2.1: JSON schema validation tests ──────────────────────────────

#[test]
fn schema_validation_passes_valid_args() {
    let tools = vec![ToolDefinition {
        tool_type: "function".to_string(),
        function: codingbuddy_core::FunctionDefinition {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            strict: None,
            parameters: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" }
                }
            }),
        },
    }];
    let args = json!({"path": "src/main.rs"});
    assert!(validate_tool_args_schema("test_tool", &args, &tools).is_ok());
}

#[test]
fn schema_validation_rejects_missing_required() {
    let tools = vec![ToolDefinition {
        tool_type: "function".to_string(),
        function: codingbuddy_core::FunctionDefinition {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            strict: None,
            parameters: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" }
                }
            }),
        },
    }];
    let args = json!({});
    let result = validate_tool_args_schema("test_tool", &args, &tools);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("path"));
}

#[test]
fn schema_validation_rejects_wrong_type() {
    let tools = vec![ToolDefinition {
        tool_type: "function".to_string(),
        function: codingbuddy_core::FunctionDefinition {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            strict: None,
            parameters: json!({
                "type": "object",
                "properties": {
                    "count": { "type": "integer" }
                }
            }),
        },
    }];
    let args = json!({"count": "not a number"});
    let result = validate_tool_args_schema("test_tool", &args, &tools);
    assert!(result.is_err());
}

#[test]
fn schema_validation_skips_unknown_tools() {
    let tools = vec![];
    // Unknown tool should not fail validation
    let args = json!({"anything": "goes"});
    assert!(validate_tool_args_schema("mcp__unknown__tool", &args, &tools).is_ok());
}

#[test]
fn mcp_tool_routing_splits_name_correctly() {
    let (workspace, mut host) = temp_host();
    let captured_server = Arc::new(Mutex::new(String::new()));
    let captured_tool = Arc::new(Mutex::new(String::new()));
    let captured_args = Arc::new(Mutex::new(json!(null)));
    let cs = captured_server.clone();
    let ct = captured_tool.clone();
    let ca = captured_args.clone();

    host.set_mcp_executor(Arc::new(move |server_id, tool_name, args| {
        *cs.lock().unwrap() = server_id.to_string();
        *ct.lock().unwrap() = tool_name.to_string();
        *ca.lock().unwrap() = args.clone();
        Ok(json!({"result": "ok"}))
    }));

    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "mcp__github__search".to_string(),
            args: json!({"arguments": {"query": "test"}}),
            requires_approval: false,
        },
    });
    assert!(result.success, "MCP tool call should succeed");
    assert_eq!(*captured_server.lock().unwrap(), "github");
    assert_eq!(*captured_tool.lock().unwrap(), "search");
    assert_eq!(*captured_args.lock().unwrap(), json!({"query": "test"}));
    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn mcp_tool_without_executor_returns_error() {
    let (workspace, host) = temp_host();
    // No MCP executor set — should fail
    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "mcp__github__search".to_string(),
            args: json!({"arguments": {"query": "test"}}),
            requires_approval: false,
        },
    });
    assert!(!result.success, "should fail without executor");
    let err = result.output["error"].as_str().unwrap_or("");
    assert!(
        err.contains("no MCP executor"),
        "error should mention missing executor: {err}"
    );
    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn mcp_tool_invalid_name_returns_error() {
    let (workspace, mut host) = temp_host();
    host.set_mcp_executor(Arc::new(|_, _, _| Ok(json!({"ok": true}))));

    // "mcp__noseparator" has no second "__" — should fail to split
    let result = host.execute(ApprovedToolCall {
        invocation_id: Uuid::now_v7(),
        call: ToolCall {
            name: "mcp__noseparator".to_string(),
            args: json!({}),
            requires_approval: false,
        },
    });
    assert!(!result.success, "should fail with invalid MCP name");
    let err = result.output["error"].as_str().unwrap_or("");
    assert!(
        err.contains("invalid MCP tool name"),
        "error should mention invalid name: {err}"
    );
    let _ = fs::remove_dir_all(&workspace);
}

// ── Fuzzy edit matching tests ────────────────────────────────────────

#[test]
fn fuzzy_line_trimmed_matches_with_different_indentation() {
    let content = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
    let search = "  let x = 1;\n  let y = 2;";
    let result = fuzzy_line_trimmed(content, search);
    assert!(result.is_some(), "should match with different indentation");
    let m = result.unwrap();
    assert_eq!(m.strategy, "line_trimmed");
    assert_eq!(&content[m.start..m.end], "    let x = 1;\n    let y = 2;");
}

#[test]
fn fuzzy_line_trimmed_rejects_ambiguous() {
    let content = "let x = 1;\nlet y = 2;\nlet x = 1;\nlet y = 2;\n";
    let search = "let x = 1;\nlet y = 2;";
    let result = fuzzy_line_trimmed(content, search);
    assert!(result.is_none(), "should reject ambiguous matches");
}

#[test]
fn fuzzy_block_anchor_matches() {
    let content = "fn foo() {\n    // comment\n    let x = 1;\n    return x;\n}\n";
    // Search with slightly different middle line
    let search = "fn foo() {\n    // different comment\n    let x = 1;\n    return x;\n}";
    let result = fuzzy_block_anchor(content, search);
    assert!(result.is_some(), "should match with fuzzy middle");
    assert_eq!(result.unwrap().strategy, "block_anchor");
}

#[test]
fn fuzzy_whitespace_normalized_matches() {
    let content = "let   x   =   1;";
    let search = "let x = 1;";
    let result = fuzzy_whitespace_normalized(content, search);
    assert!(result.is_some(), "should match with normalized whitespace");
    let m = result.unwrap();
    assert_eq!(m.strategy, "whitespace_normalized");
}

#[test]
fn fuzzy_indentation_flexible_matches() {
    let content = "    fn foo() {\n        let x = 1;\n    }\n";
    let search = "fn foo() {\n    let x = 1;\n}";
    let result = fuzzy_indentation_flexible(content, search);
    assert!(result.is_some(), "should match with flexible indentation");
    assert_eq!(result.unwrap().strategy, "indentation_flexible");
}

#[test]
fn fuzzy_escape_normalized_matches() {
    let content = "hello\nworld";
    let search = "hello\\nworld";
    let result = fuzzy_escape_normalized(content, search);
    assert!(result.is_some(), "should match after unescape");
    assert_eq!(result.unwrap().strategy, "escape_normalized");
}

#[test]
fn fuzzy_trimmed_boundary_matches() {
    let content = "fn foo() {\n    let x = 1;\n}\n";
    let search = "\nfn foo() {\n    let x = 1;\n}\n\n";
    let result = fuzzy_trimmed_boundary(content, search);
    assert!(result.is_some(), "should match after trimming blank lines");
    assert_eq!(result.unwrap().strategy, "trimmed_boundary");
}

#[test]
fn fuzzy_context_aware_matches_with_50pct_middle() {
    let content = "fn foo() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n}\n";
    // First and last lines match exactly, 2 of 4 middle lines match (50%)
    let search = "fn foo() {\n    let a = 1;\n    let DIFFERENT = 99;\n    let c = 3;\n    let ALSO_DIFFERENT = 99;\n}";
    let result = fuzzy_context_aware(content, search);
    assert!(result.is_some(), "should match with ≥50% middle line match");
    assert_eq!(result.unwrap().strategy, "context_aware");
}

#[test]
fn apply_single_edit_uses_fuzzy_fallback() {
    let mut content = "    fn main() {\n        println!(\"hello\");\n    }\n".to_string();
    let edit = json!({
        "search": "fn main() {\n    println!(\"hello\");\n}",
        "replace": "fn main() {\n    println!(\"world\");\n}",
        "all": false
    });
    let result = apply_single_edit(&mut content, &edit);
    assert!(result.is_ok(), "fuzzy fallback should succeed: {result:?}");
    assert!(content.contains("world"), "replacement should be applied");
}

#[test]
fn apply_single_edit_exact_match_preferred_over_fuzzy() {
    let mut content = "let x = 1;\nlet y = 2;\n".to_string();
    let edit = json!({
        "search": "let x = 1;",
        "replace": "let x = 42;",
        "all": false
    });
    let result = apply_single_edit(&mut content, &edit);
    assert!(result.is_ok());
    assert_eq!(content, "let x = 42;\nlet y = 2;\n");
}

#[cfg(any(unix, windows))]
#[test]
fn workspace_file_tools_confine_links() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    fs::create_dir_all(workspace.join("inside")).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("existing.txt"), "outside marker").unwrap();
    fs::write(workspace.join("inside/ok.txt"), "inside marker").unwrap();

    #[cfg(unix)]
    let link_dir = |target: &Path, path: &Path| std::os::unix::fs::symlink(target, path).unwrap();
    #[cfg(windows)]
    let link_dir = |target: &Path, path: &Path| {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(path)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
    };
    link_dir(&outside, &workspace.join("escape"));
    link_dir(&workspace.join("inside"), &workspace.join("internal"));

    let host = LocalToolHost::new(&workspace, PolicyEngine::default()).unwrap();
    let execute = |name: &str, args| {
        host.execute(ApprovedToolCall {
            invocation_id: Uuid::now_v7(),
            call: ToolCall {
                name: name.to_string(),
                args,
                requires_approval: false,
            },
        })
    };
    assert!(!execute("fs.read", json!({"path": "escape/existing.txt"})).success);
    assert!(
        !execute(
            "fs.write",
            json!({"path": "escape/new.txt", "content": "marker"})
        )
        .success
    );
    assert!(!outside.join("new.txt").exists());
    assert!(!execute("fs.list", json!({"dir": "escape"})).success);
    let search = execute("fs.search_rg", json!({"query": "outside marker"}));
    assert_eq!(search.output["matches"], json!([]));
    let read = execute("fs.read", json!({"path": "internal/ok.txt"}));
    assert!(read.success, "{}", read.output);
    assert!(read.output.to_string().contains("inside marker"));
    let written = execute(
        "fs.write",
        json!({"path": "internal/deep/new.txt", "content": "ok"}),
    );
    assert!(written.success, "{}", written.output);
    assert_eq!(
        fs::read_to_string(workspace.join("inside/deep/new.txt")).unwrap(),
        "ok"
    );

    let multi = execute(
        "multi_edit",
        json!({"files": [
            {"path": "inside/ok.txt", "edits": [{"search": "inside marker", "replace": "changed"}]},
            {"path": "escape/existing.txt", "edits": [{"search": "outside marker", "replace": "changed"}]}
        ]}),
    );
    assert!(!multi.success);
    assert_eq!(
        fs::read_to_string(workspace.join("inside/ok.txt")).unwrap(),
        "inside marker"
    );
    let patch = execute(
        "patch.stage",
        json!({"unified_diff":
            "diff --git a/escape/new.txt b/escape/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/escape/new.txt\n@@ -0,0 +1 @@\n+outside\n"
        }),
    );
    assert!(!patch.success);
}

#[cfg(unix)]
#[test]
fn workspace_file_tools_reject_dangling_and_blocked_aliases() {
    let workspace = tempfile::tempdir().unwrap();
    fs::create_dir(workspace.path().join(".ssh")).unwrap();
    fs::write(workspace.path().join(".ssh/id_rsa"), "blocked").unwrap();
    std::os::unix::fs::symlink(".ssh/id_rsa", workspace.path().join("alias")).unwrap();
    std::os::unix::fs::symlink("missing", workspace.path().join("dangling")).unwrap();
    let host = LocalToolHost::new(workspace.path(), PolicyEngine::default()).unwrap();
    for (name, args) in [
        ("fs.read", json!({"path": "alias"})),
        (
            "fs.write",
            json!({"path": "dangling", "content": "invalid"}),
        ),
    ] {
        let result = host.execute(ApprovedToolCall {
            invocation_id: Uuid::now_v7(),
            call: ToolCall {
                name: name.to_owned(),
                args,
                requires_approval: false,
            },
        });
        assert!(!result.success, "{}", result.output);
    }
    assert!(!workspace.path().join("missing").exists());
}

#[test]
fn delivery_zero_fresh_launch_cannot_be_changed_or_replayed() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = AppConfig {
        workspace_trust: Some(true),
        ..Default::default()
    };
    config.policy.allowlist = vec!["echo".into()];
    config.policy.sandbox.enabled = true;
    config.policy.sandbox.excluded_commands = vec!["echo fixture".into()];
    let mut policy = PolicyEngine::from_app_config(&config.policy);
    policy.set_permission_mode(codingbuddy_policy::PermissionMode::BypassPermissions);
    policy.set_persistent_bash_approvals(vec!["echo fixture".into()]);
    let host = LocalToolHost::with_config(
        workspace.path(),
        policy,
        Arc::new(PlatformShellRunner),
        &config,
    )?;
    let call = ToolCall {
        name: "bash.run".into(),
        args: json!({"cmd": "echo fixture"}),
        requires_approval: false,
    };
    let (proposal, decision) = host.propose_with_permission(call.clone());
    assert!(matches!(
        decision,
        codingbuddy_core::ToolPermission::Ask { fresh: true, .. }
    ));
    assert!(
        host.approval_details(proposal.invocation_id)
            .unwrap()
            .contains("Unsandboxed")
    );
    let mut changed = proposal.call;
    changed.args["timeout"] = json!(190);
    let request = ApprovedToolCall {
        invocation_id: proposal.invocation_id,
        call: changed,
    };
    assert!(!host.execute(request).success);
    let (proposal, _) = host.propose_with_permission(call);
    let request = ApprovedToolCall {
        invocation_id: proposal.invocation_id,
        call: proposal.call,
    };
    let result = host.execute(request.clone());
    assert!(result.success, "{:?}", result.output);
    assert_eq!(result.output["status"], 0);
    assert!(!host.execute(request).success);
    Ok(())
}

#[test]
fn delivery_zero_denies_persisted_commands_mcp_and_batch_children() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    fs::write(workspace.path().join("fixture.txt"), "permission fixture")?;
    let config = AppConfig::default();
    let mut policy = PolicyEngine::from_app_config(&config.policy);
    for rule in ["Bash(git status)", "Mcp(fixture)", "Read(fixture.txt)"] {
        policy.add_runtime_rule(codingbuddy_policy::PermissionRule {
            rule: rule.into(),
            decision: "deny".into(),
        });
    }
    policy.set_persistent_bash_approvals(vec!["git status".into()]);
    policy.set_permission_mode(codingbuddy_policy::PermissionMode::BypassPermissions);
    let mut host = LocalToolHost::with_config(
        workspace.path(),
        policy,
        Arc::new(PlatformShellRunner),
        &config,
    )?;
    host.set_mcp_executor(Arc::new(|_, _, _| panic!("denied MCP executed")));
    for (name, args) in [
        ("bash.run", json!({"cmd":"git status"})),
        ("bash.run", json!({"cmd":"git diff | git status"})),
        ("mcp__fixture__tool", json!({})),
    ] {
        let call = ToolCall {
            name: name.into(),
            args,
            requires_approval: false,
        };
        let (proposal, decision) = host.propose_with_permission(call);
        assert!(matches!(
            decision,
            codingbuddy_core::ToolPermission::Deny { .. }
        ));
        assert!(
            !host
                .execute(ApprovedToolCall {
                    invocation_id: proposal.invocation_id,
                    call: proposal.call
                })
                .success
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("fixture.txt", workspace.path().join("alias.txt"))?;
    let path = if cfg!(unix) {
        "alias.txt"
    } else {
        "fixture.txt"
    };
    let batch = host.propose(ToolCall {
        name: "batch".into(),
        args: json!({"tool_calls":[{"tool":"fs_read","parameters":{"path":path}}]}),
        requires_approval: false,
    });
    let result = host.execute(ApprovedToolCall {
        invocation_id: batch.invocation_id,
        call: batch.call,
    });
    assert_eq!(result.output["failed"], 1);
    assert!(!result.output.to_string().contains("permission fixture"));
    let nested = host.propose(ToolCall { name: "batch".into(), args: json!({"tool_calls":[
        {"tool":"batch","parameters":{"tool_calls":[{"tool":"fs_read","parameters":{"path":path}}]}}
    ]}), requires_approval: false });
    assert!(
        !host
            .execute(ApprovedToolCall {
                invocation_id: nested.invocation_id,
                call: nested.call
            })
            .success
    );
    Ok(())
}
