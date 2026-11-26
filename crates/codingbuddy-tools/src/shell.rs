use anyhow::{Result, anyhow};
use codingbuddy_core::CancellationToken;
use codingbuddy_core::execution::{AsyncChild, Ending, drain_into, finish_drain, supervise};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Whether to prefer PTY-based execution for colored output from programs
/// that check `isatty()`. Falls back to pipe-based execution on failure.
pub static USE_PTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRunResult {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
}

/// Callback for streaming live output from a running command.
pub type ProgressCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Runs shell commands as async child processes: waiting holds no thread,
/// and timeout or cancellation kills the whole process group.
#[async_trait::async_trait]
pub trait ShellRunner {
    async fn run(&self, cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellRunResult>;

    async fn run_verification(
        &self,
        cmd: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ShellRunResult> {
        self.run(cmd, cwd, timeout).await
    }

    /// Run with a progress callback that receives live output chunks.
    async fn run_with_progress(
        &self,
        cmd: &str,
        cwd: &Path,
        timeout: Duration,
        on_progress: ProgressCallback,
    ) -> Result<ShellRunResult> {
        // Default: ignore progress callback, just run normally
        let _ = on_progress;
        self.run(cmd, cwd, timeout).await
    }
}

#[derive(Debug, Default)]
pub struct PlatformShellRunner;

#[async_trait::async_trait]
impl ShellRunner for PlatformShellRunner {
    async fn run(&self, cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellRunResult> {
        run_shell(cmd, cwd, timeout, "shell").await
    }

    async fn run_verification(
        &self,
        cmd: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ShellRunResult> {
        run_shell(cmd, cwd, timeout, "language_tools").await
    }
}

async fn run_shell(
    cmd: &str,
    cwd: &Path,
    timeout: Duration,
    consumer: &str,
) -> Result<ShellRunResult> {
    let command = shell_command(cmd, cwd, consumer)?;
    spawn_piped(command, cwd, timeout, None).await
}

pub(crate) async fn run_command(
    mut command: Command,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    cwd: &Path,
    timeout: Duration,
) -> Result<ShellRunResult> {
    command.args(args);
    spawn_piped(command, cwd, timeout, None).await
}

pub(crate) async fn run_shell_launch(
    command: Command,
    cwd: &Path,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    #[cfg(unix)] on_progress: Option<ProgressCallback>,
) -> Result<ShellRunResult> {
    #[cfg(unix)]
    if let Some(callback) = on_progress
        && USE_PTY.load(std::sync::atomic::Ordering::Relaxed)
    {
        return run_prepared_with_pty(command, cwd, timeout, Some(callback), cancellation).await;
    }
    spawn_piped(command, cwd, timeout, cancellation).await
}

/// A token that never fires, for callers without one.
fn token_or_never(cancellation: Option<&CancellationToken>) -> CancellationToken {
    cancellation.cloned().unwrap_or_default()
}

async fn spawn_piped(
    command: Command,
    cwd: &Path,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
) -> Result<ShellRunResult> {
    let mut command = tokio::process::Command::from(command);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let deadline = tokio::time::Instant::now() + timeout;
    let mut child = AsyncChild::spawn(&mut command)?;
    let out_sink = Arc::new(Mutex::new(Vec::new()));
    let err_sink = Arc::new(Mutex::new(Vec::new()));
    let stdout = tokio::spawn(drain_into(child.stdout.take(), out_sink.clone(), None));
    let stderr = tokio::spawn(drain_into(child.stderr.take(), err_sink.clone(), None));
    let (status, ending) = supervise(&mut child, &token_or_never(cancellation), deadline).await?;
    Ok(ShellRunResult {
        status: status.code(),
        stdout: String::from_utf8_lossy(&finish_drain(stdout, out_sink).await?).into_owned(),
        stderr: String::from_utf8_lossy(&finish_drain(stderr, err_sink).await?).into_owned(),
        timed_out: ending == Ending::TimedOut,
        cancelled: ending == Ending::Cancelled,
    })
}

/// PTY-aware shell runner. Programs see a real terminal (isatty() = true)
/// so they emit colored output. A failed PTY launch is never replayed.
#[derive(Debug, Default)]
pub struct PtyShellRunner;

#[async_trait::async_trait]
impl ShellRunner for PtyShellRunner {
    async fn run_verification(
        &self,
        cmd: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ShellRunResult> {
        PlatformShellRunner
            .run_verification(cmd, cwd, timeout)
            .await
    }

    async fn run(&self, cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellRunResult> {
        #[cfg(unix)]
        if USE_PTY.load(std::sync::atomic::Ordering::Relaxed) {
            return run_with_pty(cmd, cwd, timeout, None).await;
        }
        // Fallback to standard pipe-based execution
        PlatformShellRunner.run(cmd, cwd, timeout).await
    }

    async fn run_with_progress(
        &self,
        cmd: &str,
        cwd: &Path,
        timeout: Duration,
        on_progress: ProgressCallback,
    ) -> Result<ShellRunResult> {
        #[cfg(unix)]
        if USE_PTY.load(std::sync::atomic::Ordering::Relaxed) {
            return run_with_pty(cmd, cwd, timeout, Some(on_progress)).await;
        }
        PlatformShellRunner.run(cmd, cwd, timeout).await
    }
}

/// Run a command with a pseudo-terminal so child process sees isatty()=true.
#[cfg(unix)]
async fn run_with_pty(
    cmd: &str,
    cwd: &Path,
    timeout: Duration,
    on_progress: Option<ProgressCallback>,
) -> Result<ShellRunResult> {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(cmd);
    harden_command_env(&mut command, cwd, "shell");
    command.env("TERM", "xterm-256color");
    run_prepared_with_pty(command, cwd, timeout, on_progress, None).await
}

/// The master side of a PTY, readable without blocking a thread.
#[cfg(unix)]
struct PtyMaster(tokio::io::unix::AsyncFd<std::fs::File>);

#[cfg(unix)]
impl tokio::io::AsyncRead for PtyMaster {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::io::Read;
        loop {
            let mut guard = std::task::ready!(self.0.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| {
                let mut file: &std::fs::File = inner.get_ref();
                file.read(unfilled)
            }) {
                Ok(Ok(count)) => {
                    buf.advance(count);
                    return std::task::Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return std::task::Poll::Ready(Err(error)),
                Err(_would_block) => continue,
            }
        }
    }
}

#[cfg(unix)]
async fn run_prepared_with_pty(
    mut command: Command,
    cwd: &Path,
    timeout: Duration,
    on_progress: Option<ProgressCallback>,
    cancellation: Option<&CancellationToken>,
) -> Result<ShellRunResult> {
    use std::os::fd::FromRawFd;

    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    // Open a PTY pair
    let mut master_fd: libc::c_int = 0;
    let mut slave_fd: libc::c_int = 0;
    // SAFETY: openpty writes two fds into the provided out-parameters.
    let ret = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        return Err(anyhow!("openpty failed"));
    }
    // SAFETY: openpty just returned these fds; each is owned exactly once.
    let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::os::fd::OwnedFd::from_raw_fd(slave_fd) };

    command.current_dir(&cwd);
    command.stdin(Stdio::null());

    let slave_raw = slave_fd;
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            libc::setsid();
            libc::dup2(slave_raw, 1);
            libc::dup2(slave_raw, 2);
            libc::close(slave_raw);
            Ok(())
        });
    }

    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    // setsid makes the child its own process-group leader.
    let mut child = AsyncChild::from_process_group(command.spawn()?);
    // The child holds its copy; ours must close so EOF arrives on exit.
    drop(slave);

    // SAFETY: plain fcntl on a valid fd we own.
    unsafe {
        let flags = libc::fcntl(master_fd, libc::F_GETFL);
        libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    // SAFETY: `master` owns the fd exclusively and AsyncFd owns `master`, so
    // the fd stays open (and is not reused) for as long as it is registered.
    let master = PtyMaster(unsafe { tokio::io::unix::AsyncFd::register(master) }?);

    let deadline = tokio::time::Instant::now() + timeout;
    let token = token_or_never(cancellation);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let on_chunk = on_progress.map(|callback| -> codingbuddy_core::execution::ChunkCallback {
        Arc::new(move |bytes: &[u8]| {
            // Raw lossy text; the final result does the authoritative strip.
            let chunk = String::from_utf8_lossy(bytes);
            let trimmed = chunk.trim();
            if !trimmed.is_empty() {
                callback(trimmed);
            }
        })
    });
    let reader = tokio::spawn(drain_into(Some(master), sink.clone(), on_chunk));
    let (status, ending) = supervise(&mut child, &token, deadline).await?;
    let output = finish_drain(reader, sink).await?;
    let text = String::from_utf8_lossy(&output).to_string();

    Ok(ShellRunResult {
        status: status.code(),
        stdout: strip_ansi_escapes(&text),
        stderr: String::new(), // PTY merges stdout+stderr
        timed_out: ending == Ending::TimedOut,
        cancelled: ending == Ending::Cancelled,
    })
}

/// Strip ANSI escape sequences from terminal output.
fn strip_ansi_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip CSI sequences: ESC [ ... final_byte
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    // CSI final byte range: 0x40-0x7E per ECMA-48
                    if (0x40..=0x7E).contains(&(next as u32)) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                // OSC: skip until ST (ESC \ or BEL)
                chars.next();
                for c in chars.by_ref() {
                    if c == '\x07' || c == '\x1b' {
                        break;
                    }
                }
            }
        } else if c == '\r' {
            // Skip carriage returns (PTY outputs \r\n)
            continue;
        } else {
            result.push(c);
        }
    }
    result
}

