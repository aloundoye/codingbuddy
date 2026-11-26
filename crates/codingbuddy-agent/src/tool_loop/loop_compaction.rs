//! Context pressure handling for the tool loop: pruning old tool output and compacting the conversation.

use super::*;

impl ToolUseLoop<'_> {
    /// Phase 1: Lightweight pruning of old tool outputs.
    ///
    /// Truncates verbose tool results that are older than `PRUNE_AGE_TURNS` turn groups
    /// from the end. Keeps the last write result per file path and all recent results.
    /// This preserves conversation structure while reducing token usage.
    pub(crate) fn prune_old_tool_outputs(&mut self) {
        self.truncate_old_tool_outputs(200, 200);
    }

    /// Trim large old tool outputs once the conversation has been idle long
    /// enough for the provider's prompt cache to expire, whatever the context
    /// size. Rewriting old messages then costs no cache hits, and the next
    /// request is smaller and cheaper.
    pub(crate) fn prune_after_idle(&mut self) {
        let Some(last) = self.last_model_response_at else {
            return;
        };
        if last.elapsed() < IDLE_PRUNE_AFTER {
            return;
        }
        // Once per pause: the next response restarts the clock.
        self.last_model_response_at = None;
        let before = estimate_message_tokens(&self.messages);
        let trimmed = self.truncate_old_tool_outputs(IDLE_PRUNE_MIN_CHARS, IDLE_PRUNE_KEEP_CHARS);
        if trimmed == 0 {
            return;
        }
        let after = estimate_message_tokens(&self.messages);
        tracing::info!(
            trimmed,
            pre_tokens = before,
            post_tokens = after,
            "trimmed old tool outputs after idle pause"
        );
        let messages = self.messages.len() as u64;
        self.emit_event_if_present(EventKind::CompactionTriggered {
            phase: "idle_prune".to_string(),
            pre_tokens: before,
            post_tokens: after,
            messages_before: messages,
            messages_after: messages,
        });
    }

    /// Truncate tool results older than `PRUNE_AGE_TURNS` turn groups that are
    /// longer than `min_chars`, keeping their first `keep_chars` characters.
    /// Results about files used in recent turns are kept whole. Returns how
    /// many results were truncated.
    fn truncate_old_tool_outputs(&mut self, min_chars: usize, keep_chars: usize) -> usize {
        // Count turn groups from the end (a turn group starts at each User message)
        let mut turn_count = 0;
        let mut recent_boundary = self.messages.len();
        for (i, msg) in self.messages.iter().enumerate().rev() {
            if matches!(msg, ChatMessage::User { .. }) {
                turn_count += 1;
                if turn_count >= PRUNE_AGE_TURNS {
                    recent_boundary = i;
                    break;
                }
            }
        }

        if recent_boundary <= 1 {
            return 0; // Not enough history to prune
        }

        // Track file paths from recent tool results — keep those untouched
        let mut recent_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
        for msg in self.messages[recent_boundary..].iter().rev() {
            if let ChatMessage::Tool { content, .. } = msg
                && let Some(path) = extract_tool_path(content)
            {
                recent_paths.insert(path);
            }
        }

        // Truncate old tool results in the "old" zone (messages[1..recent_boundary])
        const TRUNCATED_MARKER: &str = "[output pruned — re-read file if needed]";
        let mut truncated_count = 0;

        for msg in &mut self.messages[1..recent_boundary] {
            if let ChatMessage::Tool { content, .. } = msg {
                // Skip if already truncated or short
                if content.len() <= min_chars.max(keep_chars) || content.contains(TRUNCATED_MARKER)
                {
                    continue;
                }
                // Keep results referencing files still active in recent turns
                if let Some(path) = extract_tool_path(content)
                    && recent_paths.contains(&path)
                {
                    continue;
                }
                // Keep the head + marker (safe on multi-byte UTF-8)
                let safe_end = content.floor_char_boundary(keep_chars.min(content.len()));
                *content = format!("{}\n{}", &content[..safe_end], TRUNCATED_MARKER);
                truncated_count += 1;
            }
        }
        truncated_count
    }

    /// Where this session's memory is saved, when the loop knows its
    /// workspace and session.
    fn session_memory_path(&self) -> Option<std::path::PathBuf> {
        let workspace = self.config.workspace.as_ref()?;
        let session = self
            .config
            .session_id
            .or_else(|| self.durable_session.as_ref().map(|(_, id)| *id))?;
        Some(
            codingbuddy_core::runtime_dir(workspace)
                .join("session_memory")
                .join(format!("{session}.md")),
        )
    }

    /// Add this compaction's decisions to session memory, save it, and
    /// return the text to show the model (None when there is nothing yet).
    /// Takes the fields it needs so it can run while the messages are borrowed.
    fn update_session_memory(
        session_memory: &mut Option<Vec<String>>,
        path: Option<std::path::PathBuf>,
        summary: &str,
    ) -> Option<String> {
        let memory = session_memory.get_or_insert_with(|| {
            // First compaction in this process: pick up what an earlier run
            // of the same session saved.
            path.as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map(|text| parse_session_memory(&text))
                .unwrap_or_default()
        });
        let changed = merge_session_memory(memory, session_memory_entries(summary));
        if memory.is_empty() {
            return None;
        }
        let text = render_session_memory(memory);
        if changed && let Some(path) = path {
            let saved = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, &text));
            if let Err(error) = saved {
                tracing::warn!(%error, path = %path.display(), "could not save session memory");
            }
        }
        Some(text)
    }

    /// Compact messages by removing middle exchanges while preserving tool-call/result pairing.
    ///
    /// Walks backward from the end, keeping complete exchange groups (User + Assistant + Tool
    /// messages form a group). Never orphans a Tool message from its corresponding Assistant
    /// tool_calls. Respects `target_tokens` budget for kept messages.
    pub(crate) async fn compact_messages(&mut self, target_tokens: u64) -> bool {
        if self.messages.len() <= 7 {
            return false;
        }

        let system = self.messages[0].clone();

        // Walk backward to find group boundaries.
        // A group starts at a User message and includes everything until the next User message.
        let mut keep_from = self.messages.len();
        let mut kept_tokens: u64 = 0;
        let target = target_tokens;

        while keep_from > 1 {
            // Find the start of this group (previous User message)
            let group_start = self.messages[1..keep_from]
                .iter()
                .rposition(|m| matches!(m, ChatMessage::User { .. }))
                .map(|i| i + 1) // +1 for the offset from [1..]
                .unwrap_or(1);

            let group_tokens: u64 = self.messages[group_start..keep_from]
                .iter()
                .map(|m| estimate_message_tokens(std::slice::from_ref(m)))
                .sum();

            if kept_tokens + group_tokens > target && keep_from < self.messages.len() {
                break; // This group would exceed budget
            }
            kept_tokens += group_tokens;
            keep_from = group_start;
        }

        if keep_from <= 1 {
            return false; // Nothing to compact
        }

        let middle_count = keep_from - 1;
        let compacted_msgs = &self.messages[1..keep_from];
        // Try LLM-based compaction first, fall back to code-based extraction on any error.
        let summary = match build_compaction_summary_with_llm(self.llm, compacted_msgs).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "LLM compaction failed; falling back to code-based extraction"
                );
                build_compaction_summary(compacted_msgs)
            }
        };

        // P2.7: Validate summary before applying compaction.
        // Reject empty or trivially short summaries that would lose context.
        if summary.trim().is_empty() || summary.trim().len() < 50 {
            tracing::warn!(
                chars = summary.trim().len(),
                "rejecting compaction: summary too short"
            );
            return false;
        }

        // Fire PreCompact hook if configured
        if let Some(ref hooks) = self.hooks {
            let mut input = HookInput::new(HookEvent::PreCompact, self.workspace_str());
            input.tool_result = Some(serde_json::Value::String(summary.clone()));
            self.fire_hook_logged(hooks, HookEvent::PreCompact, &input)
                .await;
        }

        // Scan compacted messages for any directives we haven't captured yet
        for msg in compacted_msgs {
            if let ChatMessage::User { content } = msg {
                for directive in extract_user_directives(content) {
                    if !self.pinned_directives.contains(&directive) {
                        self.pinned_directives.push(directive);
                    }
                }
            }
        }
        self.pinned_directives.truncate(20);

        // Auto-extract memory observations from compacted messages.
        // These capture user corrections and decisions that should persist.
        let auto_memories = extract_memory_observations(compacted_msgs);
        if !auto_memories.is_empty()
            && let Some(ref cb) = self.event_cb
        {
            cb(EventKind::MemoryObservations {
                observations: auto_memories,
            });
        }

        // Build the new message list in a local vec, validate, then swap.
        // This avoids cloning the entire (potentially large) messages vec for rollback.
        let summary_msg = ChatMessage::User {
            content: format!(
                "CONVERSATION_HISTORY (compacted from {middle_count} messages):\n{summary}"
            ),
        };

        let mut new_messages = vec![system, summary_msg];

        // Session memory: decisions from every compaction so far, so a
        // correction made long ago survives repeated summarising.
        let memory_path = self.session_memory_path();
        if let Some(memory) =
            Self::update_session_memory(&mut self.session_memory, memory_path, &summary)
        {
            new_messages.push(ChatMessage::System { content: memory });
        }

        if let Some(work_state_message) = self.compaction_work_state_message() {
            new_messages.push(ChatMessage::System {
                content: work_state_message,
            });
        }

        // Re-inject pinned user directives so they survive compaction.
        // Placed as a System message right after the summary so the model
        // always sees them regardless of how many compaction rounds occur.
        if !self.pinned_directives.is_empty() {
            let directives_text = self
                .pinned_directives
                .iter()
                .map(|d| format!("- {d}"))
                .collect::<Vec<_>>()
                .join("\n");
            new_messages.push(ChatMessage::System {
                content: format!(
                    "USER DIRECTIVES (must follow throughout this conversation):\n{directives_text}"
                ),
            });
        }

        new_messages.extend(self.messages[keep_from..].to_vec());
        self.append_compaction_followup(&mut new_messages, compacted_msgs, middle_count);

        // P2.7: Post-compaction validation — ensure the result is structurally sound.
        // Must have at least 2 messages and start with a System message.
        if new_messages.len() < 2 || !matches!(&new_messages[0], ChatMessage::System { .. }) {
            tracing::warn!(
                messages = new_messages.len(),
                "post-compaction validation failed, discarding"
            );
            return false;
        }

        self.messages = new_messages;

        // Re-inject recently active files so the model retains awareness
        if let Some(file_context) = self.build_post_compact_file_context(5) {
            self.messages.push(ChatMessage::System {
                content: file_context,
            });
        }

        // Fire PostCompact hook if configured
        if let Some(ref hooks) = self.hooks {
            let input = HookInput::new(HookEvent::PostCompact, self.workspace_str());
            self.fire_hook_logged(hooks, HookEvent::PostCompact, &input)
                .await;
        }

        true
    }
}
