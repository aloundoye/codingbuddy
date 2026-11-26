//! Integration tests proving the tool-use loop is the default Code path.
//!
//! Uses `ScriptedLlm` to simulate tool-call responses from the LLM,
//! and verifies that the AgentEngine correctly routes through the tool-use loop.
#![allow(clippy::unwrap_used)] // integration test helpers may panic

use anyhow::{Result, anyhow};
use codingbuddy_agent::{AgentEngine, ChatMode, ChatOptions};
use codingbuddy_core::{
    AppConfig, ChatMessage, ChatRequest, LlmRequest, LlmResponse, LlmToolCall, PreferredEditTool,
    ProviderKind, Session, SessionBudgets, SessionState, StreamCallback, StreamChunk, TokenUsage,
    model_capabilities,
};
use codingbuddy_llm::LlmClient;
use codingbuddy_store::{BackgroundJobRecord, Store, SubagentRunRecord, TaskQueueRecord};
use codingbuddy_testkit::ScriptedLlm;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// ── Response builders ──

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
        transport_parts: Vec::new(),
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
        transport_parts: Vec::new(),
        compatibility: None,
    }
}

fn tool_call_response_with_text(text: &str, calls: Vec<(&str, &str, &str)>) -> LlmResponse {
    let mut response = tool_call_response(calls);
    response.text = text.to_string();
    response
}

// ── Helpers ──

fn init_workspace(path: &Path) -> Result<()> {
    fs::create_dir_all(path.join("src"))?;
    fs::write(path.join("src/main.rs"), "fn main() {}\n")?;
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .output()?;
    if !init.status.success() {
        return Err(anyhow!(
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        ));
    }
    Ok(())
}

/// Engine whose corrective heuristics are on regardless of which provider the
/// machine's user settings select (they default to off for Claude/GPT/Gemini).
fn engine_with_heuristics(
    path: &Path,
    llm: Box<dyn codingbuddy_llm::LlmClient + Send + Sync>,
) -> Result<AgentEngine> {
    let mut cfg = AppConfig::ensure(path)?;
    cfg.agent_loop.heuristics.overrides = codingbuddy_core::heuristics::HeuristicOverrides {
        grounding_nudge: Some(true),
        unverified_file_refs: Some(true),
        shell_command_nudge: Some(true),
        error_recovery_guidance: Some(true),
        complex_checklist_nudge: Some(true),
        verbosity_nudge: Some(true),
    };
    AgentEngine::new_with_config_and_llm(path, cfg, llm)
}

fn phase_loop_config() -> AppConfig {
    let mut config = AppConfig::default();
    config.agent_loop.phase_loop_enabled = true;
    config.workspace_trust = Some(true);
    config
}

fn build_engine(path: &Path, responses: Vec<LlmResponse>) -> Result<AgentEngine> {
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(ScriptedLlm::new(responses));
    AgentEngine::new_with_llm(path, llm)
}

#[derive(Clone)]
struct SharedScriptedLlm(Arc<ScriptedLlm>);

#[async_trait::async_trait]
impl LlmClient for SharedScriptedLlm {
    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        self.0.complete(req).await
    }

    async fn complete_streaming(
        &self,
        req: &LlmRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        self.0.complete_streaming(req, cb).await
    }

    async fn complete_chat(&self, req: &ChatRequest) -> Result<LlmResponse> {
        self.0.complete_chat(req).await
    }

    async fn complete_chat_streaming(
        &self,
        req: &ChatRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        self.0.complete_chat_streaming(req, cb).await
    }

    async fn complete_fim(&self, req: &codingbuddy_core::FimRequest) -> Result<LlmResponse> {
        self.0.complete_fim(req).await
    }

    async fn complete_fim_streaming(
        &self,
        req: &codingbuddy_core::FimRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        self.0.complete_fim_streaming(req, cb).await
    }
}

fn build_engine_with_shared_llm(path: &Path, llm: Arc<ScriptedLlm>) -> Result<AgentEngine> {
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(SharedScriptedLlm(llm));
    AgentEngine::new_with_llm(path, llm)
}

/// Stream the scripted responses the same way as a live provider, including per-step Done.
struct StreamingScript(ScriptedLlm);

#[async_trait::async_trait]
impl LlmClient for StreamingScript {
    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        self.0.complete(req).await
    }

    async fn complete_streaming(
        &self,
        req: &LlmRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        self.0.complete_streaming(req, cb).await
    }

    async fn complete_fim(&self, req: &codingbuddy_core::FimRequest) -> Result<LlmResponse> {
        self.0.complete_fim(req).await
    }

    async fn complete_fim_streaming(
        &self,
        req: &codingbuddy_core::FimRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        self.0.complete_fim_streaming(req, cb).await
    }

    async fn complete_chat(&self, req: &ChatRequest) -> Result<LlmResponse> {
        self.0.complete_chat(req).await
    }

    async fn complete_chat_streaming(
        &self,
        req: &ChatRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        let response = self.complete_chat(req).await?;
        cb(StreamChunk::ContentDelta(response.text.clone()));
        cb(StreamChunk::Done {
            reason: Some(response.finish_reason.clone()),
        });
        Ok(response)
    }
}

#[test]
fn prompt_stream_replaces_discarded_draft_and_completes_after_tools() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    let engine = engine_with_heuristics(
        temp.path(),
        Box::new(StreamingScript(ScriptedLlm::new(vec![
            text_response(&"Discard this draft. ".repeat(25)),
            tool_call_response_with_text(
                "Reading now.",
                vec![("read", "fs_read", r#"{"path":"src/main.rs"}"#)],
            ),
            text_response("Read it."),
        ]))),
    )?;
    let chunks = Arc::new(Mutex::new(Vec::new()));
    engine.set_stream_callback(Arc::new({
        let chunks = chunks.clone();
        move |chunk| chunks.lock().unwrap().push(chunk)
    }));

    assert_eq!(
        codingbuddy_core::runtime::block_on(engine.chat("Read the main source"))?,
        "Read it."
    );
    let chunks = chunks.lock().unwrap();
    let presentation: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ContentDelta(text) if text.starts_with("Discard") => Some("draft"),
            StreamChunk::ContentDelta(text) => Some(text.as_str()),
            StreamChunk::ClearStreamingText => Some("clear"),
            StreamChunk::ToolCallStart { .. } => Some("start"),
            StreamChunk::ToolCallEnd { .. } => Some("end"),
            StreamChunk::Done { .. } => Some("done"),
            _ => None,
        })
        .collect();
    assert_eq!(
        presentation,
        [
            "draft",
            "clear",
            "Reading now.",
            "start",
            "end",
            "Read it.",
            "done"
        ]
    );
    Ok(())
}

