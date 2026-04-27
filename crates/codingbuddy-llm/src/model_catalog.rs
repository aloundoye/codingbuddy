use anyhow::{Context, Result};
use codingbuddy_core::{LlmConfig, ModelCatalog, ModelCatalogCache, ModelCatalogSource};
use reqwest::blocking::Client;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCatalogResolutionSource {
    Disabled,
    Bundled,
    FreshCache,
    StaleCache,
    Remote,
    Offline,
}

#[derive(Debug, Clone)]
pub struct ModelCatalogResolution {
    pub catalog: ModelCatalog,
    pub source: ModelCatalogResolutionSource,
    pub cache_path: PathBuf,
    pub cache_fresh: bool,
    pub refresh_error: Option<String>,
}

pub fn fetch_models_dev_catalog(url: &str, timeout: Duration) -> Result<ModelCatalog> {
    let client = Client::builder().timeout(timeout).build()?;
    let value = client
        .get(url)
        .send()
        .with_context(|| format!("failed to fetch model catalog from {url}"))?
        .error_for_status()
        .with_context(|| format!("model catalog endpoint returned an error: {url}"))?
        .json::<serde_json::Value>()
        .context("failed to parse model catalog JSON")?;
    ModelCatalog::from_models_dev_json(&value)
}

pub fn resolve_model_catalog(
    config: &LlmConfig,
    runtime_dir: &Path,
) -> Result<ModelCatalogResolution> {
    let catalog_config = &config.model_catalog;
    let cache_path = catalog_config.cache_path_for(runtime_dir);
    if !catalog_config.enabled {
        return Ok(ModelCatalogResolution {
            catalog: ModelCatalog::from_config(config),
            source: ModelCatalogResolutionSource::Disabled,
            cache_path,
            cache_fresh: false,
            refresh_error: None,
        });
    }

    let now = current_unix_seconds();
    let cached = ModelCatalogCache::load(&cache_path).ok();
    let mut base = ModelCatalog::bundled();
    let mut source = ModelCatalogResolutionSource::Bundled;
    let mut cache_fresh = false;
    let mut refresh_error = None;

    if let Some(cache) = cached.as_ref()
        && cache.is_fresh_at(now, catalog_config.cache_ttl_seconds)
    {
        base.merge_from(&cache.catalog);
        source = ModelCatalogResolutionSource::FreshCache;
        cache_fresh = true;
    } else if catalog_config.offline {
        if let Some(cache) = cached.as_ref() {
            base.merge_from(&cache.catalog);
            source = ModelCatalogResolutionSource::StaleCache;
        } else {
            source = ModelCatalogResolutionSource::Offline;
        }
    } else {
        match fetch_models_dev_catalog(
            &catalog_config.remote_url,
            Duration::from_secs(catalog_config.refresh_timeout_seconds.max(1)),
        ) {
            Ok(mut remote) => {
                remote.source = ModelCatalogSource::Remote;
                ModelCatalogCache::new(remote.clone(), catalog_config.remote_url.clone(), now)
                    .save(&cache_path)?;
                base.merge_from(&remote);
                source = ModelCatalogResolutionSource::Remote;
            }
            Err(err) => {
                refresh_error = Some(err.to_string());
                if let Some(cache) = cached.as_ref() {
                    base.merge_from(&cache.catalog);
                    source = ModelCatalogResolutionSource::StaleCache;
                }
            }
        }
    }

    let mut catalog = ModelCatalog::from_config_with_base(config, base);
    if let Some(path) = catalog_config.overrides_path_for(runtime_dir) {
        let overrides = ModelCatalog::load_overrides(&path).with_context(|| {
            format!(
                "failed to load model catalog overrides from {}",
                path.display()
            )
        })?;
        catalog.merge_from(&overrides);
    }

    Ok(ModelCatalogResolution {
        catalog,
        source,
        cache_path,
        cache_fresh,
        refresh_error,
    })
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codingbuddy_core::{ModelCatalogConfig, ModelInfo, ModelModality};
    use tempfile::tempdir;

    #[test]
    fn fresh_cache_is_used_without_remote_fetch() {
        let dir = tempdir().expect("tempdir");
        let cache_path = dir.path().join("catalog.json");
        let mut remote = ModelCatalog::empty(ModelCatalogSource::Remote);
        remote.upsert(ModelInfo {
            provider: "openrouter".to_string(),
            id: "cached/model".to_string(),
            display_name: "Cached Model".to_string(),
            modalities: vec![ModelModality::Text],
            ..ModelInfo::default()
        });
        ModelCatalogCache::new(remote, "https://example.invalid", current_unix_seconds())
            .save(&cache_path)
            .expect("save cache");

        let config = LlmConfig {
            model_catalog: ModelCatalogConfig {
                cache_path: Some(cache_path.display().to_string()),
                remote_url: "http://127.0.0.1:9/catalog.json".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let resolution = resolve_model_catalog(&config, dir.path()).expect("resolution");
        assert_eq!(resolution.source, ModelCatalogResolutionSource::FreshCache);
        assert!(resolution.cache_fresh);
        assert!(
            resolution
                .catalog
                .find("openrouter", "cached/model")
                .is_some()
        );
    }

    #[test]
    fn offline_mode_keeps_configured_models_without_cache() {
        let dir = tempdir().expect("tempdir");
        let config = LlmConfig {
            model_catalog: ModelCatalogConfig {
                offline: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let resolution = resolve_model_catalog(&config, dir.path()).expect("resolution");
        assert_eq!(resolution.source, ModelCatalogResolutionSource::Offline);
        assert!(
            resolution
                .catalog
                .find("deepseek", "deepseek-chat")
                .is_some()
        );
    }
}
