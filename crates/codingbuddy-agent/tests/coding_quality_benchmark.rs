use anyhow::{Result, anyhow};
use codingbuddy_agent::{AgentEngine, ChatOptions};
use codingbuddy_core::{
    ChatRequest, FimRequest, LlmRequest, LlmResponse, LlmToolCall, StreamCallback, TokenUsage,
};
use codingbuddy_llm::LlmClient;
use codingbuddy_store::Store;
use codingbuddy_testkit::{
    CodingBenchmarkCaseResult, CodingBenchmarkGateThresholds, CodingBenchmarkReport, ScriptedLlm,
    evaluate_coding_benchmark_gate_with_thresholds, read_coding_benchmark_report,
    write_coding_benchmark_report,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
struct SharedScriptedLlm(Arc<ScriptedLlm>);

impl LlmClient for SharedScriptedLlm {
    fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        self.0.complete(req)
    }

    fn complete_streaming(&self, req: &LlmRequest, cb: StreamCallback) -> Result<LlmResponse> {
        self.0.complete_streaming(req, cb)
    }

    fn complete_chat(&self, req: &ChatRequest) -> Result<LlmResponse> {
        self.0.complete_chat(req)
    }

    fn complete_chat_streaming(
        &self,
        req: &ChatRequest,
        cb: StreamCallback,
    ) -> Result<LlmResponse> {
        self.0.complete_chat_streaming(req, cb)
    }

    fn complete_fim(&self, req: &FimRequest) -> Result<LlmResponse> {
        self.0.complete_fim(req)
    }

    fn complete_fim_streaming(&self, req: &FimRequest, cb: StreamCallback) -> Result<LlmResponse> {
        self.0.complete_fim_streaming(req, cb)
    }
}

fn tool_call_response(calls: Vec<(&str, &str, &str)>) -> LlmResponse {
    LlmResponse {
        text: String::new(),
        finish_reason: "tool_calls".to_string(),
        reasoning_content: String::new(),
        tool_calls: calls
            .into_iter()
            .map(|(id, name, args)| LlmToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args.to_string(),
            })
            .collect(),
        usage: Some(TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 40,
            ..Default::default()
        }),
    }
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.to_string(),
        finish_reason: "stop".to_string(),
        reasoning_content: String::new(),
        tool_calls: vec![],
        usage: Some(TokenUsage {
            prompt_tokens: 50,
            completion_tokens: 20,
            ..Default::default()
        }),
    }
}

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

fn build_engine(path: &Path, responses: Vec<LlmResponse>) -> Result<AgentEngine> {
    let llm = Arc::new(ScriptedLlm::new(responses));
    let llm: Box<dyn LlmClient + Send + Sync> = Box::new(SharedScriptedLlm(llm));
    AgentEngine::new_with_llm(path, llm)
}

struct BenchmarkCaseSpec {
    case_id: &'static str,
    category: &'static str,
    prompt: &'static str,
    setup_files: &'static [(&'static str, &'static str)],
    responses: Vec<LlmResponse>,
    expected_output_contains: &'static str,
    expected_file_contains: &'static [(&'static str, &'static str)],
    min_tool_invocations: usize,
}

fn run_case(spec: BenchmarkCaseSpec) -> Result<CodingBenchmarkCaseResult> {
    let temp = tempfile::tempdir()?;
    init_workspace(temp.path())?;

    for (rel, content) in spec.setup_files {
        let file_path = temp.path().join(rel);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(file_path, content)?;
    }

    let mut scripted = spec.responses;
    // Keep one fallback turn so benchmark cases do not fail from harmless
    // extra model round-trips (e.g. additional acknowledgment turn).
    scripted.push(text_response("Benchmark fallback completion."));
    let mut engine = build_engine(temp.path(), scripted)?;
    engine.set_permission_mode("bypassPermissions");
    engine.set_max_turns(Some(12));

    let started = Instant::now();
    let output = engine.chat_with_options(
        spec.prompt,
        ChatOptions {
            tools: true,
            session_id: None,
            ..Default::default()
        },
    )?;
    let duration_ms = started.elapsed().as_millis();

    let store = Store::new(temp.path())?;
    let session_id = store
        .load_latest_session()?
        .ok_or_else(|| anyhow!("expected benchmark session to exist"))?
        .session_id;
    let projection = store.rebuild_from_events(session_id)?;
    let tool_invocations = projection.tool_invocations.len();
    let retries = tool_invocations.saturating_sub(spec.min_tool_invocations);

    let mut notes = Vec::new();
    if !spec.expected_output_contains.is_empty() && !output.contains(spec.expected_output_contains)
    {
        notes.push(format!(
            "output missing expected marker '{}'",
            spec.expected_output_contains
        ));
    }
    if tool_invocations < spec.min_tool_invocations {
        notes.push(format!(
            "tool invocations below minimum (got {tool_invocations}, expected at least {})",
            spec.min_tool_invocations
        ));
    }

    for (rel, expected_snippet) in spec.expected_file_contains {
        let actual = fs::read_to_string(temp.path().join(rel))
            .map_err(|err| anyhow!("failed to read {rel}: {err}"))?;
        if !actual.contains(expected_snippet) {
            notes.push(format!(
                "file '{rel}' missing expected snippet '{expected_snippet}'"
            ));
        }
    }

    let passed = notes.is_empty();
    let completion_quality_score = if passed {
        1.0
    } else if notes.len() == 1 {
        0.5
    } else {
        0.0
    };

    Ok(CodingBenchmarkCaseResult {
        case_id: spec.case_id.to_string(),
        category: spec.category.to_string(),
        passed,
        tool_invocations,
        retries,
        completion_quality_score,
        duration_ms,
        note: if notes.is_empty() {
            None
        } else {
            Some(notes.join("; "))
        },
    })
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .expect("workspace root")
}