#[test]
fn long_answer_after_tools_is_kept_on_screen() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    let answer = "The main source prints a greeting and exits. ".repeat(10);
    let engine = AgentEngine::new_with_llm(
        temp.path(),
        Box::new(StreamingScript(ScriptedLlm::new(vec![
            tool_call_response_with_text(
                "Reading now.",
                vec![("read", "fs_read", r#"{"path":"src/main.rs"}"#)],
            ),
            text_response(&answer),
        ]))),
    )?;
    let chunks = Arc::new(Mutex::new(Vec::new()));
    engine.set_stream_callback(Arc::new({
        let chunks = chunks.clone();
        move |chunk| chunks.lock().unwrap().push(chunk)
    }));

    assert_eq!(
        codingbuddy_core::runtime::block_on(engine.chat("Read the main source"))?,
        answer
    );
    let chunks = chunks.lock().unwrap();
    assert!(
        !chunks
            .iter()
            .any(|chunk| matches!(chunk, StreamChunk::ClearStreamingText)),
        "grounded answer after tool use must not be discarded"
    );
    assert!(matches!(chunks.last(), Some(StreamChunk::Done { .. })));
    Ok(())
}

#[test]
fn failed_prompt_is_closed_before_the_next_prompt() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    let mut truncated = text_response("Partial");
    truncated.finish_reason = "length".to_string();
    let llm = Arc::new(ScriptedLlm::new(vec![truncated, text_response("Done.")]));
    let engine = AgentEngine::new_with_llm(temp.path(), Box::new(SharedScriptedLlm(llm.clone())))?;

    assert!(codingbuddy_core::runtime::block_on(engine.chat("Explain lifetimes")).is_err());
    assert_eq!(
        codingbuddy_core::runtime::block_on(engine.chat("Say done"))?,
        "Done."
    );

    let captured = llm.captured_messages();
    let second = captured.last().unwrap();
    let stale = second
        .iter()
        .position(
            |m| matches!(m, ChatMessage::User { content } if content.contains("Explain lifetimes")),
        )
        .expect("failed prompt stays in history");
    assert!(
        matches!(
            &second[stale + 1],
            ChatMessage::Assistant { content: Some(text), tool_calls, .. }
                if tool_calls.is_empty() && text.contains("interrupted")
        ),
        "failed prompt must be followed by an interrupted marker, got {:?}",
        second[stale + 1]
    );

    // The marker is persisted, so a third prompt does not add another one.
    let store = Store::new(temp.path())?;
    let session = store.load_latest_session()?.unwrap();
    let history = store.rebuild_from_events(session.session_id)?.chat_messages;
    let markers = history
        .iter()
        .filter(|m| matches!(m, ChatMessage::Assistant { content: Some(t), .. } if t.contains("interrupted")))
        .count();
    assert_eq!(markers, 1);
    Ok(())
}

#[test]
fn failed_and_cancelled_prompts_do_not_emit_done() -> Result<()> {
    for cancelled in [false, true] {
        let temp = tempfile::tempdir()?;
        init_workspace(temp.path())?;
        let responses = if cancelled {
            vec![text_response("Partial reply")]
        } else {
            vec![text_response(&"Draft that requires a retry. ".repeat(25))]
        };
        let engine = engine_with_heuristics(
            temp.path(),
            Box::new(StreamingScript(ScriptedLlm::new(responses))),
        )?;
        let cancellation = codingbuddy_core::CancellationToken::new();
        let chunks = Arc::new(Mutex::new(Vec::new()));
        engine.set_stream_callback(Arc::new({
            let chunks = chunks.clone();
            let cancellation = cancellation.clone();
            move |chunk| {
                if cancelled && matches!(chunk, StreamChunk::ContentDelta(_)) {
                    cancellation.cancel();
                }
                chunks.lock().unwrap().push(chunk);
            }
        }));
        let result = codingbuddy_core::runtime::block_on(engine.chat_with_options(
            "Read the main source",
            ChatOptions {
                tools: true,
                cancellation: Some(cancellation),
                ..Default::default()
            },
        ));
        assert!(result.is_err());
        let chunks = chunks.lock().unwrap();
        assert!(
            chunks
                .iter()
                .any(|chunk| matches!(chunk, StreamChunk::ContentDelta(_)))
        );
        assert!(
            !chunks
                .iter()
                .any(|chunk| matches!(chunk, StreamChunk::Done { .. }))
        );

        let store = Store::new(temp.path())?;
        let session = store.load_latest_session()?.unwrap();
        let prompt = store.latest_prompt(session.session_id)?.unwrap();
        assert_eq!(
            prompt.status,
            if cancelled { "cancelled" } else { "failed" }
        );
    }
    Ok(())
}

// ── Tests ──

