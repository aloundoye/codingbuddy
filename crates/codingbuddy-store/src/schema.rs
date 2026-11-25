//! SQLite schema migrations, applied in order by Store

pub(crate) const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        "CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            seq_no INTEGER NOT NULL,
            at TEXT NOT NULL,
            kind TEXT NOT NULL,
            payload TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sessions (
            session_id TEXT PRIMARY KEY,
            workspace_root TEXT NOT NULL,
            baseline_commit TEXT,
            status TEXT NOT NULL,
            budgets TEXT NOT NULL,
            active_plan_id TEXT,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS plans (
            plan_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            version INTEGER NOT NULL,
            goal TEXT NOT NULL,
            payload TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );",
    ),
    (
        2,
        "CREATE TABLE IF NOT EXISTS plugin_state (
            plugin_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            version TEXT NOT NULL,
            path TEXT NOT NULL,
            enabled INTEGER NOT NULL,
            manifest_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS approvals_ledger (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            invocation_id TEXT NOT NULL,
            approved_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS verification_runs (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            command TEXT NOT NULL,
            success INTEGER NOT NULL,
            output TEXT NOT NULL,
            run_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS router_stats (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            decision_id TEXT NOT NULL,
            selected_model TEXT NOT NULL,
            score REAL NOT NULL,
            reasons TEXT NOT NULL,
            recorded_at TEXT NOT NULL
         );",
    ),
    (
        3,
        "CREATE TABLE IF NOT EXISTS usage_ledger (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            unit TEXT NOT NULL,
            model TEXT NOT NULL,
            input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            recorded_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_usage_ledger_session_time ON usage_ledger(session_id, recorded_at);
         CREATE TABLE IF NOT EXISTS context_compactions (
            summary_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            from_turn INTEGER NOT NULL,
            to_turn INTEGER NOT NULL,
            token_delta_estimate INTEGER NOT NULL,
            replay_pointer TEXT NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS autopilot_runs (
            run_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            prompt TEXT NOT NULL,
            status TEXT NOT NULL,
            stop_reason TEXT,
            completed_iterations INTEGER NOT NULL,
            failed_iterations INTEGER NOT NULL,
            consecutive_failures INTEGER NOT NULL,
            last_error TEXT,
            stop_file TEXT NOT NULL,
            heartbeat_file TEXT NOT NULL,
            tools INTEGER NOT NULL,
            max_think INTEGER NOT NULL,
            started_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS hook_executions (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            phase TEXT NOT NULL,
            hook_path TEXT NOT NULL,
            success INTEGER NOT NULL,
            timed_out INTEGER NOT NULL,
            exit_code INTEGER,
            recorded_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS plugin_catalog_cache (
            plugin_id TEXT NOT NULL,
            name TEXT NOT NULL,
            version TEXT NOT NULL,
            description TEXT NOT NULL,
            source TEXT NOT NULL,
            signature TEXT,
            verified INTEGER NOT NULL,
            metadata_json TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (plugin_id, version)
         );",
    ),
    (
        4,
        "CREATE TABLE IF NOT EXISTS checkpoints (
            checkpoint_id TEXT PRIMARY KEY,
            reason TEXT NOT NULL,
            snapshot_path TEXT NOT NULL,
            files_count INTEGER NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS transcript_exports (
            export_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            format TEXT NOT NULL,
            output_path TEXT NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS mcp_servers (
            server_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            transport TEXT NOT NULL,
            endpoint TEXT NOT NULL,
            enabled INTEGER NOT NULL,
            metadata_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS mcp_tools_cache (
            server_id TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            description TEXT NOT NULL,
            schema_json TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (server_id, tool_name)
         );
         CREATE TABLE IF NOT EXISTS subagent_runs (
            run_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            goal TEXT NOT NULL,
            status TEXT NOT NULL,
            output TEXT,
            error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS cost_ledger (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            estimated_cost_usd REAL NOT NULL,
            recorded_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS profile_runs (
            profile_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            summary TEXT NOT NULL,
            elapsed_ms INTEGER NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS memory_versions (
            version_id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            content TEXT NOT NULL,
            note TEXT NOT NULL,
            created_at TEXT NOT NULL
         );",
    ),
    (
        5,
        "CREATE TABLE IF NOT EXISTS background_jobs (
            job_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            reference TEXT NOT NULL,
            status TEXT NOT NULL,
            metadata_json TEXT NOT NULL,
            started_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS replay_cassettes (
            cassette_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            deterministic INTEGER NOT NULL,
            events_count INTEGER NOT NULL,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS skill_registry (
            skill_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            enabled INTEGER NOT NULL,
            metadata_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS provider_metrics (
            id INTEGER PRIMARY KEY,
            provider TEXT NOT NULL,
            model TEXT NOT NULL,
            cache_key TEXT,
            cache_hit INTEGER NOT NULL,
            latency_ms INTEGER NOT NULL,
            recorded_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS marketplace_catalog (
            plugin_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            version TEXT NOT NULL,
            source TEXT NOT NULL,
            metadata_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS visual_artifacts (
            artifact_id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            mime TEXT NOT NULL,
            metadata_json TEXT NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS remote_env_profiles (
            profile_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            endpoint TEXT NOT NULL,
            auth_mode TEXT NOT NULL,
            metadata_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );",
    ),
    (
        6,
        "CREATE TABLE IF NOT EXISTS task_queue (
            task_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            title TEXT NOT NULL,
            priority INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'pending',
            outcome TEXT,
            artifact_path TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS web_search_cache (
            query_hash TEXT PRIMARY KEY,
            query TEXT NOT NULL,
            results_json TEXT NOT NULL,
            results_count INTEGER NOT NULL,
            cached_at TEXT NOT NULL,
            ttl_seconds INTEGER NOT NULL DEFAULT 900
         );
         CREATE TABLE IF NOT EXISTS session_locks (
            session_id TEXT PRIMARY KEY,
            lock_holder TEXT NOT NULL,
            locked_at TEXT NOT NULL,
            heartbeat_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS review_runs (
            review_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            preset TEXT NOT NULL,
            target TEXT NOT NULL,
            findings_json TEXT NOT NULL,
            findings_count INTEGER NOT NULL,
            critical_count INTEGER NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS artifacts (
            artifact_id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            artifact_path TEXT NOT NULL,
            files_json TEXT NOT NULL,
            created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS permission_mode_log (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            from_mode TEXT NOT NULL,
            to_mode TEXT NOT NULL,
            changed_at TEXT NOT NULL
         );",
    ),
    (
        7,
        "ALTER TABLE usage_ledger ADD COLUMN cache_hit_tokens INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE usage_ledger ADD COLUMN cache_miss_tokens INTEGER NOT NULL DEFAULT 0;",
    ),
    (
        8,
        "CREATE TABLE IF NOT EXISTS runs (
            run_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            status TEXT NOT NULL,
            prompt TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );",
    ),
    (
        9,
        "CREATE TABLE IF NOT EXISTS persistent_approvals (
            id INTEGER PRIMARY KEY,
            tool_name TEXT NOT NULL,
            command_pattern TEXT NOT NULL,
            project_hash TEXT NOT NULL,
            approved_at TEXT NOT NULL,
            UNIQUE(tool_name, command_pattern, project_hash)
         );",
    ),
    (
        10,
        "ALTER TABLE task_queue ADD COLUMN description TEXT;",
    ),
    (
        11,
        "ALTER TABLE subagent_runs ADD COLUMN session_id TEXT;
         ALTER TABLE subagent_runs ADD COLUMN task_id TEXT;
         ALTER TABLE subagent_runs ADD COLUMN child_session_id TEXT;
         ALTER TABLE subagent_runs ADD COLUMN background_job_id TEXT;",
    ),
    (
        12,
        "CREATE TABLE IF NOT EXISTS session_todos (
            todo_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            content TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            position INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_session_todos_session_position
            ON session_todos(session_id, position, created_at);",
    ),
    (
        13,
        "CREATE INDEX IF NOT EXISTS idx_events_session_id ON events(session_id);
         CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at);
         CREATE INDEX IF NOT EXISTS idx_subagent_runs_session ON subagent_runs(session_id);
         CREATE INDEX IF NOT EXISTS idx_cost_ledger_session ON cost_ledger(session_id);",
    ),
    // 14 is taken by EVENT_STORAGE_VERSION, which shares this ledger.
    (
        15,
        "CREATE TABLE IF NOT EXISTS session_titles (
            session_id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );",
    ),
];
