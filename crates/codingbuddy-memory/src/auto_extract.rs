use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// A single extracted memory observation from a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedMemory {
    pub category: MemoryCategory,
    pub content: String,
    pub source: String,
    pub extracted_at: String,
}

/// Categories for auto-extracted memories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    Correction,
    Preference,
    Decision,
    Convention,
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Correction => write!(f, "correction"),
            Self::Preference => write!(f, "preference"),
            Self::Decision => write!(f, "decision"),
            Self::Convention => write!(f, "convention"),
        }
    }
}

/// Extract actionable memories from a compaction summary.
pub fn extract_from_summary(summary: &str, session_label: &str) -> Vec<ExtractedMemory> {
    let mut memories = Vec::new();
    let now = Utc::now().to_rfc3339();

    for line in summary.lines() {
        let trimmed = line.trim().trim_start_matches("- ").trim();
        if trimmed.is_empty() {
            continue;
        }

        let lower = trimmed.to_lowercase();
        if let Some(mem) = detect_by_triggers(trimmed, &lower, session_label, &now) {
            memories.push(mem);
        }
    }

    memories
}

/// Persist extracted memories to disk, deduplicating by content hash
/// so identical insights from different sessions aren't saved twice.
pub fn persist_memories(workspace: &Path, memories: &[ExtractedMemory]) -> Result<usize> {
    if memories.is_empty() {
        return Ok(0);
    }

    let dir = workspace.join(".codingbuddy/memory/auto");
    fs::create_dir_all(&dir)?;

    let mut written = 0;
    for mem in memories {
        let hash = content_hash(&mem.content);
        let filename = format!("{}_{}.md", mem.category, &hash[..8]);
        let path = dir.join(&filename);

        if path.exists() {
            continue;
        }

        let frontmatter = format!(
            "---\ncategory: {}\nsource: {}\nextracted_at: {}\n---\n\n{}",
            mem.category, mem.source, mem.extracted_at, mem.content
        );
        fs::write(&path, frontmatter)?;
        written += 1;
    }

    Ok(written)
}

/// Load all auto-extracted memories from the memory directory.
pub fn load_auto_memories(workspace: &Path) -> Vec<ExtractedMemory> {
    let dir = workspace.join(".codingbuddy/memory/auto");
    let mut memories = Vec::new();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return memories,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Ok(content) = fs::read_to_string(&path)
            && let Some(mem) = parse_memory_file(&content)
        {
            memories.push(mem);
        }
    }

    memories
}

/// Format auto-extracted memories for injection into LLM context.
pub fn format_for_context(memories: &[ExtractedMemory]) -> String {
    if memories.is_empty() {
        return String::new();
    }

    let mut sections: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for mem in memories {
        let key = match mem.category {
            MemoryCategory::Correction => "Corrections",
            MemoryCategory::Preference => "Preferences",
            MemoryCategory::Decision => "Decisions",
            MemoryCategory::Convention => "Conventions",
        };
        sections.entry(key).or_default().push(&mem.content);
    }

    let mut out = String::from("## Auto-extracted memories\n\n");
    for (section, items) in &sections {
        out.push_str(&format!("### {section}\n"));
        for item in items {
            out.push_str(&format!("- {item}\n"));
        }
        out.push('\n');
    }
    out
}

/// Return the memory directory path for auto-extracted memories.
pub fn auto_memory_dir(workspace: &Path) -> PathBuf {
    workspace.join(".codingbuddy/memory/auto")
}

// ── Detection heuristics ──

const CATEGORY_TRIGGERS: &[(MemoryCategory, &[&str])] = &[
    (
        MemoryCategory::Correction,
        &[
            "don't ",
            "do not ",
            "stop ",
            "never ",
            "wrong ",
            "incorrect ",
            "that's not ",
            "that is not ",
            "should not ",
            "shouldn't ",
            "no, ",
            "actually, ",
            "correction:",
            "fix: ",
        ],
    ),
    (
        MemoryCategory::Preference,
        &[
            "prefer ",
            "i like ",
            "i want ",
            "always use ",
            "use ... instead",
            "my preference ",
            "i'd rather ",
            "please use ",
            "default to ",
        ],
    ),
    (
        MemoryCategory::Decision,
        &[
            "decided to ",
            "we decided ",
            "the plan is ",
            "going with ",
            "chose ",
            "selected ",
            "agreed to ",
            "decision:",
            "approach: ",
        ],
    ),
    (
        MemoryCategory::Convention,
        &[
            "convention:",
            "naming: ",
            "style: ",
            "pattern: ",
            "always run ",
            "before committing",
            "after editing",
            "code style ",
        ],
    ),
];