/// Default ChatMode::Code routes to the tool-use loop (not the old pipeline).
/// The LLM calls fs_read then responds with text.
#[test]
fn default_code_mode_uses_tool_loop() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    fs::write(temp.path().join("hello.txt"), "Hello, world!\n")?;

    let engine = build_engine(
        temp.path(),
        vec![
            // Turn 1: LLM calls fs_read
            tool_call_response(vec![("call_1", "fs_read", r#"{"path":"hello.txt"}"#)]),
            // Turn 2: LLM responds with text
            text_response("The file contains a greeting."),
        ],
    )?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "What's in hello.txt?",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert_eq!(output, "The file contains a greeting.");
    Ok(())
}

/// The tool-use loop reads a file, edits it, then responds.
/// Uses bypassPermissions mode which auto-approves write tools.
#[test]
fn tool_loop_reads_then_edits_file() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    fs::write(temp.path().join("demo.txt"), "old content\n")?;

    let mut engine = build_engine(
        temp.path(),
        vec![
            // Turn 1: read file
            tool_call_response(vec![("call_1", "fs_read", r#"{"path":"demo.txt"}"#)]),
            // Turn 2: edit file (fs_edit uses search/replace fields)
            tool_call_response(vec![(
                "call_2",
                "fs_edit",
                r#"{"path":"demo.txt","search":"old content","replace":"new content"}"#,
            )]),
            // Turn 3: done
            text_response("Updated demo.txt successfully."),
        ],
    )?;

    // Bypass permissions so fs_edit goes through in tests
    engine.set_permission_mode("bypassPermissions");

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Replace 'old content' with 'new content' in demo.txt",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Updated"));
    // Verify the edit was applied
    let content = fs::read_to_string(temp.path().join("demo.txt"))?;
    assert_eq!(content, "new content\n");
    Ok(())
}

/// The tool-use loop runs a bash command.
#[test]
fn tool_loop_runs_bash_command() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = build_engine(
        temp.path(),
        vec![
            // Turn 1: run bash
            tool_call_response(vec![(
                "call_1",
                "bash_run",
                r#"{"command":"echo hello from bash"}"#,
            )]),
            // Turn 2: respond
            text_response("Command output: hello from bash"),
        ],
    )?;

    // bash_run requires approval — without an approval handler, it gets denied
    // so the LLM gets a denial feedback and should respond accordingly
    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Run echo hello",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    // The loop ran and returned a text response
    assert!(!output.is_empty());
    Ok(())
}

/// When a tool is denied (no approval handler), the denial is fed back to the LLM.
#[test]
fn tool_loop_denied_tool_feeds_back() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = build_engine(
        temp.path(),
        vec![
            // Turn 1: LLM tries bash_run (requires approval)
            tool_call_response(vec![("call_1", "bash_run", r#"{"command":"rm -rf /"}"#)]),
            // Turn 2: after denial, LLM responds with alternative
            text_response("I cannot execute that command. Can I help another way?"),
        ],
    )?;

    // No approval handler set → denial by default
    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Delete everything",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("cannot execute"));
    Ok(())
}

/// Multi-turn conversation: first prompt gets tool calls, second prompt continues.
#[test]
fn tool_loop_multi_turn_conversation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    fs::write(temp.path().join("a.txt"), "aaa\n")?;

    let engine = build_engine(
        temp.path(),
        vec![
            // First prompt, turn 1: read file
            tool_call_response(vec![("call_1", "fs_read", r#"{"path":"a.txt"}"#)]),
            // First prompt, turn 2: respond
            text_response("File a.txt contains 'aaa'."),
        ],
    )?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Read a.txt",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("aaa"));
    Ok(())
}

/// ChatMode::Ask restricts to read-only tools — write tools get filtered out.
#[test]
fn ask_mode_restricts_to_read_only() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = build_engine(
        temp.path(),
        vec![
            // Turn 1: LLM reads a file (should work in Ask mode)
            tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/main.rs"}"#)]),
            // Turn 2: respond
            text_response("The file has a main function."),
        ],
    )?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "What's in src/main.rs?",
        ChatOptions {
            tools: true,
            mode: ChatMode::Ask,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("main function"));
    Ok(())
}

/// ChatMode::Context also restricts to read-only tools.
#[test]
fn context_mode_restricts_to_read_only() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = build_engine(
        temp.path(),
        vec![text_response("Here's the project overview.")],
    )?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Describe this project",
        ChatOptions {
            tools: true,
            mode: ChatMode::Context,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("project overview"));
    Ok(())
}

// ── Capturing LLM variant (records requests for inspection) ──

struct CapturingLlm {
    responses: Mutex<VecDeque<LlmResponse>>,
    captured_requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl CapturingLlm {
    fn new(responses: Vec<LlmResponse>) -> (Self, Arc<Mutex<Vec<ChatRequest>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                captured_requests: captured.clone(),
            },
            captured,
        )
    }
}

#[async_trait::async_trait]
impl LlmClient for CapturingLlm {
    async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse> {
        Err(anyhow!("complete() not used"))
    }
    async fn complete_streaming(
        &self,
        _req: &LlmRequest,
        _cb: StreamCallback,
    ) -> Result<LlmResponse> {
        Err(anyhow!("complete_streaming() not used"))
    }
    async fn complete_chat(&self, req: &ChatRequest) -> Result<LlmResponse> {
        self.captured_requests.lock().unwrap().push(req.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("capturing llm exhausted"))
    }
    async fn complete_chat_streaming(
        &self,
        req: &ChatRequest,
        _cb: StreamCallback,
    ) -> Result<LlmResponse> {
        self.complete_chat(req).await
    }
    async fn complete_fim(&self, _req: &codingbuddy_core::FimRequest) -> Result<LlmResponse> {
        Err(anyhow!("complete_fim() not used"))
    }
    async fn complete_fim_streaming(
        &self,
        _req: &codingbuddy_core::FimRequest,
        _cb: StreamCallback,
    ) -> Result<LlmResponse> {
        Err(anyhow!("complete_fim_streaming() not used"))
    }
}

/// When the LLM keeps making tool calls beyond max_turns, the loop stops gracefully.
#[test]
fn tool_loop_handles_max_turns() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    fs::write(temp.path().join("test.txt"), "data\n")?;

    // Provide enough responses for many tool calls + a final text response
    let mut responses: Vec<LlmResponse> = (0..60)
        .map(|i| {
            tool_call_response(vec![(
                &format!("call_{i}"),
                "fs_read",
                r#"{"path":"test.txt"}"#,
            )])
        })
        .collect();
    responses.push(text_response("finally done"));

    let mut engine = build_engine(temp.path(), responses)?;
    engine.set_max_turns(Some(3));

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Keep reading forever",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    // Should have stopped due to max turns, returning empty response
    // (max_turns stop doesn't produce text)
    assert!(output.is_empty() || output.contains("done"));
    Ok(())
}

// ── Batch 7: New integration tests ──

/// Tool descriptions in the LLM request are enriched (long, behavioral instructions).
#[test]
fn tool_descriptions_are_enriched_in_request() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![
        tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/main.rs"}"#)]),
        text_response("Done."),
    ]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_llm(temp.path(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Read file",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    assert!(
        !requests.is_empty(),
        "should have captured at least one request"
    );
    let first_req = &requests[0];
    // Find fs_read tool in the request tools
    let fs_read_tool = first_req
        .tools
        .iter()
        .find(|t| t.function.name == "fs_read")
        .expect("fs_read should be in tools");
    assert!(
        fs_read_tool.function.description.len() >= 200,
        "fs_read description should be enriched (>=200 chars), got {}",
        fs_read_tool.function.description.len()
    );
    assert!(
        fs_read_tool.function.description.contains("MUST"),
        "fs_read description should contain behavioral instructions"
    );
    Ok(())
}

/// First turn of tool-use loop uses tool_choice=required.
#[test]
fn first_turn_tool_choice_required() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![
        tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/main.rs"}"#)]),
        text_response("Read the file."),
    ]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_llm(temp.path(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "What's in this project?",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty());
    // All turns use auto — required causes hangs on some models
    assert_eq!(
        requests[0].tool_choice,
        codingbuddy_core::ToolChoice::auto(),
        "first turn should use tool_choice=auto"
    );
    Ok(())
}

/// After 1+ Assistant turns, tool_choice switches to auto.
/// Only the very first LLM call per user question forces tool_choice=required.
#[test]
fn subsequent_turns_tool_choice_auto() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    fs::write(temp.path().join("test.txt"), "data\n")?;
    fs::write(temp.path().join("other.txt"), "more data\n")?;

    let (llm, captured) = CapturingLlm::new(vec![
        // Turn 1: first tool call (0 Assistant turns → required)
        tool_call_response(vec![("call_1", "fs_read", r#"{"path":"test.txt"}"#)]),
        // Turn 2: second tool call (1 Assistant turn → auto)
        tool_call_response(vec![("call_2", "fs_read", r#"{"path":"other.txt"}"#)]),
        // Turn 3: text response
        text_response("Files read successfully."),
    ]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_llm(temp.path(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Read test.txt and other.txt",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    assert!(requests.len() >= 3, "should have at least 3 LLM calls");
    // All turns use auto
    for (i, req) in requests.iter().enumerate() {
        assert_eq!(
            req.tool_choice,
            codingbuddy_core::ToolChoice::auto(),
            "turn {} should use tool_choice=auto",
            i
        );
    }
    Ok(())
}

/// Ask mode uses read-only tools (no fs_edit, fs_write, bash_run in tool list).
#[test]
fn ask_mode_uses_read_only_tools() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![
        tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/main.rs"}"#)]),
        text_response("It's a Rust project."),
    ]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_llm(temp.path(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "What language is this?",
        ChatOptions {
            tools: true,
            mode: ChatMode::Ask,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty());
    let tool_names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|t| t.function.name.as_str())
        .collect();
    // Read-only tools should be present
    assert!(tool_names.contains(&"fs_read"), "should have fs_read");
    assert!(tool_names.contains(&"fs_glob"), "should have fs_glob");
    assert!(tool_names.contains(&"fs_grep"), "should have fs_grep");
    // Write tools should NOT be present
    assert!(!tool_names.contains(&"fs_edit"), "should not have fs_edit");
    assert!(
        !tool_names.contains(&"fs_write"),
        "should not have fs_write"
    );
    assert!(
        !tool_names.contains(&"bash_run"),
        "should not have bash_run"
    );
    Ok(())
}

/// Context mode also uses read-only tools.
#[test]
fn context_mode_uses_read_only_tools() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![text_response("Project overview.")]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_llm(temp.path(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Describe this project",
        ChatOptions {
            tools: true,
            mode: ChatMode::Context,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty());
    let tool_names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|t| t.function.name.as_str())
        .collect();
    assert!(
        !tool_names.contains(&"fs_edit"),
        "Context mode should not have fs_edit"
    );
    assert!(
        !tool_names.contains(&"bash_run"),
        "Context mode should not have bash_run"
    );
    Ok(())
}

#[test]
fn code_mode_build_profile_uses_smaller_tool_surface() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![text_response("Scoped tool set ready.")]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_llm(temp.path(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Fix the bug in src/main.rs",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty());
    let tool_names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|t| t.function.name.as_str())
        .collect();
    assert!(
        tool_names.contains(&"fs_edit"),
        "build profile should keep fs_edit"
    );
    assert!(
        tool_names.contains(&"bash_run"),
        "build profile should keep bash_run"
    );
    assert!(
        tool_names.contains(&"tool_search"),
        "tool_search should appear when discoverable tools exist"
    );
    assert!(
        !tool_names.contains(&"web_search"),
        "build profile should not expose web_search by default"
    );
    assert!(
        !tool_names.contains(&"chrome_navigate"),
        "chrome tools should stay hidden without chrome-specific signals"
    );
    Ok(())
}

#[test]
fn tool_search_enables_matching_tools_for_followup_turns() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![
        tool_call_response(vec![("call_1", "tool_search", r#"{"query":"plan mode"}"#)]),
        tool_call_response(vec![("call_2", "enter_plan_mode", "{}")]),
        text_response("Planning mode entered."),
    ]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_llm(temp.path(), llm)?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Inspect this repository",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Planning mode entered"));
    let requests = captured.lock().unwrap();
    assert!(
        requests.len() >= 2,
        "expected at least two captured requests"
    );
    let first_tools: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|t| t.function.name.as_str())
        .collect();
    let second_tools: Vec<&str> = requests[1]
        .tools
        .iter()
        .map(|t| t.function.name.as_str())
        .collect();
    assert!(first_tools.contains(&"tool_search"));
    assert!(
        !first_tools.contains(&"enter_plan_mode"),
        "enter_plan_mode should be hidden until discovered"
    );
    assert!(
        second_tools.contains(&"enter_plan_mode"),
        "tool_search should promote matching tools into the next request"
    );
    Ok(())
}

/// When tools=false, the analysis path is used (no tools in request).
#[test]
fn tools_false_uses_analysis_path() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![text_response("Analysis complete.")]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_llm(temp.path(), llm)?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "What is 2+2?",
        ChatOptions {
            tools: false,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Analysis complete"));
    // Analysis path should have been used (check captured requests)
    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty());
    // With tools=false, the request should have no tools
    assert!(
        requests[0].tools.is_empty(),
        "analysis path should not include tools"
    );
    Ok(())
}

/// Default tool-use loop always has thinking enabled (adaptive thinking).
#[test]
fn default_tool_loop_has_thinking_enabled() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![
        tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/main.rs"}"#)]),
        text_response("Done."),
    ]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_config_and_llm(temp.path(), AppConfig::default(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "What's in this project?",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty());
    // Thinking should always be enabled by default
    let thinking = requests[0]
        .thinking
        .as_ref()
        .expect("thinking should be enabled by default");
    assert!(
        thinking.budget_tokens.unwrap_or(0) > 0,
        "thinking budget should be positive"
    );
    Ok(())
}

/// force_max_think overrides thinking budget to maximum (65536).
#[test]
fn force_max_think_overrides_budget() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![
        tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/main.rs"}"#)]),
        text_response("Done."),
    ]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_config_and_llm(temp.path(), AppConfig::default(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "fix typo",
        ChatOptions {
            tools: true,
            force_max_think: true,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    assert!(!requests.is_empty());
    let thinking = requests[0]
        .thinking
        .as_ref()
        .expect("thinking should be enabled with force_max_think");
    // force_max_think should use COMPLEX_THINK_BUDGET * 2 = 65536 (max budget)
    assert_eq!(
        thinking.budget_tokens,
        Some(65536),
        "force_max_think should use maximum budget"
    );
    Ok(())
}

/// Tool loop with thinking mode (reasoning_content preserved).
#[test]
fn tool_loop_with_thinking_mode() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    fs::write(
        temp.path().join("src/main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )?;

    // Simulate thinking mode response (with reasoning_content)
    let mut response = tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/main.rs"}"#)]);
    response.reasoning_content = "Let me think about which file to read...".to_string();

    let mut final_response = text_response("The main function prints hello.");
    final_response.reasoning_content = "Based on the file contents...".to_string();

    let engine = build_engine(temp.path(), vec![response, final_response])?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "What does the main function do?",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("prints hello"));
    Ok(())
}

// ── spawn_task tests ──

fn build_trusted_engine(path: &Path, responses: Vec<LlmResponse>) -> Result<AgentEngine> {
    let cfg = AppConfig {
        workspace_trust: Some(true),
        ..AppConfig::default()
    };
    let llm = Box::new(ScriptedLlm::new(responses));
    AgentEngine::new_with_config_and_llm(path, cfg, llm)
}

/// When spawn_task is called with a worker, the worker is invoked.
#[test]
fn spawn_task_calls_worker() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let worker_called = Arc::new(Mutex::new(false));
    let worker_called_clone = worker_called.clone();

    let engine = build_trusted_engine(
        temp.path(),
        vec![
            // Turn 1: LLM calls spawn_task
            tool_call_response(vec![(
                "call_1",
                "spawn_task",
                r#"{"prompt":"analyze the project","description":"explore project","subagent_type":"explore"}"#,
            )]),
            // Turn 2: respond with results
            text_response("The subagent analyzed the project."),
        ],
    )?;

    // Wire a subagent worker
    let worker: codingbuddy_agent::SubagentWorkerFn = Arc::new(move |task| {
        *worker_called_clone.lock().unwrap() = true;
        Ok(format!("Subagent completed: {}", task.goal))
    });
    engine.set_subagent_worker(worker);

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Analyze this project",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(
        *worker_called.lock().unwrap(),
        "subagent worker should have been called"
    );
    assert!(!output.is_empty());
    Ok(())
}

/// When spawn_task is called without a worker, it falls back gracefully.
#[test]
fn spawn_task_without_worker_falls_back() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = build_trusted_engine(
        temp.path(),
        vec![
            // Turn 1: LLM calls spawn_task (no worker wired)
            tool_call_response(vec![(
                "call_1",
                "spawn_task",
                r#"{"prompt":"explore the code","description":"code exploration","subagent_type":"explore"}"#,
            )]),
            // Turn 2: respond
            text_response("I'll explore the code directly."),
        ],
    )?;

    // No worker set — should fall back gracefully
    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Explore the code",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(!output.is_empty());
    Ok(())
}

/// spawn_task correctly maps subagent_type to SubagentRole.
#[test]
fn spawn_task_role_maps_correctly() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let captured_role = Arc::new(Mutex::new(String::new()));
    let captured_role_clone = captured_role.clone();

    let engine = build_trusted_engine(
        temp.path(),
        vec![
            tool_call_response(vec![(
                "call_1",
                "spawn_task",
                r#"{"prompt":"plan the implementation","description":"plan task","subagent_type":"plan"}"#,
            )]),
            text_response("Planning complete."),
        ],
    )?;

    let worker: codingbuddy_agent::SubagentWorkerFn = Arc::new(move |task| {
        *captured_role_clone.lock().unwrap() = format!("{:?}", task.role);
        Ok("Done".to_string())
    });
    engine.set_subagent_worker(worker);

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Plan the implementation",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    let role = captured_role.lock().unwrap();
    assert_eq!(
        *role, "Plan",
        "subagent_type 'plan' should map to SubagentRole::Plan"
    );
    Ok(())
}

#[test]
fn spawn_task_persists_child_session_and_run_metadata() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = build_trusted_engine(
        temp.path(),
        vec![
            tool_call_response(vec![(
                "call_1",
                "spawn_task",
                r#"{"prompt":"analyze the project","description":"explore project","subagent_type":"explore"}"#,
            )]),
            text_response("Delegated task complete."),
        ],
    )?;

    let worker: codingbuddy_agent::SubagentWorkerFn =
        Arc::new(move |task| Ok(format!("Subagent completed: {}", task.goal)));
    engine.set_subagent_worker(worker);

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Analyze this project",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Delegated task complete"));
    let store = Store::new(temp.path())?;
    let tasks = store.list_tasks(None)?;
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task.status, "completed");
    assert!(
        task.artifact_path
            .as_deref()
            .is_some_and(|value| value.starts_with("session://"))
    );

    let run = store
        .load_subagent_run_for_task(task.task_id)?
        .expect("subagent run");
    assert_eq!(run.session_id, Some(task.session_id));
    assert_eq!(run.task_id, Some(task.task_id));
    assert_eq!(run.status, "completed");
    assert_eq!(
        run.output.as_deref(),
        Some("Subagent completed: analyze the project")
    );
    let child_session_id = run.child_session_id.expect("child session id");
    assert!(store.load_session(child_session_id)?.is_some());
    Ok(())
}

#[test]
fn spawn_task_background_keeps_task_running() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let job_id = Uuid::now_v7();
    let engine = build_trusted_engine(
        temp.path(),
        vec![
            tool_call_response(vec![(
                "call_1",
                "spawn_task",
                r#"{"prompt":"analyze the project","description":"explore project","subagent_type":"explore","run_in_background":true,"max_turns":5,"model":"deepseek-chat"}"#,
            )]),
            text_response("Delegated task queued."),
        ],
    )?;

    let worker: codingbuddy_agent::SubagentWorkerFn = Arc::new(move |_task| {
        Ok(serde_json::json!({
            "job_id": job_id,
            "status": "running",
        })
        .to_string())
    });
    engine.set_subagent_worker(worker);

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Analyze this project in background",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Delegated task queued"));
    let store = Store::new(temp.path())?;
    let tasks = store.list_tasks(None)?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].status, "in_progress");
    let run = store
        .load_subagent_run_for_task(tasks[0].task_id)?
        .expect("subagent run");
    assert_eq!(run.status, "running");
    assert_eq!(run.background_job_id, Some(job_id));
    Ok(())
}

