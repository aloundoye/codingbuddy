use anyhow::Result;
use codingbuddy_core::AppConfig;
use codingbuddy_local_ml::model_registry;
use codingbuddy_local_ml::{ModelManager, ModelStatus};
use serde_json::json;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::SetupArgs;
use crate::output::print_json;

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
            "chat_protocol": provider.chat_protocol,
            "auth_strategy": provider.auth_strategy,
            "discovery": provider.discovery,
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
        println!(
            "chat_protocol: {}",
            provider
                .chat_protocol
                .as_deref()
                .unwrap_or("auto (see /models)")
        );
        println!(
            "auth_strategy: {}",
            provider
                .auth_strategy
                .as_deref()
                .unwrap_or("auto (provider default)")
        );
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

fn run_interactive_wizard(cwd: &Path, json_mode: bool) -> Result<()> {
    if json_mode || !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return run_status_display(cwd, json_mode);
    }
    super::chat::run_setup_menu(cwd)
}

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
    codingbuddy_llm::check_provider(&cfg.llm).is_ok()
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
}
