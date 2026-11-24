use anyhow::{Result, anyhow};
use codingbuddy_core::execution::{ExecutionContext, OwnedChild};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
pub type LanguageLauncher =
    Arc<dyn Fn(&str, &[&str], &ExecutionContext) -> Result<OwnedChild> + Send + Sync>;

use crate::client::{LspClient, format_hover, format_locations, format_symbols};
use crate::language_map;

/// Per-file tracking: language ID + version counter for didChange.
struct OpenedFile {
    language_id: String,
    version: i32,
}

/// Manages multiple LSP server instances, one per language.
pub struct LspServerManager {
    workspace: PathBuf,
    launcher: Option<LanguageLauncher>,
    servers: Mutex<HashMap<String, LspClient>>,
    opened_files: Mutex<HashMap<PathBuf, OpenedFile>>,
}

impl LspServerManager {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            launcher: None,
            servers: Mutex::new(HashMap::new()),
            opened_files: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_launcher(&mut self, launcher: LanguageLauncher) {
        self.launcher = Some(launcher);
    }
    /// Get hover information. Returns formatted text for LLM consumption.
    pub fn hover(&self, file_path: &Path, line: u32, col: u32) -> Result<String> {
        self.hover_with_context(file_path, line, col, &ExecutionContext::default())
    }
    pub fn hover_with_context(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
        context: &ExecutionContext,
    ) -> Result<String> {
        self.with_client(file_path, context, |client, abs| {
            let result = client.hover(abs, line, col)?;
            Ok(format_hover(&result))
        })
    }

    /// Get goto-definition results. Returns formatted text for LLM consumption.
    pub fn definition(&self, file_path: &Path, line: u32, col: u32) -> Result<String> {
        self.definition_with_context(file_path, line, col, &ExecutionContext::default())
    }
    pub fn definition_with_context(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
        context: &ExecutionContext,
    ) -> Result<String> {
        self.with_client(file_path, context, |client, abs| {
            let locs = client.definition(abs, line, col)?;
            Ok(format_locations(&locs))
        })
    }

    /// Get all references. Returns formatted text for LLM consumption.
    pub fn references(&self, file_path: &Path, line: u32, col: u32) -> Result<String> {
        self.references_with_context(file_path, line, col, &ExecutionContext::default())
    }
    pub fn references_with_context(
        &self,
        file_path: &Path,
        line: u32,
        col: u32,
        context: &ExecutionContext,
    ) -> Result<String> {
        self.with_client(file_path, context, |client, abs| {
            let locs = client.references(abs, line, col)?;
            Ok(format_locations(&locs))
        })
    }

    /// Get document symbols. Returns formatted text for LLM consumption.
    pub fn symbols(&self, file_path: &Path) -> Result<String> {
        self.symbols_with_context(file_path, &ExecutionContext::default())
    }
    pub fn symbols_with_context(
        &self,
        file_path: &Path,
        context: &ExecutionContext,
    ) -> Result<String> {
        self.with_client(file_path, context, |client, abs| {
            let syms = client.document_symbols(abs)?;
            let formatted = format_symbols(&syms, 0);
            if formatted.is_empty() {
                return Ok("No symbols found.".to_string());
            }
            Ok(formatted)
        })
    }

    /// Notify the server that a file changed (call after edits).
    pub fn notify_file_changed(&self, file_path: &Path) -> Result<()> {
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
        let servers = self.servers.lock().map_err(|_| anyhow!("lock poisoned"))?;
        if let Some(client) = servers.get(&version.0) {
            client.did_change(&abs_path, version.1, &text)?;
        }
        Ok(())
    }

    /// Shut down all running servers.
    pub fn shutdown_all(&self) {
        if let Ok(mut servers) = self.servers.lock() {
            for (_, client) in servers.drain() {
                let _ = client.shutdown();
            }
        }
    }

    /// List running server language IDs.
    pub fn running_servers(&self) -> Vec<String> {
        self.servers
            .lock()
            .ok()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default()
    }

    // ── Private helpers ──

    fn lock_servers(
        &self,
        context: &ExecutionContext,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, LspClient>>> {
        loop {
            context.check()?;
            match self.servers.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                Err(_) => anyhow::bail!("Language registry unavailable"),
            }
        }
    }
    /// Ensure a server is available, the file is opened, then call `f` with the client.
    /// Returns a "no server" message if no LSP is available for this file type.
    fn with_client<T>(
        &self,
        file_path: &Path,
        context: &ExecutionContext,
        f: impl FnOnce(&LspClient, &Path) -> Result<T>,
    ) -> Result<T>
    where
        T: NoServerFallback,
    {
        let lang_id = match self.ensure_server_for_file(file_path, context)? {
            Some(id) => id,
            None => return Ok(T::no_server()),
        };
        {
            let servers = self
                .servers
                .lock()
                .map_err(|_| anyhow!("Language registry unavailable"))?;
            if let Some(client) = servers.get(&lang_id) {
                client.set_context(context)?;
            }
        }
        self.ensure_file_opened(file_path, &lang_id, context)?;

        let abs_path = self.abs_path(file_path);
        let servers = self.lock_servers(context)?;
        let client = servers
            .get(&lang_id)
            .ok_or_else(|| anyhow!("server disappeared"))?;
        client.set_context(context)?;
        f(client, &abs_path)
    }

    fn ensure_server_for_file(
        &self,
        file_path: &Path,
        context: &ExecutionContext,
    ) -> Result<Option<String>> {
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

        let mut servers = self.lock_servers(context)?;
        if let Some(existing) = servers.get(&lang_id) {
            if existing.is_alive() {
                return Ok(Some(lang_id));
            }
            servers.remove(&lang_id);
        }

        if !crate::is_command_available(entry.server_command) {
            return Ok(None);
        }

        let child = launcher(entry.server_command, entry.server_args, context)?;
        let client = LspClient::from_owned(
            child,
            entry.server_command,
            &self.workspace,
            context.clone(),
        )?;
        client.initialize()?;
        servers.insert(lang_id.clone(), client);

        Ok(Some(lang_id))
    }

    fn ensure_file_opened(
        &self,
        file_path: &Path,
        language_id: &str,
        context: &ExecutionContext,
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

        let servers = self.lock_servers(context)?;
        if let Some(client) = servers.get(language_id) {
            client.did_open(&abs_path, language_id, &text)?;
        }
        drop(servers);

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
        self.shutdown_all();
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