#[test]
fn task_output_tool_surfaces_persisted_run_payload() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let store = Store::new(temp.path())?;
    let parent_session = Session {
        session_id: Uuid::now_v7(),
        workspace_root: temp.path().to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1024,
        },
        active_plan_id: None,
    };
    store.save_session(&parent_session)?;
    let child_session = store.fork_session(parent_session.session_id)?;
    store.save_session(&parent_session)?;

    let task_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();
    let now = chrono::Utc::now().to_rfc3339();
    store.insert_task(&TaskQueueRecord {
        task_id,
        session_id: parent_session.session_id,
        title: "Explore project".to_string(),
        description: Some("Inspect parser pipeline".to_string()),
        priority: 1,
        status: "completed".to_string(),
        outcome: Some("Indexed 12 files".to_string()),
        artifact_path: Some(format!("session://{}", child_session.session_id)),
        created_at: now.clone(),
        updated_at: now.clone(),
    })?;
    store.upsert_subagent_run(&SubagentRunRecord {
        run_id,
        session_id: Some(parent_session.session_id),
        task_id: Some(task_id),
        child_session_id: Some(child_session.session_id),
        background_job_id: None,
        name: "explore project".to_string(),
        goal: "inspect parser pipeline".to_string(),
        status: "completed".to_string(),
        output: Some("Indexed 12 files".to_string()),
        error: None,
        created_at: now.clone(),
        updated_at: now,
    })?;

    let scripted = Arc::new(ScriptedLlm::new(vec![
        tool_call_response(vec![(
            "call_1",
            "task_output",
            &format!(r#"{{"task_id":"{task_id}"}}"#),
        )]),
        text_response("Read task output."),
    ]));
    let engine = build_engine_with_shared_llm(temp.path(), scripted.clone())?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Read the delegated task output",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Read task output"));
    let captured = scripted.captured_messages();
    assert!(captured.len() >= 2, "expected two llm calls");
    let tool_message = captured[1]
        .iter()
        .find_map(|msg| match msg {
            ChatMessage::Tool { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("tool message");
    assert!(tool_message.contains(&task_id.to_string()));
    assert!(tool_message.contains(&run_id.to_string()));
    assert!(tool_message.contains(&child_session.session_id.to_string()));
    assert!(tool_message.contains("Indexed 12 files"));
    Ok(())
}

#[test]
fn task_stop_cancels_background_run() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let store = Store::new(temp.path())?;
    let session = Session {
        session_id: Uuid::now_v7(),
        workspace_root: temp.path().to_string_lossy().to_string(),
        baseline_commit: None,
        status: SessionState::Idle,
        budgets: SessionBudgets {
            per_turn_seconds: 30,
            max_think_tokens: 1024,
        },
        active_plan_id: None,
    };
    store.save_session(&session)?;

    let task_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();
    let job_id = Uuid::now_v7();
    let now = chrono::Utc::now().to_rfc3339();
    store.insert_task(&TaskQueueRecord {
        task_id,
        session_id: session.session_id,
        title: "Background task".to_string(),
        description: None,
        priority: 1,
        status: "in_progress".to_string(),
        outcome: None,
        artifact_path: None,
        created_at: now.clone(),
        updated_at: now.clone(),
    })?;
    store.upsert_subagent_run(&SubagentRunRecord {
        run_id,
        session_id: Some(session.session_id),
        task_id: Some(task_id),
        child_session_id: None,
        background_job_id: Some(job_id),
        name: "bg".to_string(),
        goal: "work".to_string(),
        status: "running".to_string(),
        output: None,
        error: None,
        created_at: now.clone(),
        updated_at: now.clone(),
    })?;
    store.upsert_background_job(&BackgroundJobRecord {
        job_id,
        kind: "subagent".to_string(),
        reference: format!("subagent:{run_id}"),
        status: "running".to_string(),
        metadata_json: serde_json::json!({}).to_string(),
        started_at: now.clone(),
        updated_at: now,
    })?;

    store.begin_operation(
        job_id,
        Some(session.session_id),
        None,
        "background",
        &serde_json::json!({}),
    )?;
    let engine = build_engine(
        temp.path(),
        vec![
            tool_call_response(vec![(
                "call_1",
                "task_stop",
                &format!(r#"{{"task_id":"{task_id}"}}"#),
            )]),
            text_response("Stopped the task."),
        ],
    )?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Stop the background task",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Stopped the task"));
    let task = store.load_task(task_id)?.expect("task");
    assert_eq!(
        task.status, "in_progress",
        "Cancellation is pending until the worker acknowledges it"
    );
    let run = store.load_subagent_run(run_id)?.expect("run");
    assert_eq!(run.status, "cancelling");
    let job = store.load_background_job(job_id)?.expect("job");
    assert_eq!(job.status, "cancelling");
    assert!(store.operation(job_id)?.cancel_requested);
    Ok(())
}

