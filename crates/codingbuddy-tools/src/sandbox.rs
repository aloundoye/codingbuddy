use anyhow::{Context, Result, ensure};
use codingbuddy_core::SandboxConfig;
use codingbuddy_core::process_env::{self, Source, Variables};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::shell::{ShellRunResult, run_shell_launch};

#[derive(Debug, PartialEq, Eq)]
enum Backend {
    Direct,
    #[cfg(target_os = "linux")]
    Bubblewrap(PathBuf),
    #[cfg(target_os = "macos")]
    Seatbelt,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ShellLaunch {
    expression: String,
    workspace: PathBuf,
    #[cfg(unix)]
    workspace_identity: (u64, u64),
    environment: Variables,
    backend: Backend,
    read_only: bool,
    block_network: bool,
}

impl ShellLaunch {
    pub(super) fn resolve(
        workspace: &Path,
        expression: &str,
        mode: &str,
        config: &SandboxConfig,
    ) -> Result<Self> {
        let workspace = workspace.canonicalize()?;
        let excluded = config.excluded_commands.iter().any(|entry| {
            let entry = entry.trim();
            !entry.is_empty()
                && (expression.trim() == entry
                    || expression
                        .trim()
                        .strip_prefix(entry)
                        .is_some_and(|tail| tail.starts_with(char::is_whitespace)))
        });
        let read_only = matches!(mode, "read-only" | "readonly");
        let workspace_write = matches!(mode, "workspace-write" | "workspace_write");
        let strict_mode = super::command_guard::is_isolated_sandbox_mode(mode);
        let isolation_required = config.enabled || read_only || workspace_write || strict_mode;
        let block_network = config.network.block_all || read_only || workspace_write || strict_mode;
        let backend = if excluded
            || !isolation_required
                && !block_network
                && config.network.allowed_domains.is_empty()
                && !config.network.allow_local_binding
                && !config.network.allow_unix_sockets
        {
            Backend::Direct
        } else {
            ensure!(
                config.network.allowed_domains.is_empty()
                    && !config.network.allow_local_binding
                    && !config.network.allow_unix_sockets,
                "sandbox backend does not support the requested network exceptions"
            );
            platform_backend()?
        };
        #[cfg(unix)]
        let workspace_identity = {
            use std::os::unix::fs::MetadataExt;
            let metadata = workspace.metadata()?;
            (metadata.dev(), metadata.ino())
        };
        if backend != Backend::Direct {
            ensure!(
                workspace.parent().is_some(),
                "a filesystem root cannot be used as an isolated workspace"
            );
            #[cfg(target_os = "linux")]
            for mount in [
                "/usr", "/bin", "/sbin", "/lib", "/lib64", "/proc", "/dev", "/tmp", "/etc",
            ] {
                let mount = Path::new(mount);
                ensure!(
                    !mount.starts_with(&workspace)
                        && (mount == Path::new("/tmp") || !workspace.starts_with(mount)),
                    "workspace overlaps a system sandbox mount"
                );
            }
        }
        let environment = process_env::variables(&workspace, "shell", Source::Project);
        Ok(Self {
            expression: expression.to_owned(),
            workspace,
            #[cfg(unix)]
            workspace_identity,
            environment,
            backend,
            read_only,
            block_network,
        })
    }

    pub(super) fn is_isolated(&self) -> bool {
        self.backend != Backend::Direct
    }

    pub(super) fn restrictions(&self) -> String {
        let backend = match self.backend {
            Backend::Direct => {
                return "Unsandboxed shell · filesystem and network use OS permissions".into();
            }
            #[cfg(target_os = "linux")]
            Backend::Bubblewrap(_) => "Bubblewrap",
            #[cfg(target_os = "macos")]
            Backend::Seatbelt => "Seatbelt",
        };
        let files = if self.read_only {
            "read-only workspace"
        } else {
            "workspace write access"
        };
        let network = if self.block_network {
            "network blocked"
        } else {
            "network unrestricted"
        };
        format!("{backend} · {files} · {network}")
    }

    #[cfg(test)]
    fn run(self, timeout: Duration) -> Result<ShellRunResult> {
        self.run_with_progress(timeout, None, None)
    }

