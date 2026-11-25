//! Process-wide diagnostics: a rotating file log and crash reports.
//!
//! Everything here writes to files only, never to stdout or stderr, so it can
//! run under the TUI or `--json` output without corrupting either. The one
//! exception is the short "crash report written to …" note printed after a
//! panic on the main thread, when the process is about to exit anyway.
//!
//! Log level, in order of precedence:
//! 1. `CODINGBUDDY_LOG` (full `EnvFilter` syntax, e.g. `debug,codingbuddy_mcp=trace`)
//! 2. `--debug` → `debug`
//! 3. default → `info`
//!
//! Location: `CODINGBUDDY_LOG_DIR`, otherwise `~/.codingbuddy/logs/`.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tracing_appender::rolling::{Builder as RollingBuilder, Rotation};
use tracing_subscriber::EnvFilter;

/// Environment variable holding an `EnvFilter` directive string.
pub const LOG_ENV: &str = "CODINGBUDDY_LOG";
/// Environment variable overriding the log directory.
pub const LOG_DIR_ENV: &str = "CODINGBUDDY_LOG_DIR";

const LOG_PREFIX: &str = "codingbuddy";
const LOG_SUFFIX: &str = "log";
const CRASH_PREFIX: &str = "crash-";
const MAX_LOG_FILES: usize = 7;
const CRASH_TAIL_LINES: usize = 200;
/// Only the end of the log is read for crash reports, so a large daily file
/// cannot make the panic hook slow.
const CRASH_TAIL_BYTES: u64 = 256 * 1024;

/// Dependencies that are chatty at `debug` and rarely useful for our bugs.
const QUIET_DEPENDENCIES: &str = "aws_config=warn,aws_runtime=warn,aws_sigv4=warn,\
aws_smithy_runtime=warn,aws_smithy_runtime_api=warn,aws_smithy_http=warn,hyper=warn,\
hyper_util=warn,h2=warn,reqwest=warn,rustls=warn,tantivy=warn,notify=warn";

static ACTIVE_LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
/// Set while a full-screen UI owns the terminal; see [`claim_terminal`].
static TERMINAL_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Held by a full-screen UI for as long as it owns the terminal. While held,
/// panics on background threads are still written to a crash report but are
/// not printed to stderr, where they would garble the screen. Panics on the
/// main thread always print, because they end the process.
#[must_use = "the claim is released when this guard is dropped"]
pub struct TerminalClaim(());

impl Drop for TerminalClaim {
    fn drop(&mut self) {
        TERMINAL_CLAIMED.store(false, Ordering::SeqCst);
    }
}

pub fn claim_terminal() -> TerminalClaim {
    TERMINAL_CLAIMED.store(true, Ordering::SeqCst);
    TerminalClaim(())
}

#[derive(Debug, Clone, Default)]
pub struct LoggingOptions {
    /// Lower the default level to `debug` (ignored when `CODINGBUDDY_LOG` is set).
    pub debug: bool,
    /// With `debug`, limit debug output to these crates (`mcp` means
    /// `codingbuddy_mcp`); everything else stays at `info`. Empty means all.
    pub categories: Vec<String>,
    /// Override the log directory; `None` uses [`default_log_dir`].
    pub log_dir: Option<PathBuf>,
}

/// `CODINGBUDDY_LOG_DIR`, otherwise `~/.codingbuddy/logs`.
pub fn default_log_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(LOG_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".codingbuddy").join("logs"))
}

/// The directory passed to a successful [`init_logging`] call, if any.
pub fn active_log_dir() -> Option<&'static Path> {
    ACTIVE_LOG_DIR.get().map(PathBuf::as_path)
}

/// Install the global file logger. Returns the log directory in use, or
/// `None` when logging could not be set up (no home directory, unwritable
/// directory, or a subscriber was already installed). Failure is silent by
/// design: diagnostics must never stop the CLI from starting.
pub fn init_logging(options: LoggingOptions) -> Option<PathBuf> {
    let dir = options.log_dir.or_else(default_log_dir)?;
    fs::create_dir_all(&dir).ok()?;
    let appender = RollingBuilder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_PREFIX)
        .filename_suffix(LOG_SUFFIX)
        .max_log_files(MAX_LOG_FILES)
        .build(&dir)
        .ok()?;

    let directives = filter_directives(
        options.debug,
        &options.categories,
        std::env::var(LOG_ENV).ok().as_deref(),
    );
    let filter = EnvFilter::try_new(&directives).unwrap_or_else(|_| EnvFilter::new("info"));

    // A blocking writer behind a mutex: log volume is low, and it guarantees
    // every line is on disk before a crash report reads the tail back.
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_names(true)
        .with_writer(Mutex::new(appender))
        .finish();
    tracing::subscriber::set_global_default(subscriber).ok()?;

    let _ = ACTIVE_LOG_DIR.set(dir.clone());
    Some(dir)
}