#[test]
fn task_create_persists_to_store() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = build_engine(
        temp.path(),
        vec![
            tool_call_response(vec![(
                "call_1",
                "task_create",
                r#"{"subject":"Audit parser","description":"Inspect parser edge cases","priority":2}"#,
            )]),
            text_response("Created the task."),
        ],
    )?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Create a task for parser auditing",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Created"));
    let store = Store::new(temp.path())?;
    let tasks = store.list_tasks(None)?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Audit parser");
    assert_eq!(
        tasks[0].description.as_deref(),
        Some("Inspect parser edge cases")
    );
    assert_eq!(tasks[0].priority, 2);
    Ok(())
}

#[test]
fn exit_plan_mode_persists_plan_and_session_state() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = build_engine(
        temp.path(),
        vec![
            tool_call_response(vec![("call_1", "enter_plan_mode", "{}")]),
            tool_call_response_with_text(
                "1. Read src/main.rs\n2. Update the parser wiring\n3. Verify with cargo test",
                vec![("call_2", "exit_plan_mode", "{}")],
            ),
            text_response("Plan saved for review."),
        ],
    )?;

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Plan the parser refactor",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Plan saved"));
    let store = Store::new(temp.path())?;
    let session = store.load_latest_session()?.expect("session");
    assert_eq!(
        session.status,
        codingbuddy_core::SessionState::AwaitingApproval
    );
    let plan_id = session.active_plan_id.expect("active plan");
    let plan = store.load_plan(plan_id)?.expect("plan");
    assert_eq!(plan.version, 2);
    assert_eq!(plan.goal, "Plan the parser refactor");
    assert!(!plan.steps.is_empty());
    Ok(())
}

