//! Replays provider traffic through the real `ApiClient` and `AgentEngine`.
//!
//! Every cassette under `cassettes/<provider>/<scenario>.json` is replayed on
//! each test run, with no network and no keys. See `cassettes/README.md` for
//! how to record new ones with `record_cassettes`.
#![allow(clippy::unwrap_used)] // integration test helpers may panic
use anyhow::{Context, Result, bail, ensure};
use codingbuddy_agent::{AgentEngine, ChatOptions};
use codingbuddy_core::{AppConfig, StreamChunk};
use codingbuddy_testkit::cassette::{
    CASSETTE_SCHEMA_VERSION, Cassette, Expectations, RecordingProxy, ReplayServer,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn cassette_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("cassettes")
}

fn cassette_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let root = cassette_root();
    for provider in std::fs::read_dir(&root).with_context(|| format!("{}", root.display()))? {
        let provider = provider?.path();
        if !provider.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&provider)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// Point `provider`'s profile at `origin`, keeping the profile's own path
/// prefix (e.g. `/v1`), the way the real base URL would be used.
fn route_provider_to(cfg: &mut AppConfig, provider: &str, origin: &str) -> Result<String> {
    cfg.llm.select_provider(provider);
    let profile = cfg
        .llm
        .providers
        .get_mut(provider)
        .with_context(|| format!("no provider profile named {provider}"))?;
    ensure!(
        profile.cloud.is_none(),
        "{provider} uses cloud identity; only API-key HTTP providers can be recorded"
    );
    let url = reqwest::Url::parse(&profile.base_url)
        .with_context(|| format!("bad base_url for {provider}: {}", profile.base_url))?;
    let upstream = url.origin().ascii_serialization();
    let suffix = url.path().trim_end_matches('/').to_string();
    profile.base_url = format!("{origin}{suffix}");
    Ok(upstream)
}

#[derive(Debug, Default)]
struct Run {
    output: String,
    tool_calls: Vec<String>,
    chunks: Vec<StreamChunk>,
}

