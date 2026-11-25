//! Context analysis RPC methods: suggest, analyze, compress, related files and debug.

use super::*;

impl IdeRpcHandler {
    pub(crate) fn handle_context_suggest(&self, params: Value) -> Result<Value> {
        let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        let context_manager = self.context_manager.lock().expect("context_manager lock");

        if let Some(manager) = context_manager.as_ref() {
            let suggestions = manager.suggest_relevant_files(query, limit);
            let suggestions_json: Vec<Value> = suggestions
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "path": s.path.to_string_lossy(),
                        "score": s.score,
                        "reasons": s.reasons,
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "suggestions": suggestions_json,
                "count": suggestions.len(),
                "query": query,
            }))
        } else {
            Ok(serde_json::json!({
                "suggestions": [],
                "count": 0,
                "query": query,
                "note": "Context analysis unavailable",
            }))
        }
    }

    pub(crate) fn handle_context_analyze(&self, _params: Value) -> Result<Value> {
        let mut context_manager = self.context_manager.lock().expect("context_manager lock");

        if let Some(manager) = context_manager.as_mut() {
            match manager.analyze_workspace() {
                Ok(()) => Ok(serde_json::json!({
                    "status": "analyzed",
                    "file_count": manager.file_count(),
                })),
                Err(e) => Ok(serde_json::json!({
                    "status": "error",
                    "error": e.to_string(),
                })),
            }
        } else {
            Ok(serde_json::json!({
                "status": "unavailable",
                "note": "Context manager not initialized",
            }))
        }
    }

    pub(crate) fn handle_context_compress(&self, params: Value) -> Result<Value> {
        let context = params.get("context").and_then(|v| v.as_str()).unwrap_or("");
        let max_tokens = params
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(4000) as usize;

        let context_manager = self.context_manager.lock().expect("context_manager lock");

        if let Some(manager) = context_manager.as_ref() {
            let compressed = manager.compress_context(context, max_tokens);
            let original_lines = context.lines().count();
            let compressed_lines = compressed.lines().count();

            Ok(serde_json::json!({
                "compressed": compressed,
                "original_lines": original_lines,
                "compressed_lines": compressed_lines,
                "compression_ratio": if original_lines > 0 {
                    (compressed_lines as f64) / (original_lines as f64)
                } else {
                    1.0
                },
            }))
        } else {
            // Fallback simple compression
            let lines: Vec<&str> = context.lines().collect();
            let total_lines = lines.len();
            let keep_lines = (max_tokens / 10).min(total_lines);

            let compressed = if total_lines > keep_lines {
                let start = keep_lines / 2;
                let end = keep_lines - start;
                let mut result: Vec<&str> = lines.iter().take(start).copied().collect();
                result.push("// ... context truncated ...");
                result.extend(lines.iter().rev().take(end).rev());
                result.join("\n")
            } else {
                context.to_string()
            };

            Ok(serde_json::json!({
                "compressed": compressed,
                "original_lines": total_lines,
                "compressed_lines": compressed.lines().count(),
                "compression_ratio": if total_lines > 0 {
                    (compressed.lines().count() as f64) / (total_lines as f64)
                } else {
                    1.0
                },
                "note": "Using fallback compression",
            }))
        }
    }

    pub(crate) fn handle_context_related(&self, params: Value) -> Result<Value> {
        let file_path = params
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'file_path' parameter"))?;
        let depth = params.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize;

        let path = Path::new(file_path);
        let context_manager = self.context_manager.lock().expect("context_manager lock");

        if let Some(manager) = context_manager.as_ref() {
            let related = manager.get_related_files(path, depth);
            let related_json: Vec<Value> = related
                .iter()
                .map(|p| serde_json::json!(p.to_string_lossy()))
                .collect();

            Ok(serde_json::json!({
                "file_path": file_path,
                "depth": depth,
                "related_files": related_json,
                "count": related.len(),
            }))
        } else {
            Ok(serde_json::json!({
                "file_path": file_path,
                "depth": depth,
                "related_files": [],
                "count": 0,
                "note": "Context analysis unavailable",
            }))
        }
    }

    pub(crate) fn handle_context_debug(&self, params: Value) -> Result<Value> {
        let session_id = require_uuid(&params, "session_id")?;
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing 'prompt' parameter"))?
            .to_string();

        let session = self
            .store
            .load_session(session_id)?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let workspace_root = PathBuf::from(session.workspace_root);

        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .map(parse_chat_mode)
            .unwrap_or(ChatMode::Code);
        let tools = params
            .get("tools")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let repo_root_override = params
            .get("repo_root")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let engine = AgentEngine::new(&workspace_root)?;
        let digest = engine.context_debug_preview(
            &prompt,
            &ChatOptions {
                tools,
                mode,
                repo_root_override,
                debug_context: true,
                ..Default::default()
            },
        );

        Ok(serde_json::json!({
            "schema": "deepseek.context.debug.v1",
            "session_id": session_id.to_string(),
            "digest": digest,
        }))
    }

    // -- Task methods --
}
