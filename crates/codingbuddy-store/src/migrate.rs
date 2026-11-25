//! Database initialisation, schema migrations, legacy archive import and event projection.

use super::*;

impl Store {
    pub(crate) fn init_db(&self) -> Result<()> {
        let mut conn = self.db()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
             );",
        )?;
        let latest: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        if latest >= EVENT_STORAGE_VERSION {
            return Ok(());
        }

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let had_events: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'events')",
            [],
            |row| row.get(0),
        )?;
        for (version, sql) in MIGRATIONS {
            if !Self::has_migration(&tx, *version)? {
                tx.execute_batch(sql)?;
                Self::record_migration(&tx, *version)?;
            }
        }
        if !Self::has_migration(&tx, EVENT_STORAGE_VERSION)? {
            self.migrate_events(&tx, had_events)?;
            Self::record_migration(&tx, EVENT_STORAGE_VERSION)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn has_migration(conn: &Connection, version: i64) -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
            [version],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn record_migration(conn: &Connection, version: i64) -> Result<()> {
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub(crate) fn migrate_events(&self, conn: &Connection, had_events: bool) -> Result<()> {
        conn.execute_batch(
            "ALTER TABLE events RENAME TO legacy_events;
             CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                seq_no INTEGER NOT NULL CHECK (seq_no > 0),
                at TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                event_id TEXT NOT NULL UNIQUE,
                UNIQUE (session_id, seq_no)
             );",
        )?;
        if had_events {
            let mut statement = conn.prepare(
                "SELECT MIN(id), session_id, seq_no, at, kind, payload FROM legacy_events
                 GROUP BY session_id, seq_no, at, kind, payload ORDER BY MIN(id)",
            )?;
            let mut rows = statement.query([])?;
            let mut sequences = std::collections::HashMap::new();
            while let Some(row) = rows.next()? {
                let session: String = row.get(1)?;
                let seq = assign_legacy_sequence(&mut sequences, &session, row.get(2)?)?;
                conn.execute(
                    "INSERT INTO events (id, session_id, seq_no, at, kind, payload, event_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        row.get::<_, i64>(0)?,
                        session,
                        seq,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        Uuid::now_v7().to_string()
                    ],
                )?;
            }
        } else {
            self.import_archive(conn)?;
        }
        conn.execute_batch("DROP TABLE legacy_events;")?;
        Ok(())
    }

    pub(crate) fn import_archive(&self, conn: &Connection) -> Result<()> {
        let file = match fs::File::open(&self.events_path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).context("open legacy events.jsonl"),
        };
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut number = 0;
        let mut sequences = std::collections::HashMap::new();
        let mut seen = HashSet::new();
        while reader.read_until(b'\n', &mut line)? != 0 {
            number += 1;
            let parsed = std::str::from_utf8(&line)
                .map_err(anyhow::Error::from)
                .and_then(parse_event_envelope_compat);
            let mut event = match parsed {
                Ok(event) => event,
                Err(err) if !line.ends_with(b"\n") => {
                    eprintln!(
                        "codingbuddy-store: skipped incomplete events.jsonl line {number}: {err}"
                    );
                    break;
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("invalid events.jsonl line {number}"));
                }
            };
            line.clear();
            let payload = serde_json::to_string(&event.kind)?;
            if !seen.insert((event.session_id, event.seq_no, event.at, payload.clone())) {
                continue;
            }
            let session = event.session_id.to_string();
            event.seq_no =
                assign_legacy_sequence(&mut sequences, &session, i64::try_from(event.seq_no)?)?
                    as u64;
            let at = event.at.to_rfc3339();
            if !event.session_id.is_nil() {
                let workspace = self.root.parent().unwrap_or(&self.root).to_string_lossy();
                let budgets = codingbuddy_core::AppConfig::default().budgets;
                let budgets = codingbuddy_core::SessionBudgets {
                    per_turn_seconds: budgets.max_turn_duration_secs,
                    max_think_tokens: budgets.max_reasoner_tokens_per_session as u32,
                };
                conn.execute(
                    "INSERT INTO sessions (session_id, workspace_root, status, budgets, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(session_id) DO UPDATE SET updated_at = max(updated_at, excluded.updated_at)",
                    params![session, workspace, serde_json::to_string(&SessionState::Idle)?, serde_json::to_string(&budgets)?, at],
                )?;
            }
            conn.execute(
                "INSERT INTO events (session_id, seq_no, at, kind, payload, event_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![session, i64::try_from(event.seq_no)?, at, event_kind_name(&event.kind), payload, Uuid::now_v7().to_string()],
            )?;
            Self::project_event(conn, &event)?;
        }
        Ok(())
    }

    pub(crate) fn project_event(conn: &Connection, event: &EventEnvelope) -> Result<()> {
        let at = event.at.to_rfc3339();
        match &event.kind {
            EventKind::SessionStateChanged { to, .. } => {
                conn.execute(
                    "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE session_id = ?3",
                    params![serde_json::to_string(to)?, at, event.session_id.to_string()],
                )?;
            }
            EventKind::PlanCreated { plan } | EventKind::PlanRevised { plan } => {
                Self::save_plan_row(conn, event.session_id, plan)?;
                conn.execute(
                    "UPDATE sessions SET active_plan_id = ?1, status = ?2, updated_at = ?3 WHERE session_id = ?4",
                    params![
                        plan.plan_id.to_string(),
                        serde_json::to_string(&SessionState::Planning)?,
                        at,
                        event.session_id.to_string(),
                    ],
                )?;
            }
            EventKind::ToolApproved { invocation_id } => {
                conn.execute(
                    "INSERT INTO approvals_ledger (session_id, invocation_id, approved_at) VALUES (?1, ?2, ?3)",
                    params![
                        event.session_id.to_string(),
                        invocation_id.to_string(),
                        at,
                    ],
                )?;
            }
            EventKind::VerificationRun {
                command,
                success,
                output,
            } => {
                conn.execute(
                    "INSERT INTO verification_runs (session_id, command, success, output, run_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event.session_id.to_string(),
                        command,
                        if *success { 1 } else { 0 },
                        output,
                        at,
                    ],
                )?;
            }
            EventKind::UsageUpdated {
                unit,
                model,
                input_tokens,
                cache_hit_tokens,
                cache_miss_tokens,
                output_tokens,
            } => {
                conn.execute(
                    "INSERT INTO usage_ledger (session_id, unit, model, input_tokens, cache_hit_tokens, cache_miss_tokens, output_tokens, recorded_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        event.session_id.to_string(),
                        serde_json::to_string(unit)?,
                        model,
                        *input_tokens as i64,
                        *cache_hit_tokens as i64,
                        *cache_miss_tokens as i64,
                        *output_tokens as i64,
                        at,
                    ],
                )?;
            }
            EventKind::ContextCompacted {
                summary_id,
                from_turn,
                to_turn,
                token_delta_estimate,
                replay_pointer,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO context_compactions
                     (summary_id, session_id, from_turn, to_turn, token_delta_estimate, replay_pointer, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        summary_id.to_string(),
                        event.session_id.to_string(),
                        *from_turn as i64,
                        *to_turn as i64,
                        *token_delta_estimate,
                        replay_pointer,
                        at,
                    ],
                )?;
            }
            EventKind::AutopilotRunStarted { run_id, prompt } => {
                conn.execute(
                    "INSERT OR REPLACE INTO autopilot_runs
                     (run_id, session_id, prompt, status, stop_reason, completed_iterations, failed_iterations,
                      consecutive_failures, last_error, stop_file, heartbeat_file, tools, max_think, started_at, updated_at)
                     VALUES (?1, ?2, ?3, 'running', NULL, 0, 0, 0, NULL, '', '', 0, 0, ?4, ?4)",
                    params![
                        run_id.to_string(),
                        event.session_id.to_string(),
                        prompt,
                        at,
                    ],
                )?;
            }
            EventKind::AutopilotRunHeartbeat {
                run_id,
                completed_iterations,
                failed_iterations,
                consecutive_failures,
                last_error,
            } => {
                conn.execute(
                    "UPDATE autopilot_runs
                     SET completed_iterations = ?1, failed_iterations = ?2, consecutive_failures = ?3, last_error = ?4, updated_at = ?5
                     WHERE run_id = ?6",
                    params![
                        *completed_iterations as i64,
                        *failed_iterations as i64,
                        *consecutive_failures as i64,
                        last_error,
                        at,
                        run_id.to_string(),
                    ],
                )?;
            }
            EventKind::AutopilotRunStopped {
                run_id,
                stop_reason,
                completed_iterations,
                failed_iterations,
            } => {
                conn.execute(
                    "UPDATE autopilot_runs
                     SET status = 'stopped', stop_reason = ?1, completed_iterations = ?2, failed_iterations = ?3, updated_at = ?4
                     WHERE run_id = ?5",
                    params![
                        stop_reason,
                        *completed_iterations as i64,
                        *failed_iterations as i64,
                        at,
                        run_id.to_string(),
                    ],
                )?;
            }
            EventKind::HookExecuted {
                phase,
                hook_path,
                success,
                timed_out,
                exit_code,
            } => {
                conn.execute(
                    "INSERT INTO hook_executions (session_id, phase, hook_path, success, timed_out, exit_code, recorded_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        event.session_id.to_string(),
                        phase,
                        hook_path,
                        if *success { 1 } else { 0 },
                        if *timed_out { 1 } else { 0 },
                        exit_code,
                        at,
                    ],
                )?;
            }
            EventKind::CheckpointCreated {
                checkpoint_id,
                reason,
                files_count,
                snapshot_path,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO checkpoints (checkpoint_id, reason, snapshot_path, files_count, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        checkpoint_id.to_string(),
                        reason,
                        snapshot_path,
                        *files_count as i64,
                        at,
                    ],
                )?;
            }
            EventKind::CheckpointRewound {
                checkpoint_id,
                reason,
            } => {
                conn.execute(
                    "UPDATE checkpoints SET reason = ?1 WHERE checkpoint_id = ?2",
                    params![reason, checkpoint_id.to_string()],
                )?;
            }
            EventKind::TranscriptExported {
                export_id,
                format,
                output_path,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO transcript_exports (export_id, session_id, format, output_path, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        export_id.to_string(),
                        event.session_id.to_string(),
                        format,
                        output_path,
                        at,
                    ],
                )?;
            }
            EventKind::McpServerAdded {
                server_id,
                transport,
                endpoint,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO mcp_servers (server_id, name, transport, endpoint, enabled, metadata_json, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 1, '{}', ?5)",
                    params![
                        server_id,
                        server_id,
                        transport,
                        endpoint,
                        at,
                    ],
                )?;
            }
            EventKind::McpServerRemoved { server_id } => {
                conn.execute("DELETE FROM mcp_servers WHERE server_id = ?1", [server_id])?;
                conn.execute(
                    "DELETE FROM mcp_tools_cache WHERE server_id = ?1",
                    [server_id],
                )?;
            }
            EventKind::McpToolDiscovered {
                server_id,
                tool_name,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO mcp_tools_cache (server_id, tool_name, description, schema_json, updated_at)
                     VALUES (?1, ?2, '', '{}', ?3)",
                    params![server_id, tool_name, at],
                )?;
            }
            EventKind::SubagentSpawned { run_id, name, goal } => {
                conn.execute(
                    "INSERT INTO subagent_runs (
                        run_id, session_id, task_id, child_session_id, background_job_id,
                        name, goal, status, output, error, created_at, updated_at
                     )
                     VALUES (?1, ?2, NULL, NULL, NULL, ?3, ?4, 'running', NULL, NULL, ?5, ?5)
                     ON CONFLICT(run_id) DO UPDATE SET
                        session_id = COALESCE(subagent_runs.session_id, excluded.session_id),
                        name = excluded.name,
                        goal = excluded.goal,
                        status = excluded.status,
                        updated_at = excluded.updated_at",
                    params![
                        run_id.to_string(),
                        event.session_id.to_string(),
                        name,
                        goal,
                        at,
                    ],
                )?;
            }
            EventKind::SubagentCompleted { run_id, output } => {
                conn.execute(
                    "UPDATE subagent_runs SET status='completed', output=?1, error=NULL, updated_at=?2 WHERE run_id=?3",
                    params![output, at, run_id.to_string()],
                )?;
            }
            EventKind::SubagentFailed { run_id, error } => {
                conn.execute(
                    "UPDATE subagent_runs SET status='failed', output=NULL, error=?1, updated_at=?2 WHERE run_id=?3",
                    params![error, at, run_id.to_string()],
                )?;
            }
            EventKind::CostUpdated {
                input_tokens,
                output_tokens,
                estimated_cost_usd,
            } => {
                conn.execute(
                    "INSERT INTO cost_ledger (session_id, input_tokens, output_tokens, estimated_cost_usd, recorded_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event.session_id.to_string(),
                        *input_tokens as i64,
                        *output_tokens as i64,
                        *estimated_cost_usd,
                        at,
                    ],
                )?;
            }
            EventKind::ProfileCaptured {
                profile_id,
                summary,
                elapsed_ms,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO profile_runs (profile_id, session_id, summary, elapsed_ms, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        profile_id.to_string(),
                        event.session_id.to_string(),
                        summary,
                        *elapsed_ms as i64,
                        at,
                    ],
                )?;
            }
            EventKind::MemorySynced {
                version_id,
                path,
                note,
            } => {
                let content = fs::read_to_string(path).unwrap_or_default();
                conn.execute(
                    "INSERT OR REPLACE INTO memory_versions (version_id, path, content, note, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        version_id.to_string(),
                        path,
                        content,
                        note,
                        at,
                    ],
                )?;
            }
            EventKind::BackgroundJobStarted {
                job_id,
                kind,
                reference,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO background_jobs (job_id, kind, reference, status, metadata_json, started_at, updated_at)
                     VALUES (?1, ?2, ?3, 'running', '{}', ?4, ?4)",
                    params![
                        job_id.to_string(),
                        kind,
                        reference,
                        at,
                    ],
                )?;
            }
            EventKind::BackgroundJobResumed { job_id, reference } => {
                conn.execute(
                    "UPDATE background_jobs SET status='running', reference=?1, updated_at=?2 WHERE job_id=?3",
                    params![reference, at, job_id.to_string()],
                )?;
            }
            EventKind::BackgroundJobStopped { job_id, reason } => {
                conn.execute(
                    "UPDATE background_jobs SET status='stopped', metadata_json=?1, updated_at=?2 WHERE job_id=?3",
                    params![
                        serde_json::json!({"reason": reason}).to_string(),
                        at,
                        job_id.to_string()
                    ],
                )?;
            }
            EventKind::SkillLoaded {
                skill_id,
                source_path,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO skill_registry (skill_id, name, path, enabled, metadata_json, updated_at)
                     VALUES (?1, ?2, ?3, 1, '{}', ?4)",
                    params![skill_id, skill_id, source_path, at],
                )?;
            }
            EventKind::ReplayExecuted {
                session_id,
                deterministic,
                events_replayed,
            } => {
                conn.execute(
                    "INSERT INTO replay_cassettes (cassette_id, session_id, deterministic, events_count, payload_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        Uuid::now_v7().to_string(),
                        session_id.to_string(),
                        if *deterministic { 1 } else { 0 },
                        *events_replayed as i64,
                        "{}",
                        at,
                    ],
                )?;
            }
            EventKind::PromptCacheHit { cache_key, model } => {
                conn.execute(
                    "INSERT INTO provider_metrics (provider, model, cache_key, cache_hit, latency_ms, recorded_at)
                     VALUES ('deepseek', ?1, ?2, 1, 0, ?3)",
                    params![model, cache_key, at],
                )?;
            }
            EventKind::OffPeakScheduled {
                reason,
                resume_after,
            } => {
                conn.execute(
                    "INSERT INTO provider_metrics (provider, model, cache_key, cache_hit, latency_ms, recorded_at)
                     VALUES ('scheduler', 'off_peak', ?1, 0, 0, ?2)",
                    params![format!("{reason}:{resume_after}"), at],
                )?;
            }
            EventKind::VisualArtifactCaptured {
                artifact_id,
                path,
                mime,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO visual_artifacts (artifact_id, path, mime, metadata_json, created_at)
                     VALUES (?1, ?2, ?3, '{}', ?4)",
                    params![
                        artifact_id.to_string(),
                        path,
                        mime,
                        at,
                    ],
                )?;
            }
            EventKind::RemoteEnvConfigured {
                profile_id,
                name,
                endpoint,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO remote_env_profiles (profile_id, name, endpoint, auth_mode, metadata_json, updated_at)
                     VALUES (?1, ?2, ?3, 'token', '{}', ?4)",
                    params![
                        profile_id.to_string(),
                        name,
                        endpoint,
                        at,
                    ],
                )?;
            }
            EventKind::RemoteEnvExecutionStarted { .. }
            | EventKind::RemoteEnvExecutionCompleted { .. } => {}
            EventKind::TeleportBundleCreated { bundle_id, path } => {
                conn.execute(
                    "INSERT INTO replay_cassettes (cassette_id, session_id, deterministic, events_count, payload_json, created_at)
                     VALUES (?1, ?2, 1, 0, ?3, ?4)",
                    params![
                        bundle_id.to_string(),
                        event.session_id.to_string(),
                        serde_json::json!({"path": path}).to_string(),
                        at,
                    ],
                )?;
            }
            EventKind::TeleportHandoffLinkCreated { .. }
            | EventKind::TeleportHandoffLinkConsumed { .. } => {}
            EventKind::PermissionModeChanged { from, to } => {
                conn.execute(
                    "INSERT INTO permission_mode_log (session_id, from_mode, to_mode, changed_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![event.session_id.to_string(), from, to, at,],
                )?;
            }
            EventKind::TaskCreated {
                task_id,
                title,
                priority,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO task_queue (task_id, session_id, title, priority, status, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5)",
                    params![
                        task_id.to_string(),
                        event.session_id.to_string(),
                        title,
                        *priority as i64,
                        at,
                    ],
                )?;
            }
            EventKind::TaskCompleted { task_id, outcome } => {
                conn.execute(
                    "UPDATE task_queue SET status = 'completed', outcome = ?1, updated_at = ?2 WHERE task_id = ?3",
                    params![outcome, at, task_id.to_string()],
                )?;
            }
            EventKind::TaskUpdated { task_id, status } => {
                conn.execute(
                    "UPDATE task_queue SET status = ?1, updated_at = ?2 WHERE task_id = ?3",
                    params![status, at, task_id],
                )?;
            }
            EventKind::TaskDeleted { task_id } => {
                conn.execute("DELETE FROM task_queue WHERE task_id = ?1", [task_id])?;
            }
            EventKind::EnterPlanMode { .. } => {
                conn.execute(
                    "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE session_id = ?3",
                    params![
                        serde_json::to_string(&SessionState::Planning)?,
                        at,
                        event.session_id.to_string(),
                    ],
                )?;
            }
            EventKind::ExitPlanMode { .. } => {
                conn.execute(
                    "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE session_id = ?3",
                    params![
                        serde_json::to_string(&SessionState::AwaitingApproval)?,
                        at,
                        event.session_id.to_string(),
                    ],
                )?;
            }
            EventKind::ReviewStarted {
                review_id,
                preset,
                target,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO review_runs (review_id, session_id, preset, target, findings_json, findings_count, critical_count, created_at)
                     VALUES (?1, ?2, ?3, ?4, '[]', 0, 0, ?5)",
                    params![
                        review_id.to_string(),
                        event.session_id.to_string(),
                        preset,
                        target,
                        at,
                    ],
                )?;
            }
            EventKind::ReviewCompleted {
                review_id,
                findings_count,
                critical_count,
            } => {
                conn.execute(
                    "UPDATE review_runs SET findings_count = ?1, critical_count = ?2 WHERE review_id = ?3",
                    params![
                        *findings_count as i64,
                        *critical_count as i64,
                        review_id.to_string()
                    ],
                )?;
            }
            EventKind::ReviewPublished { .. } => {}
            EventKind::ArtifactBundled {
                task_id,
                artifact_path,
                files,
            } => {
                conn.execute(
                    "INSERT OR REPLACE INTO artifacts (artifact_id, task_id, artifact_path, files_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        Uuid::now_v7().to_string(),
                        task_id.to_string(),
                        artifact_path,
                        serde_json::to_string(files)?,
                        at,
                    ],
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    // ── Session garbage collection ────────────────────────────────────────
}
