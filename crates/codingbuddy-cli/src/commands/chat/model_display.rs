//! Model catalog display

use super::*;

#[derive(Debug)]
pub(crate) struct CatalogDisplayResolution {
    pub(crate) catalog: codingbuddy_core::ModelCatalog,
    pub(crate) source: String,
    pub(crate) cache_path: PathBuf,
    pub(crate) cache_fresh: bool,
    pub(crate) refresh_error: Option<String>,
}

pub(crate) fn refresh_model_catalog_background(cwd: &Path, cfg: &AppConfig) {
    if !cfg.llm.model_catalog.enabled || cfg.llm.model_catalog.offline {
        return;
    }
    let llm = cfg.llm.clone();
    let runtime = runtime_dir(cwd);
    thread::spawn(move || {
        let _ = codingbuddy_llm::model_catalog::resolve_model_catalog(&llm, &runtime);
    });
}

pub(crate) fn resolve_catalog_for_display(cwd: &Path, cfg: &AppConfig) -> CatalogDisplayResolution {
    let runtime = runtime_dir(cwd);
    match codingbuddy_llm::model_catalog::resolve_model_catalog(&cfg.llm, &runtime) {
        Ok(resolution) => CatalogDisplayResolution {
            catalog: resolution.catalog,
            source: format!("{:?}", resolution.source),
            cache_path: resolution.cache_path,
            cache_fresh: resolution.cache_fresh,
            refresh_error: resolution.refresh_error,
        },
        Err(error) => CatalogDisplayResolution {
            catalog: cfg.llm.model_catalog_for_runtime(&runtime),
            source: "RuntimeCacheFallback".to_string(),
            cache_path: cfg.llm.model_catalog.cache_path_for(&runtime),
            cache_fresh: false,
            refresh_error: Some(error.to_string()),
        },
    }
}

/// Format a catalog-backed list of models with pricing, limits, and provider capabilities.
pub(crate) fn format_models_list(cwd: &Path, cfg: &AppConfig) -> String {
    let resolution = resolve_catalog_for_display(cwd, cfg);
    let catalog = resolution.catalog;
    let items = catalog.selector_items(&cfg.llm);

    let mut out = format!(
        "Models (catalog: {}, {} total, cache: {}, fresh: {}). Use `/model provider/model` to switch:\n",
        resolution.source,
        catalog.models.len(),
        resolution.cache_path.display(),
        resolution.cache_fresh
    );
    if let Some(error) = resolution.refresh_error {
        out.push_str(&format!("Catalog refresh warning: {error}\n"));
    }
    out.push_str(&format!(
        "  {:<18} {:<36} {:>8} {:>8} {:>15} {:<7} {:<14} {:<20} {:<18}  {}\n",
        "Provider",
        "Model",
        "Context",
        "Output",
        "$/M in/out",
        "Where",
        "Auth",
        "Protocol",
        "Status",
        "Capabilities"
    ));
    out.push_str(&format!("  {}\n", "-".repeat(176)));

    for item in items {
        let marker = if item.active { "* " } else { "  " };
        let place = if item.local { "local" } else { "cloud" };
        let auth = format_model_auth(&item);
        out.push_str(&format!(
            "{marker}{:<18} {:<36} {:>8} {:>8} {:>15} {:<7} {:<14} {:<20} {:<18}  {}\n",
            item.provider,
            truncate_inline(&item.id, 36),
            format_token_limit(item.context_tokens),
            format_output_limit(item.output_tokens),
            format_model_cost(item.cost),
            place,
            auth,
            truncate_inline(&item.chat_protocol, 20),
            format_model_status(&item),
            format_selector_capabilities(&item)
        ));
    }

    out
}

pub(crate) fn format_model_auth(item: &codingbuddy_core::ModelSelectorItem) -> String {
    if !item.requires_api_key {
        "none".to_string()
    } else if item.api_key_available {
        "ready".to_string()
    } else {
        truncate_inline(&format!("missing:{}", item.api_key_env), 14)
    }
}

pub(crate) fn format_model_status(item: &codingbuddy_core::ModelSelectorItem) -> String {
    let mut parts = Vec::new();
    match item.status {
        codingbuddy_core::ModelStatus::Stable => {}
        codingbuddy_core::ModelStatus::Preview => parts.push("preview"),
        codingbuddy_core::ModelStatus::Deprecated => parts.push("deprecated"),
        codingbuddy_core::ModelStatus::Unknown => parts.push("unknown"),
    }
    match item.provider_status {
        codingbuddy_core::ProviderStatus::Available => {}
        codingbuddy_core::ProviderStatus::Degraded => parts.push("provider-degraded"),
        codingbuddy_core::ProviderStatus::Unavailable => parts.push("provider-down"),
        codingbuddy_core::ProviderStatus::Unknown => parts.push("provider-unknown"),
    }
    if parts.is_empty() {
        "ok".to_string()
    } else {
        truncate_inline(&parts.join(","), 18)
    }
}

pub(crate) fn format_token_limit(tokens: u64) -> String {
    if tokens == 0 {
        "?".to_string()
    } else if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else {
        format!("{}K", tokens / 1_000)
    }
}

