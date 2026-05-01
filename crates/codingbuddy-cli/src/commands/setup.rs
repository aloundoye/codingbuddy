use anyhow::Result;
use codingbuddy_core::{AppConfig, KNOWN_PROVIDER_ENV_VARS};
use codingbuddy_local_ml::model_registry;
use codingbuddy_local_ml::{ModelManager, ModelStatus};
use serde_json::json;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::SetupArgs;
use crate::output::print_json;

/// Marker file that records we already ran first-time setup.
const SETUP_MARKER: &str = ".setup_done";

/// Counter file that tracks how many times the soft onboarding banner has been shown.
const BANNER_COUNTER: &str = ".banner_count";

/// Maximum number of times to show the soft onboarding banner before auto-dismissing.
const MAX_BANNER_SHOWS: u32 = 4;

/// Called from `run_chat` on first run. Interactive terminals may launch the
/// setup wizard; non-interactive sessions receive a compact banner only.
///
/// Returns `true` if config was potentially modified (only when fully configured
/// and marker is auto-written).
pub(crate) fn maybe_first_time_setup(cwd: &Path, cfg: &AppConfig) -> Result<bool> {
    let settings_dir = AppConfig::project_settings_path(cwd)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| cwd.join(".codingbuddy"));

    let marker = settings_dir.join(SETUP_MARKER);

    if marker.exists() {
        return Ok(false);
    }

    // If an API key is available (either explicitly configured or auto-detected
    // from env vars by AppConfig::ensure → auto_detect_provider), show a
    // one-time confirmation message and write the marker.
    if has_api_key(cfg) {
        let provider_name = &cfg.llm.provider;
        let model = cfg.llm.active_base_model();
        let env_var = &cfg.llm.active_provider().api_key_env;
        write_setup_marker(&marker)?;
        eprintln!(
            "  \x1b[32m\u{2713}\x1b[0m Using \x1b[1m{provider_name}\x1b[0m ({env_var}). Model: \x1b[1m{model}\x1b[0m."
        );
        eprintln!("    Run \x1b[1mcodingbuddy setup\x1b[0m to customize or enable local ML.");
        eprintln!();
        return Ok(false);
    }

    // No provider detected — show compact listing (interactive terminals only)
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Ok(false);
    }

    // Check banner counter — stop showing after MAX_BANNER_SHOWS
    let counter_path = settings_dir.join(BANNER_COUNTER);
    let count = read_banner_count(&counter_path);
    if count >= MAX_BANNER_SHOWS {
        return Ok(false);
    }

    eprintln!("  \x1b[33m!\x1b[0m No API key detected.");
    eprintln!(
        "    CodingBuddy can set up a model provider now: DeepSeek, OpenAI, Anthropic, Gemini, OpenRouter, Ollama, and more."
    );
    if prompt_yes_no("    Run interactive setup now? [Y/n]: ")? {
        run_wizard_steps(cwd, cfg)?;
        write_setup_marker(&marker)?;
        return Ok(true);
    }

    eprintln!("    Set one of these environment variables when you are ready:");
    for &(name, env_var) in KNOWN_PROVIDER_ENV_VARS {
        eprintln!("    \x1b[1m{env_var}\x1b[0m  ({name})");
    }
    eprintln!("    Or run \x1b[1mcodingbuddy setup\x1b[0m to configure interactively.");
    eprintln!();

    write_banner_count(&counter_path, count + 1);

    Ok(false)
}

fn read_banner_count(path: &Path) -> u32 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_banner_count(path: &Path, count: u32) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, count.to_string());
}

fn write_setup_marker(marker: &Path) -> Result<()> {
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(marker, "")?;
    Ok(())
}

/// Collected wizard choices — written atomically after all steps complete.
struct WizardState {
    /// Built-in provider/model selection.
    model_selection: Option<ModelSelectionChoice>,
    /// Custom provider info.
    custom_provider: Option<CustomProviderChoice>,
    /// Whether to enable local ML.
    enable_ml: bool,
    /// Detected device string (e.g. "metal", "cuda", "cpu").
    detected_device: Option<String>,
    /// Recommended completion model ID.
    recommended_model: Option<&'static str>,
    /// Whether to enable privacy scanning.
    enable_privacy: bool,
}

