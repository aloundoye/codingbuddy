use super::*;
use serde_json::json;

#[test]
fn delivery_one_prompt_replay_is_deduplicated_and_launch_scoped() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let local = IdeRpcHandler::new(workspace.path())?;
    let session = local.handle_http("session/open", json!({}))?;
    let id = require_uuid(&session, "session_id")?;
    let request = json!({"session_id":id,"request_id":Uuid::now_v7(),"prompt":"test","mock_output":"fixture"});
    let first = local.handle("prompt/start", request.clone())?;
    let prompt_id = require_uuid(&first, "prompt_id")?;
    let repeat = local.handle("prompt/start", request)?;
    assert_eq!(repeat["prompt_id"], first["prompt_id"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while local.store.prompt_state(prompt_id)?.status == "running" {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "prompt did not settle"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let page = local.handle_http("prompt/events", json!({"prompt_id":prompt_id,"limit":1}))?;
    assert_eq!(page["events"][0]["event"]["text"], "fixture");
    assert_eq!(page["done"], false);
    let next = local.handle_http(
        "prompt/events",
        json!({"prompt_id":prompt_id,"cursor":page["next_cursor"]}),
    )?;
    assert_eq!(next["events"][0]["event"]["type"], "done");
    assert_eq!(next["done"], true);
    let mut restarted = IdeRpcHandler::new(workspace.path())?;
    assert!(
        restarted
            .handle_http("prompt/events", json!({"prompt_id":prompt_id}))
            .is_err()
    );
    let unrelated = restarted.handle_http("session/open", json!({"session_id":id}))?;
    assert_ne!(unrelated["session_id"], session["session_id"]);
    restarted.expose_session(id)?;
    let resumed = restarted.handle_http("session/open", json!({}))?;
    assert_eq!(resumed["session_id"], session["session_id"]);
    assert_eq!(resumed["latest_prompt"]["prompt_id"], first["prompt_id"]);
    assert_eq!(resumed["history"][1]["content"], "fixture");
    assert!(restarted.handle_http("config/set", json!({})).is_err());
    assert!(restarted.handle_http("prompt/start", json!({"session_id":id,"prompt":"test","request_id":Uuid::now_v7(),"additional_dirs":["/"]})).is_err());
    Ok(())
}

#[test]
fn jsonrpc_request_round_trip() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        method: "initialize".to_string(),
        params: json!({}),
    };
    let serialized = serde_json::to_string(&req).expect("serialize");
    let deserialized: JsonRpcRequest = serde_json::from_str(&serialized).expect("deserialize");
    assert_eq!(deserialized.method, "initialize");
    assert_eq!(deserialized.id, json!(1));
}

#[test]
fn parse_error_returns_code_32700() {
    let bad_json = "not json";
    let result = serde_json::from_str::<JsonRpcRequest>(bad_json);
    assert!(result.is_err());
    let resp = JsonRpcResponse::error(Value::Null, ERR_PARSE, "Parse error");
    assert_eq!(resp.error.as_ref().unwrap().code, -32700);
}

#[test]
fn success_response_has_no_error() {
    let resp = JsonRpcResponse::success(json!(42), json!({"ok": true}));
    assert!(resp.error.is_none());
    assert_eq!(resp.result.unwrap()["ok"], true);
}

#[test]
fn error_response_has_no_result() {
    let resp = JsonRpcResponse::error(json!(1), ERR_METHOD_NOT_FOUND, "method not found");
    assert!(resp.result.is_none());
    assert_eq!(resp.error.as_ref().unwrap().code, -32601);
}

#[test]
fn default_handler_initialize() {
    let handler = DefaultRpcHandler;
    let result = handler.handle("initialize", json!({})).expect("initialize");
    assert_eq!(result["name"], "codingbuddy-cli");
}

