//! Tool result cache for read-only tools.
//!
//! Caches results of `fs_read`, `fs_grep`, `fs_glob`, `fs_list` to avoid
//! redundant file I/O. Entries expire after a TTL and are invalidated when
//! write tools modify cached paths.

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
    /// The raw `"tool_name:args"` string used for path-based invalidation.
    pub raw_key: String,
}

/// Tool result cache with TTL, path invalidation, and size limits.
pub(crate) struct ToolCache {
    entries: HashMap<String, CacheEntry>,
}

impl ToolCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Build the cache key and raw string from a tool call.
    pub fn key_with_raw(tool_name: &str, args: &serde_json::Value) -> (String, String) {
        use sha2::{Digest, Sha256};
        let raw = format!("{}:{}", tool_name, args);
        let hash = Sha256::digest(raw.as_bytes());
        let bytes: [u8; 8] = hash[..8].try_into().expect("SHA-256 always >= 8 bytes");
        (format!("{:016x}", u64::from_be_bytes(bytes)), raw)
    }

    pub fn key(tool_name: &str, args: &serde_json::Value) -> String {
        Self::key_with_raw(tool_name, args).0
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
        let (key, raw) = Self::key_with_raw(tool_name, args);
        self.entries.insert(
            key,
            CacheEntry {
                result: result.clone(),
                timestamp: Instant::now(),
                raw_key: raw,
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

    /// Invalidate entries when a write tool modifies a path.
    pub fn invalidate_path(&mut self, path: &str) {
        let quoted = format!("\"{path}\"");
        let parent_quoted = path
            .rsplit_once('/')
            .map(|(parent, _)| format!("\"{parent}\""));

        self.entries.retain(|_key, entry| {
            if entry.raw_key.starts_with("fs_glob:")
                || entry.raw_key.starts_with("fs_list:")
                || entry.raw_key.starts_with("index_query:")
            {
                return false;
            }
            if entry.raw_key.contains(&quoted) {
                return false;
            }
            if let Some(ref pq) = parent_quoted
                && entry.raw_key.contains(pq.as_str())
            {
                return false;
            }
            true
        });
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
