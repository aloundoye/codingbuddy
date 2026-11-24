//! Ownership shared by command, MCP, verification, and worker execution.
use crate::CancellationToken;
use anyhow::{Result, ensure};
use std::{
    process::{Child, Command},
    time::Instant,
};
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct ExecutionContext {
    pub session_id: Option<Uuid>,
    pub prompt_id: Option<Uuid>,
    pub invocation_id: Option<Uuid>,
    pub parent_run_id: Option<Uuid>,
    pub cancellation: CancellationToken,
    pub deadline: Option<Instant>,
}
impl ExecutionContext {
    pub fn check(&self) -> Result<()> {
        ensure!(!self.cancellation.is_cancelled(), "Execution cancelled");
        ensure!(
            self.deadline
                .is_none_or(|deadline| Instant::now() < deadline),
            "Execution deadline exceeded"
        );
        Ok(())
    }
    pub fn child(&self, parent: Uuid) -> Self {
        Self {
            parent_run_id: Some(parent),
            invocation_id: None,
            cancellation: self.cancellation.child_token(),
            ..self.clone()
        }
    }
}

/// Reaps the process and its ordinary descendants, including on error paths.
/// This owns lifecycle only; filesystem/network isolation remains the launcher's responsibility.
pub struct OwnedChild {
    child: Child,
    #[cfg(unix)]
    group: bool,
    #[cfg(windows)]
    job: windows::Job,
}
impl OwnedChild {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            Ok(Self {
                child: command.spawn()?,
                group: true,
            })
        }
        #[cfg(windows)]
        {
            windows::spawn(command)
        }
        #[cfg(not(any(unix, windows)))]
        {
            anyhow::bail!("Owned process execution unsupported on this platform")
        }
    }
    #[cfg(unix)]
    pub fn from_process_group(child: Child) -> Self {
        Self { child, group: true }
    }
    pub fn terminate(&mut self) {
        #[cfg(unix)]
        if self.group {
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            self.group = false;
        }
        #[cfg(windows)]
        self.job.terminate();
        let _ = self.child.kill();
    }
}
impl std::ops::Deref for OwnedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::{io::AsRawHandle, process::CommandExt};
    use windows_sys::Win32::{
        Foundation::*,
        System::{Diagnostics::ToolHelp::*, JobObjects::*, Threading::*},
    };
    pub struct Job(HANDLE);
    unsafe impl Send for Job {}
    impl Job {
        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    pub fn spawn(command: &mut Command) -> Result<OwnedChild> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            ensure!(!handle.is_null(), "Cannot create owned process job");
            let job = Job(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            ensure!(
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as _,
                    std::mem::size_of_val(&limits) as u32
                ) != 0,
                "Cannot configure owned process job"
            );
            let mut child = command.creation_flags(CREATE_SUSPENDED).spawn()?;
            if AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("Cannot attach child to owned process job");
            }
            let mut owned = OwnedChild { child, job };
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            ensure!(
                snapshot != INVALID_HANDLE_VALUE,
                "Cannot inspect suspended child"
            );
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of_val(&entry) as u32;
            let mut found = false;
            let mut next = Thread32First(snapshot, &mut entry);
            while next != 0 {
                if entry.th32OwnerProcessID == owned.id() {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        found = ResumeThread(thread) != u32::MAX;
                        CloseHandle(thread);
                    }
                    break;
                }
                next = Thread32Next(snapshot, &mut entry);
            }
            CloseHandle(snapshot);
            if !found {
                owned.terminate();
                anyhow::bail!("Cannot resume owned child");
            }
            Ok(owned)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delivery_three_child_cancellation_does_not_cancel_siblings_or_resurrect() {
        let root = CancellationToken::new();
        let a = root.child_token();
        let b = root.child_token();
        let grandchild = a.child_token();
        a.cancel();
        a.reset();
        assert!(grandchild.is_cancelled());
        assert!(!root.is_cancelled());
        assert!(!b.is_cancelled());
        root.cancel();
        root.reset();
        assert!(b.is_cancelled());
        assert!(root.is_cancelled());
    }
}