#[test]
fn default_handler_unknown_method() {
    let handler = DefaultRpcHandler;
    let err = handler
        .handle("nonexistent", json!({}))
        .expect_err("should fail");
    assert!(err.to_string().contains("method not found"));
}

#[test]
fn ide_handler_initialize() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();
    let result = handler.handle("initialize", json!({})).expect("initialize");
    assert_eq!(result["name"], "codingbuddy-cli");
    let caps = result["capabilities"].as_array().unwrap();
    assert!(caps.iter().any(|c| c == "session/open"));
    assert!(caps.iter().any(|c| c == "prompt/execute"));
    assert!(caps.iter().any(|c| c == "prompt/stream_next"));
    assert!(caps.iter().any(|c| c == "session/handoff_export"));
    assert!(caps.iter().any(|c| c == "session/handoff_import"));
    assert!(caps.iter().any(|c| c == "session/handoff_link_create"));
    assert!(caps.iter().any(|c| c == "session/handoff_link_consume"));
    assert!(caps.iter().any(|c| c == "session/remote_resume"));
    assert!(caps.iter().any(|c| c == "events/poll"));
    assert!(caps.iter().any(|c| c == "tool/approve"));
    assert!(caps.iter().any(|c| c == "patch/preview"));
    assert!(caps.iter().any(|c| c == "context/debug"));
}

#[test]
fn ide_handler_session_open_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    // Open a session.
    let result = handler
        .handle("session/open", json!({"workspace_root": "/tmp/project"}))
        .expect("session/open");
    let sid = result["session_id"].as_str().unwrap();
    assert!(!sid.is_empty());
    assert_eq!(result["status"], "idle");

    // List sessions.
    let list = handler
        .handle("session/list", json!({}))
        .expect("session/list");
    let sessions = list["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty());
    assert!(sessions.iter().any(|s| s["session_id"] == sid));
}

#[test]
fn ide_handler_session_fork() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    // Open a session.
    let result = handler
        .handle("session/open", json!({"workspace_root": "/tmp/test"}))
        .unwrap();
    let sid = result["session_id"].as_str().unwrap();

    // Fork it.
    let forked = handler
        .handle("session/fork", json!({"session_id": sid}))
        .expect("session/fork");
    assert_eq!(forked["forked_from"], sid);
    assert_ne!(forked["session_id"], sid);
    assert_eq!(forked["status"], "idle");
}

#[test]
fn ide_handler_session_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    let result = handler
        .handle("session/open", json!({"workspace_root": "/tmp/r"}))
        .unwrap();
    let sid = result["session_id"].as_str().unwrap();

    let resumed = handler
        .handle("session/resume", json!({"session_id": sid}))
        .expect("session/resume");
    assert_eq!(resumed["session_id"], sid);
    assert_eq!(resumed["turn_count"], 0);
}

#[test]
fn ide_handler_prompt_execute() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    let result = handler
        .handle("session/open", json!({"workspace_root": "/tmp/p"}))
        .unwrap();
    let sid = result["session_id"].as_str().unwrap();

    let exec = handler
        .handle(
            "prompt/execute",
            json!({
                "session_id": sid,
                "prompt": "explain this code",
                "mock_output": "Initial Analysis\n- sample"
            }),
        )
        .expect("prompt/execute");
    assert_eq!(exec["status"], "completed");
    assert_eq!(exec["prompt"], "explain this code");
    assert!(exec["prompt_id"].as_str().is_some());
    assert!(exec["output"].as_str().is_some());
}

#[test]
fn ide_handler_context_debug_returns_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();
    let opened = handler
        .handle("session/open", json!({"workspace_root": tmp.path()}))
        .unwrap();
    let sid = opened["session_id"].as_str().unwrap();

    let debug = handler
        .handle(
            "context/debug",
            json!({
                "session_id": sid,
                "prompt": "analyze this project",
                "mode": "ask",
                "tools": false
            }),
        )
        .expect("context/debug");
    assert_eq!(debug["schema"], "deepseek.context.debug.v1");
    assert!(debug["digest"].as_str().is_some());
}

