//! Store accessors for auxiliary tables: plugins, approvals, MCP, jobs, tasks, todos, caches and artifacts.

use super::*;

impl Store {
    pub fn set_plugin_state(&self, record: &PluginStateRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO plugin_state (plugin_id, name, version, path, enabled, manifest_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.plugin_id,
                record.name,
                record.version,
                record.path,
                if record.enabled { 1 } else { 0 },
                record.manifest_json,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn remove_plugin_state(&self, plugin_id: &str) -> Result<()> {
        let conn = self.db()?;
        conn.execute("DELETE FROM plugin_state WHERE plugin_id = ?1", [plugin_id])?;
        Ok(())
    }

    pub fn list_plugin_states(&self) -> Result<Vec<PluginStateRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT plugin_id, name, version, path, enabled, manifest_json
             FROM plugin_state ORDER BY plugin_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PluginStateRecord {
                plugin_id: r.get(0)?,
                name: r.get(1)?,
                version: r.get(2)?,
                path: r.get(3)?,
                enabled: r.get::<_, i64>(4)? != 0,
                manifest_json: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_plugin_catalog_entries(
        &self,
        source: &str,
        entries: &[PluginCatalogEntryRecord],
    ) -> Result<()> {
        let conn = self.db()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "DELETE FROM plugin_catalog_cache WHERE source = ?1",
            [source.to_string()],
        )?;
        for entry in entries {
            conn.execute(
                "INSERT OR REPLACE INTO plugin_catalog_cache
                 (plugin_id, name, version, description, source, signature, verified, metadata_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    entry.plugin_id,
                    entry.name,
                    entry.version,
                    entry.description,
                    source,
                    entry.signature,
                    if entry.verified { 1 } else { 0 },
                    entry.metadata_json,
                    now,
                ],
            )?;
        }
        Ok(())
    }

    pub fn list_plugin_catalog_entries(&self) -> Result<Vec<PluginCatalogEntryRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT plugin_id, name, version, description, source, signature, verified, metadata_json, updated_at
             FROM plugin_catalog_cache ORDER BY plugin_id ASC, version DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PluginCatalogEntryRecord {
                plugin_id: r.get(0)?,
                name: r.get(1)?,
                version: r.get(2)?,
                description: r.get(3)?,
                source: r.get(4)?,
                signature: r.get(5)?,
                verified: r.get::<_, i64>(6)? != 0,
                metadata_json: r.get(7)?,
                updated_at: r.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn upsert_autopilot_run(&self, record: &AutopilotRunRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO autopilot_runs
             (run_id, session_id, prompt, status, stop_reason, completed_iterations, failed_iterations,
              consecutive_failures, last_error, stop_file, heartbeat_file, tools, max_think, started_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                record.run_id.to_string(),
                record.session_id.to_string(),
                record.prompt,
                record.status,
                record.stop_reason,
                record.completed_iterations as i64,
                record.failed_iterations as i64,
                record.consecutive_failures as i64,
                record.last_error,
                record.stop_file,
                record.heartbeat_file,
                if record.tools { 1 } else { 0 },
                if record.max_think { 1 } else { 0 },
                record.started_at,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_autopilot_run(&self, run_id: Uuid) -> Result<Option<AutopilotRunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, session_id, prompt, status, stop_reason, completed_iterations, failed_iterations,
                    consecutive_failures, last_error, stop_file, heartbeat_file, tools, max_think, started_at, updated_at
             FROM autopilot_runs WHERE run_id = ?1",
        )?;
        let mut rows = stmt.query([run_id.to_string()])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(AutopilotRunRecord {
                run_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                session_id: Uuid::parse_str(row.get::<_, String>(1)?.as_str())?,
                prompt: row.get(2)?,
                status: row.get(3)?,
                stop_reason: row.get(4)?,
                completed_iterations: row.get::<_, i64>(5)? as u64,
                failed_iterations: row.get::<_, i64>(6)? as u64,
                consecutive_failures: row.get::<_, i64>(7)? as u64,
                last_error: row.get(8)?,
                stop_file: row.get(9)?,
                heartbeat_file: row.get(10)?,
                tools: row.get::<_, i64>(11)? != 0,
                max_think: row.get::<_, i64>(12)? != 0,
                started_at: row.get(13)?,
                updated_at: row.get(14)?,
            }));
        }
        Ok(None)
    }

    pub fn load_latest_autopilot_run(&self) -> Result<Option<AutopilotRunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, session_id, prompt, status, stop_reason, completed_iterations, failed_iterations,
                    consecutive_failures, last_error, stop_file, heartbeat_file, tools, max_think, started_at, updated_at
             FROM autopilot_runs ORDER BY updated_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(AutopilotRunRecord {
                run_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                session_id: Uuid::parse_str(row.get::<_, String>(1)?.as_str())?,
                prompt: row.get(2)?,
                status: row.get(3)?,
                stop_reason: row.get(4)?,
                completed_iterations: row.get::<_, i64>(5)? as u64,
                failed_iterations: row.get::<_, i64>(6)? as u64,
                consecutive_failures: row.get::<_, i64>(7)? as u64,
                last_error: row.get(8)?,
                stop_file: row.get(9)?,
                heartbeat_file: row.get(10)?,
                tools: row.get::<_, i64>(11)? != 0,
                max_think: row.get::<_, i64>(12)? != 0,
                started_at: row.get(13)?,
                updated_at: row.get(14)?,
            }));
        }
        Ok(None)
    }

    pub fn usage_summary(
        &self,
        session_id: Option<Uuid>,
        last_hours: Option<i64>,
    ) -> Result<UsageSummary> {
        let conn = self.db()?;
        let since = last_hours.map(|h| Utc::now() - chrono::Duration::hours(h));
        let (input_tokens, cache_hit, cache_miss, output_tokens, records) = match (session_id, since) {
            (Some(session_id), Some(since)) => conn.query_row(
                "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(cache_hit_tokens),0), COALESCE(SUM(cache_miss_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(COUNT(1),0)
                 FROM usage_ledger WHERE session_id = ?1 AND recorded_at >= ?2",
                params![session_id.to_string(), since.to_rfc3339()],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)),
            )?,
            (Some(session_id), None) => conn.query_row(
                "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(cache_hit_tokens),0), COALESCE(SUM(cache_miss_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(COUNT(1),0)
                 FROM usage_ledger WHERE session_id = ?1",
                [session_id.to_string()],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)),
            )?,
            (None, Some(since)) => conn.query_row(
                "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(cache_hit_tokens),0), COALESCE(SUM(cache_miss_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(COUNT(1),0)
                 FROM usage_ledger WHERE recorded_at >= ?1",
                [since.to_rfc3339()],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)),
            )?,
            (None, None) => conn.query_row(
                "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(cache_hit_tokens),0), COALESCE(SUM(cache_miss_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(COUNT(1),0)
                 FROM usage_ledger",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)),
            )?,
        };
        Ok(UsageSummary {
            input_tokens: input_tokens.max(0) as u64,
            cache_hit_tokens: cache_hit.max(0) as u64,
            cache_miss_tokens: cache_miss.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            records: records.max(0) as u64,
        })
    }

    /// Return per-session usage breakdown for the last N days.
    pub fn session_history(&self, last_days: i64) -> Result<Vec<SessionHistoryEntry>> {
        let conn = self.db()?;
        let since = Utc::now() - chrono::Duration::days(last_days);
        let mut stmt = conn.prepare(
            "SELECT session_id,
                    MIN(recorded_at) AS started_at,
                    COALESCE(SUM(input_tokens), 0) AS input_tokens,
                    COALESCE(SUM(output_tokens), 0) AS output_tokens,
                    COUNT(1) AS turn_count
             FROM usage_ledger
             WHERE recorded_at >= ?1
             GROUP BY session_id
             ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([since.to_rfc3339()], |r| {
            Ok(SessionHistoryEntry {
                session_id: r.get(0)?,
                started_at: r.get(1)?,
                input_tokens: r.get::<_, i64>(2)?.max(0) as u64,
                output_tokens: r.get::<_, i64>(3)?.max(0) as u64,
                turn_count: r.get::<_, i64>(4)?.max(0) as u64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn usage_by_unit(&self, session_id: Uuid) -> Result<Vec<UsageByUnitSummary>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT unit, COALESCE(SUM(input_tokens),0), COALESCE(SUM(cache_hit_tokens),0), COALESCE(SUM(cache_miss_tokens),0), COALESCE(SUM(output_tokens),0)
             FROM usage_ledger WHERE session_id = ?1 GROUP BY unit ORDER BY unit",
        )?;
        let rows = stmt.query_map([session_id.to_string()], |r| {
            Ok(UsageByUnitSummary {
                unit: r.get(0)?,
                input_tokens: r.get::<_, i64>(1)?.max(0) as u64,
                cache_hit_tokens: r.get::<_, i64>(2)?.max(0) as u64,
                cache_miss_tokens: r.get::<_, i64>(3)?.max(0) as u64,
                output_tokens: r.get::<_, i64>(4)?.max(0) as u64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_recent_verification_runs(
        &self,
        session_id: Uuid,
        limit: usize,
    ) -> Result<Vec<VerificationRunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT command, success, output, run_at
             FROM verification_runs
             WHERE session_id = ?1
             ORDER BY run_at DESC
             LIMIT ?2",
        )?;
        let mut out = Vec::new();
        let rows = stmt.query_map(params![session_id.to_string(), limit as i64], |r| {
            Ok(VerificationRunRecord {
                command: r.get(0)?,
                success: r.get::<_, i64>(1)? != 0,
                output: r.get(2)?,
                run_at: r.get(3)?,
            })
        })?;
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_context_compactions(
        &self,
        session_id: Option<Uuid>,
    ) -> Result<Vec<ContextCompactionRecord>> {
        let conn = self.db()?;
        let sql = if session_id.is_some() {
            "SELECT summary_id, session_id, from_turn, to_turn, token_delta_estimate, replay_pointer, created_at
             FROM context_compactions WHERE session_id = ?1 ORDER BY created_at DESC"
        } else {
            "SELECT summary_id, session_id, from_turn, to_turn, token_delta_estimate, replay_pointer, created_at
             FROM context_compactions ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let mut out = Vec::new();
        if let Some(session_id) = session_id {
            let rows = stmt.query_map([session_id.to_string()], |r| {
                Ok(ContextCompactionRecord {
                    summary_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                    session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                    from_turn: r.get::<_, i64>(2)? as u64,
                    to_turn: r.get::<_, i64>(3)? as u64,
                    token_delta_estimate: r.get(4)?,
                    replay_pointer: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let rows = stmt.query_map([], |r| {
                Ok(ContextCompactionRecord {
                    summary_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                    session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                    from_turn: r.get::<_, i64>(2)? as u64,
                    to_turn: r.get::<_, i64>(3)? as u64,
                    token_delta_estimate: r.get(4)?,
                    replay_pointer: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    pub fn insert_checkpoint(&self, record: &CheckpointRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO checkpoints (checkpoint_id, reason, snapshot_path, files_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.checkpoint_id.to_string(),
                record.reason,
                record.snapshot_path,
                record.files_count as i64,
                record.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_checkpoint(&self, checkpoint_id: Uuid) -> Result<Option<CheckpointRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT checkpoint_id, reason, snapshot_path, files_count, created_at
             FROM checkpoints WHERE checkpoint_id = ?1",
        )?;
        let mut rows = stmt.query([checkpoint_id.to_string()])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(CheckpointRecord {
                checkpoint_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                reason: row.get(1)?,
                snapshot_path: row.get(2)?,
                files_count: row.get::<_, i64>(3)? as u64,
                created_at: row.get(4)?,
            }));
        }
        Ok(None)
    }

    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT checkpoint_id, reason, snapshot_path, files_count, created_at
             FROM checkpoints ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CheckpointRecord {
                checkpoint_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                reason: r.get(1)?,
                snapshot_path: r.get(2)?,
                files_count: r.get::<_, i64>(3)? as u64,
                created_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Delete checkpoints older than `max_age_days`. Returns the number of deleted rows.
    pub fn delete_old_checkpoints(&self, max_age_days: u32) -> Result<u64> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(max_age_days as i64);
        let cutoff_str = cutoff.to_rfc3339();
        let conn = self.db()?;
        let mut stmt =
            conn.prepare("SELECT snapshot_path FROM checkpoints WHERE created_at < ?1")?;
        let paths: Vec<String> = stmt
            .query_map([&cutoff_str], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        let deleted = conn.execute(
            "DELETE FROM checkpoints WHERE created_at < ?1",
            [&cutoff_str],
        )?;
        for path in &paths {
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
        Ok(deleted as u64)
    }

    // ── Persistent Approvals ────────────────────────────────────────────────

    pub fn insert_persistent_approval(
        &self,
        tool_name: &str,
        command_pattern: &str,
        project_hash: &str,
    ) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO persistent_approvals (tool_name, command_pattern, project_hash, approved_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![tool_name, command_pattern, project_hash, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn is_persistently_approved(
        &self,
        tool_name: &str,
        command: &str,
        project_hash: &str,
    ) -> Result<bool> {
        let conn = self.db()?;
        // Load all patterns for this tool+project and check glob matching
        let mut stmt = conn.prepare(
            "SELECT command_pattern FROM persistent_approvals
             WHERE tool_name = ?1 AND project_hash = ?2",
        )?;
        let patterns: Vec<String> = stmt
            .query_map(params![tool_name, project_hash], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        for pattern in &patterns {
            if glob_matches_command(pattern, command) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn list_persistent_approvals(&self, project_hash: &str) -> Result<Vec<PersistentApproval>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT tool_name, command_pattern, project_hash
             FROM persistent_approvals WHERE project_hash = ?1",
        )?;
        let rows = stmt.query_map([project_hash], |r| {
            Ok(PersistentApproval {
                tool_name: r.get(0)?,
                command_pattern: r.get(1)?,
                project_hash: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn remove_persistent_approval(
        &self,
        tool_name: &str,
        command_pattern: &str,
        project_hash: &str,
    ) -> Result<bool> {
        let conn = self.db()?;
        let deleted = conn.execute(
            "DELETE FROM persistent_approvals
             WHERE tool_name = ?1 AND command_pattern = ?2 AND project_hash = ?3",
            params![tool_name, command_pattern, project_hash],
        )?;
        Ok(deleted > 0)
    }

    pub fn insert_transcript_export(&self, record: &TranscriptExportRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO transcript_exports (export_id, session_id, format, output_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.export_id.to_string(),
                record.session_id.to_string(),
                record.format,
                record.output_path,
                record.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_mcp_server(&self, record: &McpServerRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO mcp_servers (server_id, name, transport, endpoint, enabled, metadata_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.server_id,
                record.name,
                record.transport,
                record.endpoint,
                if record.enabled { 1 } else { 0 },
                record.metadata_json,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn remove_mcp_server(&self, server_id: &str) -> Result<()> {
        let conn = self.db()?;
        conn.execute("DELETE FROM mcp_servers WHERE server_id = ?1", [server_id])?;
        conn.execute(
            "DELETE FROM mcp_tools_cache WHERE server_id = ?1",
            [server_id],
        )?;
        Ok(())
    }

    pub fn list_mcp_servers(&self) -> Result<Vec<McpServerRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT server_id, name, transport, endpoint, enabled, metadata_json, updated_at
             FROM mcp_servers ORDER BY server_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(McpServerRecord {
                server_id: r.get(0)?,
                name: r.get(1)?,
                transport: r.get(2)?,
                endpoint: r.get(3)?,
                enabled: r.get::<_, i64>(4)? != 0,
                metadata_json: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn upsert_mcp_tool_cache(&self, record: &McpToolCacheRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO mcp_tools_cache (server_id, tool_name, description, schema_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.server_id,
                record.tool_name,
                record.description,
                record.schema_json,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_mcp_tool_cache(&self) -> Result<Vec<McpToolCacheRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT server_id, tool_name, description, schema_json, updated_at
             FROM mcp_tools_cache ORDER BY server_id ASC, tool_name ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(McpToolCacheRecord {
                server_id: r.get(0)?,
                tool_name: r.get(1)?,
                description: r.get(2)?,
                schema_json: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn replace_mcp_tool_cache_for_server(
        &self,
        server_id: &str,
        records: &[McpToolCacheRecord],
    ) -> Result<()> {
        let mut conn = self.db()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM mcp_tools_cache WHERE server_id = ?1",
            [server_id],
        )?;
        for record in records {
            tx.execute(
                "INSERT OR REPLACE INTO mcp_tools_cache (server_id, tool_name, description, schema_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    record.server_id,
                    record.tool_name,
                    record.description,
                    record.schema_json,
                    record.updated_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_subagent_run(&self, record: &SubagentRunRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT INTO subagent_runs (
                run_id, session_id, task_id, child_session_id, background_job_id,
                name, goal, status, output, error, created_at, updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(run_id) DO UPDATE SET
                session_id = COALESCE(excluded.session_id, subagent_runs.session_id),
                task_id = COALESCE(excluded.task_id, subagent_runs.task_id),
                child_session_id = COALESCE(excluded.child_session_id, subagent_runs.child_session_id),
                background_job_id = COALESCE(excluded.background_job_id, subagent_runs.background_job_id),
                name = excluded.name,
                goal = excluded.goal,
                status = excluded.status,
                output = excluded.output,
                error = excluded.error,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at",
            params![
                record.run_id.to_string(),
                record.session_id.map(|id| id.to_string()),
                record.task_id.map(|id| id.to_string()),
                record.child_session_id.map(|id| id.to_string()),
                record.background_job_id.map(|id| id.to_string()),
                record.name,
                record.goal,
                record.status,
                record.output,
                record.error,
                record.created_at,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_subagent_run(&self, run_id: Uuid) -> Result<Option<SubagentRunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, session_id, task_id, child_session_id, background_job_id,
                    name, goal, status, output, error, created_at, updated_at
             FROM subagent_runs WHERE run_id = ?1",
        )?;
        let mut rows = stmt.query([run_id.to_string()])?;
        if let Some(r) = rows.next()? {
            return Ok(Some(SubagentRunRecord {
                run_id: parse_uuid_or_nil(&r.get::<_, String>(0)?),
                session_id: r
                    .get::<_, Option<String>>(1)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                task_id: r
                    .get::<_, Option<String>>(2)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                child_session_id: r
                    .get::<_, Option<String>>(3)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                background_job_id: r
                    .get::<_, Option<String>>(4)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                name: r.get(5)?,
                goal: r.get(6)?,
                status: r.get(7)?,
                output: r.get(8)?,
                error: r.get(9)?,
                created_at: r.get(10)?,
                updated_at: r.get(11)?,
            }));
        }
        Ok(None)
    }

    pub fn load_subagent_run_for_task(&self, task_id: Uuid) -> Result<Option<SubagentRunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, session_id, task_id, child_session_id, background_job_id,
                    name, goal, status, output, error, created_at, updated_at
             FROM subagent_runs WHERE task_id = ?1
             ORDER BY updated_at DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query([task_id.to_string()])?;
        if let Some(r) = rows.next()? {
            return Ok(Some(SubagentRunRecord {
                run_id: parse_uuid_or_nil(&r.get::<_, String>(0)?),
                session_id: r
                    .get::<_, Option<String>>(1)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                task_id: r
                    .get::<_, Option<String>>(2)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                child_session_id: r
                    .get::<_, Option<String>>(3)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                background_job_id: r
                    .get::<_, Option<String>>(4)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                name: r.get(5)?,
                goal: r.get(6)?,
                status: r.get(7)?,
                output: r.get(8)?,
                error: r.get(9)?,
                created_at: r.get(10)?,
                updated_at: r.get(11)?,
            }));
        }
        Ok(None)
    }

    pub fn list_subagent_runs(
        &self,
        session_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<SubagentRunRecord>> {
        let conn = self.db()?;
        let capped_limit = limit.clamp(1, 1000) as i64;
        if let Some(session_id) = session_id {
            let mut event_stmt = conn.prepare(
                "SELECT payload
                 FROM events
                 WHERE session_id = ?1
                   AND kind IN ('SubagentSpawned@v1', 'SubagentCompleted@v1', 'SubagentFailed@v1')
                 ORDER BY seq_no ASC",
            )?;
            let event_rows =
                event_stmt.query_map(params![session_id.to_string()], |r| r.get::<_, String>(0))?;
            let mut session_run_ids = HashSet::new();
            for payload_row in event_rows {
                let payload = payload_row?;
                let Ok(kind) = parse_event_kind_compat(&payload) else {
                    continue;
                };
                match kind {
                    EventKind::SubagentSpawned { run_id, .. }
                    | EventKind::SubagentCompleted { run_id, .. }
                    | EventKind::SubagentFailed { run_id, .. } => {
                        session_run_ids.insert(run_id.to_string());
                    }
                    _ => {}
                }
            }
            if session_run_ids.is_empty() {
                return Ok(Vec::new());
            }
            let mut stmt = conn.prepare(
                "SELECT run_id, session_id, task_id, child_session_id, background_job_id,
                        name, goal, status, output, error, created_at, updated_at
                 FROM subagent_runs
                 ORDER BY updated_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SubagentRunRecord {
                    run_id: parse_uuid_or_nil(&r.get::<_, String>(0)?),
                    session_id: r
                        .get::<_, Option<String>>(1)?
                        .and_then(|value| Uuid::parse_str(&value).ok()),
                    task_id: r
                        .get::<_, Option<String>>(2)?
                        .and_then(|value| Uuid::parse_str(&value).ok()),
                    child_session_id: r
                        .get::<_, Option<String>>(3)?
                        .and_then(|value| Uuid::parse_str(&value).ok()),
                    background_job_id: r
                        .get::<_, Option<String>>(4)?
                        .and_then(|value| Uuid::parse_str(&value).ok()),
                    name: r.get(5)?,
                    goal: r.get(6)?,
                    status: r.get(7)?,
                    output: r.get(8)?,
                    error: r.get(9)?,
                    created_at: r.get(10)?,
                    updated_at: r.get(11)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                let record = row?;
                if !session_run_ids.contains(&record.run_id.to_string()) {
                    continue;
                }
                out.push(record);
                if out.len() >= capped_limit as usize {
                    break;
                }
            }
            Ok(out)
        } else {
            let mut stmt = conn.prepare(
                "SELECT run_id, session_id, task_id, child_session_id, background_job_id,
                        name, goal, status, output, error, created_at, updated_at
                 FROM subagent_runs
                 ORDER BY updated_at DESC
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![capped_limit], |r| {
                Ok(SubagentRunRecord {
                    run_id: parse_uuid_or_nil(&r.get::<_, String>(0)?),
                    session_id: r
                        .get::<_, Option<String>>(1)?
                        .and_then(|value| Uuid::parse_str(&value).ok()),
                    task_id: r
                        .get::<_, Option<String>>(2)?
                        .and_then(|value| Uuid::parse_str(&value).ok()),
                    child_session_id: r
                        .get::<_, Option<String>>(3)?
                        .and_then(|value| Uuid::parse_str(&value).ok()),
                    background_job_id: r
                        .get::<_, Option<String>>(4)?
                        .and_then(|value| Uuid::parse_str(&value).ok()),
                    name: r.get(5)?,
                    goal: r.get(6)?,
                    status: r.get(7)?,
                    output: r.get(8)?,
                    error: r.get(9)?,
                    created_at: r.get(10)?,
                    updated_at: r.get(11)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        }
    }

    pub fn insert_cost_ledger(
        &self,
        session_id: Uuid,
        input_tokens: u64,
        output_tokens: u64,
        estimated_cost_usd: f64,
    ) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT INTO cost_ledger (session_id, input_tokens, output_tokens, estimated_cost_usd, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id.to_string(),
                input_tokens as i64,
                output_tokens as i64,
                estimated_cost_usd,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn total_session_cost(&self, session_id: Uuid) -> Result<f64> {
        let conn = self.db()?;
        let total: f64 = conn.query_row(
            "SELECT COALESCE(SUM(estimated_cost_usd), 0.0) FROM cost_ledger WHERE session_id = ?1",
            params![session_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    pub fn insert_profile_run(&self, record: &ProfileRunRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO profile_runs (profile_id, session_id, summary, elapsed_ms, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.profile_id.to_string(),
                record.session_id.to_string(),
                record.summary,
                record.elapsed_ms as i64,
                record.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn insert_memory_version(
        &self,
        version_id: Uuid,
        path: &str,
        content: &str,
        note: &str,
        created_at: &str,
    ) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO memory_versions (version_id, path, content, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![version_id.to_string(), path, content, note, created_at],
        )?;
        Ok(())
    }

    pub fn upsert_background_job(&self, record: &BackgroundJobRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO background_jobs (job_id, kind, reference, status, metadata_json, started_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.job_id.to_string(),
                record.kind,
                record.reference,
                record.status,
                record.metadata_json,
                record.started_at,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_background_job(&self, job_id: Uuid) -> Result<Option<BackgroundJobRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, kind, reference, status, metadata_json, started_at, updated_at
             FROM background_jobs WHERE job_id = ?1",
        )?;
        let mut rows = stmt.query([job_id.to_string()])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(BackgroundJobRecord {
                job_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                kind: row.get(1)?,
                reference: row.get(2)?,
                status: row.get(3)?,
                metadata_json: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
            }));
        }
        Ok(None)
    }

    pub fn load_background_job_by_reference(
        &self,
        reference: &str,
    ) -> Result<Option<BackgroundJobRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, kind, reference, status, metadata_json, started_at, updated_at
             FROM background_jobs
             WHERE reference = ?1
             ORDER BY updated_at DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query([reference])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(BackgroundJobRecord {
                job_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                kind: row.get(1)?,
                reference: row.get(2)?,
                status: row.get(3)?,
                metadata_json: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
            }));
        }
        Ok(None)
    }

    pub fn list_background_jobs(&self) -> Result<Vec<BackgroundJobRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, kind, reference, status, metadata_json, started_at, updated_at
             FROM background_jobs ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(BackgroundJobRecord {
                job_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                kind: r.get(1)?,
                reference: r.get(2)?,
                status: r.get(3)?,
                metadata_json: r.get(4)?,
                started_at: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn insert_replay_cassette(&self, record: &ReplayCassetteRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO replay_cassettes (cassette_id, session_id, deterministic, events_count, payload_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.cassette_id.to_string(),
                record.session_id.to_string(),
                if record.deterministic { 1 } else { 0 },
                record.events_count as i64,
                record.payload_json,
                record.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_replay_cassettes(
        &self,
        session_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<ReplayCassetteRecord>> {
        let conn = self.db()?;
        let bounded_limit = limit.clamp(1, 500) as i64;
        let mut out = Vec::new();
        if let Some(session_id) = session_id {
            let mut stmt = conn.prepare(
                "SELECT cassette_id, session_id, deterministic, events_count, payload_json, created_at
                 FROM replay_cassettes
                 WHERE session_id = ?1
                 ORDER BY created_at DESC
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![session_id.to_string(), bounded_limit], |r| {
                Ok(ReplayCassetteRecord {
                    cassette_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                    session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                    deterministic: r.get::<_, i64>(2)? != 0,
                    events_count: r.get::<_, i64>(3)? as u64,
                    payload_json: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
            return Ok(out);
        }

        let mut stmt = conn.prepare(
            "SELECT cassette_id, session_id, deterministic, events_count, payload_json, created_at
             FROM replay_cassettes
             ORDER BY created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![bounded_limit], |r| {
            Ok(ReplayCassetteRecord {
                cassette_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                deterministic: r.get::<_, i64>(2)? != 0,
                events_count: r.get::<_, i64>(3)? as u64,
                payload_json: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_skill_registry(&self, record: &SkillRegistryRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO skill_registry (skill_id, name, path, enabled, metadata_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.skill_id,
                record.name,
                record.path,
                if record.enabled { 1 } else { 0 },
                record.metadata_json,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_skill_registry(&self) -> Result<Vec<SkillRegistryRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT skill_id, name, path, enabled, metadata_json, updated_at
             FROM skill_registry ORDER BY skill_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SkillRegistryRecord {
                skill_id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                enabled: r.get::<_, i64>(3)? != 0,
                metadata_json: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn remove_skill_registry(&self, skill_id: &str) -> Result<()> {
        let conn = self.db()?;
        conn.execute("DELETE FROM skill_registry WHERE skill_id = ?1", [skill_id])?;
        Ok(())
    }

    pub fn insert_provider_metric(&self, record: &ProviderMetricRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT INTO provider_metrics (provider, model, cache_key, cache_hit, latency_ms, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.provider,
                record.model,
                record.cache_key,
                if record.cache_hit { 1 } else { 0 },
                record.latency_ms as i64,
                record.recorded_at,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_marketplace_catalog(&self, record: &MarketplaceCatalogRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO marketplace_catalog (plugin_id, name, version, source, metadata_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.plugin_id,
                record.name,
                record.version,
                record.source,
                record.metadata_json,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_marketplace_catalog(&self) -> Result<Vec<MarketplaceCatalogRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT plugin_id, name, version, source, metadata_json, updated_at
             FROM marketplace_catalog ORDER BY plugin_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MarketplaceCatalogRecord {
                plugin_id: r.get(0)?,
                name: r.get(1)?,
                version: r.get(2)?,
                source: r.get(3)?,
                metadata_json: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn insert_visual_artifact(&self, record: &VisualArtifactRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO visual_artifacts (artifact_id, path, mime, metadata_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.artifact_id.to_string(),
                record.path,
                record.mime,
                record.metadata_json,
                record.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_visual_artifacts(&self, limit: usize) -> Result<Vec<VisualArtifactRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT artifact_id, path, mime, metadata_json, created_at
             FROM visual_artifacts ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit.max(1) as i64], |r| {
            Ok(VisualArtifactRecord {
                artifact_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                path: r.get(1)?,
                mime: r.get(2)?,
                metadata_json: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn upsert_remote_env_profile(&self, record: &RemoteEnvProfileRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO remote_env_profiles (profile_id, name, endpoint, auth_mode, metadata_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.profile_id.to_string(),
                record.name,
                record.endpoint,
                record.auth_mode,
                record.metadata_json,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_remote_env_profiles(&self) -> Result<Vec<RemoteEnvProfileRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT profile_id, name, endpoint, auth_mode, metadata_json, updated_at
             FROM remote_env_profiles ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RemoteEnvProfileRecord {
                profile_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                name: r.get(1)?,
                endpoint: r.get(2)?,
                auth_mode: r.get(3)?,
                metadata_json: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn load_remote_env_profile(
        &self,
        profile_id: Uuid,
    ) -> Result<Option<RemoteEnvProfileRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT profile_id, name, endpoint, auth_mode, metadata_json, updated_at
             FROM remote_env_profiles WHERE profile_id = ?1",
        )?;
        let mut rows = stmt.query([profile_id.to_string()])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(RemoteEnvProfileRecord {
                profile_id: Uuid::parse_str(row.get::<_, String>(0)?.as_str())?,
                name: row.get(1)?,
                endpoint: row.get(2)?,
                auth_mode: row.get(3)?,
                metadata_json: row.get(4)?,
                updated_at: row.get(5)?,
            }));
        }
        Ok(None)
    }

    pub fn remove_remote_env_profile(&self, profile_id: Uuid) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "DELETE FROM remote_env_profiles WHERE profile_id = ?1",
            [profile_id.to_string()],
        )?;
        Ok(())
    }

    // --- Session Locking ---

    pub fn try_acquire_session_lock(&self, session_id: Uuid, holder: &str) -> Result<bool> {
        let conn = self.db()?;
        let now = Utc::now().to_rfc3339();
        // Try to acquire; fail if another holder has it and heartbeat is recent (< 60s)
        let existing: Option<(String, String)> = conn
            .prepare("SELECT lock_holder, heartbeat_at FROM session_locks WHERE session_id = ?1")?
            .query_row([session_id.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .ok();
        if let Some((existing_holder, heartbeat)) = existing
            && existing_holder != holder
        {
            // Check if heartbeat is stale (> 60 seconds)
            if let Ok(hb) = chrono::DateTime::parse_from_rfc3339(&heartbeat) {
                let age = Utc::now().signed_duration_since(hb.with_timezone(&Utc));
                if age.num_seconds() < 60 {
                    return Ok(false); // Lock held by another process
                }
            }
        }
        conn.execute(
            "INSERT OR REPLACE INTO session_locks (session_id, lock_holder, locked_at, heartbeat_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id.to_string(), holder, &now, &now],
        )?;
        Ok(true)
    }

    pub fn release_session_lock(&self, session_id: Uuid, holder: &str) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "DELETE FROM session_locks WHERE session_id = ?1 AND lock_holder = ?2",
            params![session_id.to_string(), holder],
        )?;
        Ok(())
    }

    pub fn heartbeat_session_lock(&self, session_id: Uuid, holder: &str) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "UPDATE session_locks SET heartbeat_at = ?1 WHERE session_id = ?2 AND lock_holder = ?3",
            params![Utc::now().to_rfc3339(), session_id.to_string(), holder],
        )?;
        Ok(())
    }

    // --- Session Forking ---

    pub fn fork_session(&self, from_session_id: Uuid) -> Result<Session> {
        self.fork_session_with_messages(from_session_id, &[])
    }

    pub fn fork_session_with_messages(
        &self,
        from_session_id: Uuid,
        messages: &[ChatMessage],
    ) -> Result<Session> {
        let source = self
            .load_session(from_session_id)?
            .ok_or_else(|| anyhow::anyhow!("session not found: {from_session_id}"))?;
        let new_id = Uuid::now_v7();
        let forked = Session {
            session_id: new_id,
            workspace_root: source.workspace_root.clone(),
            baseline_commit: source.baseline_commit.clone(),
            status: SessionState::Idle,
            budgets: source.budgets.clone(),
            active_plan_id: None,
        };
        let mut conn = self.db()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::save_session_row(&tx, &forked)?;
        let kind = EventKind::SessionForked {
            from_session_id,
            to_session_id: new_id,
        };
        Self::append_event_row(&tx, &NewEvent::new(from_session_id, kind.clone()))?;
        Self::append_event_row(&tx, &NewEvent::new(new_id, kind))?;
        for message in messages {
            Self::append_event_row(
                &tx,
                &NewEvent::new(
                    new_id,
                    EventKind::ChatTurn {
                        message: message.clone(),
                    },
                ),
            )?;
        }
        tx.commit()?;
        Ok(forked)
    }

    // --- Task Queue ---

    pub fn insert_task(&self, record: &TaskQueueRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO task_queue (task_id, session_id, title, description, priority, status, outcome, artifact_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.task_id.to_string(),
                record.session_id.to_string(),
                record.title,
                record.description,
                record.priority as i64,
                record.status,
                record.outcome,
                record.artifact_path,
                record.created_at,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_task(&self, task_id: Uuid) -> Result<Option<TaskQueueRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, session_id, title, description, priority, status, outcome, artifact_path, created_at, updated_at
             FROM task_queue WHERE task_id = ?1",
        )?;
        let mut rows = stmt.query([task_id.to_string()])?;
        if let Some(r) = rows.next()? {
            return Ok(Some(TaskQueueRecord {
                task_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                title: r.get(2)?,
                description: r.get(3)?,
                priority: r.get::<_, i64>(4)? as u32,
                status: r.get(5)?,
                outcome: r.get(6)?,
                artifact_path: r.get(7)?,
                created_at: r.get(8)?,
                updated_at: r.get(9)?,
            }));
        }
        Ok(None)
    }

    pub fn delete_task(&self, task_id: Uuid) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "DELETE FROM task_queue WHERE task_id = ?1",
            [task_id.to_string()],
        )?;
        Ok(())
    }

    pub fn update_task_status(
        &self,
        task_id: Uuid,
        status: &str,
        outcome: Option<&str>,
    ) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "UPDATE task_queue SET status = ?1, outcome = ?2, updated_at = ?3 WHERE task_id = ?4",
            params![
                status,
                outcome,
                Utc::now().to_rfc3339(),
                task_id.to_string()
            ],
        )?;
        Ok(())
    }

    pub fn list_tasks(&self, session_id: Option<Uuid>) -> Result<Vec<TaskQueueRecord>> {
        let conn = self.db()?;
        let mut out = Vec::new();
        if let Some(sid) = session_id {
            let mut stmt = conn.prepare(
                "SELECT task_id, session_id, title, description, priority, status, outcome, artifact_path, created_at, updated_at
                 FROM task_queue WHERE session_id = ?1 ORDER BY priority DESC, created_at ASC",
            )?;
            let rows = stmt.query_map([sid.to_string()], |r| {
                Ok(TaskQueueRecord {
                    task_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                    session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                    title: r.get(2)?,
                    description: r.get(3)?,
                    priority: r.get::<_, i64>(4)? as u32,
                    status: r.get(5)?,
                    outcome: r.get(6)?,
                    artifact_path: r.get(7)?,
                    created_at: r.get(8)?,
                    updated_at: r.get(9)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT task_id, session_id, title, description, priority, status, outcome, artifact_path, created_at, updated_at
                 FROM task_queue ORDER BY priority DESC, created_at ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(TaskQueueRecord {
                    task_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                    session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                    title: r.get(2)?,
                    description: r.get(3)?,
                    priority: r.get::<_, i64>(4)? as u32,
                    status: r.get(5)?,
                    outcome: r.get(6)?,
                    artifact_path: r.get(7)?,
                    created_at: r.get(8)?,
                    updated_at: r.get(9)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    // --- Session Todos ---

    pub fn upsert_session_todo(&self, record: &SessionTodoRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO session_todos
             (todo_id, session_id, content, status, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.todo_id.to_string(),
                record.session_id.to_string(),
                record.content,
                record.status,
                record.position as i64,
                record.created_at,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_session_todos(&self, session_id: Uuid) -> Result<Vec<SessionTodoRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT todo_id, session_id, content, status, position, created_at, updated_at
             FROM session_todos
             WHERE session_id = ?1
             ORDER BY position ASC, created_at ASC",
        )?;
        let rows = stmt.query_map([session_id.to_string()], |r| {
            Ok(SessionTodoRecord {
                todo_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                content: r.get(2)?,
                status: r.get(3)?,
                position: r.get::<_, i64>(4)? as u32,
                created_at: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn delete_session_todo(&self, todo_id: Uuid) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "DELETE FROM session_todos WHERE todo_id = ?1",
            [todo_id.to_string()],
        )?;
        Ok(())
    }

    pub fn replace_session_todos(
        &self,
        session_id: Uuid,
        todos: &[SessionTodoRecord],
    ) -> Result<()> {
        let mut conn = self.db()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM session_todos WHERE session_id = ?1",
            [session_id.to_string()],
        )?;
        for todo in todos {
            tx.execute(
                "INSERT OR REPLACE INTO session_todos
                 (todo_id, session_id, content, status, position, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    todo.todo_id.to_string(),
                    todo.session_id.to_string(),
                    todo.content,
                    todo.status,
                    todo.position as i64,
                    todo.created_at,
                    todo.updated_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_plan(&self, plan_id: Uuid) -> Result<Option<Plan>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare("SELECT payload FROM plans WHERE plan_id = ?1")?;
        let mut rows = stmt.query([plan_id.to_string()])?;
        if let Some(row) = rows.next()? {
            let payload: String = row.get(0)?;
            return Ok(Some(serde_json::from_str(&payload)?));
        }
        Ok(None)
    }

    // --- Web Search Cache ---

    pub fn get_web_search_cache(&self, query_hash: &str) -> Result<Option<WebSearchCacheRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT query_hash, query, results_json, results_count, cached_at, ttl_seconds
             FROM web_search_cache WHERE query_hash = ?1",
        )?;
        let mut rows = stmt.query([query_hash])?;
        if let Some(row) = rows.next()? {
            let cached_at: String = row.get(4)?;
            let ttl: i64 = row.get(5)?;
            // Check if cache is still valid
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&cached_at) {
                let age = Utc::now().signed_duration_since(ts.with_timezone(&Utc));
                if age.num_seconds() > ttl {
                    // Expired
                    conn.execute(
                        "DELETE FROM web_search_cache WHERE query_hash = ?1",
                        [query_hash],
                    )?;
                    return Ok(None);
                }
            }
            return Ok(Some(WebSearchCacheRecord {
                query_hash: row.get(0)?,
                query: row.get(1)?,
                results_json: row.get(2)?,
                results_count: row.get::<_, i64>(3)? as u64,
                cached_at,
                ttl_seconds: ttl as u64,
            }));
        }
        Ok(None)
    }

    pub fn set_web_search_cache(&self, record: &WebSearchCacheRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO web_search_cache (query_hash, query, results_json, results_count, cached_at, ttl_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.query_hash,
                record.query,
                record.results_json,
                record.results_count as i64,
                record.cached_at,
                record.ttl_seconds as i64,
            ],
        )?;
        Ok(())
    }

    // --- Review Runs ---

    pub fn insert_review_run(&self, record: &ReviewRunRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO review_runs (review_id, session_id, preset, target, findings_json, findings_count, critical_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.review_id.to_string(),
                record.session_id.to_string(),
                record.preset,
                record.target,
                record.findings_json,
                record.findings_count as i64,
                record.critical_count as i64,
                record.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_review_runs(&self, session_id: Uuid) -> Result<Vec<ReviewRunRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT review_id, session_id, preset, target, findings_json, findings_count, critical_count, created_at
             FROM review_runs WHERE session_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([session_id.to_string()], |r| {
            Ok(ReviewRunRecord {
                review_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                session_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                preset: r.get(2)?,
                target: r.get(3)?,
                findings_json: r.get(4)?,
                findings_count: r.get::<_, i64>(5)? as u64,
                critical_count: r.get::<_, i64>(6)? as u64,
                created_at: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // --- Artifacts ---

    pub fn insert_artifact(&self, record: &ArtifactRecord) -> Result<()> {
        let conn = self.db()?;
        conn.execute(
            "INSERT OR REPLACE INTO artifacts (artifact_id, task_id, artifact_path, files_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.artifact_id.to_string(),
                record.task_id.to_string(),
                record.artifact_path,
                record.files_json,
                record.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_artifacts_for_task(&self, task_id: Uuid) -> Result<Vec<ArtifactRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT artifact_id, task_id, artifact_path, files_json, created_at
             FROM artifacts WHERE task_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([task_id.to_string()], |r| {
            Ok(ArtifactRecord {
                artifact_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                task_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                artifact_path: r.get(2)?,
                files_json: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_all_artifacts(&self, limit: usize) -> Result<Vec<ArtifactRecord>> {
        let conn = self.db()?;
        let mut stmt = conn.prepare(
            "SELECT artifact_id, task_id, artifact_path, files_json, created_at
             FROM artifacts ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(ArtifactRecord {
                artifact_id: parse_uuid_or_nil(r.get::<_, String>(0)?.as_str()),
                task_id: parse_uuid_or_nil(r.get::<_, String>(1)?.as_str()),
                artifact_path: r.get(2)?,
                files_json: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // --- Sessions by workspace (for --continue) ---

    /// Name a session (`/rename`). An empty title clears the name.
    pub fn set_session_title(&self, session_id: Uuid, title: &str) -> Result<()> {
        let title = title.trim();
        let conn = self.db()?;
        if title.is_empty() {
            conn.execute(
                "DELETE FROM session_titles WHERE session_id = ?1",
                params![session_id.to_string()],
            )?;
        } else {
            conn.execute(
                "INSERT INTO session_titles(session_id, title, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET title = excluded.title,
                 updated_at = excluded.updated_at",
                params![session_id.to_string(), title, Utc::now().to_rfc3339()],
            )?;
        }
        Ok(())
    }

    /// The most recent sessions with a readable title: the `/rename` name,
    /// else the first prompt shortened to one line.
    pub fn session_summaries(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        let conn = self.db()?;
        let mut named = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT session_id, title FROM session_titles")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, title) = row?;
                named.insert(id, title);
            }
        }
        let mut updated = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT session_id, updated_at FROM sessions")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, at) = row?;
                updated.insert(id, at);
            }
        }
        let mut out = Vec::new();
        for session in self.list_sessions()?.into_iter().take(limit) {
            let key = session.session_id.to_string();
            let (title, renamed) = match named.get(&key) {
                Some(title) => (title.clone(), true),
                None => (self.first_prompt_title(session.session_id), false),
            };
            out.push(SessionSummary {
                updated_at: updated.get(&key).cloned().unwrap_or_default(),
                session,
                title,
                renamed,
            });
        }
        Ok(out)
    }

    /// Messages across the most recent `max_sessions` sessions that contain
    /// every word of `query` (case-insensitive), newest session first, at
    /// most `limit` hits. Only user prompts and assistant answers are
    /// searched; tool output is not.
    pub fn search_sessions(
        &self,
        query: &str,
        max_sessions: usize,
        limit: usize,
    ) -> Result<Vec<SessionSearchHit>> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut hits = Vec::new();
        for summary in self.session_summaries(max_sessions)? {
            let Ok(state) = self.rebuild_from_events(summary.session.session_id) else {
                continue;
            };
            for message in state.chat_messages {
                let (role, text) = match message {
                    codingbuddy_core::ChatMessage::User { content } => ("user", content),
                    codingbuddy_core::ChatMessage::Assistant {
                        content: Some(content),
                        ..
                    } => ("assistant", content),
                    _ => continue,
                };
                let lower = text.to_lowercase();
                if !words.iter().all(|word| lower.contains(word.as_str())) {
                    continue;
                }
                hits.push(SessionSearchHit {
                    session_id: summary.session.session_id,
                    title: summary.title.clone(),
                    updated_at: summary.updated_at.clone(),
                    role: role.to_string(),
                    snippet: search_snippet(&text, &words[0]),
                });
                if hits.len() >= limit {
                    return Ok(hits);
                }
            }
        }
        Ok(hits)
    }

    fn first_prompt_title(&self, session_id: Uuid) -> String {
        let first = self
            .rebuild_from_events(session_id)
            .ok()
            .and_then(|history| {
                history
                    .chat_messages
                    .into_iter()
                    .find_map(|message| match message {
                        codingbuddy_core::ChatMessage::User { content } => Some(content),
                        _ => None,
                    })
            });
        let Some(text) = first else {
            return "(no prompts yet)".to_string();
        };
        let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.chars().count() > 60 {
            format!("{}…", line.chars().take(59).collect::<String>())
        } else {
            line
        }
    }
}

/// About 100 characters of `text` on one line, centred on the first
/// case-insensitive occurrence of `word` (already lowercase).
fn search_snippet(text: &str, word: &str) -> String {
    const RADIUS: usize = 50;
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let needle: Vec<char> = word.chars().collect();
    let at = (0..lower.len())
        .find(|&i| lower[i..].starts_with(&needle))
        .unwrap_or(0);
    let start = at.saturating_sub(RADIUS);
    let end = (at + needle.len() + RADIUS).min(chars.len());
    let body: String = chars[start..end].iter().collect();
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{}{body}{}",
        if start > 0 { "…" } else { "" },
        if end < chars.len() { "…" } else { "" }
    )
}