    pub(super) fn run_with_progress(
        self,
        timeout: Duration,
        on_progress: Option<crate::shell::ProgressCallback>,
        cancellation: Option<&codingbuddy_core::CancellationToken>,
    ) -> Result<ShellRunResult> {
        #[cfg(target_os = "macos")]
        let temp = self.is_isolated().then(tempfile::tempdir).transpose()?;
        #[cfg(target_os = "macos")]
        let private_temp = temp
            .as_ref()
            .map(|dir| dir.path().canonicalize())
            .transpose()?;
        let mut command = match &self.backend {
            Backend::Direct => direct_shell(&self.expression)?,
            #[cfg(target_os = "linux")]
            Backend::Bubblewrap(program) => self.bubblewrap(program)?,
            #[cfg(target_os = "macos")]
            Backend::Seatbelt => {
                let mut command = Command::new("/usr/bin/sandbox-exec");
                command.args([
                    "-p",
                    &self.seatbelt(private_temp.as_deref().expect("sandbox temp"))?,
                    "--",
                    "/bin/sh",
                    "-c",
                    &self.expression,
                ]);
                command
            }
        };
        process_env::apply(&mut command, &self.environment);
        #[cfg(target_os = "macos")]
        if let Some(path) = private_temp {
            set_temp(&mut command, &path);
        }
        #[cfg(target_os = "linux")]
        if self.is_isolated() {
            set_temp(&mut command, Path::new("/tmp"));
        }
        #[cfg(not(unix))]
        let _ = on_progress;
        run_shell_launch(
            command,
            &self.workspace,
            timeout,
            cancellation,
            #[cfg(unix)]
            if self.is_isolated() {
                None
            } else {
                on_progress
            },
        )
        .with_context(|| format!("could not launch {}", self.restrictions()))
    }

    #[cfg(target_os = "linux")]
    fn bubblewrap(&self, program: &Path) -> Result<Command> {
        let mut command = Command::new(program);
        command.args([
            "--die-with-parent",
            "--new-session",
            "--unshare-all",
            "--cap-drop",
            "ALL",
        ]);
        if !self.block_network {
            command.arg("--share-net");
        }
        for system in [
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/etc/ld.so.cache",
        ] {
            if Path::new(system).exists() {
                command.args(["--ro-bind", system, system]);
            }
        }
        if !self.block_network {
            for system in [
                "/etc/resolv.conf",
                "/etc/hosts",
                "/etc/nsswitch.conf",
                "/etc/ssl/certs",
            ] {
                if Path::new(system).exists() {
                    command.args(["--ro-bind", system, system]);
                }
            }
        }
        command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        command
            .arg(if self.read_only {
                "--ro-bind"
            } else {
                "--bind"
            })
            .arg(&self.workspace)
            .arg(&self.workspace)
            .arg("--chdir")
            .arg(&self.workspace);
        if self.block_network {
            block_linux_sockets(&mut command)?;
        }
        command.args(["--", "/bin/sh", "-c", &self.expression]);
        Ok(command)
    }

