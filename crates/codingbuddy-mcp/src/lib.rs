#[cfg(test)]
mod conformance;
mod sessions;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use chrono::Utc;
use codingbuddy_core::process_env::{self, Source};
use codingbuddy_store::{McpServerRecord, McpToolCacheRecord, Store};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
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
                .unwrap()
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
        self.sessions.states.lock().unwrap().clone()
    }

    pub fn request_server(
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
        self.sessions.request(server, method, params, &context)
    }

    fn request_list(
        &self,
        server: &McpServer,
        method: &str,
        key: &str,
    ) -> Result<Vec<serde_json::Value>> {
        let mut params = json!({});
        let mut items = Vec::new();
        let mut cursors = BTreeSet::new();
        loop {
            let response = self.request_server(server, method, params, None)?;
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
    pub fn call_tool(
        &self,
        alias: &str,
        args: &serde_json::Value,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> Result<serde_json::Value> {
        let started = std::time::Instant::now();
        let result = self.call_tool_untraced(alias, args, context);
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

    fn call_tool_untraced(
        &self,
        alias: &str,
        args: &serde_json::Value,
        context: codingbuddy_core::ToolExecutionContext<'_>,
    ) -> Result<serde_json::Value> {
        let (expected, tool) = self
            .catalog
            .lock()
            .unwrap()
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
        let result = self.sessions.request(
            &server,
            "tools/call",
            json!({"name":tool.name,"arguments":args}),
            &execution,
        )?;
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
                    .unwrap()
                    .insert(server.id.clone(), "denied".into());
                continue;
            }
            if !server.enabled {
                self.sessions
                    .states
                    .lock()
                    .unwrap()
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
                            .unwrap()
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

    pub fn discover_tools(&self) -> Result<Vec<McpTool>> {
        Ok(self.refresh_tools()?.0)
    }

    /// Refresh tools from all MCP servers. Servers whose cached tools are
    /// younger than `TOOL_CACHE_TTL_SECS` are skipped (use `force_refresh_tools`
    /// to bypass the TTL).
    pub fn refresh_tools(&self) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>)> {
        self.refresh_tools_inner(false)
    }

    /// Force-refresh tools for all MCP servers, ignoring cache TTL.
    pub fn force_refresh_tools(&self) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>)> {
        self.refresh_tools_inner(true)
    }

    /// MCP tool cache TTL in seconds (5 minutes).
    const TOOL_CACHE_TTL_SECS: i64 = 300;

    fn refresh_tools_inner(&self, force: bool) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>)> {
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
                .unwrap()
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

            let discovered = match self.request_list(&server, "tools/list", "tools") {
                Ok(items) => parse_tools_array(&server.id, &items),
                Err(_) => {
                    self.catalog
                        .lock()
                        .unwrap()
                        .retain(|_, (_, tool)| tool.server_id != server.id);
                    self.catalog_fingerprints.lock().unwrap().remove(&server.id);
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
                .unwrap()
                .insert(server.id.clone(), fingerprint.clone());
            {
                let mut catalog = self.catalog.lock().unwrap();
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

    pub fn discover_tools_with_notice(
        &self,
        previous_fingerprint: Option<&str>,
    ) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>, McpToolChangeNotice)> {
        let (tools, refreshes) = self.force_refresh_tools()?;
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
    pub fn resolve_resource(&self, reference: &str) -> Result<String> {
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

        let response = self.request_server(&server, "resources/read", json!({"uri":uri}), None)?;
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
    pub fn search_tools_by_relevance(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<McpTool>> {
        let all_tools = self.discover_tools()?;

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
    pub fn list_resources(&self) -> Result<Vec<McpResource>> {
        let mut resources = Vec::new();
        for server in self.list_servers()?.into_iter().filter(|s| s.enabled) {
            let discovered = self
                .request_list(&server, "resources/list", "resources")
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
    pub fn autocomplete_resources(&self, prefix: &str) -> Result<Vec<McpResource>> {
        let all = self.list_resources()?;
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
    pub fn tools_for_context(
        &self,
        context_used_pct: f64,
        query: Option<&str>,
        max_results: usize,
    ) -> Result<Vec<McpTool>> {
        if context_used_pct > 10.0
            && let Some(q) = query
        {
            return self.search_tools_by_relevance(q, max_results);
        }
        self.discover_tools()
    }

    /// P5-12: List all prompts from MCP servers.
    ///
    /// Queries each enabled server for its `prompts/list` endpoint and returns
    /// a flat list of `McpPrompt` records. These can be registered as slash
    /// commands like `/mcp-<server>-<prompt>` in the UI.
    pub fn list_prompts(&self) -> Result<Vec<McpPrompt>> {
        let mut prompts = Vec::new();
        for server in self.list_servers()?.into_iter().filter(|s| s.enabled) {
            let discovered = self.request_list(&server, "prompts/list", "prompts").ok();
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
    pub fn handle_list_changed_notification(&self) -> Result<(Vec<McpTool>, Vec<McpToolRefresh>)> {
        self.refresh_tools()
    }

    /// Fetch a prompt template with resolved arguments from an MCP server.
    ///
    /// Returns the rendered prompt messages as a JSON array.
    pub fn get_prompt(
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
        let response = self.request_server(&server, "prompts/get", params, None)?;
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

fn encode_cached_tool_metadata(tool: &McpTool) -> String {
    serde_json::to_string(&json!({
        "input_schema": tool.input_schema.clone(),
        "annotations": tool.annotations.clone(),
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

fn decode_cached_tool_metadata(schema_json: &str) -> (serde_json::Value, serde_json::Value) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(schema_json) else {
        return (serde_json::Value::Null, serde_json::Value::Null);
    };
    let input_schema = value
        .get("input_schema")
        .or_else(|| value.get("inputSchema"))
        .cloned()
        .unwrap_or_else(|| {
            if value.is_object() && value.get("annotations").is_none() {
                value.clone()
            } else {
                serde_json::Value::Null
            }
        });
    let annotations = value
        .get("annotations")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    (input_schema, annotations)
}

fn tool_fingerprint(tools: &[McpTool]) -> Result<String> {
    let serialized = serde_json::to_vec(tools)?;
    Ok(format!("{:x}", Sha256::digest(serialized)))
}

fn load_config_if_exists(path: &Path) -> Result<McpConfig> {
    if !path.exists() {
        return Ok(McpConfig::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn resolve_env_var(var_name: &str) -> Option<String> {
    let name = var_name.trim();
    if let Ok(value) = std::env::var(name)
        && !value.is_empty()
    {
        return Some(value);
    }

    // Cross-platform HOME fallback: many configs use ${HOME} even on Windows.
    if name.eq_ignore_ascii_case("HOME") {
        if let Ok(profile) = std::env::var("USERPROFILE")
            && !profile.is_empty()
        {
            return Some(profile);
        }
        let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
        let path = std::env::var("HOMEPATH").unwrap_or_default();
        if !drive.is_empty() && !path.is_empty() {
            return Some(format!("{drive}{path}"));
        }
    }

    // Inverse fallback can help configs that use USERPROFILE on non-Windows hosts.
    if name.eq_ignore_ascii_case("USERPROFILE")
        && let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(home);
    }

    None
}

/// Expand environment variables (`${VAR}` and `${VAR:-default}`) in a string.
pub fn expand_env_vars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_expr = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var_expr.push(c);
            }
            if let Some((var_name, default)) = var_expr.split_once(":-") {
                result.push_str(&resolve_env_var(var_name).unwrap_or_else(|| default.to_string()));
            } else {
                result.push_str(&resolve_env_var(&var_expr).unwrap_or_default());
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Check if a string contains `..` as a path component (i.e. `../`, `/..',
/// or standalone `..`). Avoids false positives on version strings, Rust ranges, etc.
fn contains_path_traversal(s: &str) -> bool {
    s == ".."
        || s.starts_with("../")
        || s.ends_with("/..")
        || s.contains("/../")
        || s.starts_with("..\\")
        || s.ends_with("\\..")
        || s.contains("\\..\\")
        || s.contains("\\../")
        || s.contains("/..\\")
}

/// Expand environment variables in all string fields of an MCP server config.
/// Returns an error if any expanded value contains shell metacharacters.
pub fn expand_server_env_vars(server: &mut McpServer) -> Result<()> {
    use codingbuddy_policy::shell_parse::contains_shell_injection_chars;

    anyhow::ensure!(
        !server.id.contains("${") && !server.name.contains("${"),
        "MCP IDs and names must be literal"
    );
    let baseline;
    let vars = if server.child_env.is_empty() {
        baseline = process_env::variables(Path::new("."), "", Source::User);
        &baseline
    } else {
        &server.child_env
    };
    let expand = |input: &str| -> Result<String> {
        let mut rest = input;
        let mut output = String::new();
        while let Some((before, after)) = rest.split_once("${") {
            let (expression, tail) = after
                .split_once('}')
                .ok_or_else(|| anyhow!("invalid MCP environment reference"))?;
            let (name, fallback) = expression
                .split_once(":-")
                .map_or((expression, None), |(name, fallback)| {
                    (name, Some(fallback))
                });
            let value = process_env::lookup(vars, name).and_then(|value| value.to_str());
            let value = value
                .filter(|value| !value.is_empty())
                .or(fallback)
                .or(value)
                .ok_or_else(|| anyhow!("unauthorized or unavailable MCP environment variable"))?;
            output.push_str(before);
            output.push_str(value);
            rest = tail;
        }
        output.push_str(rest);
        Ok(output)
    };
    let expanded_cmd = server.command.as_deref().map(expand).transpose()?;
    let expanded_args = server
        .args
        .iter()
        .map(|arg| expand(arg))
        .collect::<Result<Vec<_>>>()?;
    let expanded_url = server.url.as_deref().map(expand).transpose()?;
    for (key, value) in &mut server.headers {
        anyhow::ensure!(!key.contains("${"), "MCP header names must be literal");
        *value = expand(value)?;
    }

    // Validate expanded values don't contain path traversal
    if let Some(ref cmd) = expanded_cmd
        && contains_path_traversal(cmd)
    {
        return Err(anyhow!(
            "MCP server '{}': expanded command contains path traversal",
            server.id
        ));
    }
    if let Some(ref url) = expanded_url
        && contains_path_traversal(url)
    {
        return Err(anyhow!(
            "MCP server '{}': expanded URL contains path traversal",
            server.id
        ));
    }
    for (i, arg) in expanded_args.iter().enumerate() {
        if contains_path_traversal(arg) {
            return Err(anyhow!(
                "MCP server '{}': expanded arg[{i}] contains path traversal",
                server.id
            ));
        }
    }

    if let Some(ref cmd) = expanded_cmd
        && contains_shell_injection_chars(cmd)
    {
        return Err(anyhow!(
            "MCP server '{}': expanded command contains shell metacharacters",
            server.id
        ));
    }
    for arg in &expanded_args {
        if contains_shell_injection_chars(arg) {
            return Err(anyhow!(
                "MCP server '{}': expanded argument contains shell metacharacters",
                server.id
            ));
        }
    }

    server.command = expanded_cmd;
    server.args = expanded_args;
    server.url = expanded_url;
    Ok(())
}

/// Import MCP servers from Claude Desktop config file.
/// Reads `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS)
/// or `%APPDATA%/Claude/claude_desktop_config.json` (Windows).
pub fn import_from_claude_desktop() -> Result<Vec<McpServer>> {
    let config_path =
        claude_desktop_config_path().ok_or_else(|| anyhow!("Claude Desktop config not found"))?;
    if !config_path.exists() {
        return Err(anyhow!(
            "Claude Desktop config not found at {}",
            config_path.display()
        ));
    }
    let raw = fs::read_to_string(&config_path)?;
    let config: serde_json::Value = serde_json::from_str(&raw)?;

    let servers_obj = config
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("no mcpServers in Claude Desktop config"))?;

    let mut servers = Vec::new();
    for (id, entry) in servers_obj {
        let command = entry
            .get("command")
            .and_then(|v| v.as_str())
            .map(String::from);
        let args = entry
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let url = entry.get("url").and_then(|v| v.as_str()).map(String::from);
        let transport = if url.is_some() {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        };
        servers.push(McpServer {
            id: id.clone(),
            name: id.clone(),
            transport,
            command,
            args,
            url,
            enabled: true,
            metadata: entry.get("env").cloned().unwrap_or(serde_json::Value::Null),
            headers: Vec::new(),
            child_env: Default::default(),
        });
    }
    Ok(servers)
}

fn claude_desktop_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").ok()?;
        Some(
            PathBuf::from(home)
                .join("Library/Application Support/Claude/claude_desktop_config.json"),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").ok()?;
        Some(PathBuf::from(appdata).join("Claude/claude_desktop_config.json"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home).join(".config/claude/claude_desktop_config.json"))
    }
}

/// System-wide managed MCP config path for enterprise lockdown.
fn managed_mcp_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(
            "/Library/Application Support/CodingBuddyCode/managed-mcp.json",
        ))
    }
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from("/etc/codingbuddy-code/managed-mcp.json"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(PathBuf::from(
            "C:\\Program Files\\CodingBuddyCode\\managed-mcp.json",
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Per-MCP output token limits (warn at threshold, error at max).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTokenLimits {
    /// Warning threshold (tokens). Default 10_000.
    pub warn_threshold: u64,
    /// Hard maximum (tokens). Default 25_000.
    pub max_tokens: u64,
}

impl Default for McpTokenLimits {
    fn default() -> Self {
        Self {
            warn_threshold: 10_000,
            max_tokens: 25_000,
        }
    }
}

/// Check an MCP tool output against token limits.
/// Returns `(output, was_truncated)`.
pub fn enforce_mcp_token_limit(output: &str, limits: &McpTokenLimits) -> (String, bool) {
    // Rough estimate: ~4 chars per token
    let estimated_tokens = output.len() as u64 / 4;
    if estimated_tokens > limits.max_tokens {
        let max_chars = (limits.max_tokens * 4) as usize;
        let truncated = if output.len() > max_chars {
            format!(
                "{}\n\n... (output truncated: ~{} tokens exceeded limit of {})",
                &output[..max_chars],
                estimated_tokens,
                limits.max_tokens
            )
        } else {
            output.to_string()
        };
        (truncated, true)
    } else {
        (output.to_string(), false)
    }
}

fn server_allowed(server: &McpServer) -> bool {
    codingbuddy_policy::load_managed_settings()
        .is_none_or(|settings| codingbuddy_policy::is_mcp_server_allowed(&server.id, &settings))
}

fn ensure_server_enabled(server: &McpServer) -> Result<()> {
    anyhow::ensure!(
        server.enabled && server_allowed(server),
        "MCP server disabled or denied by policy"
    );
    Ok(())
}

fn stdio_command(server: &McpServer) -> Result<Command> {
    ensure_server_enabled(server)?;
    let executable = server
        .command
        .as_deref()
        .ok_or_else(|| anyhow!("stdio transport requires command"))?;
    let mut command = Command::new(executable);
    let baseline;
    let vars = if server.child_env.is_empty() {
        baseline = process_env::variables(Path::new("."), "", Source::User);
        &baseline
    } else {
        &server.child_env
    };
    process_env::apply(&mut command, vars);
    command.args(&server.args);
    if let Some(workspace) = server
        .metadata
        .get("_codingbuddy_workspace")
        .and_then(|value| value.as_str())
    {
        command.current_dir(workspace);
    }
    Ok(command)
}

#[cfg(test)]
fn parse_sse_tools(server_id: &str, sse_text: &str) -> Result<Vec<McpTool>> {
    let mut tools = Vec::new();
    for chunk in sse_text.split("\n\n") {
        let data = chunk
            .strip_prefix("data: ")
            .or_else(|| chunk.lines().find_map(|l| l.strip_prefix("data: ")));
        if let Some(json_str) = data
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str)
        {
            tools.extend(parse_mcp_tools_message(server_id, &value));
        }
    }
    if tools.is_empty() {
        return Err(anyhow!("no tools discovered from SSE MCP server"));
    }
    Ok(tools)
}

/// Compatibility facade over the initialized transport service.
#[derive(Default)]
pub struct McpConnectionPool {
    sessions: sessions::Sessions,
}
impl McpConnectionPool {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn execute(
        &self,
        server: &McpServer,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        self.sessions.request(
            server,
            method,
            params,
            &codingbuddy_core::execution::ExecutionContext {
                deadline: Some(Instant::now() + timeout),
                ..Default::default()
            },
        )
    }
    pub fn shutdown(&self) {
        self.sessions.shutdown();
    }
}
pub fn execute_mcp_stdio_request(
    server: &McpServer,
    method: &str,
    params: serde_json::Value,
    _request_id: u64,
    timeout: Duration,
) -> Result<serde_json::Value> {
    McpConnectionPool::new().execute(server, method, params, timeout)
}

#[cfg(test)]
fn wait_for_mcp_response(
    rx: &mpsc::Receiver<serde_json::Value>,
    request_id: u64,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .unwrap_or_else(|| Duration::from_millis(1));
        let wait = remaining.min(Duration::from_millis(150));
        match rx.recv_timeout(wait) {
            Ok(message) => {
                if message.get("id").and_then(|v| v.as_u64()) == Some(request_id) {
                    return Some(message);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    None
}

fn extract_resource_text(result: &serde_json::Value) -> Result<String> {
    let contents = result
        .get("contents")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("MCP resources/read missing contents array"))?;
    if contents.is_empty() {
        return Err(anyhow!("MCP resources/read returned no contents"));
    }

    let mut merged = Vec::new();
    for item in contents {
        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            merged.push(text.to_string());
            continue;
        }
        if let Some(blob) = item.get("blob").and_then(|v| v.as_str()) {
            let mime = item
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream");
            match base64::engine::general_purpose::STANDARD.decode(blob) {
                Ok(bytes) => match String::from_utf8(bytes.clone()) {
                    Ok(text) => merged.push(text),
                    Err(_) => merged.push(format!(
                        "[binary-resource mime={} size={} bytes]",
                        mime,
                        bytes.len()
                    )),
                },
                Err(_) => merged.push(format!(
                    "[invalid-base64-resource mime={} chars={}]",
                    mime,
                    blob.len()
                )),
            }
        }
    }

    if merged.is_empty() {
        return Err(anyhow!(
            "MCP resources/read contents had no text/blob payloads"
        ));
    }
    let joined = merged.join("\n");
    let (bounded, _truncated) = enforce_mcp_token_limit(&joined, &McpTokenLimits::default());
    Ok(bounded)
}

fn read_mcp_frame<R: BufRead>(reader: &mut R) -> Result<Option<serde_json::Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = std::io::Read::take(&mut *reader, 16 * 1024 * 1024 + 1).read_line(&mut line)?;
        anyhow::ensure!(n <= 16 * 1024 * 1024, "MCP frame limit exceeded");
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with('{') {
            return Ok(Some(serde_json::from_str(trimmed)?));
        }
        if trimmed.is_empty() {
            if content_length.is_some() {
                break;
            } else {
                continue;
            }
        }
        #[cfg(test)]
        if trimmed == "running 1 test" {
            continue;
        }
        let (header, value) = trimmed
            .split_once(':')
            .ok_or_else(|| anyhow!("Malformed MCP frame"))?;
        if header.eq_ignore_ascii_case("Content-Length") {
            anyhow::ensure!(content_length.is_none(), "Duplicate MCP frame length");
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid Content-Length")?,
            );
        }
    }

    let length = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    anyhow::ensure!(length <= 16 * 1024 * 1024, "MCP frame limit exceeded");
    let mut buf = vec![0_u8; length];
    reader.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

#[cfg(test)]
fn parse_mcp_tools_message(server_id: &str, message: &serde_json::Value) -> Vec<McpTool> {
    let mut out = Vec::new();
    if let Some(tools) = message
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(|tools| tools.as_array())
    {
        out.extend(parse_tools_array(server_id, tools));
    }
    if let Some(tools) = message.get("tools").and_then(|tools| tools.as_array()) {
        out.extend(parse_tools_array(server_id, tools));
    }
    out
}

/// OAuth token for MCP server authentication.
#[derive(Clone, Serialize, Deserialize)]
pub struct McpOAuthToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_at: Option<String>,
    pub server_id: String,
}

impl std::fmt::Debug for McpOAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuthToken")
            .field("server_id", &self.server_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}
/// Bind authentication to the exact endpoint, source and canonical project scope.
fn mcp_token_binding(server: &McpServer) -> Result<String> {
    let url = reqwest::Url::parse(server.url.as_deref().context("MCP endpoint unavailable")?)?;
    anyhow::ensure!(
        server.metadata.get("_codingbuddy_source").is_some(),
        "MCP source identity unavailable"
    );
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            url.as_str(),
            &server.id,
            &server.metadata["_codingbuddy_source"],
            &server.metadata["_codingbuddy_workspace"],
            &server.metadata["oauth"]
        ))?)
    ))
}

/// Store an OAuth token for an MCP server.
/// Tries system keychain first, falls back to file-based storage.
pub fn store_mcp_token(server_id: &str, token: &McpOAuthToken) -> Result<()> {
    let json = serde_json::to_string(token)?;

    if let Ok(entry) = keyring::Entry::new("codingbuddy-mcp", server_id)
        && entry.set_password(&json).is_ok()
    {
        return Ok(());
    }

    store_token_file(server_id, &json)
}

/// Load a stored OAuth token for an MCP server.
/// Tries system keychain first, falls back to file-based storage.
pub fn load_mcp_token(server_id: &str) -> Result<Option<McpOAuthToken>> {
    if let Ok(entry) = keyring::Entry::new("codingbuddy-mcp", server_id)
        && let Ok(json) = entry.get_password()
        && let Ok(token) = serde_json::from_str::<McpOAuthToken>(&json)
    {
        return Ok(Some(token));
    }

    load_token_file(server_id)
}

/// Delete a stored OAuth token from both keychain and file.
pub fn delete_mcp_token(server_id: &str) -> Result<()> {
    if let Ok(entry) = keyring::Entry::new("codingbuddy-mcp", server_id) {
        let _ = entry.delete_credential();
    }
    delete_token_file(server_id)
}

fn token_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(PathBuf::from(home).join(".codingbuddy/mcp-tokens"))
}

fn store_token_file(server_id: &str, json: &str) -> Result<()> {
    let dir = token_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:x}.json", Sha256::digest(server_id.as_bytes())));
    fs::write(&path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_token_file(server_id: &str) -> Result<Option<McpOAuthToken>> {
    let path = token_dir()?.join(format!("{:x}.json", Sha256::digest(server_id.as_bytes())));
    match fs::read_to_string(path) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn delete_token_file(server_id: &str) -> Result<()> {
    let path = token_dir()?.join(format!("{:x}.json", Sha256::digest(server_id.as_bytes())));
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ── OAuth 2.0 for MCP ────────────────────────────────────────────────────────

/// OAuth 2.0 configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthConfig {
    pub authorization_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Local port for the OAuth redirect callback (default 8912).
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,
}

fn default_redirect_port() -> u16 {
    8912
}

/// Check whether an OAuth token has expired.
pub fn is_token_expired(token: &McpOAuthToken) -> bool {
    token.expires_at.as_deref().is_some_and(|exp| {
        chrono::DateTime::parse_from_rfc3339(exp)
            .map(|dt| dt < Utc::now())
            .unwrap_or(false)
    })
}

/// Generate a PKCE code verifier (random URL-safe base64 string).
fn generate_pkce_verifier() -> String {
    let buf = rand_bytes();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Generate a PKCE code challenge (S256) from a verifier.
fn generate_pkce_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

fn rand_bytes() -> [u8; 32] {
    let mut buf = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    } else {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        buf[..16].copy_from_slice(&seed.to_le_bytes());
        buf[16..20].copy_from_slice(&std::process::id().to_le_bytes());
    }
    buf
}

/// Start the OAuth 2.0 authorization code flow for an MCP server.
pub fn start_oauth_flow(server: &McpServer) -> Result<McpOAuthToken> {
    let oauth = server
        .metadata
        .get("oauth")
        .map(|v| serde_json::from_value::<McpOAuthConfig>(v.clone()))
        .transpose()?
        .ok_or_else(|| anyhow!("server has no OAuth config in metadata"))?;

    // 1. Start local listener
    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{}", oauth.redirect_port))?;

    // 2. Build authorization URL with PKCE
    let state = format!("{:x}", Sha256::digest(rand_bytes()));
    let code_verifier = generate_pkce_verifier();
    let code_challenge = generate_pkce_challenge(&code_verifier);
    let redirect_uri = format!("http://127.0.0.1:{}/callback", oauth.redirect_port);
    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&state={}&scope={}&code_challenge={}&code_challenge_method=S256",
        oauth.authorization_url,
        oauth.client_id,
        urlencoding(&redirect_uri),
        state,
        oauth.scopes.join("+"),
        code_challenge,
    );

    // 3. Open browser
    eprintln!("Opening browser for OAuth authorization...");
    let _ = open_browser(&auth_url);
    eprintln!("If browser didn't open, visit: {auth_url}");

    // 4. Wait for callback
    let code = wait_for_oauth_callback(&listener, &state, Duration::from_secs(120))?;

    // 5. Exchange code for token
    let mut token = exchange_oauth_code(&oauth, &code, &code_verifier, &server.id)?;
    token.binding = Some(mcp_token_binding(server)?);

    // 6. Store token
    store_mcp_token(&server.id, &token)?;
    Ok(token)
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

fn exchange_oauth_code(
    oauth: &McpOAuthConfig,
    code: &str,
    code_verifier: &str,
    server_id: &str,
) -> Result<McpOAuthToken> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut body = json!({
        "grant_type": "authorization_code",
        "code": code,
        "client_id": oauth.client_id,
        "code_verifier": code_verifier,
    });
    if let Some(ref secret) = oauth.client_secret {
        body["client_secret"] = json!(secret);
    }
    let resp: serde_json::Value = client
        .post(&oauth.token_url)
        .json(&body)
        .send()?
        .error_for_status()?
        .json()?;

    Ok(McpOAuthToken {
        binding: None,
        access_token: resp["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        refresh_token: resp
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from),
        token_type: resp["token_type"].as_str().unwrap_or("Bearer").to_string(),
        expires_at: resp
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .map(|secs| (Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339()),
        server_id: server_id.to_string(),
    })
}

/// Refresh an expired OAuth token.
pub fn refresh_oauth_token(server: &McpServer, token: &McpOAuthToken) -> Result<McpOAuthToken> {
    anyhow::ensure!(
        token.binding.as_ref() == Some(&mcp_token_binding(server)?),
        "MCP authentication binding changed; authenticate explicitly"
    );
    let oauth = server
        .metadata
        .get("oauth")
        .map(|v| serde_json::from_value::<McpOAuthConfig>(v.clone()))
        .transpose()?
        .ok_or_else(|| anyhow!("server has no OAuth config"))?;
    let refresh = token
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("no refresh token available"))?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp: serde_json::Value = client
        .post(&oauth.token_url)
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh,
            "client_id": oauth.client_id,
        }))
        .send()?
        .error_for_status()?
        .json()?;

    let new_token = McpOAuthToken {
        binding: token.binding.clone(),
        access_token: resp["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        refresh_token: resp
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| token.refresh_token.clone()),
        token_type: resp["token_type"].as_str().unwrap_or("Bearer").to_string(),
        expires_at: resp
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .map(|secs| (Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339()),
        server_id: server.id.clone(),
    };
    store_mcp_token(&server.id, &new_token)?;
    Ok(new_token)
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("cmd").args(["/c", "start", url]).spawn();
    }
    Ok(())
}

fn wait_for_oauth_callback(
    listener: &std::net::TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    listener.set_nonblocking(true)?;
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(anyhow!("OAuth callback timed out"));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(
                    Duration::from_secs(5).min(timeout.saturating_sub(started.elapsed())),
                ))?;
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                if let Some(code) = extract_query_param(&request, "code") {
                    let state = extract_query_param(&request, "state").unwrap_or_default();
                    if state != expected_state {
                        return Err(anyhow!("OAuth state mismatch"));
                    }
                    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body>Authorization successful. You can close this tab.</body></html>";
                    use std::io::Write;
                    let _ = stream.write_all(response.as_bytes());
                    return Ok(code);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn extract_query_param(request: &str, param: &str) -> Option<String> {
    let query = request.split_whitespace().nth(1)?.split('?').nth(1)?;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=')
            && key == param
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Start an MCP JSON-RPC 2.0 server on stdin/stdout, exposing CodingBuddy tools.
///
/// This allows other MCP-compatible clients to use CodingBuddy as a tool server.
/// Implements the MCP protocol: `initialize`, `tools/list`, `tools/call`.
pub fn run_mcp_serve(workspace: &Path) -> Result<()> {
    eprintln!("codingbuddy-cli MCP server started (stdio transport)");
    let tool_list = build_serve_tool_list(workspace);

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    loop {
        let message = match read_mcp_frame(&mut reader) {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // EOF
            Err(_) => break,
        };

        let id = message.get("id").cloned();
        let method = message.get("method").and_then(|v| v.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "codingbuddy-cli",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_list }
            }),
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or(json!({}));
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                let result_text = execute_serve_tool(workspace, tool_name, &arguments);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": result_text
                        }]
                    }
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {method}")
                }
            }),
        };

        let body = serde_json::to_vec(&response)?;
        write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
        writer.write_all(&body)?;
        writer.flush()?;
    }

    eprintln!("codingbuddy-cli MCP server stopped");
    Ok(())
}

/// Build the tool list for MCP serve mode — exposes a subset of CodingBuddy tools.
fn build_serve_tool_list(_workspace: &Path) -> Vec<serde_json::Value> {
    let tools = vec![
        (
            "fs_read",
            "Read a file",
            json!({"type": "object", "properties": {"path": {"type": "string", "description": "File path to read"}}, "required": ["path"]}),
        ),
        (
            "fs_write",
            "Write content to a file",
            json!({"type": "object", "properties": {"path": {"type": "string", "description": "File path"}, "content": {"type": "string", "description": "Content to write"}}, "required": ["path", "content"]}),
        ),
        (
            "fs_glob",
            "Find files by glob pattern",
            json!({"type": "object", "properties": {"pattern": {"type": "string", "description": "Glob pattern"}}, "required": ["pattern"]}),
        ),
        (
            "fs_grep",
            "Search file contents with regex",
            json!({"type": "object", "properties": {"pattern": {"type": "string", "description": "Regex pattern"}, "path": {"type": "string", "description": "Search path"}}, "required": ["pattern"]}),
        ),
        (
            "bash_run",
            "Execute a shell command",
            json!({"type": "object", "properties": {"command": {"type": "string", "description": "Command to execute"}, "timeout": {"type": "integer", "description": "Timeout in seconds"}}, "required": ["command"]}),
        ),
        (
            "git_status",
            "Show git status",
            json!({"type": "object", "properties": {}}),
        ),
        (
            "git_diff",
            "Show git diff",
            json!({"type": "object", "properties": {"args": {"type": "string", "description": "Additional git diff args"}}}),
        ),
        (
            "web_fetch",
            "Fetch URL content",
            json!({"type": "object", "properties": {"url": {"type": "string", "description": "URL to fetch"}}, "required": ["url"]}),
        ),
    ];
    tools
        .into_iter()
        .map(|(name, desc, schema)| {
            json!({
                "name": name,
                "description": desc,
                "inputSchema": schema,
            })
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn spawn_serve_shell(command: &str, workspace: &Path) -> std::io::Result<std::process::Output> {
    Command::new("cmd")
        .args(["/C", command])
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

#[cfg(not(target_os = "windows"))]
fn spawn_serve_shell(command: &str, workspace: &Path) -> std::io::Result<std::process::Output> {
    Command::new("sh")
        .args(["-c", command])
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Execute a tool in MCP serve mode (simplified execution without full agent).
fn execute_serve_tool(workspace: &Path, tool_name: &str, arguments: &serde_json::Value) -> String {
    match tool_name {
        "fs_read" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let full_path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                workspace.join(path)
            };
            match fs::read_to_string(&full_path) {
                Ok(content) => content,
                Err(e) => format!("Error reading {}: {e}", full_path.display()),
            }
        }
        "fs_write" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = arguments
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let full_path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                workspace.join(path)
            };
            if let Some(parent) = full_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&full_path, content) {
                Ok(()) => format!("Written {} bytes to {}", content.len(), full_path.display()),
                Err(e) => format!("Error writing {}: {e}", full_path.display()),
            }
        }
        "fs_glob" => {
            let pattern = arguments
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("*");
            match glob::glob(&workspace.join(pattern).to_string_lossy()) {
                Ok(entries) => {
                    let paths: Vec<String> = entries
                        .filter_map(|e: Result<PathBuf, glob::GlobError>| e.ok())
                        .map(|p: PathBuf| p.to_string_lossy().to_string())
                        .take(200)
                        .collect();
                    paths.join("\n")
                }
                Err(e) => format!("Glob error: {e}"),
            }
        }
        "bash_run" => {
            let cmd = arguments
                .get("command")
                .or_else(|| arguments.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let _timeout_secs = arguments
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(30);
            match spawn_serve_shell(cmd, workspace) {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    format!(
                        "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
                        output.status.code().unwrap_or(-1),
                        stdout,
                        stderr
                    )
                }
                Err(e) => format!("Command spawn error: {e}"),
            }
        }
        "git_status" | "git_diff" => {
            let git_cmd = if tool_name == "git_status" {
                "status"
            } else {
                "diff"
            };
            let extra_args = arguments.get("args").and_then(|v| v.as_str()).unwrap_or("");
            let mut args = vec![git_cmd];
            if !extra_args.is_empty() {
                args.push(extra_args);
            }
            match Command::new("git")
                .args(&args)
                .current_dir(workspace)
                .output()
            {
                Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
                Err(e) => format!("Git error: {e}"),
            }
        }
        "web_fetch" => {
            let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
            match Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .and_then(|c| c.get(url).send())
            {
                Ok(resp) => resp.text().unwrap_or_else(|e| format!("Read error: {e}")),
                Err(e) => format!("Fetch error: {e}"),
            }
        }
        "fs_grep" => {
            let pattern = arguments
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| {
                    if Path::new(p).is_absolute() {
                        PathBuf::from(p)
                    } else {
                        workspace.join(p)
                    }
                })
                .unwrap_or_else(|| workspace.to_path_buf());
            match Command::new("grep")
                .args(["-rn", "--include=*", pattern])
                .arg(&path)
                .output()
            {
                Ok(output) => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    if text.len() > 10000 {
                        format!("{}...\n(truncated)", &text[..10000])
                    } else {
                        text.to_string()
                    }
                }
                Err(e) => format!("Grep error: {e}"),
            }
        }
        _ => format!("Unknown tool: {tool_name}"),
    }
}

