//! API key resolution and storage (environment, keyring, settings).

use super::*;

/// Resolve an explicit credential, an environment key, or a saved key.
pub fn resolve_key_from_config(cfg: &LlmConfig) -> Option<String> {
    let provider = cfg.active_provider();
    let kind = cfg.active_provider_kind()?;
    let protocol = protocol::select_chat_protocol(&provider, kind);
    if protocol::select_auth_strategy(&provider, kind, protocol).uses_cloud_identity() {
        return None;
    }
    let env_key = provider.api_key_env.trim();
    let env_value = if env_key.is_empty() {
        None
    } else {
        std::env::var(env_key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    cfg.api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .or(env_value)
        .or_else(|| {
            (!env_key.is_empty())
                .then(|| credential_entry(cfg).ok()?.get_password().ok())
                .flatten()
        })
}

pub(crate) fn credential_entry(cfg: &LlmConfig) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "codingbuddy-provider",
        &cfg.connection_id(),
    )?)
}

pub fn save_provider_key(cfg: &LlmConfig, key: &str) -> Result<()> {
    Ok(credential_entry(cfg)?.set_password(key)?)
}

pub(crate) fn named_credential_entry(cfg: &LlmConfig, name: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "codingbuddy-provider-named",
        &format!("{}:{name}", cfg.connection_id()),
    )?)
}

pub(crate) fn credential_names_entry(cfg: &LlmConfig) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "codingbuddy-provider-key-names",
        &cfg.connection_id(),
    )?)
}

pub fn provider_key_names(cfg: &LlmConfig) -> Result<Vec<String>> {
    match credential_names_entry(cfg)?.get_password() {
        Ok(names) => Ok(serde_json::from_str(&names)?),
        Err(keyring::Error::NoEntry) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn load_named_provider_key(cfg: &LlmConfig, name: &str) -> Result<String> {
    Ok(named_credential_entry(cfg, name)?.get_password()?)
}

pub fn save_named_provider_key(cfg: &LlmConfig, name: &str, key: &str) -> Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty()
            && name.chars().count() <= 80
            && !name.chars().any(char::is_control),
        "Use a key name between 1 and 80 characters without control characters"
    );
    // Multiple terminal sessions can save keys for the same provider.
    let settings =
        codingbuddy_core::AppConfig::user_settings_path().context("Cannot locate user settings")?;
    let directory = settings.parent().context("Invalid user settings path")?;
    std::fs::create_dir_all(directory)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("named-credentials.lock"))?;
    lock.lock()?;
    let mut names = provider_key_names(cfg)?;
    anyhow::ensure!(
        !names.iter().any(|saved| saved == name),
        "That key name already exists"
    );
    names.push(name.to_owned());
    names.sort_unstable();
    let names = serde_json::to_string(&names)?;
    let index = credential_names_entry(cfg)?;
    let entry = named_credential_entry(cfg, name)?;
    entry.set_password(key)?;
    if let Err(error) = index.set_password(&names) {
        let _ = entry.delete_credential();
        return Err(error.into());
    }
    Ok(())
}

pub fn remove_provider_key(cfg: &LlmConfig) -> Result<()> {
    match credential_entry(cfg)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn provider_requires_key(cfg: &LlmConfig) -> bool {
    let provider = cfg.active_provider();
    let Some(kind) = cfg.active_provider_kind() else {
        return false;
    };
    let protocol = protocol::select_chat_protocol(&provider, kind);
    let auth = protocol::select_auth_strategy(&provider, kind, protocol);
    !provider.api_key_env.trim().is_empty()
        && auth != protocol::AuthStrategy::None
        && !auth.uses_cloud_identity()
}