fn filter_directives(debug: bool, categories: &[String], env_value: Option<&str>) -> String {
    if let Some(value) = env_value.map(str::trim).filter(|value| !value.is_empty()) {
        return value.to_string();
    }
    if !debug {
        return format!("info,{QUIET_DEPENDENCIES}");
    }
    let scoped: Vec<String> = categories
        .iter()
        .map(|category| category.trim().to_ascii_lowercase().replace('-', "_"))
        .filter(|category| {
            !category.is_empty()
                && category
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(|category| {
            let target = if category.starts_with("codingbuddy") {
                category
            } else {
                format!("codingbuddy_{category}")
            };
            format!("{target}=debug")
        })
        .collect();
    if scoped.is_empty() {
        format!("debug,{QUIET_DEPENDENCIES}")
    } else {
        format!("info,{QUIET_DEPENDENCIES},{}", scoped.join(","))
    }
}

/// Files in `dir` whose name starts with `prefix`, newest first.
fn files_with_prefix_newest_first(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata
                .is_file()
                .then_some((metadata.modified().ok()?, entry.path()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    files.into_iter().map(|(_, path)| path).collect()
}

/// Up to `limit` log files in `dir`, newest first.
pub fn recent_log_files(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = files_with_prefix_newest_first(dir, &format!("{LOG_PREFIX}."));
    files.truncate(limit);
    files
}

/// Up to `limit` crash reports in `dir`, newest first.
pub fn recent_crash_reports(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut files = files_with_prefix_newest_first(dir, CRASH_PREFIX);
    files.truncate(limit);
    files
}

/// The current log file (newest `codingbuddy.*.log`) in `dir`.
pub fn latest_log_file(dir: &Path) -> Option<PathBuf> {
    recent_log_files(dir, 1).pop()
}

/// The newest crash report in `dir`.
pub fn latest_crash_report(dir: &Path) -> Option<PathBuf> {
    recent_crash_reports(dir, 1).pop()
}

/// The log directory this process uses: the initialised one, else the default.
pub fn effective_log_dir() -> Option<PathBuf> {
    active_log_dir()
        .map(Path::to_path_buf)
        .or_else(default_log_dir)
}

/// Last `max_bytes` of a file as text, starting at a line boundary. Used to
/// keep diagnostic bundles small.
pub fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut text = tail_lines(path, usize::MAX, max_bytes)?;
    if !text.is_empty() {
        text.push('\n');
    }
    Some(text)
}

/// Last `max_lines` lines of `path`, reading at most the final `max_bytes`.
fn tail_lines(path: &Path, max_lines: usize, max_bytes: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // When we started mid-file, the first line is probably partial.
    let text = if start > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(max_lines);
    Some(lines[skip..].join("\n"))
}

struct CrashDetails<'a> {
    version: &'a str,
    thread: &'a str,
    panic: &'a str,
    backtrace: &'a str,
    log_tail: Option<(&'a Path, &'a str)>,
}

fn render_crash_report(details: &CrashDetails<'_>) -> String {
    let mut report = format!(
        "CodingBuddy crash report\n\
         version: {}\n\
         time: {}\n\
         os: {} {}\n\
         thread: {}\n\n\
         panic: {}\n\n\
         backtrace:\n{}\n",
        details.version,
        chrono::Utc::now().to_rfc3339(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        details.thread,
        details.panic,
        details.backtrace,
    );
    match details.log_tail {
        Some((path, tail)) => {
            report.push_str(&format!(
                "\nlast {CRASH_TAIL_LINES} log lines ({}):\n{tail}\n",
                path.display()
            ));
        }
        None => report.push_str("\n(no log file available)\n"),
    }
    report
}

fn write_crash_report(dir: &Path, contents: &str) -> Option<PathBuf> {
    fs::create_dir_all(dir).ok()?;
    let name = format!(
        "{CRASH_PREFIX}{}-{}.log",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        std::process::id()
    );
    let path = dir.join(name);
    fs::write(&path, contents).ok()?;
    Some(path)
}

/// Install a panic hook that records a crash report next to the logs, then
/// defers to the previously installed hook (the default one prints the panic
/// message). Install this before any hook that needs to run first, such as
/// the TUI's terminal restore: later hooks wrap earlier ones.
///
/// The report is written for every panic, including ones a worker thread
/// catches, because those are bugs too. The note pointing at the report is
/// printed only for panics on the thread that installed the hook, which is
/// the one whose panic ends the process.
pub fn install_crash_handler(version: &'static str) {
    let main_thread = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let current = std::thread::current();
        let thread = current.name().unwrap_or("<unnamed>");
        let panic = info.to_string();
        tracing::error!(target: "codingbuddy::panic", thread, "{panic}");

        let dir = effective_log_dir();
        let report_path = dir.as_deref().and_then(|dir| {
            let log_file = latest_log_file(dir);
            let tail = log_file
                .as_deref()
                .and_then(|path| tail_lines(path, CRASH_TAIL_LINES, CRASH_TAIL_BYTES));
            let report = render_crash_report(&CrashDetails {
                version,
                thread,
                panic: &panic,
                backtrace: &backtrace,
                log_tail: log_file.as_deref().zip(tail.as_deref()),
            });
            write_crash_report(dir, &report)
        });

        let on_main_thread = current.id() == main_thread;
        if on_main_thread || !TERMINAL_CLAIMED.load(Ordering::SeqCst) {
            previous(info);
        }

        if on_main_thread && let Some(path) = report_path {
            eprintln!(
                "\nCodingBuddy crashed. A crash report was saved to:\n  {}\n\
                 Please attach it when reporting the problem: \
                 https://github.com/aloundoye/codingbuddy/issues",
                path.display()
            );
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_prefers_env_then_debug_flag() {
        assert_eq!(filter_directives(true, &[], Some(" trace ")), "trace");
        assert!(filter_directives(true, &[], None).starts_with("debug,"));
        assert!(filter_directives(false, &[], Some("   ")).starts_with("info,"));
    }

    #[test]
    fn debug_categories_scope_to_crates_and_skip_invalid_names() {
        let categories = [
            "mcp".to_string(),
            " LLM ".into(),
            "codingbuddy_agent".into(),
            "bad=trace".into(),
        ];
        let directives = filter_directives(true, &categories, None);
        assert!(directives.starts_with("info,"));
        assert!(
            directives
                .ends_with(",codingbuddy_mcp=debug,codingbuddy_llm=debug,codingbuddy_agent=debug")
        );
        assert!(!directives.contains("bad"));
        // Categories are ignored without --debug.
        assert!(!filter_directives(false, &categories, None).contains("mcp"));
    }

    #[test]
    fn every_generated_filter_parses() {
        let categories = ["tools".to_string()];
        for debug in [false, true] {
            for cats in [&[][..], &categories[..]] {
                assert!(EnvFilter::try_new(filter_directives(debug, cats, None)).is_ok());
            }
        }
    }

    #[test]
    fn tail_returns_last_lines_and_drops_partial_first_line() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("codingbuddy.2026-01-01.log");
        let body: String = (0..50).map(|n| format!("line {n}\n")).collect();
        fs::write(&path, &body)?;

        let tail = tail_lines(&path, 3, 10_000).unwrap_or_default();
        assert_eq!(tail, "line 47\nline 48\nline 49");

        // Reading only the last 12 bytes starts mid-line ("e 48\n" …), which
        // must be dropped rather than reported as a garbled line.
        let tail = tail_lines(&path, 10, 12).unwrap_or_default();
        assert_eq!(tail, "line 49");
        Ok(())
    }

    #[test]
    fn crash_report_includes_panic_backtrace_and_log_tail() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let log = dir.path().join("codingbuddy.2026-01-01.log");
        let report = render_crash_report(&CrashDetails {
            version: "9.9.9",
            thread: "main",
            panic: "panicked at src/x.rs:1:1: boom",
            backtrace: "0: frame",
            log_tail: Some((&log, "INFO started")),
        });
        assert!(report.contains("version: 9.9.9"));
        assert!(report.contains("boom"));
        assert!(report.contains("0: frame"));
        assert!(report.contains("INFO started"));

        let path = write_crash_report(dir.path(), &report).expect("report written");
        assert_eq!(latest_crash_report(dir.path()), Some(path.clone()));
        assert_eq!(fs::read_to_string(path)?, report);
        Ok(())
    }

    #[test]
    fn latest_log_file_ignores_crash_reports() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("codingbuddy.2026-01-01.log"), "a")?;
        fs::write(dir.path().join("crash-20260101T000000Z-1.log"), "b")?;
        let latest = latest_log_file(dir.path()).expect("log file");
        assert!(latest.ends_with("codingbuddy.2026-01-01.log"));
        Ok(())
    }
}
