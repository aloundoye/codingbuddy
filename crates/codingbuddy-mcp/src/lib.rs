mod config;
#[cfg(test)]
mod conformance;
mod oauth;
mod serve;
mod sessions;
mod transport;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use chrono::Utc;
use codingbuddy_core::process_env::{self, Source};
use codingbuddy_store::{McpServerRecord, McpToolCacheRecord, Store};
pub use config::*;
pub use oauth::*;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
pub use serve::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::mpsc;
#[cfg(test)]
use std::thread;
use std::time::{Duration, Instant};
pub use transport::*;
#[cfg(test)]
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub enabled: bool,
    pub metadata: serde_json::Value,
    /// Custom headers for HTTP/SSE transport (e.g. authorization).
    pub headers: Vec<(String, String)>,
    #[serde(skip)]
    pub child_env: process_env::Variables,
}

impl Default for McpServer {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            transport: McpTransport::Stdio,
            command: None,
            args: Vec::new(),
            url: None,
            enabled: true,
            metadata: serde_json::Value::Null,
            headers: Vec::new(),
            child_env: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    pub servers: Vec<McpServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub server_id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub annotations: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolRefresh {
    pub server_id: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolChangeNotice {
    pub fingerprint: String,
    pub changed_servers: Vec<String>,
}

/// An MCP resource — a URI-addressable piece of content from an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub server_id: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

/// An MCP prompt — a parameterized prompt template from an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPrompt {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArgument>,
}

/// An argument for an MCP prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    pub description: String,
    pub required: bool,
}

pub struct McpManager {
    workspace: PathBuf,
    store: Store,
    trusted_workspace: bool,
    sessions: sessions::Sessions,
    catalog: std::sync::Mutex<BTreeMap<String, (Vec<u8>, McpTool)>>,
    catalog_fingerprints: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
}

impl McpManager {
    pub fn new(workspace: &Path) -> Result<Self> {
        Self::with_trust(workspace, codingbuddy_core::workspace_is_trusted(workspace))
    }

    pub fn with_trust(workspace: &Path, trusted_workspace: bool) -> Result<Self> {
        Ok(Self {
            workspace: workspace.to_path_buf(),
            store: Store::new(workspace)?,
            trusted_workspace,
            sessions: Default::default(),
            catalog: Default::default(),
            catalog_fingerprints: Default::default(),
        })
    }

