//! `codingbuddy update`: find, verify and install a new release.
//!
//! Standalone installs (the install scripts' default) replace their own
//! binary: the release archive's SHA-256 must match `checksums.txt`, the new
//! binary must run and report the expected version, and the swap keeps a
//! `.bak` so a failure leaves the old binary in place. Homebrew, npm and
//! source builds are pointed at their own upgrade command instead.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::output::print_json;
use crate::{UpdateArgs, UpdateChannelArg};

const REPO: &str = "aloundoye/codingbuddy";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Overrides the GitHub API origin (tests point it at a local server).
const API_ENV: &str = "CODINGBUDDY_UPDATE_API";

// ── Versions ────────────────────────────────────────────────────────────

/// `v1.2.3` / `1.2.3-nightly.4` → comparable key. A pre-release sorts below
/// the release with the same numbers, as in semver.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    core: (u64, u64, u64),
    /// `None` for a release, so it sorts after any pre-release.
    pre: Option<std::cmp::Reverse<String>>,
}

fn parse_version(text: &str) -> Option<Version> {
    let text = text.trim().trim_start_matches('v');
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let version = Version {
        core: (parts.next()??, parts.next()??, parts.next()??),
        pre: pre.map(|pre| std::cmp::Reverse(pre.to_string())),
    };
    parts.next().is_none().then_some(version)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => {
            // A release beats its own pre-releases; pre-releases compare by name.
            match (&candidate.pre, &current.pre) {
                (None, Some(_)) if candidate.core == current.core => true,
                (Some(_), None) if candidate.core == current.core => false,
                (Some(a), Some(b)) if candidate.core == current.core => a.0 > b.0,
                _ => candidate.core > current.core,
            }
        }
        _ => false,
    }
}

// ── Install detection ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallKind {
    Homebrew,
    Npm,
    /// Built from a checkout; the path is the checkout root.
    Source(PathBuf),
    Standalone,
}

fn detect_install(exe: &Path) -> InstallKind {
    let text = exe.to_string_lossy();
    if text.contains("/Cellar/") || text.contains("/homebrew/") || text.contains("/linuxbrew/") {
        return InstallKind::Homebrew;
    }
    if text.contains("node_modules") {
        return InstallKind::Npm;
    }
    // .../<checkout>/target/{debug,release}/codingbuddy
    if let Some(target_dir) = exe.ancestors().find(|dir| dir.ends_with("target"))
        && let Some(checkout) = target_dir.parent()
        && checkout.join("Cargo.toml").is_file()
    {
        return InstallKind::Source(checkout.to_path_buf());
    }
    InstallKind::Standalone
}

fn target_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        _ => None,
    }
}

// ── Releases ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Release {
    tag: String,
    assets: Vec<(String, String)>,
}

impl Release {
    fn version(&self) -> &str {
        self.tag.trim_start_matches('v')
    }
    fn asset_url(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|(asset, _)| asset == name)
            .map(|(_, url)| url.as_str())
    }
}