#[test]
fn complex_prompt_auto_enters_plan_and_persists_draft_state() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let engine = AgentEngine::new_with_config_and_llm(
        temp.path(),
        phase_loop_config(),
        Box::new(ScriptedLlm::new(vec![
            tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/main.rs"}"#)]),
            tool_call_response(vec![(
                "call_2",
                "fs_grep",
                r#"{"pattern":"main","path":"src"}"#,
            )]),
            tool_call_response(vec![("call_3", "git_status", "{}")]),
            text_response(
                "1. Inspect parser wiring\n2. Update implementation\n3. Verify with cargo test",
            ),
            tool_call_response(vec![("call_4", "exit_plan_mode", "{}")]),
            text_response("Plan saved for review."),
        ])),
    )?;

    let phase_chunks = Arc::new(Mutex::new(Vec::new()));
    let phase_chunks_clone = phase_chunks.clone();
    engine.set_stream_callback(Arc::new(move |chunk| {
        if let StreamChunk::PhaseTransition { from, to } = chunk {
            phase_chunks_clone.lock().unwrap().push((from, to));
        }
    }));

    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Refactor the parser across multiple files and verify the result carefully.",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    assert!(output.contains("Plan saved"));
    let phases = phase_chunks.lock().unwrap().clone();
    assert_eq!(phases, vec![("explore".to_string(), "plan".to_string())]);

    let store = Store::new(temp.path())?;
    let session = store.load_latest_session()?.expect("session");
    assert_eq!(session.status, SessionState::AwaitingApproval);
    let plan = store
        .load_plan(session.active_plan_id.expect("active plan"))?
        .expect("plan");
    assert_eq!(plan.version, 2, "auto-entered plan should create draft v1");
    assert!(!plan.steps.is_empty());
    Ok(())
}