#[test]
fn ide_handler_prompt_partial_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    let opened = handler
        .handle("session/open", json!({"workspace_root": "/tmp/stream"}))
        .unwrap();
    let sid = opened["session_id"].as_str().unwrap();
    let exec = handler
        .handle(
            "prompt/execute",
            json!({
                "session_id": sid,
                "prompt": "stream this response please",
                "include_partial_messages": true,
                "mock_output": "stream this response please"
            }),
        )
        .expect("prompt/execute");
    assert_eq!(exec["partial_messages_enabled"], true);
    let prompt_id = exec["prompt_id"].as_str().unwrap();

    let chunk_1 = handler
        .handle(
            "prompt/stream_next",
            json!({"prompt_id": prompt_id, "cursor": 0, "max_chunks": 1}),
        )
        .expect("prompt/stream_next first");
    assert_eq!(chunk_1["chunks"].as_array().unwrap().len(), 1);
    let next_cursor = chunk_1["next_cursor"].as_u64().unwrap();
    assert!(next_cursor >= 1);
    if !chunk_1["done"].as_bool().unwrap_or(false) {
        let chunk_2 = handler
            .handle(
                "prompt/stream_next",
                json!({"prompt_id": prompt_id, "cursor": next_cursor, "max_chunks": 100}),
            )
            .expect("prompt/stream_next second");
        assert_eq!(chunk_2["done"], true);
    }
}

#[test]
fn prompt_stream_resume_replays_a_draft_reset_and_replacement() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let handler = IdeRpcHandler::new(temp.path())?;
    let opened = handler.handle("session/open", json!({}))?;
    let session = require_uuid(&opened, "session_id")?;
    let lease = handler.store.session_lease(session)?;
    let prompt = handler
        .store
        .start_prompt(&lease, session, Uuid::now_v7(), &json!({}))?;
    handler.store.append_prompt_event(
        prompt,
        &stream_chunk_to_event_json(&StreamChunk::ContentDelta("Discarded".into())),
    )?;
    let first = handler.handle(
        "prompt/stream_next",
        json!({"prompt_id": prompt, "cursor": 0}),
    )?;
    assert_eq!(first["done"], false);
    assert_eq!(first["chunks"][0]["delta"], "Discarded");
    for chunk in [
        StreamChunk::ClearStreamingText,
        StreamChunk::ContentDelta("Replacement".into()),
    ] {
        handler
            .store
            .append_prompt_event(prompt, &stream_chunk_to_event_json(&chunk))?;
    }
    handler
        .store
        .finish_prompt(prompt, "completed", Some("Replacement"), None)?;
    drop(lease);

    let reconnected = IdeRpcHandler::new(temp.path())?;
    let remainder = reconnected.handle(
        "prompt/stream_next",
        json!({
            "prompt_id": prompt, "cursor": first["next_cursor"]
        }),
    )?;
    assert_eq!(remainder["chunks"][0]["type"], "clear_streaming_text");
    assert_eq!(remainder["chunks"][1]["delta"], "Replacement");
    assert_eq!(
        remainder["chunks"][2]["event"],
        json!({"type": "done", "status": "completed"})
    );
    assert_eq!(remainder["next_cursor"], 4);
    assert_eq!(remainder["done"], true);
    Ok(())
}