fn run_engine(workspace: &Path, cfg: AppConfig, prompt: &str) -> Result<Run> {
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let engine = AgentEngine::new_with_config_and_llm(
        workspace,
        cfg.clone(),
        Box::new(codingbuddy_llm::ApiClient::new(cfg.llm.clone())?),
    )?;
    engine.set_stream_callback(Arc::new({
        let chunks = chunks.clone();
        move |chunk| {
            if let Ok(mut list) = chunks.lock() {
                list.push(chunk);
            }
        }
    }));
    let output = codingbuddy_core::runtime::block_on(engine.chat_with_options(
        prompt,
        ChatOptions {
            tools: true,
            disable_team_orchestration: true,
            ..Default::default()
        },
    ))?;
    drop(engine);
    let chunks = Arc::try_unwrap(chunks)
        .map_err(|_| anyhow::anyhow!("stream callback still referenced"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("stream log poisoned"))?;
    let tool_calls = chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ToolCallStart { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect();
    Ok(Run {
        output,
        tool_calls,
        chunks,
    })
}

/// The answer a user is left looking at: text streamed after the last tool
/// call, with `ClearStreamingText` applied the way the UIs apply it.
fn final_visible_text(chunks: &[StreamChunk]) -> String {
    let mut segment = String::new();
    for chunk in chunks {
        match chunk {
            StreamChunk::ContentDelta(text) => segment.push_str(text),
            StreamChunk::ClearStreamingText
            | StreamChunk::ToolCallStart { .. }
            | StreamChunk::ToolCallEnd { .. } => segment.clear(),
            _ => {}
        }
    }
    segment
}

fn prepare_workspace(files: &BTreeMap<String, String>) -> Result<tempfile::TempDir> {
    let workspace = tempfile::tempdir()?;
    for (name, contents) in files {
        let path = workspace.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(workspace)
}

fn replay_config(cassette: &Cassette, origin: &str) -> Result<AppConfig> {
    let mut cfg = AppConfig::default();
    route_provider_to(&mut cfg, &cassette.provider, origin)?;
    let profile = cfg.llm.active_provider();
    let mut profile = profile.clone();
    profile.models.chat.clone_from(&cassette.model);
    cfg.llm.providers.insert(cassette.provider.clone(), profile);
    cfg.llm.api_key = Some("replay-placeholder-key".into());
    cfg.llm.stream = cassette.stream;
    cfg.llm.max_retries = 0;
    cfg.llm.timeout_seconds = 10;
    cfg.workspace_trust = Some(false);
    Ok(cfg)
}

fn replay(cassette: &Cassette) -> Result<()> {
    let server = ReplayServer::start(cassette)?;
    let workspace = prepare_workspace(&cassette.workspace)?;
    let cfg = replay_config(cassette, server.base())?;
    let run = match run_engine(workspace.path(), cfg, &cassette.prompt) {
        Ok(run) => run,
        Err(error) => {
            let served: Vec<String> = server
                .served()
                .iter()
                .map(|request| format!("{} {}", request.method, request.path))
                .collect();
            bail!(
                "{error:#}\n  requests served: {served:?}\n  server errors: {:?}",
                server.errors()
            );
        }
    };

    let errors = server.errors();
    ensure!(errors.is_empty(), "replay mismatches: {errors:#?}");
    let served = server.served().len();
    ensure!(
        served == cassette.interactions.len(),
        "engine made {served} model requests, the cassette recorded {}",
        cassette.interactions.len()
    );
    ensure!(
        run.tool_calls == cassette.expect.tool_calls,
        "tool calls {:?} differ from recorded {:?}",
        run.tool_calls,
        cassette.expect.tool_calls
    );
    ensure!(
        run.output == cassette.expect.output,
        "output differs from recorded\n  got:      {:?}\n  recorded: {:?}",
        run.output,
        cassette.expect.output
    );
    if cassette.stream {
        ensure!(
            matches!(run.chunks.last(), Some(StreamChunk::Done { .. })),
            "stream did not end with Done"
        );
        ensure!(
            run.chunks
                .iter()
                .filter(|chunk| matches!(chunk, StreamChunk::Done { .. }))
                .count()
                == 1,
            "stream sent Done more than once"
        );
        let visible = final_visible_text(&run.chunks);
        ensure!(
            visible.trim() == run.output.trim(),
            "the answer on screen differs from the final output\n  screen: {visible:?}\n  output: {:?}",
            run.output
        );
    }
    Ok(())
}

#[test]
fn replay_all_cassettes() -> Result<()> {
    let paths = cassette_paths()?;
    ensure!(!paths.is_empty(), "no cassettes found");
    let mut failures = Vec::new();
    for path in &paths {
        let name = path
            .strip_prefix(cassette_root())
            .unwrap_or(path)
            .display()
            .to_string();
        let result = Cassette::load(path).and_then(|cassette| replay(&cassette));
        if let Err(error) = result {
            failures.push(format!("{name}: {error:#}"));
        }
    }
    if !failures.is_empty() {
        bail!(
            "{} of {} cassettes failed:\n{}\n\nIf a behaviour change is intended, re-record with \
             `CODINGBUDDY_RECORD=1 cargo test -p codingbuddy-testkit --test provider_replay \
             record_cassettes -- --ignored` (see cassettes/README.md).",
            failures.len(),
            paths.len(),
            failures.join("\n")
        );
    }
    Ok(())
}

#[test]
fn final_visible_text_applies_clears_and_tool_boundaries() {
    let chunks = vec![
        StreamChunk::ContentDelta("draft ".into()),
        StreamChunk::ClearStreamingText,
        StreamChunk::ContentDelta("Checking.".into()),
        StreamChunk::ToolCallStart {
            tool_name: "fs_read".into(),
            args_summary: String::new(),
        },
        StreamChunk::ContentDelta("Answer ".into()),
        StreamChunk::ContentDelta("here.".into()),
        StreamChunk::Done { reason: None },
    ];
    assert_eq!(final_visible_text(&chunks), "Answer here.");
}

// ── Recording ───────────────────────────────────────────────────────────

struct Scenario {
    name: &'static str,
    prompt: &'static str,
    files: &'static [(&'static str, &'static str)],
}

const NOTES: &str = "Release checklist\n- Bump the version in Cargo.toml\n- Update CHANGELOG.md\n\
- Tag the release and push the tag\n";
const CONFIG: &str = "[server]\nhost = \"127.0.0.1\"\nport = 8471\n";

/// Scenarios chosen for the stream shapes that have broken before: text
/// before a tool call, several tool calls, and a plain answer.
const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "plain_answer",
        prompt: "Without using any tools, answer in two short sentences: what problem does a \
                 mutex solve?",
        files: &[],
    },
    Scenario {
        name: "text_then_tool",
        prompt: "Before you use any tool, explain in three or four sentences what you are going \
                 to check and why. Then read notes.txt and summarize it in one sentence.",
        files: &[("notes.txt", NOTES)],
    },
    Scenario {
        name: "two_files",
        prompt: "Read notes.txt and config.toml, then tell me which port config.toml sets.",
        files: &[("notes.txt", NOTES), ("config.toml", CONFIG)],
    },
];

fn env_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// Records real traffic. Costs a few cents per provider and needs that
/// provider's API key in the environment or user settings.
///
/// `CODINGBUDDY_RECORD=1 CODINGBUDDY_RECORD_PROVIDERS=google,deepseek \
///  cargo test -p codingbuddy-testkit --test provider_replay record_cassettes -- --ignored`
///
/// Optional: `CODINGBUDDY_RECORD_SCENARIOS=text_then_tool`,
/// `CODINGBUDDY_RECORD_MODEL=<model id>`.
#[test]
#[ignore = "calls real provider APIs; run explicitly to record cassettes"]
fn record_cassettes() -> Result<()> {
    ensure!(
        std::env::var("CODINGBUDDY_RECORD").is_ok_and(|value| value == "1"),
        "set CODINGBUDDY_RECORD=1 to confirm you want to call real provider APIs"
    );
    let providers = env_list("CODINGBUDDY_RECORD_PROVIDERS");
    ensure!(
        !providers.is_empty(),
        "set CODINGBUDDY_RECORD_PROVIDERS, e.g. google,deepseek"
    );
    let only = env_list("CODINGBUDDY_RECORD_SCENARIOS");
    let model_override = std::env::var("CODINGBUDDY_RECORD_MODEL").ok();

    for provider in &providers {
        for scenario in SCENARIOS {
            if !only.is_empty() && !only.iter().any(|name| name == scenario.name) {
                continue;
            }
            let files: BTreeMap<String, String> = scenario
                .files
                .iter()
                .map(|(name, body)| ((*name).to_string(), (*body).to_string()))
                .collect();
            let workspace = prepare_workspace(&files)?;

            // The user's own settings supply the provider profile and key.
            let mut cfg = AppConfig::load(workspace.path())?;
            cfg.workspace_trust = Some(false);
            cfg.llm.stream = true;
            cfg.llm.max_retries = 0;
            let upstream = {
                let mut probe = cfg.clone();
                route_provider_to(&mut probe, provider, "")?
            };
            let proxy = RecordingProxy::start(&upstream)?;
            route_provider_to(&mut cfg, provider, proxy.base())?;
            if let Some(model) = &model_override {
                let mut profile = cfg.llm.active_provider();
                profile.models.chat.clone_from(model);
                cfg.llm.providers.insert(provider.clone(), profile);
            }
            let model = cfg.llm.active_base_model();

            let run = run_engine(workspace.path(), cfg, scenario.prompt);
            let interactions = proxy.finish()?;
            let run = run.with_context(|| format!("{provider}/{} failed", scenario.name))?;

            let cassette = Cassette {
                schema_version: CASSETTE_SCHEMA_VERSION,
                source: "recorded".into(),
                provider: provider.clone(),
                model,
                scenario: scenario.name.into(),
                recorded_at: chrono_now(),
                prompt: scenario.prompt.into(),
                workspace: files,
                stream: true,
                interactions,
                expect: Expectations {
                    tool_calls: run.tool_calls,
                    output: run.output,
                },
            };
            // A cassette that cannot replay is useless; check before saving.
            replay(&cassette).with_context(|| {
                format!("{provider}/{} recorded but does not replay", scenario.name)
            })?;
            let path = cassette_root()
                .join(provider)
                .join(format!("{}.json", scenario.name));
            cassette.save(&path)?;
            eprintln!("recorded {}", path.display());
        }
    }
    Ok(())
}

fn chrono_now() -> String {
    // Avoids a chrono dependency in this crate for one timestamp.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    format!("unix:{secs}")
}
