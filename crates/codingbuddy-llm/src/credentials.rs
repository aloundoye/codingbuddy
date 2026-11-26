//! API key resolution and storage.
//!
//! Saved keys live in `~/.codingbuddy/credentials.json` (directory 0700, file
//! 0600), like opencode's `auth.json`. The OS keychain is only read to migrate
//! keys saved by older versions, and never when `CODINGBUDDY_NO_KEYCHAIN` is
//! set. `CODINGBUDDY_CREDENTIALS_FILE` points the store somewhere else.

use super::*;
use serde::{Deserialize, Serialize};

/// Resolve an explicit credential, an environment key, or a saved key.
pub fn resolve_key_from_config(cfg: &LlmConfig) -> Option<String> {
    let provider = cfg.active_provider();
    let kind = cfg.active_provider_kind()?;
    let protocol = protocol::select_chat_protocol(&provider, kind);
    if protocol::select_auth_strategy(&provider, kind, protocol).uses_cloud_identity() {
        return None;
    }
    let env_key = provider.api_key_env.trim();
    let read_env = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let env_value = if env_key.is_empty() {
        None
    } else {
        // Google's own tools read GEMINI_API_KEY; accept it for GOOGLE_API_KEY.
        read_env(env_key).or_else(|| {
            (env_key == "GOOGLE_API_KEY")
                .then(|| read_env("GEMINI_API_KEY"))
                .flatten()
        })
    };
    cfg.api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .or(env_value)
        .or_else(|| {
            (!env_key.is_empty())
                .then(|| saved_provider_key(cfg))
                .flatten()
        })
}

/// The key saved for this provider connection: the credentials file, or a
/// key an older version left in the keychain (copied into the file once).
fn saved_provider_key(cfg: &LlmConfig) -> Option<String> {
    let id = cfg.connection_id();
    match read_credentials() {
        Ok(file) => {
            if let Some(key) = file.providers.get(&id).and_then(|saved| saved.key.clone()) {
                return Some(key);
            }
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "cannot read saved credentials")
        }
    }
    let key = legacy_keychain_get("codingbuddy-provider", &id)?;
    if let Err(error) = save_provider_key(cfg, &key) {
        tracing::warn!(error = %format!("{error:#}"), "could not move keychain key to credentials file");
    }
    Some(key)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialFile {
    #[serde(default)]
    providers: BTreeMap<String, ProviderCredentials>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProviderCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    /// Extra keys saved under a name, for switching accounts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    named: BTreeMap<String, String>,
}

impl ProviderCredentials {
    fn is_empty(&self) -> bool {
        self.key.is_none() && self.named.is_empty()
    }
}

/// Where saved keys live.
pub fn credentials_path() -> Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("CODINGBUDDY_CREDENTIALS_FILE").filter(|p| !p.is_empty()) {
        return Ok(path.into());
    }
    let settings = codingbuddy_core::AppConfig::user_settings_path()
        .context("Cannot locate the home directory")?;
    Ok(settings.with_file_name("credentials.json"))
}

fn read_credentials() -> Result<CredentialFile> {
    let path = credentials_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid credentials JSON", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CredentialFile::default()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Read, change and rewrite the credentials file under a lock, so several
/// terminals saving keys at once do not lose each other's writes.
fn update_credentials<T>(change: impl FnOnce(&mut CredentialFile) -> Result<T>) -> Result<T> {
    use std::io::Write;
    let path = credentials_path()?;
    let directory = path.parent().context("Invalid credentials path")?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("credentials.lock"))?;
    lock.lock()?;

    let mut file = read_credentials()?;
    let result = change(&mut file)?;
    file.providers.retain(|_, saved| !saved.is_empty());

    // Write a private temporary file, then rename it over the old one.
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&temporary).and_then(|mut out| {
        out.write_all(serde_json::to_string_pretty(&file)?.as_bytes())?;
        out.write_all(b"\n")?;
        out.sync_all()
    });
    if let Err(error) = written.and_then(|()| std::fs::rename(&temporary, &path)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("cannot write {}", path.display()));
    }
    Ok(result)
}

/// Read a key an older version saved in the OS keychain.
fn legacy_keychain_get(service: &str, account: &str) -> Option<String> {
    if codingbuddy_core::keychain_disabled() {
        return None;
    }
    keyring::Entry::new(service, account)
        .ok()?
        .get_password()
        .ok()
}