struct ModelSelectionChoice {
    provider: String,
    model: String,
    api_key_env: Option<String>,
}

struct CustomProviderChoice {
    base_url: String,
    model_name: String,
    api_key_env: Option<String>,
}

/// Shared 4-step wizard logic used by both `maybe_first_time_setup` and `run_interactive_wizard`.
///
/// All choices are collected into `WizardState` and written atomically at the end,
/// so partial config is never persisted if the user cancels mid-wizard.
fn run_wizard_steps(cwd: &Path, cfg: &AppConfig) -> Result<()> {
    // Step 1: Model selection
    println!("[1/4] Model");
    let model_choices = setup_model_choices(cfg);
    for (idx, item) in model_choices.iter().enumerate() {
        println!(
            "  {}. {:<18} {:<32} {}",
            idx + 1,
            item.provider,
            item.id,
            setup_model_summary(item)
        );
    }
    let custom_choice = model_choices.len() + 1;
    println!("  {custom_choice}. Custom OpenAI-compatible endpoint");
    println!();

    let provider_choice = prompt_choice(
        &format!("  Select [1-{custom_choice}]: "),
        1,
        custom_choice as u32,
    )? as usize;

    let (model_selection, custom_provider) = if provider_choice == custom_choice {
        let base_url = loop {
            print!("  API base URL: ");
            std::io::stdout().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let url = input.trim().to_string();
            if url.starts_with("http://") || url.starts_with("https://") {
                break url;
            }
            println!("  Invalid URL — must start with http:// or https://");
        };

        print!("  Model name: ");
        std::io::stdout().flush()?;
        let mut model_name = String::new();
        std::io::stdin().read_line(&mut model_name)?;
        let model_name = model_name.trim().to_string();

        print!("  API key env var (or empty for no auth): ");
        std::io::stdout().flush()?;
        let mut api_key_env = String::new();
        std::io::stdin().read_line(&mut api_key_env)?;
        let api_key_env = api_key_env.trim().to_string();

        println!("  Provider noted.\n");
        (
            None,
            Some(CustomProviderChoice {
                base_url,
                model_name,
                api_key_env: if api_key_env.is_empty() {
                    None
                } else {
                    Some(api_key_env)
                },
            }),
        )
    } else {
        let item = model_choices
            .get(provider_choice.saturating_sub(1))
            .expect("prompt choice clamped");
        println!("  Using {} / {}.\n", item.provider, item.id);
        (
            Some(ModelSelectionChoice {
                provider: item.provider.clone(),
                model: item.id.clone(),
                api_key_env: item.requires_api_key.then(|| item.api_key_env.clone()),
            }),
            None,
        )
    };

    // Step 2: API Key
    println!("[2/4] API Key");
    let api_key_env = custom_provider
        .as_ref()
        .and_then(|provider| provider.api_key_env.clone())
        .or_else(|| {
            model_selection
                .as_ref()
                .and_then(|selection| selection.api_key_env.clone())
        });
    if api_key_env.as_deref().is_none_or(str::is_empty) {
        println!("  No API key required for this selection.\n");
    } else if provider_api_key_present(cfg, api_key_env.as_deref()) {
        println!("  API key is set.\n");
    } else {
        let env_var = api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
        println!(
            "  Set {} in your environment, or run `codingbuddy setup` to reconfigure.\n",
            env_var
        );
    }

    // Step 3: Local ML
    println!("[3/4] Local ML");
    println!("  Local ML runs models on your machine for:");
    println!("  - Code retrieval: surfaces relevant code before the LLM responds");
    println!("  - Privacy scanning: detects and redacts secrets before they reach the API");
    println!("  - Ghost text: inline code completions in the TUI\n");

    let enable_ml = prompt_yes_no("  Enable local ML? [Y/n]: ")?;

    // Detect hardware and recommend model when ML is enabled
    let (detected_device, recommended_model) = if enable_ml {
        let hw = codingbuddy_local_ml::hardware::detect_hardware();
        let device_label = match hw.device {
            codingbuddy_local_ml::hardware::DetectedDevice::Metal => "Apple Silicon (Metal)",
            codingbuddy_local_ml::hardware::DetectedDevice::Cuda => "NVIDIA GPU (CUDA)",
            codingbuddy_local_ml::hardware::DetectedDevice::Cpu => "CPU",
        };
        let core_info = match (hw.performance_cores, hw.efficiency_cores) {
            (Some(p), Some(e)) => format!(", {p}P+{e}E cores ({} threads)", hw.recommended_threads),
            (Some(p), None) => format!(", {p} cores ({} threads)", hw.recommended_threads),
            _ => format!(", {} threads", hw.recommended_threads),
        };
        println!(
            "\n  Detected: {device_label}, {} MB RAM{core_info}",
            hw.total_ram_mb
        );
        let model = codingbuddy_local_ml::model_registry::recommend_completion_model(
            hw.available_for_models_mb,
        );
        if let Some(m) = model {
            println!(
                "  Selected model: {} ({:.1}B parameters)",
                m.display_name, m.params_b
            );
        } else {
            println!("  Warning: insufficient RAM for local models (ghost text disabled)");
        }
        (Some(hw.device.to_string()), model.map(|m| m.model_id))
    } else {
        (None, None)
    };
    println!();

    // Step 4: Privacy Scanning
    println!("[4/4] Privacy Scanning");
    let enable_privacy = if enable_ml {
        prompt_yes_no("  Enable privacy scanning? [Y/n]: ")?
    } else {
        false
    };
    println!();

    // All choices collected — now write atomically.
    let state = WizardState {
        model_selection,
        custom_provider,
        enable_ml,
        detected_device,
        recommended_model,
        enable_privacy,
    };
    apply_wizard_state(cwd, cfg, &state)?;

    Ok(())
}

