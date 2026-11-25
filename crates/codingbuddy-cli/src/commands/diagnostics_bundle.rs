//! `codingbuddy doctor --bundle`: one redacted archive to attach to a bug report.
//!
//! The archive holds the doctor payload, the effective configuration, the most
//! recent log files and crash reports. Secrets are removed twice: fields whose
//! names look secret are masked structurally, then every text file goes
//! through the same `OutputScanner` redaction used for tool output.

use anyhow::{Context, Result};
use codingbuddy_core::{AppConfig, logging};
use codingbuddy_policy::output_scanner::OutputScanner;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::Value;
use std::fs::File;
use std::path::{Path, PathBuf};

const MAX_LOG_FILES: usize = 3;
const MAX_CRASH_REPORTS: usize = 3;
/// Per-file cap so a bundle stays small enough to attach to an issue.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const REDACTED: &str = "***REDACTED***";

/// Field names (lower-cased, `-` read as `_`) whose values are always masked.
fn is_secret_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('-', "_");
    // `signature_key`, `signing_key`, `key` … but not `api_key_env` or
    // `keybindings_path`, which name where a value lives rather than hold it.
    if name.ends_with("_env") || name.ends_with("_path") {
        return false;
    }
    if name == "key" || name.ends_with("_key") {
        return true;
    }
    [
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "credential",
        "private_key",
        "cookie",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

/// Mask secret-looking fields at any depth. Header and environment maps are
/// masked entirely, since their keys rarely say whether the value is secret.
fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                let lower = key.to_ascii_lowercase();
                let whole_map =
                    matches!(lower.as_str(), "headers" | "env" | "child_env") && entry.is_object();
                if whole_map {
                    if let Value::Object(inner) = entry {
                        for inner_value in inner.values_mut() {
                            *inner_value = Value::String(REDACTED.to_string());
                        }
                    }
                } else if is_secret_field(key) && !entry.is_null() && !entry.is_boolean() {
                    *entry = Value::String(REDACTED.to_string());
                } else {
                    redact_value(entry);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        _ => {}
    }
}

fn redact_text(scanner: &OutputScanner, text: &str) -> String {
    scanner.scan(text).redacted_output
}

fn append_text(
    archive: &mut tar::Builder<GzEncoder<File>>,
    name: &str,
    contents: &str,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
    );
    header.set_cksum();
    archive
        .append_data(&mut header, name, contents.as_bytes())
        .with_context(|| format!("could not add {name} to the diagnostics bundle"))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Write the bundle into the log directory and return its path. The log