fn detect_by_triggers(line: &str, lower: &str, source: &str, ts: &str) -> Option<ExtractedMemory> {
    for (category, triggers) in CATEGORY_TRIGGERS {
        if triggers.iter().any(|t| lower.contains(t)) {
            return Some(ExtractedMemory {
                category: category.clone(),
                content: line.to_string(),
                source: source.to_string(),
                extracted_at: ts.to_string(),
            });
        }
    }
    None
}

fn content_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

fn parse_memory_file(content: &str) -> Option<ExtractedMemory> {
    let body = content.strip_prefix("---\n")?;
    let (frontmatter, rest) = body.split_once("\n---\n")?;
    let text = rest.trim().to_string();

    let mut category = None;
    let mut source = String::new();
    let mut extracted_at = String::new();

    for line in frontmatter.lines() {
        if let Some(val) = line.strip_prefix("category: ") {
            category = match val.trim() {
                "correction" => Some(MemoryCategory::Correction),
                "preference" => Some(MemoryCategory::Preference),
                "decision" => Some(MemoryCategory::Decision),
                "convention" => Some(MemoryCategory::Convention),
                _ => None,
            };
        } else if let Some(val) = line.strip_prefix("source: ") {
            source = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("extracted_at: ") {
            extracted_at = val.trim().to_string();
        }
    }

    Some(ExtractedMemory {
        category: category?,
        content: text,
        source,
        extracted_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_corrections() {
        let summary = "- User said: don't use unwrap in production code\n- Fixed the login flow";
        let mems = extract_from_summary(summary, "session-1");
        assert_eq!(mems.len(), 1);
        assert_eq!(mems[0].category, MemoryCategory::Correction);
        assert!(mems[0].content.contains("don't use unwrap"));
    }

    #[test]
    fn extracts_preferences() {
        let summary = "- User: I prefer snake_case for variables\n- Completed task";
        let mems = extract_from_summary(summary, "session-2");
        assert_eq!(mems.len(), 1);
        assert_eq!(mems[0].category, MemoryCategory::Preference);
    }

    #[test]
    fn extracts_decisions() {
        let summary = "- We decided to use SQLite for persistence\n- Tests pass";
        let mems = extract_from_summary(summary, "session-3");
        assert_eq!(mems.len(), 1);
        assert_eq!(mems[0].category, MemoryCategory::Decision);
    }

    #[test]
    fn empty_summary_produces_no_memories() {
        let mems = extract_from_summary("", "session-4");
        assert!(mems.is_empty());
    }

    #[test]
    fn format_for_context_groups_by_category() {
        let mems = vec![
            ExtractedMemory {
                category: MemoryCategory::Correction,
                content: "Don't use unwrap".to_string(),
                source: "s1".to_string(),
                extracted_at: "2024-01-01T00:00:00Z".to_string(),
            },
            ExtractedMemory {
                category: MemoryCategory::Preference,
                content: "Use snake_case".to_string(),
                source: "s1".to_string(),
                extracted_at: "2024-01-01T00:00:00Z".to_string(),
            },
        ];
        let formatted = format_for_context(&mems);
        assert!(formatted.contains("### Corrections"));
        assert!(formatted.contains("### Preferences"));
        assert!(formatted.contains("- Don't use unwrap"));
        assert!(formatted.contains("- Use snake_case"));
    }

    #[test]
    fn persist_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();

        let mems = vec![ExtractedMemory {
            category: MemoryCategory::Decision,
            content: "Use async/await".to_string(),
            source: "test".to_string(),
            extracted_at: "2024-01-01T00:00:00Z".to_string(),
        }];

        let written = persist_memories(workspace, &mems).unwrap();
        assert_eq!(written, 1);

        let loaded = load_auto_memories(workspace);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].category, MemoryCategory::Decision);
        assert_eq!(loaded[0].content, "Use async/await");

        // Deduplication: same content doesn't write again
        let written2 = persist_memories(workspace, &mems).unwrap();
        assert_eq!(written2, 0);
    }
}
