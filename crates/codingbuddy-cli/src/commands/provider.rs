use anyhow::{Context, Result};
use clap::Subcommand;
use codingbuddy_core::AppConfig;
use codingbuddy_llm::certification;
use codingbuddy_store::Store;
use serde_json::json;
use std::path::Path;

#[derive(Subcommand)]
pub enum ProviderCommand {
    /// Show evidence for the configured route, without contacting the provider.
    Status,
    /// Check connectivity to the selected route with one bounded request.
    Check {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Run a live two-tool verification against an explicitly selected route.
    Verify {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Use non-streaming requests for this probe.
        #[arg(long)]
        non_streaming: bool,
    },
}

pub fn run(workspace: &Path, command: ProviderCommand, json_mode: bool) -> Result<()> {
    let mut config = AppConfig::load(workspace)?;
    let store = Store::new(workspace)?;
    let key = store.route_key()?;
    let result = match command {
        ProviderCommand::Status => certification::live_status(
            &config.llm,
            &config.llm.active_base_model(),
            workspace,
            &key,
        ),
        ProviderCommand::Check { provider, model } => {
            select_route(&mut config.llm, provider, model)?;
            codingbuddy_core::runtime::block_on(codingbuddy_llm::probe_provider(&config.llm))?;
            certification::record_connectivity(&config.llm, workspace, &key)?;
            certification::live_status(
                &config.llm,
                &config.llm.active_base_model(),
                workspace,
                &key,
            )
        }
        ProviderCommand::Verify {
            provider,
            model,
            non_streaming,
        } => {
            select_route(&mut config.llm, provider, model)?;
            config.llm.api_key = codingbuddy_llm::resolve_key_from_config(&config.llm);
            let evidence = certification::live_probe(&config.llm, !non_streaming)?;
            let route = codingbuddy_llm::session_route(&config.llm, &evidence.model, &key)?;
            let result = json!({"route":route,"evidence":evidence});
            let path = codingbuddy_core::runtime_dir(workspace).join("provider-verification.json");
            let mut file = tempfile::NamedTempFile::new_in(
                path.parent().context("Missing runtime directory")?,
            )?;
            let previous = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let mut entries = previous
                .as_ref()
                .and_then(|value| value["entries"].as_array())
                .cloned()
                .unwrap_or_default();
            entries.retain(|entry| {
                entry["route"] != result["route"]
                    || entry["evidence"]["streaming"] != result["evidence"]["streaming"]
            });
            entries.push(result.clone());
            serde_json::to_writer_pretty(
                &mut file,
                &json!({"schema_version":1,"entries":entries}),
            )?;
            file.as_file().sync_all()?;
            file.persist(path)?;
            json!({"live_verified":true,"evidence":result["evidence"]})
        }
    };
    if json_mode {
        crate::output::print_json(&result)?;
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

fn select_route(
    cfg: &mut codingbuddy_core::LlmConfig,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    if let Some(provider) = provider {
        anyhow::ensure!(
            cfg.providers.contains_key(&provider),
            "Provider profile is unavailable"
        );
        cfg.select_provider(&provider);
    }
    if let Some(model) = model {
        cfg.base_model = model.clone();
        let profile = cfg
            .providers
            .get_mut(&cfg.provider)
            .context("Provider profile is unavailable")?;
        profile.models.chat = model;
    }
    Ok(())
}