/// Harden a command's environment to prevent injection and locale issues.
fn harden_command_env(command: &mut Command, workspace: &Path, consumer: &str) {
    use codingbuddy_core::process_env::{Source, apply_for};
    apply_for(command, workspace, consumer, Source::Project);
    command.env("LC_ALL", "C");
}

fn shell_command(cmd: &str, cwd: &Path, consumer: &str) -> Result<Command> {
    // Always attempt canonicalization to resolve symlinks and prevent path traversal.
    // For non-existent paths, try canonicalizing the nearest existing parent.
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| {
        // Fallback: canonicalize the parent chain to resolve what we can
        let mut path = cwd.to_path_buf();
        while let Some(parent) = path.parent() {
            if let Ok(canonical_parent) = std::fs::canonicalize(parent)
                && let Ok(remainder) = path.strip_prefix(parent)
            {
                return canonical_parent.join(remainder);
            }
            path = parent.to_path_buf();
        }
        cwd.to_path_buf()
    });
    let vars = codingbuddy_core::process_env::variables(
        &cwd,
        consumer,
        codingbuddy_core::process_env::Source::Project,
    );
    for mut command in candidate_commands(cmd) {
        let program = command.get_program().to_string_lossy();
        if codingbuddy_core::process_env::find_program(&program, &vars).is_none()
            && !std::path::Path::new(program.as_ref()).is_absolute()
        {
            continue;
        }
        command.current_dir(&cwd).stdin(Stdio::null());
        harden_command_env(&mut command, &cwd, consumer);
        return Ok(command);
    }
    Err(anyhow!("No supported shell is available"))
}