#[test]
fn ide_handler_handoff_export_import_remote_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    let opened = handler
        .handle("session/open", json!({"workspace_root": "/tmp/handoff"}))
        .unwrap();
    let sid = opened["session_id"].as_str().unwrap();
    let _ = handler
        .handle(
            "prompt/execute",
            json!({
                "session_id": sid,
                "prompt": "save this conversation",
                "mock_output": "saved"
            }),
        )
        .expect("prompt/execute");

    let export = handler
        .handle("session/handoff_export", json!({"session_id": sid}))
        .expect("handoff_export");
    let bundle_path = export["path"].as_str().unwrap();
    assert!(std::path::Path::new(bundle_path).exists());

    let imported = handler
        .handle(
            "session/handoff_import",
            json!({"bundle_path": bundle_path, "resume": true}),
        )
        .expect("handoff_import");
    assert_eq!(imported["imported"], true);
    let imported_id = imported["session_id"].as_str().unwrap();
    assert!(!imported_id.is_empty());

    let remote = handler
        .handle(
            "session/remote_resume",
            json!({"session_id": imported_id, "transport": "stdio"}),
        )
        .expect("remote_resume");
    assert_eq!(remote["remote"], true);
    assert_eq!(remote["session_id"], imported_id);
}

#[test]
fn ide_handler_handoff_link_create_and_consume() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    let opened = handler
        .handle(
            "session/open",
            json!({"workspace_root": "/tmp/handoff-link"}),
        )
        .unwrap();
    let sid = opened["session_id"].as_str().unwrap();
    let _ = handler
        .handle(
            "prompt/execute",
            json!({
                "session_id": sid,
                "prompt": "handoff link seed",
                "mock_output": "seeded"
            }),
        )
        .expect("prompt/execute");

    let created = handler
        .handle(
            "session/handoff_link_create",
            json!({"session_id": sid, "ttl_minutes": 10}),
        )
        .expect("handoff_link_create");
    assert_eq!(created["schema"], "deepseek.handoff_link.v1");
    let handoff_id = created["handoff_id"].as_str().unwrap();
    let token = created["token"].as_str().unwrap();
    assert!(!handoff_id.is_empty());
    assert!(!token.is_empty());

    let consumed = handler
        .handle(
            "session/handoff_link_consume",
            json!({"handoff_id": handoff_id, "token": token}),
        )
        .expect("handoff_link_consume");
    assert_eq!(consumed["consumed"], true);
    assert!(consumed["imported_session_id"].as_str().is_some());
}

#[test]
fn ide_handler_events_poll_returns_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();
    let opened = handler
        .handle("session/open", json!({"workspace_root": "/tmp/events"}))
        .unwrap();
    let sid = opened["session_id"].as_str().unwrap();
    let _ = handler
        .handle(
            "prompt/execute",
            json!({
                "session_id": sid,
                "prompt": "hello",
                "mock_output": "hello there"
            }),
        )
        .expect("prompt/execute");

    let poll = handler
        .handle(
            "events/poll",
            json!({"session_id": sid, "after_seq": 0, "limit": 10}),
        )
        .expect("events/poll");
    let events = poll["events"].as_array().unwrap();
    assert!(!events.is_empty());
    assert!(events[0]["seq_no"].as_u64().is_some());
}

#[test]
fn ide_handler_tool_approve_deny() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    let result = handler
        .handle("session/open", json!({"workspace_root": "/tmp/t"}))
        .unwrap();
    let sid_str = result["session_id"].as_str().unwrap();
    let sid = Uuid::parse_str(sid_str).unwrap();

    let inv_id = Uuid::now_v7();
    handler.add_pending_approval(sid, &inv_id.to_string(), "bash.run");

    // Approve the tool.
    let approved = handler
        .handle(
            "tool/approve",
            json!({
                "session_id": sid_str,
                "invocation_id": inv_id.to_string(),
            }),
        )
        .expect("tool/approve");
    assert_eq!(approved["approved"], true);
    assert_eq!(approved["tool_name"], "bash.run");

    // Deny a different one.
    let inv2 = Uuid::now_v7();
    handler.add_pending_approval(sid, &inv2.to_string(), "fs.write");
    let denied = handler
        .handle(
            "tool/deny",
            json!({
                "session_id": sid_str,
                "invocation_id": inv2.to_string(),
            }),
        )
        .expect("tool/deny");
    assert_eq!(denied["denied"], true);
    assert_eq!(denied["tool_name"], "fs.write");
}