pub struct OwnedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cancelled: bool,
    pub timed_out: bool,
}
/// Bounded capture, concurrent pipe draining, cancellation and descendant cleanup.
pub fn run_owned(
    command: &mut Command,
    input: Vec<u8>,
    context: &ExecutionContext,
    timeout: std::time::Duration,
) -> Result<OwnedOutput> {
    use std::io::{Read, Write};
    context.check()?;
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = OwnedChild::spawn(command)?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(&input);
            }
        });
        fn drain(reader: Option<impl Read>) -> std::io::Result<Vec<u8>> {
            let mut output = Vec::new();
            if let Some(mut reader) = reader {
                let mut buffer = [0u8; 8192];
                loop {
                    let size = reader.read(&mut buffer)?;
                    if size == 0 {
                        break;
                    }
                    let keep = size.min(2_000_000usize.saturating_sub(output.len()));
                    output.extend_from_slice(&buffer[..keep]);
                }
            }
            Ok(output)
        }
        let out = scope.spawn(move || drain(stdout));
        let err = scope.spawn(move || drain(stderr));
        let started = Instant::now();
        let mut cancelled = false;
        let mut timed_out = false;
        let status = (|| -> Result<_> {
            Ok(loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                cancelled = context.cancellation.is_cancelled();
                timed_out = started.elapsed() >= timeout
                    || context
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline);
                if cancelled || timed_out {
                    child.terminate();
                    break child.wait()?;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            })
        })();
        child.terminate();
        let status = status?;
        let _ = writer.join();
        Ok(OwnedOutput {
            status,
            stdout: out
                .join()
                .map_err(|_| anyhow::anyhow!("Output reader failed"))??,
            stderr: err
                .join()
                .map_err(|_| anyhow::anyhow!("Error reader failed"))??,
            cancelled,
            timed_out: timed_out && !cancelled,
        })
    })
}

#[derive(Debug, thiserror::Error)]
#[error("Execution outcome is unconfirmed; local reconciliation is required before retry")]
pub struct UnconfirmedSideEffect;

#[cfg(test)]
mod delivery_three_process {
    use super::*;
    use std::time::Duration;
    const FIXTURE: &str = "execution::delivery_three_process::fixture";
    #[test]
    fn fixture() {
        match std::env::var("CODINGBUDDY_OWNED_FIXTURE").as_deref() {
            Ok("parent") => {
                let _child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", FIXTURE, "--nocapture"])
                    .env("CODINGBUDDY_OWNED_FIXTURE", "child")
                    .spawn()
                    .unwrap();
                let mut output = std::io::stdout().lock();
                use std::io::Write;
                let bytes = [b'x'; 8192];
                for _ in 0..400 {
                    let _ = output.write_all(&bytes);
                }
                std::thread::sleep(Duration::from_secs(10));
            }
            Ok("child") => {
                std::thread::sleep(Duration::from_millis(750));
                std::fs::write(
                    std::env::var_os("CODINGBUDDY_OWNED_MARKER").unwrap(),
                    "escaped",
                )
                .unwrap();
            }
            _ => {}
        }
    }
    #[test]
    fn timeout_drains_output_and_contains_descendants() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let marker = temp.path().join("marker");
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", FIXTURE, "--nocapture"])
            .env("CODINGBUDDY_OWNED_FIXTURE", "parent")
            .env("CODINGBUDDY_OWNED_MARKER", &marker);
        let started = Instant::now();
        let result = run_owned(
            &mut command,
            vec![],
            &ExecutionContext::default(),
            Duration::from_millis(300),
        )?;
        ensure!(
            result.timed_out && !result.cancelled,
            "Timeout must have its own outcome"
        );
        ensure!(
            started.elapsed() < Duration::from_secs(3),
            "Pipe drain hung"
        );
        ensure!(
            result.stdout.len() <= 2_000_000,
            "Unbounded captured output"
        );
        std::thread::sleep(Duration::from_millis(900));
        ensure!(!marker.exists(), "Descendant escaped timeout");
        Ok(())
    }
    #[test]
    fn cancellation_is_distinct_from_timeout() -> Result<()> {
        let token = CancellationToken::new();
        let context = ExecutionContext {
            cancellation: token.clone(),
            ..Default::default()
        };
        let mut command = Command::new(std::env::current_exe()?);
        let temp = tempfile::tempdir()?;
        command
            .args(["--exact", FIXTURE, "--nocapture"])
            .env("CODINGBUDDY_OWNED_FIXTURE", "parent")
            .env("CODINGBUDDY_OWNED_MARKER", temp.path().join("marker"));
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            token.cancel();
        });
        let result = run_owned(&mut command, vec![], &context, Duration::from_secs(5))?;
        thread.join().unwrap();
        ensure!(
            result.cancelled && !result.timed_out,
            "Cancellation must not be reported as timeout"
        );
        Ok(())
    }
}

pub struct DeadlineGuard {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl ExecutionContext {
    pub fn watch_deadline(&self) -> Result<Option<DeadlineGuard>> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        let token = self.cancellation.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new()
            .name("execution-deadline".into())
            .spawn(move || {
                while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    if Instant::now() >= deadline {
                        token.cancel();
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })?;
        Ok(Some(DeadlineGuard {
            stop,
            thread: Some(thread),
        }))
    }
}
impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
