//! Tool result cache for read-only tools.
//!
//! Caches results of `fs_read`, `fs_grep`, `fs_glob`, `fs_list` to avoid
//! redundant file I/O. Entries expire after a TTL and are invalidated when
//! write tools may have changed the workspace.

use std::collections::HashMap;
use std::time::Instant;

/// TTL for most cached tool results.
pub(crate) const TOOL_CACHE_TTL_SECS: u64 = 60;

/// Extended TTL for `fs_read` (files don't change as fast as search results).
pub(crate) const TOOL_CACHE_TTL_READ_SECS: u64 = 120;

/// Maximum number of cached entries before eviction.
pub(crate) const MAX_CACHE_ENTRIES: usize = 128;

/// Tools whose results are eligible for caching.
pub(crate) const CACHEABLE_TOOLS: &[&str] =
    &["fs_read", "fs_grep", "fs_glob", "fs_list", "index_query"];

/// A cached tool result with metadata.
pub(crate) struct CacheEntry {
    pub result: serde_json::Value,
    pub timestamp: Instant,
}

/// Tool result cache with TTL and size limits.
pub(crate) struct ToolCache {
    entries: HashMap<String, CacheEntry>,
}

impl ToolCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn key(tool_name: &str, args: &serde_json::Value) -> String {
        format!("{tool_name}:{args}")
    }

    /// Check for a cached result. Returns `None` if expired or not found.
    pub fn lookup(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        if !CACHEABLE_TOOLS.contains(&tool_name) {
            return None;
        }
        let key = Self::key(tool_name, args);
        let ttl = if tool_name == "fs_read" {
            TOOL_CACHE_TTL_READ_SECS
        } else {
            TOOL_CACHE_TTL_SECS
        };
        if let Some(entry) = self.entries.get(&key)
            && entry.timestamp.elapsed().as_secs() < ttl
        {
            return Some(entry.result.clone());
        }
        self.entries.remove(&key);
        None
    }

    /// Store a result. Evicts the oldest entry when full.
    pub fn store(&mut self, tool_name: &str, args: &serde_json::Value, result: &serde_json::Value) {
        if !CACHEABLE_TOOLS.contains(&tool_name) {
            return;
        }
        let key = Self::key(tool_name, args);
        self.entries.insert(
            key,
            CacheEntry {
                result: result.clone(),
                timestamp: Instant::now(),
            },
        );
        if self.entries.len() > MAX_CACHE_ENTRIES
            && let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.timestamp)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest_key);
        }
    }

    /// Invalidate reads after a tool may have modified the workspace.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of cached entries.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Default for ToolCache {
    fn default() -> Self {
        Self::new()
    }
}