/// Write all wizard choices to config in one pass. Called only after all steps complete.
fn apply_wizard_state(cwd: &Path, cfg: &AppConfig, state: &WizardState) -> Result<()> {
    if let Some(ref cp) = state.custom_provider {
        merge_provider_config(
            cwd,
            "openai-compat",
            &cp.base_url,
            &cp.model_name,
            cp.api_key_env.as_deref(),
        )?;
    } else if let Some(ref selection) = state.model_selection {
        merge_model_selection(
            cwd,
            cfg,
            &selection.provider,
            &selection.model,
            selection.api_key_env.as_deref(),
        )?;
    }

    if state.enable_ml || state.enable_privacy {
        merge_local_ml_config(
            cwd,
            state.enable_ml,
            state.enable_privacy,
            state.detected_device.as_deref(),
            state.recommended_model,
        )?;
    }

    if state.enable_ml {
        download_required_models(cfg, false)?;
    }

    Ok(())
}

fn setup_model_choices(cfg: &AppConfig) -> Vec<codingbuddy_core::ModelSelectorItem> {
    let catalog = cfg.llm.model_catalog();
    let mut choices: Vec<_> = catalog
        .selector_items(&cfg.llm)
        .into_iter()
        .filter(|item| {
            matches!(
                item.provider.as_str(),
                "deepseek"
                    | "openai-compatible"
                    | "anthropic"
                    | "google"
                    | "openrouter"
                    | "ollama"
                    | "mistral"
                    | "groq"
                    | "xai"
                    | "together"
            )
        })
        .collect();
    choices.truncate(12);
    choices
}

fn setup_model_summary(item: &codingbuddy_core::ModelSelectorItem) -> String {
    let place = if item.local { "local" } else { "cloud" };
    let auth = if item.requires_api_key {
        format!("env {}", item.api_key_env)
    } else {
        "no key".to_string()
    };
    let mut caps = Vec::new();
    if item.capability.tool_call {
        caps.push("tools");
    }
    if item.capability.reasoning {
        caps.push("reasoning");
    }
    if item.capability.image_input {
        caps.push("vision");
    }
    if caps.is_empty() {
        caps.push("chat");
    }
    format!(
        "{} ctx, {}, {}, {}",
        setup_token_limit(item.context_tokens),
        place,
        caps.join("/"),
        auth
    )
}

