//! Environment inherited by workspace subprocesses.
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    User,
    Managed,
    Project,
}

#[derive(Deserialize)]
struct Grant {
    consumer: String,
    source: Source,
    workspace: Option<std::path::PathBuf>,
    variables: Vec<String>,
}

#[derive(Default, Deserialize)]
struct ProcessSettings {
    #[serde(default)]
    grants: Vec<Grant>,
}

#[derive(Default, Deserialize)]
struct Settings {
    process_env: Option<ProcessSettings>,
}

pub type Variables = BTreeMap<OsString, OsString>;

fn read_settings(path: &Path) -> Option<ProcessSettings> {
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice::<Settings>(&raw).ok()?.process_env
}

pub fn variables(workspace: &Path, consumer: &str, source: Source) -> Variables {
    let mut vars = Variables::new();
    for key in [
        "HOME",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "SystemRoot",
        "WINDIR",
        "SystemDrive",
        "PATHEXT",
        "TEMP",
        "TMP",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "COLORTERM",
    ] {
        if let Some(value) = std::env::var_os(key) {
            vars.insert(key.into(), value);
        }
    }
    let home = vars
        .get(OsStr::new("HOME"))
        .or_else(|| vars.get(OsStr::new("USERPROFILE")))
        .cloned();
    if let Some(home) = home {
        vars.entry("HOME".into()).or_insert_with(|| home.clone());
        vars.entry("USERPROFILE".into()).or_insert(home);
    }
    let parent_path = std::env::var_os("PATH").unwrap_or_default();
    let mut search = std::env::split_paths(&parent_path)
        .filter(|path| path.is_absolute())
        .collect::<Vec<_>>();
    if search.is_empty() {
        #[cfg(unix)]
        search.extend(["/usr/bin", "/bin"].map(std::path::PathBuf::from));
        #[cfg(windows)]
        if let Some(root) = vars
            .get(OsStr::new("SystemRoot"))
            .map(std::path::PathBuf::from)
        {
            search.push(root.join("System32"));
        }
    }
    if let Ok(path) = std::env::join_paths(search) {
        vars.insert("PATH".into(), path);
    }

    if consumer.is_empty() || source == Source::Project && !crate::workspace_is_trusted(workspace) {
        return vars;
    }
    let managed = managed_path().as_deref().and_then(|path| {
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return Some(ProcessSettings::default()),
        };
        match serde_json::from_slice::<Settings>(&raw) {
            Ok(settings) => settings.process_env,
            Err(_) => Some(ProcessSettings::default()),
        }
    });
    // An explicitly configured managed grant list replaces the user list, including an empty list.
    let settings = managed.or_else(|| {
        crate::AppConfig::user_settings_path()
            .as_deref()
            .and_then(read_settings)
    });
    let workspace = workspace.canonicalize().ok();
    for grant in settings.into_iter().flat_map(|settings| settings.grants) {
        if grant.consumer != consumer || grant.source != source {
            continue;
        }
        let grant_root = grant
            .workspace
            .as_deref()
            .filter(|p| p.is_absolute())
            .and_then(|p| p.canonicalize().ok());
        if (source == Source::Project || grant.workspace.is_some())
            && (grant_root.is_none() || grant_root != workspace)
        {
            continue;
        }
        for key in grant.variables {
            let name = key.to_ascii_uppercase();
            if !key.is_empty()
                && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !matches!(
                    name.as_str(),
                    "PATH"
                        | "COMSPEC"
                        | "ENV"
                        | "BASH_ENV"
                        | "ZDOTDIR"
                        | "SHELLOPTS"
                        | "NODE_OPTIONS"
                        | "PYTHONPATH"
                )
                && !name.starts_with("LD_")
                && !name.starts_with("DYLD_")
                && !name.starts_with("GIT_CONFIG")
                && !name.starts_with("CODINGBUDDY_")
                && let Some(value) = std::env::var_os(&key)
            {
                vars.insert(key.into(), value);
            }
        }
    }
    vars
}

pub fn apply(command: &mut Command, vars: &Variables) {
    command.env_clear().envs(vars);
}

pub fn apply_for(command: &mut Command, workspace: &Path, consumer: &str, source: Source) {
    apply(command, &variables(workspace, consumer, source));
}

pub fn lookup<'a>(vars: &'a Variables, name: &str) -> Option<&'a OsStr> {
    #[cfg(windows)]
    return vars
        .iter()
        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_os_str());
    #[cfg(not(windows))]
    vars.get(OsStr::new(name)).map(OsString::as_os_str)
}

/// Resolve an internal executable without searching the working directory.
pub fn find_program(program: &str, vars: &Variables) -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    let program = &format!("{program}.exe");
    std::env::split_paths(lookup(vars, "PATH")?).find_map(|directory| {
        let candidate = directory.join(program);
        let metadata = candidate.metadata().ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return None;
            }
        }
        metadata.is_file().then_some(candidate)
    })
}

pub fn managed_path() -> Option<std::path::PathBuf> {
    // Check environment override first
    if let Ok(path) = std::env::var("CODINGBUDDY_MANAGED_SETTINGS_PATH") {
        let path = path.trim().to_string();
        if !path.is_empty() {
            return Some(std::path::PathBuf::from(path));
        }
    }

    #[cfg(target_os = "macos")]
    {
        let path = std::path::PathBuf::from(
            "/Library/Application Support/CodingBuddyCLI/managed-settings.json",
        );
        return Some(path);
    }

    #[cfg(target_os = "linux")]
    {
        let path = std::path::PathBuf::from("/etc/codingbuddy-cli/managed-settings.json");
        return Some(path);
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(program_data) = std::env::var("ProgramData") {
            return Some(
                std::path::PathBuf::from(program_data)
                    .join("CodingBuddyCLI")
                    .join("managed-settings.json"),
            );
        }
        return Some(std::path::PathBuf::from(
            "C:\\ProgramData\\CodingBuddyCLI\\managed-settings.json",
        ));
    }

    #[allow(unreachable_code)]
    None
}

/// Prepare a Git command whose environment is isolated from the agent's credentials.
pub fn git_command(workspace: &Path, consumer: &str) -> Command {
    let vars = variables(workspace, consumer, Source::Project);
    let program = find_program("git", &vars).unwrap_or_else(|| {
        #[cfg(windows)]
        return std::path::PathBuf::from(r"\\?\NUL\git.exe");
        #[cfg(not(windows))]
        std::path::PathBuf::from("/dev/null/git")
    });
    let mut command = Command::new(program);
    apply(&mut command, &vars);
    command.current_dir(workspace);
    command
}

/// Run built-in read-only Git inspection without repository-configured executable helpers.
pub fn git_inspection(workspace: &Path) -> Command {
    let mut command = git_command(workspace, "");
    command
        .current_dir(workspace)
        .arg("--no-pager")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=",
            "-c",
            "diff.external=",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "diff.ignoreSubmodules=all",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0");
    // Git status can invoke clean/process filters selected by .gitattributes.
    // Read their configured names without running repository helpers.
    let mut config = git_command(workspace, "");
    if let Ok(output) = config
        .current_dir(workspace)
        .args([
            "--no-pager",
            "config",
            "--name-only",
            "--null",
            "--get-regexp",
            r"^filter\..*\.(clean|process|required)$",
        ])
        .output()
    {
        for key in String::from_utf8_lossy(&output.stdout).split_terminator('\0') {
            if !key.is_empty() && !key.contains('=') {
                let value = if key.ends_with(".required") {
                    "false"
                } else {
                    ""
                };
                command.arg("-c").arg(format!("{key}={value}"));
            }
        }
    }
    command
}