pub(crate) fn format_output_limit(tokens: u32) -> String {
    if tokens == 0 {
        "?".to_string()
    } else {
        format_token_limit(u64::from(tokens))
    }
}

pub(crate) fn format_model_cost(cost: codingbuddy_core::ModelCost) -> String {
    if cost.input_per_mtok_usd == 0.0 && cost.output_per_mtok_usd == 0.0 {
        "local/free".to_string()
    } else {
        format!(
            "{:.2}/{:.2}",
            cost.input_per_mtok_usd, cost.output_per_mtok_usd
        )
    }
}

pub(crate) fn format_selector_capabilities(item: &codingbuddy_core::ModelSelectorItem) -> String {
    let mut caps = Vec::new();
    if item.capability.tool_call {
        caps.push("tools");
    }
    if item.capability.parallel_tool_call {
        caps.push("parallel");
    }
    if item.capability.reasoning {
        caps.push("reasoning");
    }
    if item.capability.thinking_config {
        caps.push("thinking");
    }
    if item.capability.image_input
        || item
            .modalities
            .contains(&codingbuddy_core::ModelModality::Image)
    {
        caps.push("vision");
    }
    if item.capability.fim {
        caps.push("fim");
    }
    if item
        .modalities
        .contains(&codingbuddy_core::ModelModality::Audio)
    {
        caps.push("audio");
    }
    if item
        .modalities
        .contains(&codingbuddy_core::ModelModality::Video)
    {
        caps.push("video");
    }
    match item.status {
        codingbuddy_core::ModelStatus::Preview => caps.push("preview"),
        codingbuddy_core::ModelStatus::Deprecated => caps.push("deprecated"),
        codingbuddy_core::ModelStatus::Unknown => caps.push("unknown"),
        codingbuddy_core::ModelStatus::Stable => {}
    }
    match item.provider_status {
        codingbuddy_core::ProviderStatus::Degraded => caps.push("provider-degraded"),
        codingbuddy_core::ProviderStatus::Unavailable => caps.push("provider-down"),
        codingbuddy_core::ProviderStatus::Unknown => caps.push("provider-unknown"),
        codingbuddy_core::ProviderStatus::Available => {}
    }
    if caps.is_empty() {
        if item.capability.tool_call {
            "text".to_string()
        } else {
            "text,no-tools".to_string()
        }
    } else {
        if !item.capability.tool_call {
            caps.push("no-tools");
        }
        caps.join(",")
    }
}

pub(crate) fn model_selector_items_json(cwd: &Path, cfg: &AppConfig) -> serde_json::Value {
    let resolution = resolve_catalog_for_display(cwd, cfg);
    let catalog = resolution.catalog;
    serde_json::json!({
        "catalog_source": resolution.source,
        "catalog_cache_path": resolution.cache_path,
        "catalog_cache_fresh": resolution.cache_fresh,
        "catalog_refresh_error": resolution.refresh_error,
        "active_provider": cfg.llm.provider,
        "active_model": cfg.llm.active_base_model(),
        "models": catalog.selector_items(&cfg.llm),
    })
}

pub(crate) fn active_model_for_display(
    cfg: &AppConfig,
    model_spec: Option<&str>,
    force_max_think: bool,
) -> String {
    let mut effective = cfg.clone();
    if let Some(spec) = model_spec {
        effective.llm.apply_model_spec(spec);
    }
    if force_max_think {
        effective.llm.active_reasoner_model()
    } else {
        effective.llm.active_base_model()
    }
}

pub(crate) fn model_switch_safety_message(
    cwd: &Path,
    cfg: &AppConfig,
    model_spec: &str,
    allow_tools: bool,
    read_only_mode: bool,
    active_chat_mode: ChatMode,
) -> Option<String> {
    if !allow_tools || read_only_mode || active_chat_mode != ChatMode::Code {
        return None;
    }
    let mut effective = cfg.clone();
    effective.llm.apply_model_spec(model_spec);
    let provider = effective.llm.provider.clone();
    let model = effective.llm.active_base_model();
    let catalog = effective.llm.model_catalog_for_runtime(&runtime_dir(cwd));
    let item = catalog
        .selector_items(&effective.llm)
        .into_iter()
        .find(|item| item.provider == provider && item.id == model);
    let supports_tools = item
        .as_ref()
        .map(|item| item.capability.tool_call)
        .or_else(|| {
            effective
                .llm
                .capabilities_for_model(&model)
                .map(|capabilities| capabilities.supports_tool_calling)
        })
        .unwrap_or(true);
    if supports_tools {
        return None;
    }
    let spec = item
        .as_ref()
        .map(|item| item.selection_spec.clone())
        .unwrap_or_else(|| format!("{provider}/{model}"));
    Some(format!(
        "model switch blocked: {spec} is marked no-tools. Use /ask, /context, or /read-only on before switching, or choose a tool-capable model from /models."
    ))
}

/// Open a file path in the system browser (cross-platform).
pub(crate) fn open_in_browser(path: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", path])
        .spawn();
}