#[cfg(target_os = "windows")]
fn candidate_commands(cmd: &str) -> Vec<Command> {
    let mut commands = Vec::new();
    let mut cmd_shell = Command::new("cmd");
    cmd_shell.args(["/D", "/C"]).arg(cmd);
    commands.push(cmd_shell);

    let mut ps_shell = Command::new("powershell");
    ps_shell
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(cmd);
    commands.push(ps_shell);

    commands
}

#[cfg(not(target_os = "windows"))]
fn candidate_commands(cmd: &str) -> Vec<Command> {
    let mut commands = Vec::new();
    let mut sh_shell = Command::new("sh");
    sh_shell.arg("-c").arg(cmd);
    commands.push(sh_shell);

    let mut bash_shell = Command::new("bash");
    bash_shell.arg("-c").arg(cmd);
    commands.push(bash_shell);

    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_runner_executes_command() {
        let runner = PlatformShellRunner;
        let out = codingbuddy_core::runtime::block_on(runner.run(
            "echo deepseek",
            Path::new("."),
            Duration::from_secs(2),
        ))
        .expect("run command");
        assert!(!out.timed_out);
        assert!(out.stdout.to_lowercase().contains("deepseek"));
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_a_command_kills_its_whole_process_group_promptly() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pid_file = dir.path().join("grandchild.pid");
        // The shell backgrounds a long sleep (a grandchild of ours) and waits.
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "sleep 30 & echo $! > '{}'; wait",
            pid_file.display()
        ));
        let token = CancellationToken::new();
        let canceller = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            canceller.cancel();
        });

        let started = std::time::Instant::now();
        let result = codingbuddy_core::runtime::block_on(run_shell_launch(
            command,
            dir.path(),
            Duration::from_secs(60),
            Some(&token),
            None,
        ))?;
        assert!(result.cancelled, "result should report cancellation");
        assert!(!result.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cancellation took {:?}",
            started.elapsed()
        );

        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)?.trim().parse()?;
        // The grandchild was in the killed group; allow a moment for reaping.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid, 0) } == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "background grandchild {pid} survived cancellation"
        );
        Ok(())
    }
}
