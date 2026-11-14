use anyhow::Result;
use codingbuddy_core::AppConfig;
use serde_json::json;
use std::io::IsTerminal;
use std::path::Path;

use crate::SetupArgs;
use crate::output::print_json;

/// Run the provider setup wizard or show its current status.
pub(crate) fn run_setup(cwd: &Path, args: SetupArgs, json_mode: bool) -> Result<()> {
    if args.status {
        return run_status_display(cwd, json_mode);
    }
    run_interactive_wizard(cwd, json_mode)
}

/// `codingbuddy setup --status` — show current setup state without prompts.
fn run_status_display(cwd: &Path, json_mode: bool) -> Result<()> {
    let cfg = AppConfig::ensure(cwd)?;
    let api_key_set = has_api_key(&cfg);
    let provider = cfg.llm.active_provider();

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
            "privacy_enabled": cfg.privacy.enabled,
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
            "privacy: {}",
            if cfg.privacy.enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
    }
    Ok(())
}

fn run_interactive_wizard(cwd: &Path, json_mode: bool) -> Result<()> {
    if json_mode || !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return run_status_display(cwd, json_mode);
    }
    super::chat::run_setup_menu(cwd)
}

/// Check whether the API key is available (env var or config).
fn has_api_key(cfg: &AppConfig) -> bool {
    codingbuddy_llm::check_provider(&cfg.llm).is_ok()
}