/// directory is used rather than the workspace so the archive is never
/// committed by accident.
pub(crate) fn write_bundle(cwd: &Path, doctor_payload: &Value) -> Result<PathBuf> {
    let log_dir = logging::effective_log_dir()
        .context("no log directory: set HOME or CODINGBUDDY_LOG_DIR")?;
    logging::create_private_dir(&log_dir)
        .with_context(|| format!("could not create {}", log_dir.display()))?;
    let bundle_path = log_dir.join(format!(
        "codingbuddy-diagnostics-{}.tar.gz",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    build_bundle(cwd, doctor_payload, &log_dir, &bundle_path)?;
    Ok(bundle_path)
}

fn build_bundle(
    cwd: &Path,
    doctor_payload: &Value,
    log_dir: &Path,
    bundle_path: &Path,
) -> Result<()> {
    let scanner = OutputScanner::new();
    let file = logging::create_private_file(bundle_path)
        .with_context(|| format!("could not create {}", bundle_path.display()))?;
    let mut archive = tar::Builder::new(GzEncoder::new(file, Compression::default()));

    let mut payload = doctor_payload.clone();
    redact_value(&mut payload);
    append_text(
        &mut archive,
        "doctor.json",
        &redact_text(&scanner, &serde_json::to_string_pretty(&payload)?),
    )?;

    let config = match AppConfig::load(cwd).and_then(|cfg| Ok(serde_json::to_value(cfg)?)) {
        Ok(mut value) => {
            redact_value(&mut value);
            redact_text(&scanner, &serde_json::to_string_pretty(&value)?)
        }
        Err(error) => format!("configuration could not be loaded: {error:#}\n"),
    };
    append_text(&mut archive, "config.json", &config)?;

    let mut manifest = vec![
        format!("codingbuddy {}", env!("CARGO_PKG_VERSION")),
        format!("created {}", chrono::Utc::now().to_rfc3339()),
        format!("os {} {}", std::env::consts::OS, std::env::consts::ARCH),
    ];
    for (folder, files) in [
        ("logs", logging::recent_log_files(log_dir, MAX_LOG_FILES)),
        (
            "crash",
            logging::recent_crash_reports(log_dir, MAX_CRASH_REPORTS),
        ),
    ] {
        for path in files {
            let Some(text) = logging::read_tail(&path, MAX_FILE_BYTES) else {
                continue;
            };
            let name = format!("{folder}/{}", file_name(&path));
            append_text(&mut archive, &name, &redact_text(&scanner, &text))?;
            manifest.push(name);
        }
    }
    manifest.push(String::new());
    append_text(&mut archive, "MANIFEST.txt", &manifest.join("\n"))?;

    archive
        .into_inner()
        .context("could not finish the diagnostics bundle")?
        .finish()
        .context("could not compress the diagnostics bundle")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::io::Read;

    fn read_bundle(path: &Path) -> Result<BTreeMap<String, String>> {
        let mut archive = tar::Archive::new(GzDecoder::new(File::open(path)?));
        let mut files = BTreeMap::new();
        for entry in archive.entries()? {
            let mut entry = entry?;
            let name = entry.path()?.to_string_lossy().into_owned();
            let mut body = String::new();
            entry.read_to_string(&mut body)?;
            files.insert(name, body);
        }
        Ok(files)
    }

    #[test]
    fn redacts_secret_fields_headers_and_env_at_any_depth() {
        let mut value = json!({
            "llm": {"api_key": "sk-live", "api_key_env": "DEEPSEEK_API_KEY", "api_key_configured": true},
            "telemetry": {"signature_key": "local-dev-key", "keybindings_path": "~/.kb.json"},
            "providers": [{"name": "x", "auth_token": "abc", "refresh-token": null}],
            "mcp": {"servers": [{"headers": {"X-Custom": "v"}, "env": {"FOO": "bar"}, "command": "node"}]},
        });
        redact_value(&mut value);
        assert_eq!(value["llm"]["api_key"], REDACTED);
        assert_eq!(value["llm"]["api_key_env"], "DEEPSEEK_API_KEY");
        assert_eq!(value["telemetry"]["signature_key"], REDACTED);
        assert_eq!(value["telemetry"]["keybindings_path"], "~/.kb.json");
        // Booleans stay readable; they say whether a key is set, not what it is.
        assert_eq!(value["llm"]["api_key_configured"], true);
        assert_eq!(value["providers"][0]["auth_token"], REDACTED);
        assert_eq!(value["providers"][0]["refresh-token"], Value::Null);
        assert_eq!(value["providers"][0]["name"], "x");
        assert_eq!(value["mcp"]["servers"][0]["headers"]["X-Custom"], REDACTED);
        assert_eq!(value["mcp"]["servers"][0]["env"]["FOO"], REDACTED);
        assert_eq!(value["mcp"]["servers"][0]["command"], "node");
    }

    #[test]
    fn bundle_contains_redacted_payload_config_logs_and_crashes() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let logs = tempfile::tempdir()?;
        std::fs::write(
            logs.path().join("codingbuddy.2026-01-01.log"),
            "INFO started\nWARN used key sk-abcdefghijklmnopqrstuvwxyz0123456789\n",
        )?;
        std::fs::write(
            logs.path().join("crash-20260101T000000Z-1.log"),
            "panic: boom\n",
        )?;
        std::fs::write(logs.path().join("unrelated.txt"), "not included")?;

        let bundle = logs.path().join("bundle.tar.gz");
        let payload = json!({"os": "test", "llm": {"api_key": "plain-secret"}});
        build_bundle(workspace.path(), &payload, logs.path(), &bundle)?;

        let files = read_bundle(&bundle)?;
        assert!(files.contains_key("config.json"));
        assert!(files["doctor.json"].contains("\"os\": \"test\""));
        assert!(!files["doctor.json"].contains("plain-secret"));
        let log = &files["logs/codingbuddy.2026-01-01.log"];
        assert!(log.contains("INFO started"));
        assert!(!log.contains("sk-abcdefghijklmnopqrstuvwxyz0123456789"));
        assert_eq!(files["crash/crash-20260101T000000Z-1.log"], "panic: boom\n");
        assert!(!files.keys().any(|name| name.contains("unrelated")));
        assert!(files["MANIFEST.txt"].contains("logs/codingbuddy.2026-01-01.log"));
        Ok(())
    }
}
