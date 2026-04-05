use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use wait_timeout::ChildExt;

/// Whether to prefer PTY-based execution for colored output from programs
/// that check `isatty()`. Falls back to pipe-based execution on failure.
pub static USE_PTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRunResult {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Callback for streaming live output from a running command.
pub type ProgressCallback = Arc<dyn Fn(&str) + Send + Sync>;

pub trait ShellRunner {
    fn run(&self, cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellRunResult>;

    /// Run with a progress callback that receives live output chunks.
    fn run_with_progress(
        &self,
        cmd: &str,
        cwd: &Path,
        timeout: Duration,
        on_progress: ProgressCallback,
    ) -> Result<ShellRunResult> {
        // Default: ignore progress callback, just run normally
        let _ = on_progress;
        self.run(cmd, cwd, timeout)
    }
}

#[derive(Debug, Default)]
pub struct PlatformShellRunner;

impl ShellRunner for PlatformShellRunner {
    fn run(&self, cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellRunResult> {
        let mut child = spawn_command(cmd, cwd)?;

        let status = child.wait_timeout(timeout)?;
        if status.is_none() {
            // Timeout: escalate SIGTERM → SIGKILL
            #[cfg(unix)]
            {
                // Send SIGTERM first for graceful shutdown
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
                }
                // Wait briefly for graceful exit, then force kill
                if child.wait_timeout(Duration::from_secs(2))?.is_none() {
                    let _ = child.kill(); // SIGKILL
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
            }
            let output = child.wait_with_output()?;
            return Ok(ShellRunResult {
                status: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                timed_out: true,
            });
        }

        let output = child.wait_with_output()?;
        Ok(ShellRunResult {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            timed_out: false,
        })
    }
}

/// PTY-aware shell runner. Programs see a real terminal (isatty() = true)
/// so they emit colored output. Falls back to pipe-based execution on failure.
#[derive(Debug, Default)]
pub struct PtyShellRunner;

impl ShellRunner for PtyShellRunner {
    fn run(&self, cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellRunResult> {
        #[cfg(unix)]
        if USE_PTY.load(std::sync::atomic::Ordering::Relaxed)
            && let Ok(result) = run_with_pty(cmd, cwd, timeout, None)
        {
            return Ok(result);
        }
        // Fallback to standard pipe-based execution
        PlatformShellRunner.run(cmd, cwd, timeout)
    }

    fn run_with_progress(
        &self,
        cmd: &str,
        cwd: &Path,
        timeout: Duration,
        on_progress: ProgressCallback,
    ) -> Result<ShellRunResult> {
        #[cfg(unix)]
        if USE_PTY.load(std::sync::atomic::Ordering::Relaxed)
            && let Ok(result) = run_with_pty(cmd, cwd, timeout, Some(on_progress))
        {
            return Ok(result);
        }
        PlatformShellRunner.run(cmd, cwd, timeout)
    }
}

/// Run a command with a pseudo-terminal so child process sees isatty()=true.
#[cfg(unix)]
fn run_with_pty(
    cmd: &str,
    cwd: &Path,
    timeout: Duration,
    on_progress: Option<ProgressCallback>,
) -> Result<ShellRunResult> {
    use std::io::Read;
    use std::os::fd::FromRawFd;

    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    // Open a PTY pair
    let mut master_fd: libc::c_int = 0;
    let mut slave_fd: libc::c_int = 0;
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

    let slave_fd_copy = slave_fd;
    let mut command = Command::new("sh");
    command.arg("-lc").arg(cmd);
    command.current_dir(&cwd);
    command.stdin(Stdio::null());
    command.env("TERM", "xterm-256color");
    harden_command_env(&mut command);

    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            libc::setsid();
            libc::dup2(slave_fd_copy, 1);
            libc::dup2(slave_fd_copy, 2);
            libc::close(slave_fd_copy);
            Ok(())
        });
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            unsafe {
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            return Err(e.into());
        }
    };

    unsafe {
        libc::close(slave_fd);
    }

    // Read from master with timeout
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let mut output = Vec::new();
    let mut buf = [0u8; 8192];
    let deadline = std::time::Instant::now() + timeout;

    // Set master to non-blocking
    unsafe {
        let flags = libc::fcntl(master_fd, libc::F_GETFL);
        libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let mut timed_out = false;
    loop {
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_millis(100));
            let _ = child.kill();
            break;
        }
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                output.extend_from_slice(&buf[..n]);
                // Stream live progress to callback (raw lossy text —
                // final result does the authoritative ANSI strip)
                if let Some(ref cb) = on_progress {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    let trimmed = chunk.trim();
                    if !trimmed.is_empty() {
                        cb(trimmed);
                    }
                }
                if output.len() > 2_000_000 {
                    let _ = child.kill();
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Check if child exited
                if let Ok(Some(_)) = child.try_wait() {
                    // Drain remaining output
                    loop {
                        match master.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => output.extend_from_slice(&buf[..n]),
                        }
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }

    let status = child.wait().ok().and_then(|s| s.code());
    let text = String::from_utf8_lossy(&output).to_string();
    let clean = strip_ansi_escapes(&text);

    Ok(ShellRunResult {
        status,
        stdout: clean,
        stderr: String::new(), // PTY merges stdout+stderr
        timed_out,
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
fn harden_command_env(command: &mut Command) {
    command.env("LC_ALL", "C");
    command.env_remove("LD_PRELOAD");
    command.env_remove("DYLD_INSERT_LIBRARIES");
    command.env_remove("LD_LIBRARY_PATH");
    command.env_remove("DYLD_LIBRARY_PATH");
}

fn spawn_command(cmd: &str, cwd: &Path) -> Result<Child> {
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
    let mut errors = Vec::new();
    for mut command in candidate_commands(cmd) {
        command.current_dir(&cwd);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.stdin(Stdio::null());
        harden_command_env(&mut command);
        let program = command.get_program().to_string_lossy().to_string();
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(err) => errors.push(format!("{program}: {err}")),
        }
    }
    Err(anyhow!(
        "failed to spawn command '{cmd}' in '{}': {}",
        cwd.display(),
        errors.join(" | ")
    ))
}

#[cfg(target_os = "windows")]
fn candidate_commands(cmd: &str) -> Vec<Command> {
    let mut commands = Vec::new();
    let mut cmd_shell = Command::new("cmd");
    cmd_shell.arg("/C").arg(cmd);
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
    sh_shell.arg("-lc").arg(cmd);
    commands.push(sh_shell);

    let mut bash_shell = Command::new("bash");
    bash_shell.arg("-lc").arg(cmd);
    commands.push(bash_shell);

    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_runner_executes_command() {
        let runner = PlatformShellRunner;
        let out = runner
            .run("echo deepseek", Path::new("."), Duration::from_secs(2))
            .expect("run command");
        assert!(!out.timed_out);
        assert!(out.stdout.to_lowercase().contains("deepseek"));
    }
}
