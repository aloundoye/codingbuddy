//! Integration tests for real file operations through the tool-use loop.
//!
//! These tests use ScriptedLlm to drive the agent through actual file
//! read/write/edit operations on real temp directories, verifying that
//! the tool pipeline works end-to-end.

use anyhow::{Result, anyhow};
use codingbuddy_agent::AgentEngine;
use codingbuddy_core::{LlmResponse, LlmToolCall, TokenUsage};
use codingbuddy_llm::LlmClient;
use codingbuddy_testkit::ScriptedLlm;
use std::fs;
use std::path::Path;

fn tool_call_response(calls: Vec<(&str, &str, &str)>) -> LlmResponse {
    LlmResponse {
        text: String::new(),
        finish_reason: "tool_calls".to_string(),
        reasoning_content: String::new(),
        tool_calls: calls
            .iter()
            .map(|(id, name, args)| LlmToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args.to_string(),
            })
            .collect(),
        usage: Some(TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            ..Default::default()
        }),
        compatibility: None,
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.to_string(),
        finish_reason: "stop".to_string(),
        reasoning_content: String::new(),
        tool_calls: vec![],
        usage: Some(TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            ..Default::default()
        }),
        compatibility: None,
    }
}

fn init_workspace(path: &Path) -> Result<()> {
    fs::create_dir_all(path.join("src"))?;
    fs::write(
        path.join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )?;
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .output()?;
    if !init.status.success() {
        return Err(anyhow!("git init failed"));
    }
    Ok(())
}

fn build_engine(path: &Path, responses: Vec<LlmResponse>) -> Result<AgentEngine> {
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(ScriptedLlm::new(responses));
    let mut engine = AgentEngine::new_with_llm(path, llm)?;
    engine.set_permission_mode("bypassPermissions");
    Ok(engine)
}

// ── Real file edit tests ──

#[test]
fn fs_edit_modifies_real_file() {
    let dir = tempfile::tempdir().unwrap();
    init_workspace(dir.path()).unwrap();

    let edit_args = serde_json::json!({
        "path": "src/main.rs",
        "search": "println!(\"hello\")",
        "replace": "println!(\"world\")",
        "all": false
    });

    let engine = build_engine(
        dir.path(),
        vec![
            tool_call_response(vec![("call_1", "fs_edit", &edit_args.to_string())]),
            text_response("Done editing."),
        ],
    )
    .unwrap();

    let result = engine.chat("Edit main.rs");
    assert!(result.is_ok(), "chat should succeed: {:?}", result);

    let content = fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(
        content.contains("world"),
        "file should contain 'world' after edit, got: {content}"
    );
    assert!(
        !content.contains("hello"),
        "file should not contain 'hello' after edit"
    );
}

#[test]
fn fs_write_creates_new_file() {
    let dir = tempfile::tempdir().unwrap();
    init_workspace(dir.path()).unwrap();

    let write_args = serde_json::json!({
        "path": "src/lib.rs",
        "content": "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"
    });

    let engine = build_engine(
        dir.path(),
        vec![
            tool_call_response(vec![("call_1", "fs_write", &write_args.to_string())]),
            text_response("Created lib.rs."),
            text_response("Done."),
        ],
    )
    .unwrap();

    let result = engine.chat("Create lib.rs");
    assert!(result.is_ok(), "fs_write should succeed: {:?}", result);

    let path = dir.path().join("src/lib.rs");
    assert!(path.exists(), "lib.rs should have been created");
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("pub fn add"));
}

#[test]
fn fs_read_returns_file_content() {
    let dir = tempfile::tempdir().unwrap();
    init_workspace(dir.path()).unwrap();

    let read_args = serde_json::json!({ "path": "src/main.rs" });

    let engine = build_engine(
        dir.path(),
        vec![
            tool_call_response(vec![("call_1", "fs_read", &read_args.to_string())]),
            text_response("The file contains a main function."),
        ],
    )
    .unwrap();

    let result = engine.chat("Read main.rs");
    assert!(result.is_ok());
}

#[test]
fn fs_glob_finds_files() {
    let dir = tempfile::tempdir().unwrap();
    init_workspace(dir.path()).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "// lib\n").unwrap();

    let glob_args = serde_json::json!({ "pattern": "src/*.rs" });

    let engine = build_engine(
        dir.path(),
        vec![
            tool_call_response(vec![("call_1", "fs_glob", &glob_args.to_string())]),
            text_response("Found 2 Rust files."),
        ],
    )
    .unwrap();

    let result = engine.chat("Find Rust files");
    assert!(result.is_ok());
}

#[test]
fn fuzzy_edit_handles_whitespace_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    init_workspace(dir.path()).unwrap();
    // File has 4-space indent
    fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .unwrap();

    // Search uses 2-space indent (mismatch) — fuzzy matching should handle it
    let edit_args = serde_json::json!({
        "path": "src/main.rs",
        "search": "fn main() {\n  println!(\"hello\");\n}",
        "replace": "fn main() {\n    println!(\"world\");\n}",
        "all": false
    });

    let engine = build_engine(
        dir.path(),
        vec![
            tool_call_response(vec![("call_1", "fs_edit", &edit_args.to_string())]),
            text_response("Edited with fuzzy match."),
        ],
    )
    .unwrap();

    let result = engine.chat("Edit with whitespace mismatch");
    assert!(result.is_ok(), "fuzzy edit should succeed: {:?}", result);

    let content = fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(
        content.contains("world"),
        "fuzzy edit should apply replacement, got: {content}"
    );
}

#[test]
fn multi_tool_sequence_read_then_edit() {
    let dir = tempfile::tempdir().unwrap();
    init_workspace(dir.path()).unwrap();

    let read_args = serde_json::json!({ "path": "src/main.rs" });
    let edit_args = serde_json::json!({
        "path": "src/main.rs",
        "search": "println!(\"hello\")",
        "replace": "println!(\"multi-step\")",
        "all": false
    });

    let engine = build_engine(
        dir.path(),
        vec![
            // Turn 1: read the file
            tool_call_response(vec![("call_1", "fs_read", &read_args.to_string())]),
            // Turn 2: edit the file
            tool_call_response(vec![("call_2", "fs_edit", &edit_args.to_string())]),
            // Turn 3: done
            text_response("Read then edited."),
        ],
    )
    .unwrap();

    let result = engine.chat("Read then edit main.rs");
    assert!(result.is_ok());

    let content = fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(content.contains("multi-step"));
}