fn setup_token_limit(tokens: u64) -> String {
    if tokens == 0 {
        "?".to_string()
    } else if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else {
        format!("{}K", tokens / 1_000)
    }
}

/// Run the setup wizard, `--local-ml` shortcut, or `--status` display.
pub(crate) fn run_setup(cwd: &Path, args: SetupArgs, json_mode: bool) -> Result<()> {
    if args.status {
        return run_status_display(cwd, json_mode);
    }
    if args.local_ml {
        return run_local_ml_shortcut(cwd, json_mode);
    }
    run_interactive_wizard(cwd, json_mode)
}

/// `codingbuddy setup --status` — show current setup state without prompts.
fn run_status_display(cwd: &Path, json_mode: bool) -> Result<()> {
    let cfg = AppConfig::ensure(cwd)?;
    let api_key_set = has_api_key(&cfg);
    let models = model_download_status(&cfg);
    let provider = cfg.llm.active_provider();
    let hw = codingbuddy_local_ml::hardware::detect_hardware();

    if json_mode {
        print_json(&json!({
            "provider": cfg.llm.provider,
            "base_url": provider.base_url,
            "chat_model": provider.models.chat,
            "reasoner_model": provider.models.reasoner,
            "api_key": if api_key_set { "configured" } else { "missing" },
            "hardware": {
                "device": hw.device.to_string(),
                "total_ram_mb": hw.total_ram_mb,
                "available_for_models_mb": hw.available_for_models_mb,
            },
            "local_ml": {
                "enabled": cfg.local_ml.enabled,
                "device": cfg.local_ml.device,
                "privacy_enabled": cfg.local_ml.privacy.enabled,
                "autocomplete_enabled": cfg.local_ml.autocomplete.enabled,
                "autocomplete_model": cfg.local_ml.autocomplete.model_id,
                "models": models,
            },
        }))?;
    } else {
        println!("provider: {}", cfg.llm.provider);
        println!("base_url: {}", provider.base_url);
        println!("chat_model: {}", provider.models.chat);
        if let Some(ref reasoner) = provider.models.reasoner {
            println!("reasoner_model: {reasoner}");
        }
        println!(
            "api_key: {}",
            if api_key_set { "configured" } else { "missing" }
        );
        println!(
            "hardware: {} ({} MB RAM, {} MB available for models)",
            hw.device, hw.total_ram_mb, hw.available_for_models_mb
        );
        println!(
            "local_ml: {} (device: {})",
            if cfg.local_ml.enabled {
                "enabled"
            } else {
                "disabled"
            },
            cfg.local_ml.device
        );
        println!(
            "privacy: {}",
            if cfg.local_ml.privacy.enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
        if cfg.local_ml.autocomplete.enabled {
            println!("autocomplete_model: {}", cfg.local_ml.autocomplete.model_id);
        }
        for (model_id, status) in &models {
            println!("model: {} ({})", model_id, status);
        }
    }
    Ok(())
}

/// `codingbuddy setup --local-ml` — non-interactive shortcut.
fn run_local_ml_shortcut(cwd: &Path, json_mode: bool) -> Result<()> {
    let cfg = AppConfig::ensure(cwd)?;

    // Auto-detect hardware for device and model selection
    let hw = codingbuddy_local_ml::hardware::detect_hardware();
    let device = hw.device.to_string();
    let model_id = codingbuddy_local_ml::model_registry::recommend_completion_model(
        hw.available_for_models_mb,
    )
    .map(|m| m.model_id);

    merge_local_ml_config(cwd, true, true, Some(&device), model_id)?;

    if !json_mode {
        println!("Local ML enabled.");
        println!("Privacy scanning enabled.");
    }

    // Download models immediately
    let download_results = download_required_models(&cfg, json_mode)?;

    if json_mode {
        print_json(&json!({
            "local_ml_enabled": true,
            "privacy_enabled": true,
            "models": download_results,
        }))?;
    } else {
        println!("\nConfig saved to {}", settings_path(cwd).display());
    }
    Ok(())
}

/// Full interactive wizard (4 steps).
fn run_interactive_wizard(cwd: &Path, json_mode: bool) -> Result<()> {
    let cfg = AppConfig::ensure(cwd)?;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    if !interactive || json_mode {
        return run_status_display(cwd, json_mode);
    }

    println!("Welcome to CodingBuddy setup!\n");

    run_wizard_steps(cwd, &cfg)?;

    println!("\nSetup complete! Run `codingbuddy chat` to start.");
    Ok(())
}

/// Download the default embedding and completion models, showing progress.
/// Returns a list of (model_id, outcome) pairs for JSON output.
fn download_required_models(cfg: &AppConfig, json_mode: bool) -> Result<Vec<(String, String)>> {
    let cache_dir = resolve_cache_dir(cfg);
    let mut manager = ModelManager::new(cache_dir);

    let embedding = model_registry::default_embedding_model();
    let completion = model_registry::default_completion_model();

    let models = [embedding, completion];

    let mut results = Vec::new();

    for entry in &models {
        let model_id = entry.model_id;
        let display_name = entry.display_name;
        let status = manager.status(model_id);
        if status == ModelStatus::Ready {
            if !json_mode {
                println!("  {} ({}) — already downloaded.", display_name, model_id);
            }
            results.push((model_id.to_string(), "ready".to_string()));
            continue;
        }

        if !json_mode {
            print!("  Downloading {} ({})...", display_name, model_id);
            std::io::stdout().flush()?;
        }

        let files = entry.download_files();
        match manager.ensure_model_with_progress(
            model_id,
            entry.hf_repo,
            &files,
            |current, total| {
                if !json_mode && total > 0 {
                    print!(
                        "\r  Downloading {} ({})... [{}/{}]",
                        display_name, model_id, current, total
                    );
                    let _ = std::io::stdout().flush();
                }
            },
        ) {
            Ok(_) => {
                if !json_mode {
                    println!(
                        "\r  Downloading {} ({})... done.       ",
                        display_name, model_id
                    );
                }
                results.push((model_id.to_string(), "downloaded".to_string()));
            }
            Err(e) => {
                if !json_mode {
                    println!(
                        "\r  Downloading {} ({})... skipped: {}       ",
                        display_name, model_id, e
                    );
                }
                results.push((model_id.to_string(), format!("error: {e}")));
            }
        }
    }

    Ok(results)
}

/// Check download status of the default models without downloading.
fn model_download_status(cfg: &AppConfig) -> Vec<(String, String)> {
    let cache_dir = resolve_cache_dir(cfg);
    let manager = ModelManager::new(cache_dir);

    let embedding = model_registry::default_embedding_model();
    let completion = model_registry::default_completion_model();

    vec![
        (
            embedding.model_id.to_string(),
            format!("{:?}", manager.status(embedding.model_id)),
        ),
        (
            completion.model_id.to_string(),
            format!("{:?}", manager.status(completion.model_id)),
        ),
    ]
}

/// Resolve the model cache directory from config.
fn resolve_cache_dir(cfg: &AppConfig) -> PathBuf {
    PathBuf::from(&cfg.local_ml.cache_dir)
}

/// Prompt user with a yes/no question. Returns true for Y/yes/empty, false for N/no.
fn prompt_yes_no(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let normalized = answer.trim().to_ascii_lowercase();
    Ok(!matches!(normalized.as_str(), "n" | "no"))
}

/// Prompt user to pick a number in [min, max]. Returns the chosen value.
fn prompt_choice(prompt: &str, min: u32, max: u32) -> Result<u32> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let chosen: u32 = answer.trim().parse().unwrap_or(min);
    Ok(chosen.clamp(min, max))
}