/// Remove a key an older version saved in the OS keychain.
fn legacy_keychain_delete(service: &str, account: &str) {
    if codingbuddy_core::keychain_disabled() {
        return;
    }
    if let Ok(entry) = keyring::Entry::new(service, account) {
        let _ = entry.delete_credential();
    }
}

pub fn save_provider_key(cfg: &LlmConfig, key: &str) -> Result<()> {
    let id = cfg.connection_id();
    update_credentials(|file| {
        file.providers.entry(id).or_default().key = Some(key.to_owned());
        Ok(())
    })
}

pub fn provider_key_names(cfg: &LlmConfig) -> Result<Vec<String>> {
    let id = cfg.connection_id();
    let mut names: Vec<String> = read_credentials()?
        .providers
        .get(&id)
        .map(|saved| saved.named.keys().cloned().collect())
        .unwrap_or_default();
    if names.is_empty()
        && let Some(legacy) = legacy_keychain_get("codingbuddy-provider-key-names", &id)
    {
        names = serde_json::from_str(&legacy).unwrap_or_default();
    }
    Ok(names)
}

pub fn load_named_provider_key(cfg: &LlmConfig, name: &str) -> Result<String> {
    let id = cfg.connection_id();
    if let Some(key) = read_credentials()?
        .providers
        .get(&id)
        .and_then(|saved| saved.named.get(name).cloned())
    {
        return Ok(key);
    }
    let key = legacy_keychain_get("codingbuddy-provider-named", &format!("{id}:{name}"))
        .with_context(|| format!("No saved key named {name:?}"))?;
    update_credentials(|file| {
        file.providers
            .entry(id)
            .or_default()
            .named
            .insert(name.to_owned(), key.clone());
        Ok(())
    })?;
    Ok(key)
}

pub fn save_named_provider_key(cfg: &LlmConfig, name: &str, key: &str) -> Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty()
            && name.chars().count() <= 80
            && !name.chars().any(char::is_control),
        "Use a key name between 1 and 80 characters without control characters"
    );
    let legacy_names = provider_key_names(cfg)?;
    let id = cfg.connection_id();
    update_credentials(|file| {
        let saved = file.providers.entry(id).or_default();
        anyhow::ensure!(
            !saved.named.contains_key(name) && !legacy_names.iter().any(|known| known == name),
            "That key name already exists"
        );
        saved.named.insert(name.to_owned(), key.to_owned());
        Ok(())
    })
}

pub fn remove_provider_key(cfg: &LlmConfig) -> Result<()> {
    let id = cfg.connection_id();
    update_credentials(|file| {
        if let Some(saved) = file.providers.get_mut(&id) {
            saved.key = None;
        }
        Ok(())
    })?;
    legacy_keychain_delete("codingbuddy-provider", &id);
    Ok(())
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

#[cfg(test)]
mod credential_file_tests {
    use super::*;

    /// A config whose connection id no other test uses. Cargo points
    /// CODINGBUDDY_CREDENTIALS_FILE at a scratch file under target/.
    fn unique_config() -> LlmConfig {
        let mut cfg = LlmConfig::default();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        cfg.endpoint = format!("https://example.invalid/{}-{nanos}", std::process::id());
        cfg
    }

    #[test]
    fn keys_are_saved_to_a_private_file_and_removed() {
        let path = credentials_path().unwrap();
        assert!(
            path.starts_with(
                env!("CARGO_MANIFEST_DIR")
                    .rsplit_once("/crates/")
                    .unwrap()
                    .0
            ) || std::env::var_os("CODINGBUDDY_CREDENTIALS_FILE").is_some(),
            "tests must not use the real credentials file: {}",
            path.display()
        );
        let cfg = unique_config();
        assert_eq!(saved_provider_key(&cfg), None);

        save_provider_key(&cfg, "primary-key").unwrap();
        assert_eq!(saved_provider_key(&cfg).as_deref(), Some("primary-key"));

        save_named_provider_key(&cfg, "work", "work-key").unwrap();
        assert!(save_named_provider_key(&cfg, "work", "other").is_err());
        assert_eq!(provider_key_names(&cfg).unwrap(), vec!["work".to_string()]);
        assert_eq!(load_named_provider_key(&cfg, "work").unwrap(), "work-key");

        remove_provider_key(&cfg).unwrap();
        assert_eq!(saved_provider_key(&cfg), None);
        assert_eq!(load_named_provider_key(&cfg, "work").unwrap(), "work-key");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                dir_mode & 0o077,
                0,
                "directory must not be group/world accessible"
            );
        }
    }
}