    #[cfg(target_os = "macos")]
    fn seatbelt(&self, temp: &Path) -> Result<String> {
        let ancestor_metadata = self
            .workspace
            .ancestors()
            .skip(1)
            .map(|ancestor| Ok(format!("(literal \"{}\")", profile_path(ancestor)?)))
            .collect::<Result<Vec<_>>>()?
            .join(" ");
        let workspace = profile_path(&self.workspace)?;
        let temp = profile_path(temp)?;
        let workspace_access = if self.read_only {
            "file-read*"
        } else {
            "file-read* file-write*"
        };
        let network = if self.block_network {
            "(deny network*)"
        } else {
            "(allow network*)"
        };
        Ok(format!("(version 1)
(deny default)
(import \"dyld-support.sb\")
(allow process*)
(allow sysctl-read)
(allow file-read* (subpath \"/usr\") (subpath \"/bin\") (subpath \"/System\") (subpath \"/Library/Apple\") (literal \"/dev/urandom\") (literal \"/private/var/select/sh\"))
(allow file-read-metadata {ancestor_metadata})
(allow file-read* file-write* (literal \"/dev/null\"))
(allow {workspace_access} (subpath \"{workspace}\"))
(allow file-read* file-write* (subpath \"{temp}\"))
{network}"))
    }
}

#[cfg(target_os = "macos")]
fn profile_path(path: &Path) -> Result<String> {
    let value = path.to_str().context("sandbox path must be UTF-8")?;
    ensure!(
        !value.contains(char::is_control),
        "sandbox path contains control characters"
    );
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_temp(command: &mut Command, path: &Path) {
    command
        .env("TMPDIR", path)
        .env("TMP", path)
        .env("TEMP", path);
}

fn platform_backend() -> Result<Backend> {
    #[cfg(target_os = "linux")]
    {
        ["/usr/bin/bwrap", "/bin/bwrap"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .map(Backend::Bubblewrap)
            .context("isolation requires Bubblewrap")
    }
    #[cfg(target_os = "macos")]
    {
        ensure!(
            Path::new("/usr/bin/sandbox-exec").is_file(),
            "isolation requires Seatbelt"
        );
        Ok(Backend::Seatbelt)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(anyhow::anyhow!(
            "OS shell isolation is unavailable on this platform"
        ))
    }
}

fn direct_shell(expression: &str) -> Result<Command> {
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
        let path = PathBuf::from(system_root).join("System32").join("cmd.exe");
        ensure!(path.is_absolute(), "SystemRoot must be absolute");
        let mut shell = Command::new(path);
        shell.args(["/D", "/S", "/C"]).arg(expression);
        Ok(shell)
    }
    #[cfg(not(windows))]
    {
        let mut shell = Command::new("/bin/sh");
        shell.args(["-c", expression]);
        Ok(shell)
    }
}

#[cfg(target_os = "linux")]
fn block_linux_sockets(command: &mut Command) -> Result<()> {
    use std::io::{Seek, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    // Reject alternate syscall ABIs and socket creation, including host Unix sockets
    // visible through the workspace mount.
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc00000b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(anyhow::anyhow!(
        "network isolation is unavailable for this Linux architecture"
    ));

    let deny = 0x00050000 | libc::EPERM as u32;
    let instructions = [
        (0x20u16, 0, 0, 4u32), // seccomp_data.arch
        (0x15, 1, 0, arch),
        (0x06, 0, 0, 0x80000000), // SECCOMP_RET_KILL_PROCESS
        (0x20, 0, 0, 0),          // seccomp_data.nr
        (0x35, 3, 0, 0x40000000), // Reject x32 syscalls.
        (0x15, 2, 0, libc::SYS_socket as u32),
        (0x15, 1, 0, libc::SYS_io_uring_setup as u32),
        (0x06, 0, 0, 0x7fff0000), // SECCOMP_RET_ALLOW
        (0x06, 0, 0, deny),
    ];
    let mut filter = tempfile::tempfile()?;
    for (opcode, yes, no, value) in instructions {
        filter.write_all(&opcode.to_ne_bytes())?;
        filter.write_all(&[yes, no])?;
        filter.write_all(&value.to_ne_bytes())?;
    }
    filter.rewind()?;
    command.arg("--seccomp").arg(filter.as_raw_fd().to_string());
    // Bubblewrap reads and closes this descriptor before starting the command.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(filter.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_zero_excluded_launch_is_unsandboxed_and_network_exceptions_fail_closed()
    -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let mut config = SandboxConfig {
            enabled: true,
            excluded_commands: vec!["echo allowed".into()],
            ..Default::default()
        };
        config.network.block_all = true;
        config.network.allow_local_binding = true;
        assert!(
            ShellLaunch::resolve(workspace.path(), "echo allowed-more", "isolated", &config)
                .is_err()
        );
        let launch = ShellLaunch::resolve(workspace.path(), "echo allowed", "isolated", &config)?;
        assert!(!launch.is_isolated());
        assert!(launch.restrictions().contains("Unsandboxed"));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn delivery_zero_windows_isolation_is_unavailable() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let config = SandboxConfig::default();
        for mode in ["isolated", "read-only", "workspace-write"] {
            let error = ShellLaunch::resolve(workspace.path(), "echo marker", mode, &config)
                .expect_err("Windows must fail closed");
            assert!(error.to_string().contains("unavailable"));
        }
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn delivery_zero_native_isolation_enforces_process_tree() -> Result<()> {
        use std::net::TcpListener;
        let fixture = tempfile::tempdir()?;
        let workspace = fixture.path().join("workspace with spaces");
        std::fs::create_dir(&workspace)?;
        let outside = fixture.path().join("outside");
        std::fs::write(&outside, "private-marker")?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let endpoint = listener.local_addr()?;
        let config = SandboxConfig::default();
        let outside_arg = crate::command_guard::shell_quote(outside.to_str().unwrap());
        let cmd = format!(
            r#"printf harmless | /bin/sh -c 'printf kept > inside'; if (cat {outside_arg}); then exit 1; fi; (printf rejected > {outside_arg}) || :; if (OPENSSL_CONF=/dev/null /usr/bin/curl -fsS --noproxy '*' --max-time 2 http://{endpoint}); then exit 3; fi; printf temp > "$TMPDIR/probe"; cat "$TMPDIR/probe""#
        );
        let run = |cmd, mode| {
            ShellLaunch::resolve(&workspace, cmd, mode, &config)?.run(Duration::from_secs(15))
        };
        let result = run(&cmd, "workspace-write")?;
        assert_eq!(result.status, Some(0), "{result:?}");
        assert_eq!(result.stdout, "temp");
        assert_eq!(std::fs::read_to_string(outside)?, "private-marker");
        assert_eq!(std::fs::read_to_string(workspace.join("inside"))?, "kept");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let result = run("printf forbidden > read-only-marker", "read-only")?;
        assert_ne!(result.status, Some(0));
        assert!(!workspace.join("read-only-marker").exists());
        let result = run("/bin/sh -c 'sleep 0.1' | cat", "read-only")?;
        assert_eq!(result.status, Some(0), "{result:?}");
        Ok(())
    }
}