/// Merge a custom provider into `.codingbuddy/settings.json`.
fn merge_provider_config(
    cwd: &Path,
    provider_name: &str,
    base_url: &str,
    model: &str,
    api_key_env: Option<&str>,
) -> Result<()> {
    let path = settings_path(cwd);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut root = if path.exists() {
        let raw = fs::read_to_string(&path)?;
        serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }

    let map = root.as_object_mut().expect("root is object");
    let llm = map.entry("llm".to_string()).or_insert_with(|| json!({}));
    if !llm.is_object() {
        *llm = json!({});
    }
    if let Some(llm_obj) = llm.as_object_mut() {
        llm_obj.insert("provider".to_string(), json!(provider_name));
        llm_obj.insert("base_url".to_string(), json!(base_url));
        llm_obj.insert("base_model".to_string(), json!(model));
        if let Some(env) = api_key_env {
            llm_obj.insert("api_key_env".to_string(), json!(env));
        }

        // Also save to providers map
        let providers = llm_obj
            .entry("providers".to_string())
            .or_insert_with(|| json!({}));
        if !providers.is_object() {
            *providers = json!({});
        }
        if let Some(p) = providers.as_object_mut() {
            p.insert(
                provider_name.to_string(),
                json!({
                    "base_url": base_url,
                    "api_key_env": api_key_env.unwrap_or(""),
                    "models": {
                        "chat": model,
                    }
                }),
            );
        }
    }

    fs::write(&path, serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// Persist a catalog-backed built-in provider/model selection.
fn merge_model_selection(
    cwd: &Path,
    cfg: &AppConfig,
    provider_name: &str,
    model: &str,
    api_key_env: Option<&str>,
) -> Result<()> {
    let path = settings_path(cwd);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut root = if path.exists() {
        let raw = fs::read_to_string(&path)?;
        serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }

    let provider_cfg = cfg.llm.providers.get(provider_name);
    let base_url = provider_cfg
        .map(|provider| provider.base_url.as_str())
        .unwrap_or("");
    let provider_kind = provider_cfg
        .map(|provider| provider.kind.as_str())
        .unwrap_or(provider_name);
    let openai_compat_prefix = provider_cfg
        .map(|provider| provider.openai_compat_prefix)
        .unwrap_or(true);
    let resolved_api_key_env = api_key_env
        .or_else(|| provider_cfg.map(|provider| provider.api_key_env.as_str()))
        .unwrap_or("");
    let reasoner = provider_cfg.and_then(|provider| provider.models.reasoner.as_deref());

    let map = root.as_object_mut().expect("root is object");
    let llm = map.entry("llm".to_string()).or_insert_with(|| json!({}));
    if !llm.is_object() {
        *llm = json!({});
    }
    if let Some(llm_obj) = llm.as_object_mut() {
        llm_obj.insert("provider".to_string(), json!(provider_name));
        llm_obj.insert("base_model".to_string(), json!(model));
        llm_obj.insert("base_url".to_string(), json!(base_url));
        llm_obj.insert(
            "openai_compat_prefix".to_string(),
            json!(openai_compat_prefix),
        );
        llm_obj.insert("api_key_env".to_string(), json!(resolved_api_key_env));

        let providers = llm_obj
            .entry("providers".to_string())
            .or_insert_with(|| json!({}));
        if !providers.is_object() {
            *providers = json!({});
        }
        if let Some(p) = providers.as_object_mut() {
            let mut models = serde_json::Map::new();
            models.insert("chat".to_string(), json!(model));
            if let Some(reasoner) = reasoner {
                models.insert("reasoner".to_string(), json!(reasoner));
            }
            p.insert(
                provider_name.to_string(),
                json!({
                    "kind": provider_kind,
                    "base_url": base_url,
                    "api_key_env": resolved_api_key_env,
                    "openai_compat_prefix": openai_compat_prefix,
                    "models": models,
                }),
            );
        }
    }

    fs::write(&path, serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// Merge local_ml keys into `.codingbuddy/settings.json` without clobbering other settings.
///
/// `device` and `model_id` are optional — when `Some`, they're written to the config
/// so the auto-detected values persist across sessions.
fn merge_local_ml_config(
    cwd: &Path,
    enabled: bool,
    privacy_enabled: bool,
    device: Option<&str>,
    model_id: Option<&str>,
) -> Result<()> {
    let path = settings_path(cwd);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut root = if path.exists() {
        let raw = fs::read_to_string(&path)?;
        serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }

    let map = root
        .as_object_mut()
        .expect("root is guaranteed to be an object");
    let local_ml = map
        .entry("local_ml".to_string())
        .or_insert_with(|| json!({}));
    if !local_ml.is_object() {
        *local_ml = json!({});
    }
    if let Some(ml) = local_ml.as_object_mut() {
        ml.insert("enabled".to_string(), json!(enabled));
        if let Some(dev) = device {
            ml.insert("device".to_string(), json!(dev));
        }
        if let Some(mid) = model_id {
            let autocomplete = ml
                .entry("autocomplete".to_string())
                .or_insert_with(|| json!({}));
            if !autocomplete.is_object() {
                *autocomplete = json!({});
            }
            if let Some(ac) = autocomplete.as_object_mut() {
                ac.insert("model_id".to_string(), json!(mid));
            }
        }
        let privacy = ml.entry("privacy".to_string()).or_insert_with(|| json!({}));
        if !privacy.is_object() {
            *privacy = json!({});
        }
        if let Some(p) = privacy.as_object_mut() {
            p.insert("enabled".to_string(), json!(privacy_enabled));
        }
    }

    fs::write(&path, serde_json::to_vec_pretty(&root)?)?;
    Ok(())
}

/// Project settings path (`.codingbuddy/settings.json`).
fn settings_path(cwd: &Path) -> PathBuf {
    AppConfig::project_settings_path(cwd)
}

/// Check whether the API key is available (env var or config).
fn has_api_key(cfg: &AppConfig) -> bool {
    let env_set = std::env::var(&cfg.llm.api_key_env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let config_set = cfg
        .llm
        .api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|v| !v.is_empty());
    env_set || config_set
}

fn provider_api_key_present(cfg: &AppConfig, env_var: Option<&str>) -> bool {
    let env_set = env_var
        .filter(|name| !name.trim().is_empty())
        .and_then(|name| std::env::var(name).ok())
        .is_some_and(|value| !value.trim().is_empty());
    env_set
        || cfg
            .llm
            .api_key
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn setup_merges_config_preserves_existing() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();

        // Write pre-existing settings
        let dir = cwd.join(".codingbuddy");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "llm": { "profile": "v3_2" },
                "custom_key": "preserve_me"
            }))
            .unwrap(),
        )
        .unwrap();

        // Merge local_ml config
        merge_local_ml_config(cwd, true, true, Some("metal"), Some("qwen2.5-coder-7b")).unwrap();

        // Read back and verify
        let raw = fs::read_to_string(&path).unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();

        // Existing keys preserved
        assert_eq!(root["custom_key"], "preserve_me");
        assert_eq!(root["llm"]["profile"], "v3_2");

        // New keys written
        assert_eq!(root["local_ml"]["enabled"], true);
        assert_eq!(root["local_ml"]["privacy"]["enabled"], true);
        assert_eq!(root["local_ml"]["device"], "metal");
        assert_eq!(
            root["local_ml"]["autocomplete"]["model_id"],
            "qwen2.5-coder-7b"
        );
    }

    #[test]
    fn model_download_status_returns_entries() {
        let cfg = AppConfig::default();
        let statuses = model_download_status(&cfg);
        assert_eq!(statuses.len(), 2);
        // Both should report NotDownloaded for a fresh config
        for (model_id, status) in &statuses {
            assert!(!model_id.is_empty());
            assert!(status.contains("NotDownloaded") || status.contains("Ready"));
        }
    }

    #[test]
    fn resolve_cache_dir_uses_config() {
        let cfg = AppConfig::default();
        let dir = resolve_cache_dir(&cfg);
        assert_eq!(dir, PathBuf::from(".codingbuddy/models"));
    }

    #[test]
    fn first_time_setup_skips_when_fully_configured() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".codingbuddy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("settings.json"),
            serde_json::to_vec_pretty(&json!({ "local_ml": { "enabled": true } })).unwrap(),
        )
        .unwrap();

        // Set API key so has_api_key returns true
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "test-key") };

        let cfg = AppConfig::ensure(cwd).unwrap();
        // Should write the marker without prompting (already configured + non-interactive)
        maybe_first_time_setup(cwd, &cfg).unwrap();

        // Marker should exist
        assert!(dir.join(SETUP_MARKER).exists());

        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
    }

    #[test]
    fn first_time_setup_skips_when_marker_exists() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".codingbuddy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), "{}").unwrap();
        fs::write(dir.join(SETUP_MARKER), "").unwrap();

        let cfg = AppConfig::ensure(cwd).unwrap();
        // Should return immediately — no prompt, no config change
        maybe_first_time_setup(cwd, &cfg).unwrap();
    }

    #[test]
    fn merge_provider_config_writes_provider_settings() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".codingbuddy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), "{}").unwrap();

        merge_provider_config(
            cwd,
            "openai-compat",
            "http://localhost:11434/v1",
            "llama3",
            Some("OLLAMA_API_KEY"),
        )
        .unwrap();

        let path = dir.join("settings.json");
        let raw = fs::read_to_string(&path).unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(root["llm"]["provider"], "openai-compat");
        assert_eq!(root["llm"]["base_url"], "http://localhost:11434/v1");
        assert_eq!(root["llm"]["base_model"], "llama3");
        assert_eq!(root["llm"]["api_key_env"], "OLLAMA_API_KEY");
        assert_eq!(
            root["llm"]["providers"]["openai-compat"]["base_url"],
            "http://localhost:11434/v1"
        );
        assert_eq!(
            root["llm"]["providers"]["openai-compat"]["models"]["chat"],
            "llama3"
        );
    }

    #[test]
    fn merge_model_selection_writes_builtin_provider_settings() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".codingbuddy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), "{}").unwrap();

        let cfg = AppConfig::default();
        merge_model_selection(
            cwd,
            &cfg,
            "openai-compatible",
            "gpt-4o-mini",
            Some("OPENAI_API_KEY"),
        )
        .unwrap();

        let raw = fs::read_to_string(dir.join("settings.json")).unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(root["llm"]["provider"], "openai-compatible");
        assert_eq!(root["llm"]["base_model"], "gpt-4o-mini");
        assert_eq!(root["llm"]["api_key_env"], "OPENAI_API_KEY");
        assert_eq!(
            root["llm"]["providers"]["openai-compatible"]["models"]["chat"],
            "gpt-4o-mini"
        );
        assert_eq!(
            root["llm"]["providers"]["openai-compatible"]["openai_compat_prefix"],
            true
        );
    }

    #[test]
    fn merge_provider_config_preserves_existing_settings() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".codingbuddy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("settings.json"),
            serde_json::to_vec_pretty(&json!({
                "local_ml": { "enabled": true },
                "custom": "preserved"
            }))
            .unwrap(),
        )
        .unwrap();

        merge_provider_config(cwd, "custom-llm", "http://my-llm:8000/v1", "my-model", None)
            .unwrap();

        let path = dir.join("settings.json");
        let raw = fs::read_to_string(&path).unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();

        // Provider written
        assert_eq!(root["llm"]["provider"], "custom-llm");
        assert_eq!(root["llm"]["base_model"], "my-model");

        // Existing settings preserved
        assert_eq!(root["local_ml"]["enabled"], true);
        assert_eq!(root["custom"], "preserved");
    }

    #[test]
    fn auto_detect_writes_marker_and_shows_provider() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".codingbuddy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), "{}").unwrap();

        // Set an API key — AppConfig::ensure will auto-detect it
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-key") };

        let cfg = AppConfig::ensure(cwd).unwrap();
        // auto_detect_provider already switched to anthropic
        assert_eq!(cfg.llm.provider, "anthropic");

        let result = maybe_first_time_setup(cwd, &cfg).unwrap();

        // Returns false (no config reload needed), but marker is written
        assert!(!result);
        assert!(dir.join(SETUP_MARKER).exists());

        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    }
}
