use anyhow::Result;
use chrono::Utc;
use codingbuddy_core::{TelemetryConfig, runtime_dir};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Engine-level diagnostics. Messages go to the process log
/// (`codingbuddy_core::logging`); warnings are also appended to the
/// workspace's `.codingbuddy/observe.log`, which `codingbuddy status` and
/// `codingbuddy clean` still read.
///
/// Nothing is printed to stderr unless `--verbose` is on, because stderr
/// output would corrupt the TUI.
pub struct Observer {
    log_path: PathBuf,
    verbose: bool,
}

impl Observer {
    pub fn new(workspace: &Path, _telemetry_cfg: &TelemetryConfig) -> Result<Self> {
        let dir = runtime_dir(workspace);
        fs::create_dir_all(&dir)?;
        Ok(Self {
            log_path: dir.join("observe.log"),
            verbose: false,
        })
    }

    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    pub fn verbose_log(&self, msg: &str) {
        tracing::debug!(target: "codingbuddy_agent::observe", "{msg}");
        if self.verbose && !codingbuddy_core::logging::terminal_claimed() {
            eprintln!("[codingbuddy] {msg}"); // guard: allow (--verbose, skipped while a UI owns the terminal)
        }
    }

    pub fn warn_log(&self, msg: &str) {
        tracing::warn!(target: "codingbuddy_agent::observe", "{msg}");
        if self.verbose && !codingbuddy_core::logging::terminal_claimed() {
            eprintln!("[codingbuddy WARN] {msg}"); // guard: allow (--verbose, skipped while a UI owns the terminal)
        }
        let _ = self.append_log_line(&format!("{} WARN {msg}", Utc::now().to_rfc3339()));
    }

    fn append_log_line(&self, line: &str) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        writeln!(f, "{line}")?;
        Ok(())
    }
}