    pub fn diagnostics(&self) -> Result<BTreeMap<String, String>> {
        let servers = self.list_servers()?;
        let mut states = self.session_states();
        for server in servers {
            let current = self
                .catalog_fingerprints
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&server.id)
                .is_some_and(|fingerprint| {
                    sessions::fingerprint(&server).is_ok_and(|current| *fingerprint == current)
                });
            if !current || !states.contains_key(&server.id) {
                states.insert(
                    server.id,
                    if server.enabled {
                        "configured"
                    } else {
                        "disabled"
                    }
                    .into(),
                );
            }
        }
        Ok(states)
    }
    pub fn session_states(&self) -> BTreeMap<String, String> {
        self.sessions
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub async fn request_server(
        &self,
        server: &McpServer,
        method: &str,
        params: serde_json::Value,
        cancellation: Option<&codingbuddy_core::CancellationToken>,
    ) -> Result<serde_json::Value> {
        let context = codingbuddy_core::execution::ExecutionContext {
            cancellation: cancellation.cloned().unwrap_or_default(),
            deadline: Some(Instant::now() + Duration::from_secs(30)),
            ..Default::default()
        };
        self.sessions
            .request(server, method, params, &context)
            .await
    }

    async fn request_list(
        &self,
        server: &McpServer,
        method: &str,
        key: &str,
    ) -> Result<Vec<serde_json::Value>> {
        let mut params = json!({});
        let mut items = Vec::new();
        let mut cursors = BTreeSet::new();
        loop {
            let response = self.request_server(server, method, params, None).await?;
            let result = response.get("result").context("MCP list result missing")?;
            items.extend(
                result
                    .get(key)
                    .and_then(|v| v.as_array())
                    .context("MCP list items missing")?
                    .iter()
                    .cloned(),
            );
            anyhow::ensure!(items.len() <= 100_000, "MCP catalog limit exceeded");
            let Some(cursor) = result.get("nextCursor").and_then(|v| v.as_str()) else {
                return Ok(items);
            };
            anyhow::ensure!(
                cursors.len() < 1024 && cursors.insert(cursor.to_owned()),
                "MCP pagination did not progress"
            );
            params = json!({"cursor":cursor});
        }
    }
    pub async fn call_tool(
        &self,
        alias: &str,
        args: &serde_json::Value,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> Result<serde_json::Value> {
        let started = std::time::Instant::now();
        let result = self.call_tool_untraced(alias, args, context).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => tracing::debug!(tool = alias, elapsed_ms, "MCP tool call finished"),
            Err(error) => tracing::warn!(
                tool = alias,
                elapsed_ms,
                error = %format!("{error:#}"),
                "MCP tool call failed"
            ),
        }
        result
    }

    async fn call_tool_untraced(
        &self,
        alias: &str,
        args: &serde_json::Value,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> Result<serde_json::Value> {
        let (expected, tool) = self
            .catalog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(alias)
            .cloned()
            .ok_or_else(|| anyhow!("MCP tool is not in the active registry"))?;
        let server = self
            .get_server(&tool.server_id)?
            .ok_or_else(|| anyhow!("MCP server disabled or denied"))?;
        anyhow::ensure!(
            !self.sessions.invalidated(&server.id),
            "MCP tool list changed or connection unavailable; refresh before approval"
        );
        anyhow::ensure!(
            expected == sessions::fingerprint(&server)?,
            "MCP configuration changed; refresh tools before approval"
        );
        let validator = jsonschema::validator_for(&tool.input_schema)
            .map_err(|_| anyhow!("Invalid MCP input schema"))?;
        anyhow::ensure!(
            validator.is_valid(args),
            "MCP arguments do not match the discovered schema"
        );
        let mut execution = context.execution();
        execution.deadline = Some(
            execution
                .deadline
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(30)),
        );
        let result = self
            .sessions
            .request(
                &server,
                "tools/call",
                json!({"name":tool.name,"arguments":args}),
                &execution,
            )
            .await?;
        result
            .get("result")
            .cloned()
            .context("MCP response missing result")
    }

    pub fn project_config_path(&self) -> PathBuf {
        self.workspace.join(".mcp.json")
    }

    pub fn user_config_path() -> Option<PathBuf> {
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())?;
        Some(Path::new(&home).join(".codingbuddy/mcp.json"))
    }

    pub fn user_local_config_path() -> Option<PathBuf> {
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())?;
        Some(Path::new(&home).join(".codingbuddy/mcp.local.json"))
    }

    pub fn load_project_config(&self) -> Result<McpConfig> {
        let path = self.project_config_path();
        if !path.exists() {
            return Ok(McpConfig::default());
        }
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }

    pub fn save_project_config(&self, config: &McpConfig) -> Result<()> {
        let path = self.project_config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(config)?)?;
        Ok(())
    }

    pub fn list_servers(&self) -> Result<Vec<McpServer>> {
        let mut candidates = Vec::new();
        let mut append = |config: McpConfig, source| {
            candidates.extend(config.servers.into_iter().map(|server| (server, source)));
        };
        if let Some(managed) = Self::load_managed_mcp_config() {
            append(managed, Source::Managed);
        }
        for path in [Self::user_config_path(), Self::user_local_config_path()]
            .into_iter()
            .flatten()
        {
            append(load_config_if_exists(&path)?, Source::User);
        }
        if self.trusted_workspace {
            append(self.load_project_config()?, Source::Project);
        } else if self.project_config_path().is_file() {
            eprintln!("[trust] Ignored project MCP configuration");
        }
        let mut seen = BTreeSet::new();
        let mut servers = Vec::new();
        for (mut server, source) in candidates {
            if !seen.insert(server.id.clone()) {
                continue;
            }
            if !server_allowed(&server) {
                self.sessions
                    .states
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(server.id.clone(), "denied".into());
                continue;
            }
            if !server.enabled {
                self.sessions
                    .states
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(server.id.clone(), "disabled".into());
            }
            if !server.metadata.is_object() {
                server.metadata = json!({});
            }
            server.metadata["_codingbuddy_source"] = json!(format!("{source:?}"));
            server.metadata["_codingbuddy_workspace"] = json!(self.workspace.canonicalize()?);
            server.child_env =
                process_env::variables(&self.workspace, &format!("mcp:{}", server.id), source);
            if expand_server_env_vars(&mut server).is_err() {
                eprintln!(
                    "[mcp] Skipped server with invalid configuration or unauthorized environment reference"
                );
                continue;
            }
            if server.url.is_some()
                && server.metadata.get("oauth").is_some()
                && !server
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
            {
                match load_mcp_token(&server.id)? {
                    Some(token)
                        if token.binding.as_ref() == Some(&mcp_token_binding(&server)?)
                            && !is_token_expired(&token)
                            && token.token_type.eq_ignore_ascii_case("Bearer") =>
                    {
                        server.headers.push((
                            "Authorization".into(),
                            format!("Bearer {}", token.access_token),
                        ))
                    }
                    _ => {
                        self.sessions
                            .states
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(server.id.clone(), "authentication_required".into());
                        continue;
                    }
                }
            }
            servers.push(server);
        }
        servers.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(servers)
    }

    /// Check whether a server ID comes from managed (enterprise) config.
    pub fn is_managed_server(server_id: &str) -> bool {
        Self::load_managed_mcp_config()
            .map(|cfg| cfg.servers.iter().any(|s| s.id == server_id))
            .unwrap_or(false)
    }

    pub fn get_server(&self, id: &str) -> Result<Option<McpServer>> {
        Ok(self.list_servers()?.into_iter().find(|s| s.id == id))
    }

    pub fn add_server(&self, server: McpServer) -> Result<()> {
        if server.id.trim().is_empty() {
            return Err(anyhow!("server id cannot be empty"));
        }
        let mut cfg = self.load_project_config()?;
        cfg.servers.retain(|existing| existing.id != server.id);
        cfg.servers.push(server.clone());
        self.save_project_config(&cfg)?;
        self.store.upsert_mcp_server(&McpServerRecord {
            server_id: server.id,
            name: server.name,
            transport: match server.transport {
                McpTransport::Stdio => "stdio".to_string(),
                McpTransport::Http => "http".to_string(),
                McpTransport::Sse => "sse".to_string(),
            },
            endpoint: server.command.or(server.url).unwrap_or_default(),
            enabled: server.enabled,
            metadata_json: serde_json::to_string(&server.metadata)?,
            updated_at: Utc::now().to_rfc3339(),
        })?;
        Ok(())
    }

    pub fn remove_server(&self, id: &str) -> Result<bool> {
        if Self::is_managed_server(id) {
            return Err(anyhow!("cannot remove managed server: {id}"));
        }
        let mut cfg = self.load_project_config()?;
        let before = cfg.servers.len();
        cfg.servers.retain(|existing| existing.id != id);
        let removed = cfg.servers.len() != before;
        if removed {
            self.save_project_config(&cfg)?;
            self.store.remove_mcp_server(id)?;
        }
        Ok(removed)
    }

    pub async fn discover_tools(&self) -> Result<Vec<McpTool>> {
        Ok(self.refresh_tools().await?.0)
    }

    /// Refresh tools from all MCP servers. Servers whose cached tools are
    /// younger than `TOOL_CACHE_TTL_SECS` are skipped (use `force_refresh_tools`
    /// to bypass the TTL).
    pub async fn refresh_tools(&self) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>)> {
        self.refresh_tools_inner(false).await
    }

    /// Force-refresh tools for all MCP servers, ignoring cache TTL.
    pub async fn force_refresh_tools(&self) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>)> {
        self.refresh_tools_inner(true).await
    }

    /// MCP tool cache TTL in seconds (5 minutes).
    const TOOL_CACHE_TTL_SECS: i64 = 300;

    async fn refresh_tools_inner(
        &self,
        force: bool,
    ) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>)> {
        let mut tools = Vec::new();
        let mut refreshes = Vec::new();
        let cached_rows = self.store.list_mcp_tool_cache()?;

        let existing = cached_rows.iter().fold(
            BTreeMap::<String, BTreeSet<String>>::new(),
            |mut acc, row| {
                acc.entry(row.server_id.clone())
                    .or_default()
                    .insert(row.tool_name.clone());
                acc
            },
        );

        // Build a map of server_id -> newest updated_at for TTL checking
        let newest_by_server: BTreeMap<String, chrono::DateTime<Utc>> = cached_rows
            .iter()
            .filter_map(|r| {
                chrono::DateTime::parse_from_rfc3339(&r.updated_at)
                    .ok()
                    .map(|dt| (r.server_id.clone(), dt.with_timezone(&Utc)))
            })
            .fold(BTreeMap::new(), |mut acc, (sid, dt)| {
                acc.entry(sid)
                    .and_modify(|prev| {
                        if dt > *prev {
                            *prev = dt;
                        }
                    })
                    .or_insert(dt);
                acc
            });

        for server in self.list_servers()?.into_iter().filter(|s| s.enabled) {
            // Skip re-discovery if cached tools are fresh enough
            let same_configuration = self
                .catalog_fingerprints
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&server.id)
                .is_some_and(|old| {
                    sessions::fingerprint(&server).is_ok_and(|current| *old == current)
                });
            if !force
                && !self.sessions.invalidated(&server.id)
                && same_configuration
                && let Some(newest) = newest_by_server.get(&server.id)
            {
                let age = Utc::now().signed_duration_since(*newest).num_seconds();
                if age < Self::TOOL_CACHE_TTL_SECS {
                    // Serve from cache
                    let cached_rows_for_server = cached_rows
                        .iter()
                        .filter(|row| row.server_id == server.id)
                        .collect::<Vec<_>>();
                    if !cached_rows_for_server.is_empty() {
                        for row in cached_rows_for_server {
                            let (input_schema, annotations) =
                                decode_cached_tool_metadata(&row.schema_json);
                            tools.push(McpTool {
                                server_id: server.id.clone(),
                                name: row.tool_name.clone(),
                                description: row.description.clone(),
                                input_schema,
                                annotations,
                            });
                        }
                    }
                    continue;
                }
            }

            let discovered = match self.request_list(&server, "tools/list", "tools").await {
                Ok(items) => parse_tools_array(&server.id, &items),
                Err(_) => {
                    self.catalog
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .retain(|_, (_, tool)| tool.server_id != server.id);
                    self.catalog_fingerprints
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&server.id);
                    eprintln!(
                        "[mcp] Discovery failed for {}; no tools activated",
                        server.id
                    );
                    continue;
                }
            };
            let fingerprint = sessions::fingerprint(&server)?;
            self.catalog_fingerprints
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(server.id.clone(), fingerprint.clone());
            {
                let mut catalog = self
                    .catalog
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                catalog.retain(|_, (_, tool)| tool.server_id != server.id);
                for tool in &discovered {
                    let alias = format!("mcp__{}__{}", server.id, tool.name);
                    anyhow::ensure!(!catalog.contains_key(&alias), "Ambiguous MCP tool alias");
                    catalog.insert(alias, (fingerprint.clone(), tool.clone()));
                }
            }
            let previous = existing.get(&server.id).cloned().unwrap_or_default();
            let mut current = BTreeSet::new();
            let mut replacement_rows = Vec::new();
            for tool in discovered {
                current.insert(tool.name.clone());
                replacement_rows.push(McpToolCacheRecord {
                    server_id: tool.server_id.clone(),
                    tool_name: tool.name.clone(),
                    description: tool.description.clone(),
                    schema_json: encode_cached_tool_metadata(&tool),
                    updated_at: Utc::now().to_rfc3339(),
                });
                tools.push(tool);
            }
            self.store
                .replace_mcp_tool_cache_for_server(&server.id, &replacement_rows)?;
            let added = current
                .difference(&previous)
                .cloned()
                .collect::<Vec<String>>();
            let removed = previous
                .difference(&current)
                .cloned()
                .collect::<Vec<String>>();
            refreshes.push(McpToolRefresh {
                server_id: server.id,
                added,
                removed,
                total: current.len(),
            });
        }
        Ok((tools, refreshes))
    }

    pub async fn discover_tools_with_notice(
        &self,
        previous_fingerprint: Option<&str>,
    ) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>, McpToolChangeNotice)> {
        let (tools, refreshes) = self.force_refresh_tools().await?;
        let fingerprint = tool_fingerprint(&tools)?;
        let changed_servers = if previous_fingerprint.is_some_and(|value| value == fingerprint) {
            Vec::new()
        } else {
            refreshes
                .iter()
                .filter(|refresh| !(refresh.added.is_empty() && refresh.removed.is_empty()))
                .map(|refresh| refresh.server_id.clone())
                .collect::<Vec<_>>()
        };
        Ok((
            tools,
            refreshes,
            McpToolChangeNotice {
                fingerprint,
                changed_servers,
            },
        ))
    }

    /// Resolve an MCP resource reference (`@server:protocol://path`).
    /// Returns the resource content as a string.
    pub async fn resolve_resource(&self, reference: &str) -> Result<String> {
        // Parse @server:uri format
        let stripped = reference.strip_prefix('@').unwrap_or(reference);
        let (server_id, uri) = stripped
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid resource reference: {reference}"))?;

        // Validate URI to prevent path traversal and abuse
        if contains_path_traversal(uri) || uri.len() > 1024 {
            return Err(anyhow!(
                "invalid resource URI: path traversal or excessive length"
            ));
        }

        let server = self
            .get_server(server_id)?
            .ok_or_else(|| anyhow!("MCP server not found: {server_id}"))?;
        ensure_server_enabled(&server)?;

        let response = self
            .request_server(&server, "resources/read", json!({"uri":uri}), None)
            .await?;
        extract_resource_text(
            response
                .get("result")
                .context("MCP resources/read response missing result")?,
        )
    }

    /// Import MCP servers from Claude Desktop configuration.
    pub fn import_from_claude_desktop(&self) -> Result<usize> {
        let servers = import_from_claude_desktop()?;
        let count = servers.len();
        for server in servers {
            self.add_server(server)?;
        }
        Ok(count)
    }

    /// Search tools by relevance when >50 tools are registered.
    /// Uses keyword scoring to surface the most relevant subset.
    pub async fn search_tools_by_relevance(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<McpTool>> {
        let all_tools = self.discover_tools().await?;

        // Below threshold, return all tools
        if all_tools.len() <= 50 {
            return Ok(all_tools);
        }

        let query_lower = query.to_ascii_lowercase();
        let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(f64, McpTool)> = all_tools
            .into_iter()
            .map(|tool| {
                let name_lower = tool.name.to_ascii_lowercase();
                let desc_lower = tool.description.to_ascii_lowercase();

                let mut score = 0.0_f64;

                // Exact name match is highest priority
                if name_lower == query_lower {
                    score += 10.0;
                }

                // Name contains query
                if name_lower.contains(&query_lower) {
                    score += 5.0;
                }

                // Per-term scoring: each query term that matches gets a score boost
                for term in &query_terms {
                    if name_lower.contains(term) {
                        score += 3.0;
                    }
                    if desc_lower.contains(term) {
                        score += 1.0;
                    }
                }

                // Server ID match bonus
                if tool.server_id.to_ascii_lowercase().contains(&query_lower) {
                    score += 2.0;
                }

                (score, tool)
            })
            .filter(|(score, _)| *score > 0.0)
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(max_results);
        Ok(scored.into_iter().map(|(_, tool)| tool).collect())
    }

    /// List MCP resources from all enabled servers for autocomplete.
    pub async fn list_resources(&self) -> Result<Vec<McpResource>> {
        let mut resources = Vec::new();
        for server in self.list_servers()?.into_iter().filter(|s| s.enabled) {
            let discovered = self
                .request_list(&server, "resources/list", "resources")
                .await
                .ok();
            if let Some(items) = discovered {
                for item in items {
                    resources.push(McpResource {
                        server_id: server.id.clone(),
                        uri: item
                            .get("uri")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        description: item
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        mime_type: item
                            .get("mimeType")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    });
                }
            }
        }
        Ok(resources)
    }

    /// Search resources by prefix for autocomplete (e.g. `@server:uri`).
    pub async fn autocomplete_resources(&self, prefix: &str) -> Result<Vec<McpResource>> {
        let all = self.list_resources().await?;
        let prefix_lower = prefix.to_ascii_lowercase();
        let mut matches: Vec<McpResource> = all
            .into_iter()
            .filter(|r| {
                let label = format!("@{}:{}", r.server_id, r.uri).to_ascii_lowercase();
                label.starts_with(&prefix_lower)
                    || r.name.to_ascii_lowercase().contains(&prefix_lower)
            })
            .collect();
        matches.truncate(10);
        Ok(matches)
    }

    /// Returns tools appropriate for the current context usage.
    /// When `context_used_pct` > 10%, uses search-based filtering if a query is provided.
    pub async fn tools_for_context(
        &self,
        context_used_pct: f64,
        query: Option<&str>,
        max_results: usize,
    ) -> Result<Vec<McpTool>> {
        if context_used_pct > 10.0
            && let Some(q) = query
        {
            return self.search_tools_by_relevance(q, max_results).await;
        }
        self.discover_tools().await
    }

    /// P5-12: List all prompts from MCP servers.
    ///
    /// Queries each enabled server for its `prompts/list` endpoint and returns
    /// a flat list of `McpPrompt` records. These can be registered as slash
    /// commands like `/mcp-<server>-<prompt>` in the UI.
    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>> {
        let mut prompts = Vec::new();
        for server in self.list_servers()?.into_iter().filter(|s| s.enabled) {
            let discovered = self
                .request_list(&server, "prompts/list", "prompts")
                .await
                .ok();
            if let Some(items) = discovered {
                for item in items {
                    let arguments = item
                        .get("arguments")
                        .and_then(|a| a.as_array())
                        .map(|arr| {
                            arr.iter()
                                .map(|a| McpPromptArgument {
                                    name: a
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    description: a
                                        .get("description")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    required: a
                                        .get("required")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    prompts.push(McpPrompt {
                        server_id: server.id.clone(),
                        name: item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        description: item
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments,
                    });
                }
            }
        }
        Ok(prompts)
    }

    /// P5-13: Handle `notifications/tools/list_changed` from MCP server.
    ///
    /// Re-discovers tools from all servers and returns the refresh results,
    /// allowing the caller to update tool definitions mid-session.
    pub async fn handle_list_changed_notification(
        &self,
    ) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>)> {
        self.refresh_tools().await
    }

    /// Fetch a prompt template with resolved arguments from an MCP server.
    ///
    /// Returns the rendered prompt messages as a JSON array.
    pub async fn get_prompt(
        &self,
        server_id: &str,
        prompt_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let server = self
            .get_server(server_id)?
            .ok_or_else(|| anyhow!("MCP server '{server_id}' not found"))?;
        ensure_server_enabled(&server)?;
        let params = json!({
            "name": prompt_name,
            "arguments": arguments,
        });
        let response = self
            .request_server(&server, "prompts/get", params, None)
            .await?;
        response
            .get("result")
            .cloned()
            .context("MCP prompts/get returned no result")
    }

    /// Handle an MCP elicitation request — the server is asking the user for input.
    ///
    /// Takes the elicitation parameters (message, schema) and a callback that
    /// prompts the user. Returns the user's response to send back to the server.
    pub fn handle_elicitation(
        &self,
        server_id: &str,
        params: &serde_json::Value,
        user_prompt_cb: &dyn Fn(&str, &serde_json::Value) -> Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Server requests input");
        let schema = params.get("requestedSchema").cloned().unwrap_or(json!({}));
        match user_prompt_cb(message, &schema) {
            Some(response) => Ok(json!({
                "action": "accept",
                "content": response,
            })),
            None => Ok(json!({
                "action": "decline",
                "content": {
                    "reason": format!("User declined elicitation from server '{server_id}'")
                },
            })),
        }
    }

    /// Load managed MCP config (enterprise lockdown).
    /// This is read from a system-wide location and cannot be overridden by users.
    pub fn load_managed_mcp_config() -> Option<McpConfig> {
        let path = managed_mcp_config_path()?;
        if !path.exists() {
            return None;
        }
        let raw = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

#[cfg(test)]
mod tests;