#[test]
fn approved_plan_execution_emits_full_phase_path_and_loads_plan_context() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    let (llm, captured) = CapturingLlm::new(vec![
        tool_call_response(vec![("call_1", "tool_search", r#"{"query":"plan mode"}"#)]),
        tool_call_response(vec![("call_2", "enter_plan_mode", "{}")]),
        tool_call_response_with_text(
            "1. Read src/main.rs\n2. Update parser wiring\n3. Verify with cargo test",
            vec![("call_3", "exit_plan_mode", "{}")],
        ),
        text_response("Plan saved for review."),
        tool_call_response(vec![("call_4", "fs_read", r#"{"path":"src/main.rs"}"#)]),
        tool_call_response(vec![(
            "call_5",
            "fs_edit",
            r#"{"path":"src/main.rs","search":"fn main() {}","replace":"fn main() { println!(\"ok\"); }"}"#,
        )]),
        text_response("Implemented the approved plan."),
        tool_call_response(vec![(
            "call_6",
            "bash_run",
            r#"{"command":"printf verified"}"#,
        )]),
        text_response("Verified the change."),
    ]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let mut engine = AgentEngine::new_with_config_and_llm(temp.path(), phase_loop_config(), llm)?;
    engine.set_permission_mode("bypassPermissions");

    let phase_chunks = Arc::new(Mutex::new(Vec::new()));
    let phase_chunks_clone = phase_chunks.clone();
    engine.set_stream_callback(Arc::new(move |chunk| {
        if let StreamChunk::PhaseTransition { from, to } = chunk {
            phase_chunks_clone.lock().unwrap().push((from, to));
        }
    }));

    let planning_output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Refactor the parser across multiple steps, then verify the result.",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;
    assert!(planning_output.contains("Plan saved"));

    let store = Store::new(temp.path())?;
    let session = store.load_latest_session()?.expect("session");
    assert_eq!(session.status, SessionState::AwaitingApproval);

    let final_output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Go ahead and implement the approved plan.",
        ChatOptions {
            tools: true,
            session_id: Some(session.session_id),
            ..Default::default()
        },
    ))?;

    assert!(final_output.contains("Verified the change"));
    assert!(fs::read_to_string(temp.path().join("src/main.rs"))?.contains("println!(\"ok\")"));

    let phases = phase_chunks.lock().unwrap().clone();
    assert_eq!(
        phases,
        vec![
            ("explore".to_string(), "plan".to_string()),
            ("plan".to_string(), "execute".to_string()),
            ("execute".to_string(), "verify".to_string()),
        ]
    );

    let requests = captured.lock().unwrap();
    assert!(
        requests.len() >= 5,
        "expected captured requests from both planning and execution"
    );
    let execution_request = &requests[4];
    let system_messages = execution_request
        .messages
        .iter()
        .filter_map(|msg| match msg {
            ChatMessage::System { content } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        system_messages
            .iter()
            .any(|content| content.contains("## Active Approved Plan")),
        "execution request should include the approved plan context"
    );
    assert!(
        system_messages
            .iter()
            .any(|content| content.contains("Update parser wiring")),
        "execution request should include persisted plan steps"
    );
    Ok(())
}

#[test]
fn openai_compatible_surface_prefers_patch_direct_in_requests() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    let mut cfg = AppConfig::default();
    cfg.llm.provider = "openai-compatible".to_string();

    let (llm, captured) = CapturingLlm::new(vec![text_response("Surface ready.")]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_config_and_llm(temp.path(), cfg, llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Fix a bug in src/main.rs.",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    let tool_names = requests[0]
        .tools
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"patch_direct"));
    assert!(!tool_names.contains(&"fs_edit"));
    assert!(!tool_names.contains(&"multi_edit"));
    assert!(tool_names.len() <= 18);
    Ok(())
}

#[test]
fn ollama_surface_clamps_request_tool_count() -> Result<()> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;
    let mut cfg = AppConfig::default();
    cfg.llm.provider = "ollama".to_string();

    let (llm, captured) = CapturingLlm::new(vec![text_response("Surface ready.")]);
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(llm);
    let engine = AgentEngine::new_with_config_and_llm(temp.path(), cfg.clone(), llm)?;

    let _output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Refactor src/main.rs and add verification.",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;

    let requests = captured.lock().unwrap();
    let tool_names = requests[0]
        .tools
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect::<Vec<_>>();

    let model_name = cfg
        .llm
        .providers
        .get("ollama")
        .map(|provider| provider.models.chat.clone())
        .unwrap_or_else(|| "qwen2.5-coder:7b".to_string());
    let capabilities = model_capabilities(ProviderKind::Ollama, &model_name);
    let preferred_edit = match capabilities.preferred_edit_tool {
        PreferredEditTool::FsEdit => "fs_edit",
        PreferredEditTool::MultiEdit => "multi_edit",
        PreferredEditTool::PatchDirect => "patch_direct",
    };

    assert!(
        tool_names.contains(&preferred_edit),
        "surface should include preferred edit tool: {preferred_edit}"
    );
    assert!(tool_names.contains(&"tool_search"));
    if preferred_edit != "patch_direct" {
        assert!(!tool_names.contains(&"patch_direct"));
    }
    assert!(tool_names.len() <= capabilities.max_safe_tool_count);
    Ok(())
}

#[test]
fn p0_untrusted_shell_needs_an_explicit_approval() -> Result<()> {
    for approved in [false, true] {
        let workspace = tempfile::tempdir()?;
        init_workspace(workspace.path())?;
        let config = AppConfig {
            workspace_trust: Some(false),
            hooks: serde_json::json!({"PermissionRequest":[{"hooks":[{"type":"command",
                "command":if cfg!(windows) { r#"echo {"permissionDecision":"allow"}"# }
                    else { r#"printf '%s' '{"permissionDecision":"allow"}'"# }
            }]}]}),
            ..Default::default()
        };
        let call = serde_json::json!({"command": "git status"});
        let llm = Box::new(ScriptedLlm::new(vec![
            tool_call_response(vec![("p0-command", "bash_run", &call.to_string())]),
            text_response("The approval fixture is complete."),
        ]));
        let mut engine = AgentEngine::new_with_config_and_llm(workspace.path(), config, llm)?;
        engine.set_permission_mode("bypassPermissions");
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if approved {
            let count = count.clone();
            engine.set_approval_handler(Box::new(move |_| {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(true)
            }));
        }
        codingbuddy_core::runtime::block_on(engine.chat_with_options(
            "Run the requested approval fixture.",
            ChatOptions {
                tools: true,
                ..Default::default()
            },
        ))?;
        let store = Store::new(workspace.path())?;
        let session = store.load_latest_session()?.expect("session");
        let projection = store.rebuild_from_events(session.session_id)?;
        assert_eq!(projection.approved_invocations.len(), usize::from(approved));
        assert_eq!(projection.denied_invocations.len(), usize::from(!approved));
        let results = store.session_events(session.session_id)?.into_iter()
            .filter(|event| matches!(&event.kind, codingbuddy_core::EventKind::ToolResult { result } if result.success))
            .count();
        assert_eq!(results, usize::from(approved));
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            usize::from(approved)
        );
    }
    Ok(())
}

#[test]
fn p0_untrusted_edit_skips_automatic_verification() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    init_workspace(workspace.path())?;
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"verification-fixture\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(
        workspace.path().join("build.rs"),
        r#"fn main() { std::fs::write("build-marker", "ran").unwrap(); }"#,
    )?;
    let mut config = AppConfig {
        workspace_trust: Some(false),
        ..Default::default()
    };
    config.policy.lint_after_edit = Some(if cfg!(windows) {
        "echo ran>lint-marker".into()
    } else {
        "touch lint-marker".into()
    });
    config.tools.diagnostics_after_edit = true;
    config.lsp.enabled = true;
    let llm = Box::new(ScriptedLlm::new(vec![
        tool_call_response(vec![(
            "p0-edit",
            "fs_edit",
            r#"{"path":"src/main.rs","search":"fn main() {}","replace":"fn main() { println!(\"edited\"); }"}"#,
        )]),
        text_response("The edit fixture is complete."),
    ]));
    let mut engine = AgentEngine::new_with_config_and_llm(workspace.path(), config, llm)?;
    engine.set_permission_mode("bypassPermissions");
    codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Make the fixture edit.",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;
    assert!(fs::read_to_string(workspace.path().join("src/main.rs"))?.contains("edited"));
    assert!(!workspace.path().join("build-marker").exists());
    assert!(!workspace.path().join("lint-marker").exists());
    Ok(())
}

#[test]
fn delivery_zero_explicit_deny_survives_bypass_and_hooks() -> Result<()> {
    for rewrite in [false, true] {
        let workspace = tempfile::tempdir()?;
        init_workspace(workspace.path())?;
        let mut config = AppConfig {
            workspace_trust: Some(true),
            ..Default::default()
        };
        let output = if rewrite {
            r#"{"updatedInput":{"command":"git status"}}"#
        } else {
            r#"{"permissionDecision":"allow"}"#
        };
        let hook = if cfg!(windows) {
            format!("echo {output}")
        } else {
            format!("printf '%s' '{output}'")
        };
        let event = if rewrite {
            "PreToolUse"
        } else {
            "PermissionRequest"
        };
        config.hooks = serde_json::json!({
            (event): [{"hooks":[{"type":"command","command":hook}]}]
        });
        config.policy.allowlist = vec!["git status".into(), "git diff".into()];
        config
            .policy
            .permission_rules
            .push(codingbuddy_core::PermissionRuleConfig {
                rule: "Bash(git status)".into(),
                decision: "deny".into(),
            });
        let command = if rewrite { "git diff" } else { "git status" };
        let arguments = serde_json::json!({"command":command}).to_string();
        let llm = Box::new(ScriptedLlm::new(vec![
            tool_call_response(vec![("delivery-zero-shell", "bash_run", &arguments)]),
            text_response("The permission fixture is complete."),
        ]));
        let mut engine = AgentEngine::new_with_config_and_llm(workspace.path(), config, llm)?;
        engine.set_permission_mode("bypassPermissions");
        engine.set_approval_handler(Box::new(|_| {
            panic!("denied tool opened an approval prompt")
        }));
        codingbuddy_core::runtime::block_on(engine.chat_with_options(
            "Run the permission fixture.",
            ChatOptions {
                tools: true,
                ..Default::default()
            },
        ))?;
        let store = Store::new(workspace.path())?;
        let session = store.load_latest_session()?.expect("session");
        let projection = store.rebuild_from_events(session.session_id)?;
        assert_eq!(projection.denied_invocations.len(), 1);
        assert!(projection.approved_invocations.is_empty());
    }
    Ok(())
}

#[test]
fn delivery_three_worktree_executes_scoped_and_legacy_merge_only_stages() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let root = workspace.path();
    init_workspace(root)?;
    fs::write(root.join(".gitignore"), ".codingbuddy/\n")?;
    for args in [
        vec!["add", "--", "src/main.rs", ".gitignore"],
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
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::write(root.join("src/main.rs"), "fn current_input() {}\n")?;
    let index = fs::read(root.join(".git/index"))?;
    let config = AppConfig {
        workspace_trust: Some(true),
        ..Default::default()
    };
    let responses = vec![
        tool_call_response(vec![(
            "enter",
            "enter_worktree",
            r#"{"branch_name":"fixture"}"#,
        )]),
        tool_call_response(vec![(
            "write",
            "fs_write",
            r#"{"path":"src/main.rs","content":"fn isolated_output() {}\n"}"#,
        )]),
        tool_call_response(vec![("exit", "exit_worktree", r#"{"action":"merge"}"#)]),
        text_response("Output staged for local review."),
    ];
    let mut engine =
        AgentEngine::new_with_config_and_llm(root, config, Box::new(ScriptedLlm::new(responses)))?;
    engine.set_permission_mode("bypassPermissions");
    codingbuddy_core::runtime::block_on(engine.chat_with_options(
        "Use an isolated worktree to update this Rust file, then stage its output.",
        ChatOptions {
            tools: true,
            ..Default::default()
        },
    ))?;
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs"))?,
        "fn current_input() {}\n"
    );
    assert_eq!(fs::read(root.join(".git/index"))?, index);
    let patches = codingbuddy_diff::PatchStore::new(root)?.list()?;
    assert_eq!(patches.len(), 1);
    assert!(patches[0].unified_diff.contains("isolated_output"));
    assert!(!patches[0].applied);
    Ok(())
}