#[test]
fn ide_handler_task_list_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();

    let result = handler.handle("task/list", json!({})).expect("task/list");
    let tasks = result["tasks"].as_array().unwrap();
    assert!(tasks.is_empty());
}

#[test]
fn ide_handler_unknown_method() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();
    let err = handler
        .handle("nonexistent", json!({}))
        .expect_err("should fail");
    assert!(err.to_string().contains("method not found"));
}

#[test]
fn ide_handler_session_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let handler = IdeRpcHandler::new(tmp.path()).unwrap();
    let fake = Uuid::now_v7();
    let err = handler
        .handle("session/resume", json!({"session_id": fake.to_string()}))
        .expect_err("should fail");
    assert!(err.to_string().contains("session not found"));
}

#[test]
fn require_uuid_helper() {
    let params = json!({"id": "not-a-uuid"});
    assert!(require_uuid(&params, "id").is_err());

    let valid = Uuid::now_v7();
    let params = json!({"id": valid.to_string()});
    assert_eq!(require_uuid(&params, "id").unwrap(), valid);

    let empty = json!({});
    assert!(require_uuid(&empty, "id").is_err());
}

#[cfg(test)]
mod delivery_three {
    use super::*;
    #[test]
    fn local_restore_requires_preview_approval_and_is_never_an_http_capability() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("file"), "before")?;
        let memory = codingbuddy_memory::MemoryManager::new(temp.path())?;
        let checkpoint = memory.create_checkpoint("fixture")?;
        std::fs::write(temp.path().join("file"), "after")?;
        let before = memory.capture_before(&["file"]);
        memory.record_step_snapshot("fs.write", "fixture", before, &["file"])?;
        let handler = IdeRpcHandler::new(temp.path())?;
        let plan = handler.handle(
            "checkpoint/preview",
            serde_json::json!({"checkpoint_id":checkpoint.checkpoint_id}),
        )?;
        let id = plan["id"].clone();
        assert_eq!(plan["entries"].as_array().unwrap().len(), 1);
        assert!(
            handler
                .handle(
                    "checkpoint/apply",
                    serde_json::json!({"plan_id":id,"approved":false})
                )
                .is_err()
        );
        assert!(
            handler
                .handle_http(
                    "checkpoint/apply",
                    serde_json::json!({"plan_id":id,"approved":true})
                )
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(temp.path().join("file"))?, "after");
        let applied = handler.handle(
            "checkpoint/apply",
            serde_json::json!({"plan_id":id,"approved":true}),
        )?;
        assert_eq!(applied["recovery_status"], "verified");
        assert_eq!(std::fs::read_to_string(temp.path().join("file"))?, "before");
        assert_eq!(
            handler.handle("checkpoint/reconcile", serde_json::json!({"plan_id":id}))?["recovery_status"],
            "applied"
        );
        Ok(())
    }
    #[test]
    fn cancellation_from_a_later_local_client_reaches_the_owned_prompt() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let handler = IdeRpcHandler::new(temp.path())?;
        let session = handler.handle(
            "session/open",
            serde_json::json!({"workspace_root":temp.path()}),
        )?;
        let session = Uuid::parse_str(session["session_id"].as_str().unwrap())?;
        let store = Store::new(temp.path())?;
        let lease = store.session_lease(session)?;
        let prompt = store.start_prompt(
            &lease,
            session,
            Uuid::now_v7(),
            &serde_json::json!({"prompt":"fixture"}),
        )?;
        let token = codingbuddy_core::CancellationToken::new();
        let _monitor = store.monitor_operation(prompt, token.clone())?;
        let response = handler.handle("cancel", serde_json::json!({"prompt_id":prompt}))?;
        assert_eq!(response["cancelled"], true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !token.is_cancelled() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(token.is_cancelled());
        Ok(())
    }
}