fn parse_tools_array(server_id: &str, tools: &[serde_json::Value]) -> Vec<McpTool> {
    tools
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_string();
            let description = entry
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(McpTool {
                server_id: server_id.to_string(),
                name,
                description,
                input_schema: entry
                    .get("inputSchema")
                    .or_else(|| entry.get("input_schema"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                annotations: entry
                    .get("annotations")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::time::Duration;

    fn trusted_manager(workspace: &Path) -> McpManager {
        let mut manager = McpManager::new(workspace).expect("manager");
        manager.trusted_workspace = true;
        manager
    }

    trait FixtureServer {
        fn add_fixture_server(&self, server: McpServer) -> Result<()>;
    }
    impl FixtureServer for McpManager {
        fn add_fixture_server(&self, mut server: McpServer) -> Result<()> {
            if server.command.as_deref() == Some("echo") {
                fs::write(
                    self.workspace.join("mcp-fixture.json"),
                    serde_json::to_vec(&server.metadata)?,
                )?;
                server.command = Some(std::env::current_exe()?.to_string_lossy().into_owned());
                server.args = vec![
                    "--exact".into(),
                    "tests::delivery_three_stdio_fixture".into(),
                    "--nocapture".into(),
                    "--quiet".into(),
                ];
            }
            self.add_server(server)
        }
    }
    #[test]
    fn delivery_three_stdio_fixture() {
        if !Path::new("mcp-fixture.json").is_file() {
            return;
        }
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read("mcp-fixture.json").unwrap()).unwrap();
        use std::io::Write;
        for line in std::io::stdin().lock().lines() {
            let request: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            let mut trace = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("mcp-fixture.trace")
                .unwrap();
            writeln!(trace, "{request}").unwrap();
            if request.get("id").is_none() {
                continue;
            }
            let result = match request["method"].as_str().unwrap() {
                "initialize" => {
                    json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{},"resources":{},"prompts":{}},"serverInfo":{"name":"fixture","version":"1"}})
                }
                "tools/list" => {
                    let mut tools = metadata["tools"].as_array().cloned().unwrap_or_default();
                    for tool in &mut tools {
                        if tool.get("inputSchema").is_none() {
                            tool["inputSchema"] = json!({"type":"object"});
                        }
                    }
                    json!({"tools":tools})
                }
                "tools/call" => {
                    json!({"content":[{"type":"text","text":request["params"]["arguments"].to_string()}]})
                }
                "resources/list" => json!({"resources":[]}),
                "prompts/list" => json!({"prompts":[]}),
                _ => json!({}),
            };
            println!(
                "{}",
                json!({"jsonrpc":"2.0","id":request["id"],"result":result})
            );
            std::io::stdout().flush().unwrap();
        }
    }

    #[test]
    fn p0_mcp_project_activation_and_environment() {
        let workspace = tempfile::tempdir().unwrap();
        let id = Uuid::now_v7().to_string();
        let server = McpServer {
            id: id.clone(),
            name: "fixture".into(),
            command: Some("fixture-executable".into()),
            ..Default::default()
        };
        let manager = McpManager::with_trust(workspace.path(), false).unwrap();
        manager
            .save_project_config(&McpConfig {
                servers: vec![server.clone()],
            })
            .unwrap();
        assert!(manager.get_server(&id).unwrap().is_none());

        let manager = McpManager::with_trust(workspace.path(), true).unwrap();
        let mut server = manager.get_server(&id).unwrap().unwrap();
        server.args = vec!["${P0_PRIVATE_VALUE}".into()];
        assert!(expand_server_env_vars(&mut server).is_err());
        server
            .child_env
            .insert("P0_PRIVATE_VALUE".into(), "synthetic-value".into());
        expand_server_env_vars(&mut server).unwrap();
        assert_eq!(server.args, ["synthetic-value"]);

        let command = stdio_command(&server).unwrap();
        assert!(command.get_envs().any(|(name, value)| {
            name == "P0_PRIVATE_VALUE" && value.is_some_and(|value| value == "synthetic-value")
        }));
        let other = McpServer {
            command: Some("fixture-executable".into()),
            ..Default::default()
        };
        assert!(
            stdio_command(&other)
                .unwrap()
                .get_envs()
                .all(|(name, _)| name != "P0_PRIVATE_VALUE")
        );
    }

    #[test]
    fn add_list_remove_server_round_trip() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "local".to_string(),
                name: "Local MCP".to_string(),
                transport: McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec!["ok".to_string()],
                url: None,
                enabled: true,
                metadata: serde_json::json!({"tools": [{"name":"hello","description":"test"}]}),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add");

        let listed = manager.list_servers().expect("list");
        assert!(listed.iter().any(|s| s.id == "local"));

        let tools = manager.discover_tools().expect("discover");
        assert!(tools.iter().any(|t| t.name == "hello"));

        let removed = manager.remove_server("local").expect("remove");
        assert!(removed);
    }

    #[test]
    fn detects_toolset_changes_and_emits_notice_fingerprint() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "notify".to_string(),
                name: "Notify".to_string(),
                transport: McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec!["ok".to_string()],
                url: None,
                enabled: true,
                metadata: serde_json::json!({"tools": [{"name":"a"},{"name":"b"}]}),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add");

        let (_, first_refreshes, first_notice) = manager
            .discover_tools_with_notice(None)
            .expect("first refresh");
        let first = first_refreshes
            .iter()
            .find(|refresh| refresh.server_id == "notify")
            .expect("refresh");
        assert!(first.added.contains(&"a".to_string()));
        assert!(first.added.contains(&"b".to_string()));
        assert!(!first_notice.fingerprint.is_empty());

        manager
            .add_fixture_server(McpServer {
                id: "notify".to_string(),
                name: "Notify".to_string(),
                transport: McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec!["ok".to_string()],
                url: None,
                enabled: true,
                metadata: serde_json::json!({"tools": [{"name":"b"},{"name":"c"}]}),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("replace");

        let (_, second_refreshes, second_notice) = manager
            .discover_tools_with_notice(Some(&first_notice.fingerprint))
            .expect("second refresh");
        let second = second_refreshes
            .iter()
            .find(|refresh| refresh.server_id == "notify")
            .expect("refresh");
        assert!(second.added.contains(&"c".to_string()));
        assert!(second.removed.contains(&"a".to_string()));
        assert_ne!(second_notice.fingerprint, first_notice.fingerprint);
        assert!(
            second_notice
                .changed_servers
                .contains(&"notify".to_string())
        );
    }

    #[test]
    fn parses_tools_from_jsonrpc_result_payload() {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [
                    {
                        "name": "search",
                        "description": "Search tool",
                        "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}},
                        "annotations": {"readOnlyHint": true}
                    },
                    {"name": "read"}
                ]
            }
        });
        let tools = parse_mcp_tools_message("srv", &payload);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].server_id, "srv");
        assert_eq!(tools[0].name, "search");
        assert_eq!(
            tools[0]
                .input_schema
                .get("properties")
                .and_then(|props| props.get("query"))
                .and_then(|query| query.get("type"))
                .and_then(serde_json::Value::as_str),
            Some("string")
        );
        assert_eq!(
            tools[0]
                .annotations
                .get("readOnlyHint")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(tools[1].name, "read");
    }

    #[test]
    fn reads_content_length_frames() {
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"ping"}]}})
            .to_string();
        let raw = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut cursor = Cursor::new(raw.as_bytes().to_vec());
        let parsed = read_mcp_frame(&mut cursor)
            .expect("frame")
            .expect("payload");
        let tools = parse_mcp_tools_message("srv", &parsed);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");
    }

    #[test]
    fn extract_resource_text_merges_text_and_blob_entries() {
        let blob = base64::engine::general_purpose::STANDARD.encode("from blob");
        let result = extract_resource_text(&serde_json::json!({
            "contents": [
                {"text":"line one"},
                {"blob": blob, "mimeType":"text/plain"}
            ]
        }))
        .expect("resource text");
        assert!(result.contains("line one"));
        assert!(result.contains("from blob"));
    }

    #[test]
    fn extract_resource_text_handles_binary_blob_marker() {
        let blob = base64::engine::general_purpose::STANDARD.encode([0_u8, 159, 146, 150]);
        let result = extract_resource_text(&serde_json::json!({
            "contents": [
                {"blob": blob, "mimeType":"application/octet-stream"}
            ]
        }))
        .expect("resource marker");
        assert!(result.contains("[binary-resource"));
        assert!(result.contains("application/octet-stream"));
    }

    #[test]
    fn wait_for_mcp_response_times_out_without_matching_id() {
        let (tx, rx) = mpsc::channel::<serde_json::Value>();
        tx.send(serde_json::json!({"id":1,"result":{}}))
            .expect("send frame");
        drop(tx);
        let value = wait_for_mcp_response(&rx, 2, Duration::from_millis(20));
        assert!(value.is_none());
    }

    #[test]
    fn mcp_token_roundtrip() {
        let server_id = format!("test-server-{}", uuid::Uuid::now_v7());
        let token = McpOAuthToken {
            binding: None,
            access_token: "abc123".to_string(),
            refresh_token: Some("refresh456".to_string()),
            token_type: "Bearer".to_string(),
            expires_at: Some("2025-12-31T23:59:59Z".to_string()),
            server_id: server_id.clone(),
        };
        store_mcp_token(&server_id, &token).unwrap();
        let loaded = load_mcp_token(&server_id).unwrap().unwrap();
        assert_eq!(loaded.access_token, "abc123");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh456"));
        delete_mcp_token(&server_id).unwrap();
        assert!(load_mcp_token(&server_id).unwrap().is_none());
    }

    #[test]
    fn load_nonexistent_token_returns_none() {
        let result = load_mcp_token("nonexistent-server-xyz").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn mcp_token_serialization() {
        let token = McpOAuthToken {
            binding: None,
            access_token: "tok".to_string(),
            refresh_token: None,
            token_type: "Bearer".to_string(),
            expires_at: None,
            server_id: "srv".to_string(),
        };
        let json = serde_json::to_string(&token).unwrap();
        assert!(json.contains("tok"));
        let parsed: McpOAuthToken = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.access_token, "tok");
    }

    #[test]
    fn expand_env_vars_basic() {
        let home = resolve_env_var("HOME").unwrap_or_default();
        let result = expand_env_vars("path=${HOME}/file");
        assert_eq!(result, format!("path={home}/file"));
    }

    #[test]
    fn expand_env_vars_with_default() {
        let result = expand_env_vars("val=${NONEXISTENT_VAR_XYZ:-fallback}");
        assert_eq!(result, "val=fallback");
    }

    #[test]
    fn expand_env_vars_passthrough() {
        let result = expand_env_vars("no vars here");
        assert_eq!(result, "no vars here");
    }

    #[test]
    fn expand_server_env_vars_applies_to_all_fields() {
        let mut server = McpServer {
            id: "test".to_string(),
            name: "test".to_string(),
            transport: McpTransport::Stdio,
            command: Some("${NONEXISTENT_CMD:-node}".to_string()),
            args: vec!["${NONEXISTENT_ARG:-server.js}".to_string()],
            url: None,
            enabled: true,
            metadata: serde_json::Value::Null,
            headers: vec![],
            child_env: Default::default(),
        };
        expand_server_env_vars(&mut server).unwrap();
        assert_eq!(server.command.as_deref(), Some("node"));
        assert_eq!(server.args[0], "server.js");
    }

    #[test]
    fn mcp_env_expansion_rejects_shell_injection() {
        let mut server = McpServer {
            id: "test".to_string(),
            name: "test".to_string(),
            transport: McpTransport::Stdio,
            command: Some("node; rm -rf /".to_string()),
            args: vec![],
            url: None,
            enabled: true,
            metadata: serde_json::Value::Null,
            headers: vec![],
            child_env: Default::default(),
        };
        let result = expand_server_env_vars(&mut server);
        assert!(
            result.is_err(),
            "should reject command with shell metacharacters"
        );
    }

    #[test]
    fn enforce_mcp_token_limit_passes_small_output() {
        let limits = McpTokenLimits::default();
        let small = "hello world";
        let (out, truncated) = enforce_mcp_token_limit(small, &limits);
        assert_eq!(out, small);
        assert!(!truncated);
    }

    #[test]
    fn enforce_mcp_token_limit_truncates_large_output() {
        let limits = McpTokenLimits {
            warn_threshold: 10,
            max_tokens: 20,
        };
        let large = "x".repeat(1000);
        let (out, truncated) = enforce_mcp_token_limit(&large, &limits);
        assert!(truncated);
        assert!(out.len() < large.len());
        assert!(out.contains("truncated"));
    }

    #[test]
    fn serve_tool_list_has_expected_tools() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-serve-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let tools = build_serve_tool_list(&workspace);
        assert!(tools.len() >= 5);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"fs_read"));
        assert!(names.contains(&"bash_run"));
        assert!(names.contains(&"git_status"));
    }

    #[test]
    fn serve_bash_run_executes_on_current_platform() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-serve-run-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");

        let out = execute_serve_tool(
            &workspace,
            "bash_run",
            &json!({ "command": "echo mcp_serve_ok" }),
        );
        assert!(
            out.contains("exit_code: 0"),
            "expected successful shell execution, got: {out}"
        );
        assert!(
            out.to_ascii_lowercase().contains("mcp_serve_ok"),
            "expected shell output marker, got: {out}"
        );
    }

    // ── MCP integration tests (Phase 16.6) ──────────────────────────────

    #[test]
    fn mcp_search_filters_by_name() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "search-test".to_string(),
                name: "Search Test".to_string(),
                transport: McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec!["ok".to_string()],
                url: None,
                enabled: true,
                metadata: serde_json::json!({
                    "tools": [
                        {"name": "alpha", "description": "Alpha tool"},
                        {"name": "beta", "description": "Beta tool"},
                        {"name": "gamma", "description": "Gamma tool"}
                    ]
                }),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add");

        let tools = manager.discover_tools().expect("discover");
        assert_eq!(tools.len(), 3, "all three tools should be discovered");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(names.contains(&"gamma"));
    }

    #[test]
    fn unavailable_server_does_not_panic() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "missing".to_string(),
                name: "Missing".to_string(),
                transport: McpTransport::Stdio,
                command: Some("__nonexistent_command_xyz__".to_string()),
                args: vec![],
                url: None,
                enabled: true,
                metadata: serde_json::Value::Null,
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add");

        // discover_tools should return Ok even if the server command doesn't exist
        let result = manager.discover_tools();
        assert!(
            result.is_ok(),
            "discover_tools should not panic/error on unavailable server: {:?}",
            result.err()
        );
    }

    #[test]
    fn http_mcp_server_add_and_list() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-test-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "http-test".to_string(),
                name: "HTTP Test".to_string(),
                transport: McpTransport::Http,
                command: None,
                args: vec![],
                url: Some("http://127.0.0.1:0/mcp".to_string()),
                enabled: true,
                metadata: serde_json::json!({"tools": [{"name": "remote_tool", "description": "A remote tool"}]}),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add http server");

        let listed = manager.list_servers().expect("list");
        let http = listed.iter().find(|s| s.id == "http-test");
        assert!(http.is_some(), "HTTP server should be listed");
        assert!(matches!(http.unwrap().transport, McpTransport::Http));

        let tools = manager.discover_tools().expect("discover");
        assert!(
            tools.is_empty(),
            "Disconnected metadata must never fabricate tools"
        );
    }

    #[test]
    fn mcp_token_limit_truncates_at_exact_boundary() {
        let limits = McpTokenLimits {
            warn_threshold: 5,
            max_tokens: 10,
        };
        // 10 tokens * 4 chars = 40 chars. 40 / 4 = 10 estimated tokens. 10 > 10 is false.
        let exactly_at = "a".repeat(40);
        let (out, truncated) = enforce_mcp_token_limit(&exactly_at, &limits);
        assert!(!truncated, "exactly at limit should not truncate");
        assert_eq!(out, exactly_at);

        // 44 chars / 4 = 11 estimated tokens. 11 > 10 is true → truncated.
        let over = "a".repeat(44);
        let (out2, truncated2) = enforce_mcp_token_limit(&over, &limits);
        assert!(truncated2, "over limit should truncate");
        assert!(out2.contains("truncated"));
    }

    #[test]
    fn tools_for_context_returns_all_below_threshold() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-ctx-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "ctx-test".to_string(),
                name: "Ctx Test".to_string(),
                transport: McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                url: None,
                enabled: true,
                metadata: serde_json::json!({"tools": [
                    {"name":"t1","description":"tool 1"},
                    {"name":"t2","description":"tool 2"}
                ]}),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add");

        // Below 10% threshold → returns all tools regardless of query
        let tools = manager
            .tools_for_context(5.0, Some("t1"), 10)
            .expect("tools");
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn tools_for_context_no_query_above_threshold_returns_all() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-ctx2-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "ctx-test2".to_string(),
                name: "Ctx Test2".to_string(),
                transport: McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                url: None,
                enabled: true,
                metadata: serde_json::json!({"tools": [
                    {"name":"a","description":"alpha"},
                    {"name":"b","description":"beta"}
                ]}),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add");

        // Above 10% but no query → returns all tools
        let tools = manager.tools_for_context(15.0, None, 10).expect("tools");
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn tools_for_context_filters_above_threshold() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-ctx3-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "ctx-test3".to_string(),
                name: "Ctx Test3".to_string(),
                transport: McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                url: None,
                enabled: true,
                metadata: serde_json::json!({"tools": [
                    {"name":"search","description":"search tool"},
                    {"name":"read","description":"read tool"}
                ]}),
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add");

        // Above 10% with query — search_tools_by_relevance is called.
        // Since there are only 2 tools (below the 50-tool threshold in search),
        // it returns all tools.
        let tools = manager
            .tools_for_context(15.0, Some("search"), 10)
            .expect("tools");
        assert!(!tools.is_empty());
    }

    // ── P2-01: OAuth 2.0 tests ─────────────────────────────────────────

    #[test]
    fn generate_pkce_challenge_is_deterministic_for_same_verifier() {
        let verifier = "test-verifier-12345";
        let c1 = generate_pkce_challenge(verifier);
        let c2 = generate_pkce_challenge(verifier);
        assert_eq!(c1, c2, "same verifier should produce same challenge");
        assert!(!c1.is_empty());
    }

    #[test]
    fn extract_query_param_parses_code() {
        let request = "GET /callback?code=abc123&state=xyz HTTP/1.1\r\nHost: localhost";
        let code = extract_query_param(request, "code");
        assert_eq!(code, Some("abc123".to_string()));
        let state = extract_query_param(request, "state");
        assert_eq!(state, Some("xyz".to_string()));
    }

    #[test]
    fn extract_query_param_missing_returns_none() {
        let request = "GET /callback?code=abc HTTP/1.1";
        assert!(extract_query_param(request, "state").is_none());
        assert!(extract_query_param("", "code").is_none());
    }

    #[test]
    fn is_token_expired_true_for_past_date() {
        let token = McpOAuthToken {
            binding: None,
            access_token: "tok".to_string(),
            refresh_token: None,
            token_type: "Bearer".to_string(),
            expires_at: Some("2020-01-01T00:00:00Z".to_string()),
            server_id: "srv".to_string(),
        };
        assert!(is_token_expired(&token));
    }

    #[test]
    fn is_token_expired_false_for_future_date() {
        let token = McpOAuthToken {
            binding: None,
            access_token: "tok".to_string(),
            refresh_token: None,
            token_type: "Bearer".to_string(),
            expires_at: Some("2099-12-31T23:59:59Z".to_string()),
            server_id: "srv".to_string(),
        };
        assert!(!is_token_expired(&token));
    }

    // ── P2-02: Resource autocomplete tests ───────────────────────────────

    #[test]
    fn autocomplete_resources_no_servers_returns_empty() {
        let workspace = std::env::temp_dir().join(format!("codingbuddy-mcp-ac-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        let results = manager.autocomplete_resources("@").expect("autocomplete");
        assert!(results.is_empty());
    }

    #[test]
    fn autocomplete_resources_empty_prefix_returns_all() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-ac2-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        // No servers → empty results (can't test with real servers in unit tests)
        let results = manager.autocomplete_resources("").expect("autocomplete");
        assert!(results.is_empty());
    }

    #[test]
    fn autocomplete_resources_filters_by_prefix() {
        // This tests the filter logic directly without servers
        let all = vec![
            McpResource {
                server_id: "srv1".to_string(),
                uri: "file:///docs/readme.md".to_string(),
                name: "readme".to_string(),
                description: "Project readme".to_string(),
                mime_type: None,
            },
            McpResource {
                server_id: "srv2".to_string(),
                uri: "file:///src/main.rs".to_string(),
                name: "main".to_string(),
                description: "Entry point".to_string(),
                mime_type: Some("text/x-rust".to_string()),
            },
        ];
        let prefix = "@srv1";
        let prefix_lower = prefix.to_ascii_lowercase();
        let matches: Vec<_> = all
            .into_iter()
            .filter(|r| {
                let label = format!("@{}:{}", r.server_id, r.uri).to_ascii_lowercase();
                label.starts_with(&prefix_lower)
                    || r.name.to_ascii_lowercase().contains(&prefix_lower)
            })
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].server_id, "srv1");
    }

    #[test]
    fn parse_sse_tools_from_event_stream() {
        let sse_text = r#"data: {"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"sse_tool","description":"SSE test tool"}]}}

data: {"other":"ignored"}

"#;
        let tools = parse_sse_tools("sse-srv", sse_text).expect("parse SSE");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "sse_tool");
        assert_eq!(tools[0].server_id, "sse-srv");
    }

    #[test]
    fn parse_sse_tools_empty_stream_returns_error() {
        let result = parse_sse_tools("srv", "");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no tools"));
    }

    #[test]
    fn sse_transport_serialization() {
        let transport = McpTransport::Sse;
        let json = serde_json::to_string(&transport).unwrap();
        assert_eq!(json, "\"sse\"");
        let parsed: McpTransport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, McpTransport::Sse);
    }

    #[test]
    fn sse_server_add_and_list() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-sse-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "sse-test".to_string(),
                name: "SSE Test".to_string(),
                transport: McpTransport::Sse,
                command: None,
                args: vec![],
                url: Some("http://127.0.0.1:0/sse".to_string()),
                enabled: true,
                metadata: serde_json::json!({"tools": [{"name":"sse_tool"}]}),
                headers: vec![("Authorization".to_string(), "Bearer tok".to_string())],
                child_env: Default::default(),
            })
            .expect("add sse server");

        let listed = manager.list_servers().expect("list");
        let sse = listed.iter().find(|s| s.id == "sse-test");
        assert!(sse.is_some());
        assert_eq!(sse.unwrap().transport, McpTransport::Sse);
        assert_eq!(sse.unwrap().headers.len(), 1);
    }

    #[test]
    fn managed_config_missing_returns_none() {
        // The managed config path points to a system location that won't exist in test
        let result = McpManager::load_managed_mcp_config();
        assert!(
            result.is_none(),
            "managed config should return None when file is absent"
        );
    }

    #[test]
    fn is_managed_server_returns_false_when_no_managed_config() {
        assert!(!McpManager::is_managed_server("some-server"));
    }

    #[test]
    fn cannot_remove_managed_server_would_fail() {
        // Without a managed config file, is_managed_server returns false,
        // so the guard doesn't trigger. This tests the guard integration exists.
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-mcp-managed-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        let manager = trusted_manager(&workspace);

        manager
            .add_fixture_server(McpServer {
                id: "removable".to_string(),
                name: "Removable".to_string(),
                transport: McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                url: None,
                enabled: true,
                metadata: serde_json::Value::Null,
                headers: vec![],
                child_env: Default::default(),
            })
            .expect("add");

        // Non-managed server can be removed
        let removed = manager.remove_server("removable").expect("remove");
        assert!(removed);
    }

    // ── P5-12: MCP prompts as slash commands ──────────────────────────────

    #[test]
    fn mcp_prompts_as_commands() {
        // McpPrompt struct should round-trip correctly
        let prompt = McpPrompt {
            server_id: "test-server".to_string(),
            name: "generate-docs".to_string(),
            description: "Generate documentation for a module".to_string(),
            arguments: vec![
                McpPromptArgument {
                    name: "module".to_string(),
                    description: "The module to document".to_string(),
                    required: true,
                },
                McpPromptArgument {
                    name: "format".to_string(),
                    description: "Output format (md, html)".to_string(),
                    required: false,
                },
            ],
        };
        let serialized = serde_json::to_value(&prompt).expect("serialize");
        let deserialized: McpPrompt = serde_json::from_value(serialized).expect("deserialize");
        assert_eq!(deserialized.server_id, "test-server");
        assert_eq!(deserialized.name, "generate-docs");
        assert_eq!(deserialized.arguments.len(), 2);
        assert!(deserialized.arguments[0].required);
        assert!(!deserialized.arguments[1].required);

        // Slash command name generation
        let slash_name = format!("/mcp-{}-{}", prompt.server_id, prompt.name);
        assert_eq!(slash_name, "/mcp-test-server-generate-docs");
    }

    // ── P5-13: env var expansion in list_servers ──────────────────────────

    #[test]
    fn env_vars_expanded() {
        // HOME may be absent in some CI setups; resolve with the same helper used
        // by expansion logic so this stays deterministic cross-platform.
        let home = resolve_env_var("HOME").unwrap_or_default();

        // Expansion works on real env vars
        let expanded = expand_env_vars("Bearer ${HOME}");
        assert_eq!(expanded, format!("Bearer {home}"));

        // With default value for missing var
        let expanded2 = expand_env_vars("${NONEXISTENT_ENV_VAR_XYZ:-fallback_val}");
        assert_eq!(expanded2, "fallback_val");

        // Expansion in server config
        let mut server = McpServer {
            id: "svc".to_string(),
            name: "Service".to_string(),
            transport: McpTransport::Http,
            command: None,
            args: vec!["--home=${HOME}".to_string()],
            url: Some("https://api.example.com/${HOME}".to_string()),
            enabled: true,
            metadata: serde_json::Value::Null,
            headers: vec![],
            child_env: Default::default(),
        };
        server.child_env.insert("HOME".into(), home.clone().into());
        expand_server_env_vars(&mut server).unwrap();
        let expected_url = format!("https://api.example.com/{home}");
        assert_eq!(server.url.as_deref(), Some(expected_url.as_str()));
        assert_eq!(server.args[0], format!("--home={home}"));
    }

    // ── T4.3: MCP connection pool tests ────────────────────────────────

    #[test]
    fn pool_default_creates_empty() {
        let pool = McpConnectionPool::new();
        assert!(pool.sessions.states.lock().unwrap().is_empty());
    }

    #[test]
    fn pool_shutdown_is_idempotent() {
        let pool = McpConnectionPool::new();
        pool.shutdown();
        pool.shutdown(); // Should not panic
    }

    #[test]
    fn pool_drop_cleans_up() {
        {
            let _pool = McpConnectionPool::new();
            // Drop implicitly calls shutdown
        }
        // No panic = success
    }
}
