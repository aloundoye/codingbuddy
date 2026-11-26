use anyhow::{Result, anyhow};
use codingbuddy_core::execution::{AsyncChild, ExecutionContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
/// Starts a language server under policy. Runs inside the async runtime.
pub type LanguageLauncher =
    Arc<dyn Fn(&str, &[&str], &ExecutionContext) -> Result<AsyncChild> + Send + Sync>;

use crate::client::{LspClient, format_hover, format_locations, format_symbols};
use crate::language_map;

/// Per-file tracking: language ID + version counter for didChange.
struct OpenedFile {
    language_id: String,
    version: i32,
}

/// Manages multiple LSP server instances, one per language.
///
/// Clients are shared (`Arc`), so requests to one server run concurrently
/// and only server startup is serialised.
pub struct LspServerManager {
    workspace: PathBuf,
    launcher: Option<LanguageLauncher>,
    servers: tokio::sync::Mutex<HashMap<String, Arc<LspClient>>>,
    opened_files: Mutex<HashMap<PathBuf, OpenedFile>>,
}

impl LspServerManager {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            launcher: None,
            servers: tokio::sync::Mutex::new(HashMap::new()),
            opened_files: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_launcher(&mut self, launcher: LanguageLauncher) {
        self.launcher = Some(launcher);
    }

    /// Get hover information. Returns formatted text for LLM consumption.
    pub async fn hover_with_context(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
        context: &ExecutionContext,
    ) -> Result<String> {
        let Some((client, abs)) = self.client_for(file_path, context).await? else {
            return Ok(String::no_server());
        };
        Ok(format_hover(&client.hover(&abs, line, col).await?))
    }

    /// Get goto-definition results. Returns formatted text for LLM consumption.
    pub async fn definition_with_context(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
        context: &ExecutionContext,
    ) -> Result<String> {
        let Some((client, abs)) = self.client_for(file_path, context).await? else {
            return Ok(String::no_server());
        };
        Ok(format_locations(&client.definition(&abs, line, col).await?))
    }

    /// Get all references. Returns formatted text for LLM consumption.
    pub async fn references_with_context(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
        context: &ExecutionContext,
    ) -> Result<String> {
        let Some((client, abs)) = self.client_for(file_path, context).await? else {
            return Ok(String::no_server());
        };
        Ok(format_locations(&client.references(&abs, line, col).await?))
    }

    /// Get document symbols. Returns formatted text for LLM consumption.
    pub async fn symbols_with_context(
        &self,
        file_path: &Path,
        context: &ExecutionContext,
    ) -> Result<String> {
        let Some((client, abs)) = self.client_for(file_path, context).await? else {
            return Ok(String::no_server());
        };
        let formatted = format_symbols(&client.document_symbols(&abs).await?, 0);
        if formatted.is_empty() {
            return Ok("No symbols found.".to_string());
        }
        Ok(formatted)
    }

    /// Notify the server that a file changed (call after edits).
    pub async fn notify_file_changed(&self, file_path: &Path) -> Result<()> {
        let abs_path = self.abs_path(file_path);

        let version = {
            let mut opened = self
                .opened_files
                .lock()
                .map_err(|_| anyhow!("lock poisoned"))?;
            let Some(entry) = opened.get_mut(&abs_path) else {
                return Ok(());
            };
            entry.version += 1;
            (entry.language_id.clone(), entry.version)
        };

        let text = codingbuddy_core::workspace_fs::WorkspaceFs::open(&self.workspace)?
            .read_to_string(&abs_path)?;
        let client = self.servers.lock().await.get(&version.0).cloned();
        if let Some(client) = client {
            client.did_change(&abs_path, version.1, &text).await?;
        }
        Ok(())
    }

    /// Shut down all running servers.
    pub fn shutdown_all(&self) {
        if let Ok(mut servers) = self.servers.try_lock() {
            for (_, client) in servers.drain() {
                let _ = client.shutdown();
            }
        }
    }

    /// List running server language IDs.
    pub fn running_servers(&self) -> Vec<String> {
        self.servers
            .try_lock()
            .ok()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default()
    }

    // ── Private helpers ──

    /// Ensure a server is running and the file is open in it, then hand
    /// back the client and the file's absolute path. `None` when no server
    /// exists for this file type.
    async fn client_for(
        &self,
        file_path: &Path,
        context: &ExecutionContext,
    ) -> Result<Option<(Arc<LspClient>, PathBuf)>> {
        let Some((lang_id, client)) = self.ensure_server_for_file(file_path, context).await? else {
            return Ok(None);
        };
        client.set_context(context)?;
        self.ensure_file_opened(file_path, &lang_id, &client)
            .await?;
        Ok(Some((client, self.abs_path(file_path))))
    }

    async fn ensure_server_for_file(
        &self,
        file_path: &Path,
        context: &ExecutionContext,
    ) -> Result<Option<(String, Arc<LspClient>)>> {
        context.check()?;
        let launcher = self.launcher.as_ref().ok_or_else(|| {
            anyhow!("Language-server startup requires policy approval; no launcher is available")
        })?;
        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let entry = match language_map::lookup_extension(ext) {
            Some(e) => e,
            None => return Ok(None),
        };

        let lang_id = entry.language_id.to_string();

        let mut servers = codingbuddy_core::runtime::with_cancellation(
            Some(&context.cancellation),
            self.servers.lock(),
        )
        .await?;
        if let Some(existing) = servers.get(&lang_id) {
            if existing.is_alive() {
                return Ok(Some((lang_id, existing.clone())));
            }
            servers.remove(&lang_id);
        }

        if !crate::is_command_available(entry.server_command) {
            return Ok(None);
        }

        let child = launcher(entry.server_command, entry.server_args, context)?;
        let client = Arc::new(LspClient::from_owned(
            child,
            entry.server_command,
            &self.workspace,
            context.clone(),
        )?);
        client.initialize().await?;
        servers.insert(lang_id.clone(), client.clone());

        Ok(Some((lang_id, client)))
    }

    async fn ensure_file_opened(
        &self,
        file_path: &Path,
        language_id: &str,
        client: &LspClient,
    ) -> Result<()> {
        let abs_path = self.abs_path(file_path);

        // Check if already opened (quick path — drop lock before I/O)
        {
            let opened = self
                .opened_files
                .lock()
                .map_err(|_| anyhow!("lock poisoned"))?;
            if opened.contains_key(&abs_path) {
                return Ok(());
            }
        }

        let text = codingbuddy_core::workspace_fs::WorkspaceFs::open(&self.workspace)?
            .read_to_string(&abs_path)
            .map_err(|e| anyhow!("failed to read {}: {}", abs_path.display(), e))?;

        client.did_open(&abs_path, language_id, &text).await?;

        let mut opened = self
            .opened_files
            .lock()
            .map_err(|_| anyhow!("lock poisoned"))?;
        opened.insert(
            abs_path,
            OpenedFile {
                language_id: language_id.to_string(),
                version: 0,
            },
        );
        Ok(())
    }

    fn abs_path(&self, file_path: &Path) -> PathBuf {
        if file_path.is_absolute() {
            file_path.to_path_buf()
        } else {
            self.workspace.join(file_path)
        }
    }
}

impl Drop for LspServerManager {
    fn drop(&mut self) {
        for (_, client) in self.servers.get_mut().drain() {
            let _ = client.shutdown();
        }
    }
}

/// Trait for types that can produce a "no LSP server" fallback value.
trait NoServerFallback {
    fn no_server() -> Self;
}

impl NoServerFallback for String {
    fn no_server() -> Self {
        "No LSP server available for this file type.".to_string()
    }
}