#[test]
fn coding_quality_benchmark_suite() -> Result<()> {
    let results = vec![
        run_case(BenchmarkCaseSpec {
            case_id: "edit-single-file",
            category: "edit",
            prompt: "Add a print statement to main",
            setup_files: &[],
            responses: vec![
                tool_call_response(vec![(
                    "call_1",
                    "fs_edit",
                    r#"{"path":"src/main.rs","search":"fn main() {}","replace":"fn main() { println!(\"hi\"); }"}"#,
                )]),
                text_response("Applied edit to main.rs."),
            ],
            expected_output_contains: "",
            expected_file_contains: &[("src/main.rs", "println!(\"hi\");")],
            min_tool_invocations: 1,
        })?,
        run_case(BenchmarkCaseSpec {
            case_id: "debug-bugfix",
            category: "debug",
            prompt: "Fix divide-by-zero in src/math.rs",
            setup_files: &[(
                "src/math.rs",
                "pub fn divide(a: i32, b: i32) -> i32 { a / 0 }\n",
            )],
            responses: vec![
                tool_call_response(vec![("call_1", "fs_read", r#"{"path":"src/math.rs"}"#)]),
                tool_call_response(vec![(
                    "call_2",
                    "fs_edit",
                    r#"{"path":"src/math.rs","search":"a / 0","replace":"a / b"}"#,
                )]),
                text_response("Fixed division bug."),
            ],
            expected_output_contains: "Fixed division",
            expected_file_contains: &[("src/math.rs", "a / b")],
            min_tool_invocations: 2,
        })?,
        run_case(BenchmarkCaseSpec {
            case_id: "refactor-rename",
            category: "refactor",
            prompt: "Rename calc_sum to sum_values in src/lib.rs",
            setup_files: &[(
                "src/lib.rs",
                "pub fn calc_sum(a: i32, b: i32) -> i32 { a + b }\n",
            )],
            responses: vec![
                tool_call_response(vec![(
                    "call_1",
                    "fs_edit",
                    r#"{"path":"src/lib.rs","search":"calc_sum","replace":"sum_values"}"#,
                )]),
                text_response("Refactor complete."),
            ],
            expected_output_contains: "Refactor complete",
            expected_file_contains: &[("src/lib.rs", "sum_values")],
            min_tool_invocations: 1,
        })?,
        run_case(BenchmarkCaseSpec {
            case_id: "multi-file-update",
            category: "multi-file",
            prompt: "Rename foo to bar in src/a.rs and src/b.rs",
            setup_files: &[
                ("src/a.rs", "pub fn foo() -> i32 { 1 }\n"),
                ("src/b.rs", "pub fn foo() -> i32 { 2 }\n"),
            ],
            responses: vec![
                tool_call_response(vec![(
                    "call_1",
                    "fs_edit",
                    r#"{"path":"src/a.rs","search":"foo","replace":"bar"}"#,
                )]),
                tool_call_response(vec![(
                    "call_2",
                    "fs_edit",
                    r#"{"path":"src/b.rs","search":"foo","replace":"bar"}"#,
                )]),
                text_response("Updated both files."),
            ],
            expected_output_contains: "Updated both files",
            expected_file_contains: &[("src/a.rs", "bar"), ("src/b.rs", "bar")],
            min_tool_invocations: 2,
        })?,
    ];

    let report = CodingBenchmarkReport::from_case_results(
        "coding-quality-core",
        "scripted-tool-loop",
        results,
    );
    assert_eq!(report.summary.total_cases, 4);
    assert!(
        report.summary.pass_rate_pct >= 75.0,
        "benchmark suite pass rate too low: {:.1}%",
        report.summary.pass_rate_pct
    );

    let root = repo_root();
    let output_dir = root.join(".codingbuddy/benchmarks");
    let report_path = write_coding_benchmark_report(&output_dir, &report)?;
    println!("coding_quality_benchmark_report={}", report_path.display());

    let baseline_path = root.join("docs/benchmarks/coding_quality_baseline.json");
    if baseline_path.exists() {
        let baseline = read_coding_benchmark_report(&baseline_path)?;
        let gate = evaluate_coding_benchmark_gate_with_thresholds(
            &report,
            &baseline,
            CodingBenchmarkGateThresholds {
                max_pass_rate_drop_pct: 5.0,
                max_quality_score_drop: 0.10,
                max_avg_retries_increase: 0.50,
            },
        );
        assert!(
            gate.passed,
            "coding benchmark regression: compatible={} pass_rate current={:.1}% baseline={:.1}% delta={:.1}% allowed_drop={:.1}% quality current={:.3} baseline={:.3} delta={:.3} allowed_drop={:.3} retries current={:.3} baseline={:.3} delta={:.3} allowed_increase={:.3}",
            gate.suite_model_compatible,
            gate.current_pass_rate_pct,
            gate.baseline_pass_rate_pct,
            gate.delta_pct,
            gate.allowed_drop_pct,
            gate.current_avg_completion_quality_score,
            gate.baseline_avg_completion_quality_score,
            gate.quality_delta,
            gate.max_quality_drop,
            gate.current_avg_retries,
            gate.baseline_avg_retries,
            gate.retries_delta,
            gate.max_retry_increase
        );
    }

    Ok(())
}
