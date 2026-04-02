use anyhow::Result;
use chrono::Utc;
use codingbuddy_core::{TelemetryConfig, runtime_dir};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

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
        if self.verbose {
            eprintln!("[codingbuddy] {msg}");
        }
    }

    pub fn warn_log(&self, msg: &str) {
        eprintln!("[codingbuddy WARN] {msg}");
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