fn api_base() -> String {
    std::env::var(API_ENV).unwrap_or_else(|_| "https://api.github.com".to_string())
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("codingbuddy/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

fn parse_release(value: &serde_json::Value) -> Option<Release> {
    let tag = value["tag_name"].as_str()?.to_string();
    let assets = value["assets"]
        .as_array()
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    Some((
                        asset["name"].as_str()?.to_string(),
                        asset["browser_download_url"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Release { tag, assets })
}

fn fetch_release(channel: UpdateChannelArg, timeout: Duration) -> Result<Release> {
    let client = http_client(timeout)?;
    let base = api_base();
    codingbuddy_core::runtime::block_on(async move {
        match channel {
            UpdateChannelArg::Stable => {
                let url = format!("{base}/repos/{REPO}/releases/latest");
                let value: serde_json::Value = client
                    .get(&url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                parse_release(&value).context("release response has no tag")
            }
            UpdateChannelArg::Nightly => {
                let url = format!("{base}/repos/{REPO}/releases?per_page=10");
                let values: Vec<serde_json::Value> = client
                    .get(&url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                values
                    .iter()
                    .filter(|value| value["draft"] != true)
                    .find_map(parse_release)
                    .context("no published releases")
            }
        }
    })
}

fn download(url: &str) -> Result<Vec<u8>> {
    let client = http_client(Duration::from_secs(300))?;
    let url = url.to_string();
    codingbuddy_core::runtime::block_on(async move {
        let bytes = client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok::<_, anyhow::Error>(bytes.to_vec())
    })
}

// ── Verification and install ────────────────────────────────────────────

/// The archive must be listed in `checksums.txt` (sha256sum format) and
/// match. A missing entry is an error: never install unverified bytes.
fn verify_checksum(archive: &[u8], checksums: &str, asset: &str) -> Result<()> {
    let expected = checksums
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim().trim_start_matches('*') == asset)
        .map(|(hash, _)| hash.trim().to_ascii_lowercase())
        .with_context(|| format!("{asset} is not listed in checksums.txt"))?;
    let actual = format!("{:x}", sha2::Sha256::digest(archive));
    ensure!(
        actual == expected,
        "checksum mismatch for {asset}: expected {expected}, got {actual}"
    );
    Ok(())
}

/// The `codingbuddy` binary inside a release `.tar.gz`.
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry
            .path()?
            .file_name()
            .is_some_and(|name| name == "codingbuddy")
        {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    bail!("the release archive does not contain a codingbuddy binary")
}

/// Swap `exe` for `binary` safely: the new file is written next to it, must
/// run and report `expected_version`, and the old binary is kept as
/// `<exe>.bak` and restored if the final rename fails.
fn replace_binary(exe: &Path, binary: &[u8], expected_version: &str) -> Result<PathBuf> {
    let staged = exe.with_extension("new");
    let backup = exe.with_extension("bak");
    std::fs::write(&staged, binary)
        .with_context(|| format!("cannot write {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let check = std::process::Command::new(&staged)
        .arg("--version")
        .output();
    let reported = check
        .as_ref()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
    if !reported
        .as_deref()
        .is_some_and(|text| text.contains(expected_version))
    {
        let _ = std::fs::remove_file(&staged);
        bail!(
            "the downloaded binary did not report version {expected_version}; nothing was changed"
        );
    }
    std::fs::rename(exe, &backup)
        .with_context(|| format!("cannot move {} aside", exe.display()))?;
    if let Err(error) = std::fs::rename(&staged, exe) {
        let _ = std::fs::rename(&backup, exe);
        let _ = std::fs::remove_file(&staged);
        return Err(
            anyhow!(error).context("could not install the new binary; restored the old one")
        );
    }
    Ok(backup)
}

// ── Command ─────────────────────────────────────────────────────────────

pub(crate) fn run_update(_cwd: &Path, args: UpdateArgs, json_mode: bool) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the running binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let install = detect_install(&exe);
    let channel = args.channel;
    let channel_name = match channel {
        UpdateChannelArg::Stable => "stable",
        UpdateChannelArg::Nightly => "nightly",
    };

    let release = fetch_release(channel, Duration::from_secs(15))
        .context("could not check for a new release")?;
    let latest = release.version().to_string();
    let available = is_newer(&latest, CURRENT);

    let mut payload = json!({
        "schema": "codingbuddy.update.v2",
        "current_version": CURRENT,
        "latest_version": latest,
        "channel": channel_name,
        "update_available": available,
        "install": format!("{install:?}"),
        "updated": false,
    });
    let instruction = match &install {
        InstallKind::Homebrew => Some("brew upgrade codingbuddy".to_string()),
        InstallKind::Npm => Some("npm install -g codingbuddy@latest".to_string()),
        InstallKind::Source(checkout) => Some(format!(
            "cd {} && git pull --ff-only && cargo build --release",
            checkout.display()
        )),
        InstallKind::Standalone if cfg!(windows) => Some(
            "irm https://raw.githubusercontent.com/aloundoye/codingbuddy/main/scripts/install.ps1 | iex"
                .to_string(),
        ),
        InstallKind::Standalone => None,
    };

    if available && !args.check {
        if let Some(command) = &instruction {
            payload["next_step"] = json!(command);
        } else {
            let target = target_triple().context("no prebuilt binary for this platform")?;
            let asset = format!("codingbuddy-{target}.tar.gz");
            let archive_url = release
                .asset_url(&asset)
                .with_context(|| format!("release {} has no {asset}", release.tag))?
                .to_string();
            let checksums_url = release
                .asset_url("checksums.txt")
                .with_context(|| format!("release {} has no checksums.txt", release.tag))?
                .to_string();
            payload["asset"] = json!(asset);
            payload["binary"] = json!(exe);
            if !args.dry_run {
                let archive = download(&archive_url)?;
                let checksums = String::from_utf8(download(&checksums_url)?)
                    .context("checksums.txt is not text")?;
                verify_checksum(&archive, &checksums, &asset)?;
                let binary = extract_binary(&archive)?;
                let backup = replace_binary(&exe, &binary, &latest)?;
                payload["updated"] = json!(true);
                payload["backup"] = json!(backup);
            }
        }
    }

    if json_mode {
        return print_json(&payload);
    }
    if !available {
        println!("codingbuddy {CURRENT} is up to date ({channel_name} channel)");
        return Ok(());
    }
    println!("codingbuddy {latest} is available (you have {CURRENT})");
    if args.check {
        return Ok(());
    }
    if let Some(command) = instruction {
        println!("This install is managed elsewhere; update with:\n  {command}");
    } else if args.dry_run {
        println!(
            "Would download {} and replace {}",
            payload["asset"].as_str().unwrap_or_default(),
            exe.display()
        );
    } else {
        println!(
            "Updated to {latest}. The previous binary is kept at {}",
            payload["backup"].as_str().unwrap_or_default()
        );
    }
    Ok(())
}

// ── Background check ────────────────────────────────────────────────────

/// Where the last background check result is cached: per user, never in
/// the workspace.
fn update_cache_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".codingbuddy")
            .join("update_check.json"),
    )
}

/// Show a one-line banner if a previous background check found a newer
/// release less than 24 hours ago.
pub(crate) fn show_update_banner(_cwd: &Path) {
    let Some(path) = update_cache_path() else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };
    let fresh = info["checked_at"]
        .as_str()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .is_some_and(|checked| {
            chrono::Utc::now()
                .signed_duration_since(checked)
                .num_hours()
                <= 24
        });
    if let Some(version) = info["latest_version"].as_str()
        && fresh
        && is_newer(version, CURRENT)
    {
        eprintln!(
            "  \x1b[36m\u{2191}\x1b[0m Update available: \x1b[1mv{CURRENT}\x1b[0m \u{2192} \x1b[1mv{version}\x1b[0m — run \x1b[1mcodingbuddy update\x1b[0m"
        );
        eprintln!();
    }
}

/// Check for a newer stable release on a background thread. Never blocks,
/// never prints; the result is shown by [`show_update_banner`] next time.
pub(crate) fn check_for_update_background(_cwd: &Path) {
    std::thread::Builder::new()
        .name("update-check".to_string())
        .spawn(|| {
            let Ok(release) = fetch_release(UpdateChannelArg::Stable, Duration::from_secs(3))
            else {
                return;
            };
            let Some(path) = update_cache_path() else {
                return;
            };
            let info = json!({
                "latest_version": release.version(),
                "current_version": CURRENT,
                "checked_at": chrono::Utc::now().to_rfc3339(),
            });
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, info.to_string());
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_releases_beat_prereleases() {
        assert!(
            is_newer("0.10.0", "0.9.9"),
            "string comparison would get this wrong"
        );
        assert!(is_newer("v1.0.0", "0.99.0"));
        assert!(is_newer("0.6.0", "0.6.0-nightly.3"));
        assert!(is_newer("0.6.0-nightly.4", "0.6.0-nightly.3"));
        assert!(!is_newer("0.6.0-nightly.9", "0.6.0"));
        assert!(!is_newer("0.5.0", "0.5.0"));
        assert!(!is_newer("garbage", "0.5.0"));
    }

    #[test]
    fn install_kind_is_detected_from_the_binary_path() {
        assert_eq!(
            detect_install(Path::new(
                "/opt/homebrew/Cellar/codingbuddy/0.5.0/bin/codingbuddy"
            )),
            InstallKind::Homebrew
        );
        assert_eq!(
            detect_install(Path::new(
                "/usr/lib/node_modules/codingbuddy/bin/codingbuddy"
            )),
            InstallKind::Npm
        );
        assert_eq!(
            detect_install(Path::new("/home/u/.local/bin/codingbuddy")),
            InstallKind::Standalone
        );
        let checkout = tempfile::tempdir().unwrap();
        std::fs::write(checkout.path().join("Cargo.toml"), "[workspace]").unwrap();
        let exe = checkout.path().join("target/release/codingbuddy");
        assert_eq!(
            detect_install(&exe),
            InstallKind::Source(checkout.path().to_path_buf())
        );
    }

    fn release_archive(binary: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "codingbuddy", binary)
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn archives_must_match_their_listed_checksum() {
        let archive = release_archive(b"binary");
        let hash = format!("{:x}", sha2::Sha256::digest(&archive));
        let asset = "codingbuddy-x86_64-unknown-linux-gnu.tar.gz";
        let listed = format!("{hash}  {asset}\nffff  other.tar.gz\n");
        verify_checksum(&archive, &listed, asset).unwrap();
        assert!(verify_checksum(b"tampered", &listed, asset).is_err());
        assert!(
            verify_checksum(&archive, "ffff  other.tar.gz\n", asset).is_err(),
            "an unlisted archive is never trusted"
        );
        assert_eq!(extract_binary(&archive).unwrap(), b"binary");
    }

    #[cfg(unix)]
    #[test]
    fn binary_swap_keeps_a_backup_and_refuses_a_bad_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("codingbuddy");
        std::fs::write(&exe, "#!/bin/sh\necho codingbuddy 0.5.0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // A binary that reports the wrong version is rejected; nothing changes.
        let wrong = b"#!/bin/sh\necho codingbuddy 0.4.0\n";
        assert!(replace_binary(&exe, wrong, "0.6.0").is_err());
        assert!(std::fs::read_to_string(&exe).unwrap().contains("0.5.0"));
        assert!(!exe.with_extension("new").exists());

        let good = b"#!/bin/sh\necho codingbuddy 0.6.0\n";
        let backup = replace_binary(&exe, good, "0.6.0").unwrap();
        assert!(std::fs::read_to_string(&exe).unwrap().contains("0.6.0"));
        assert!(std::fs::read_to_string(backup).unwrap().contains("0.5.0"));
    }
}
