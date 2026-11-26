//! Context pressure handling for the tool loop: pruning old tool output and compacting the conversation.

use super::*;

impl ToolUseLoop<'_> {
    /// Phase 1: Lightweight pruning of old tool outputs.
    ///
    /// Truncates verbose tool results that are older than `PRUNE_AGE_TURNS` turn groups
    /// from the end. Keeps the last write result per file path and all recent results.
    /// This preserves conversation structure while reducing token usage.
    pub(crate) fn prune_old_tool_outputs(&mut self) {
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
            return; // Not enough history to prune
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
        let max_kept_chars = 200;

        for msg in &mut self.messages[1..recent_boundary] {
            if let ChatMessage::Tool { content, .. } = msg {
                // Skip if already truncated or short
                if content.len() <= max_kept_chars || content.contains(TRUNCATED_MARKER) {
                    continue;
                }
                // Keep results referencing files still active in recent turns
                if let Some(path) = extract_tool_path(content)
                    && recent_paths.contains(&path)
                {
                    continue;
                }
                // Truncate: keep first 200 chars + marker (safe on multi-byte UTF-8)
                let safe_end = content.floor_char_boundary(max_kept_chars.min(content.len()));
                let truncated = format!("{}\n{}", &content[..safe_end], TRUNCATED_MARKER,);
                *content = truncated;
            }
        }
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
                eprintln!(
                    "[compact] LLM compaction failed: {e:#}; falling back to code-based extraction"
                );
                build_compaction_summary(compacted_msgs)
            }
        };

        // P2.7: Validate summary before applying compaction.
        // Reject empty or trivially short summaries that would lose context.
        if summary.trim().is_empty() || summary.trim().len() < 50 {
            eprintln!(
                "[compact] rejecting compaction: summary too short ({} chars)",
                summary.trim().len()
            );
            return false;
        }

        // Fire PreCompact hook if configured
        if let Some(ref hooks) = self.hooks {
            let mut input = HookInput::new(HookEvent::PreCompact, self.workspace_str());
            input.tool_result = Some(serde_json::Value::String(summary.clone()));
            self.fire_hook_logged(hooks, HookEvent::PreCompact, &input);
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
            eprintln!(
                "[compact] post-compaction validation failed: {} messages, discarding",
                new_messages.len()
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
            self.fire_hook_logged(hooks, HookEvent::PostCompact, &input);
        }

        true
    }
}
