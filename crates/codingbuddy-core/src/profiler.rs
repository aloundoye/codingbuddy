//! Startup profiling for CodingBuddy.
//!
//! Enabled via `CODINGBUDDY_STARTUP_TRACE=1`. Tracks labeled checkpoints
//! and prints elapsed times to stderr on drop.

use std::sync::OnceLock;
use std::time::Instant;

/// Whether startup tracing is enabled.
static ENABLED: OnceLock<bool> = OnceLock::new();

fn is_enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("CODINGBUDDY_STARTUP_TRACE").is_ok())
}

/// Lightweight startup profiler. Create at process entry, call `mark()` at
/// each checkpoint. Prints all timings to stderr on `finish()` or drop.
pub struct StartupProfiler {
    start: Instant,
    checkpoints: Vec<(&'static str, Instant)>,
    enabled: bool,
}

impl StartupProfiler {
    /// Create a new profiler. No-op if `CODINGBUDDY_STARTUP_TRACE` is not set.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            checkpoints: Vec::new(),
            enabled: is_enabled(),
        }
    }

    /// Record a named checkpoint.
    pub fn mark(&mut self, label: &'static str) {
        if self.enabled {
            self.checkpoints.push((label, Instant::now()));
        }
    }

    /// Print all timings and consume the profiler.
    pub fn finish(self) {
        if !self.enabled || self.checkpoints.is_empty() {
            return;
        }
        eprintln!("[startup-trace] Startup timing:");
        let mut prev = self.start;
        for (label, ts) in &self.checkpoints {
            let delta = ts.duration_since(prev);
            let total = ts.duration_since(self.start);
            eprintln!("  {label:<30} +{delta:>8.1?}  (total {total:>8.1?})");
            prev = *ts;
        }
        eprintln!("  {:<30} {:>10.1?}", "TOTAL", self.start.elapsed());
    }
}

impl Default for StartupProfiler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiler_works_when_disabled() {
        let mut p = StartupProfiler {
            start: Instant::now(),
            checkpoints: Vec::new(),
            enabled: false,
        };
        p.mark("test");
        assert!(p.checkpoints.is_empty());
        p.finish(); // should not panic
    }

    #[test]
    fn profiler_collects_when_enabled() {
        let mut p = StartupProfiler {
            start: Instant::now(),
            checkpoints: Vec::new(),
            enabled: true,
        };
        p.mark("step1");
        p.mark("step2");
        assert_eq!(p.checkpoints.len(), 2);
    }
}
