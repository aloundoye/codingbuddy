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
    resources: Vec<Box<dyn std::any::Any + Send>>,
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
                resources: vec![],
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
        Self {
            child,
            group: true,
            resources: vec![],
        }
    }
    pub fn retain_resource(&mut self, resource: impl std::any::Any + Send) {
        self.resources.push(Box::new(resource));
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
        let mut child = command.creation_flags(CREATE_SUSPENDED).spawn()?;
        match attach(child.as_raw_handle() as HANDLE, child.id()) {
            Ok(job) => Ok(OwnedChild {
                child,
                job,
                resources: vec![],
            }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    /// Put a child that was started with `CREATE_SUSPENDED` into a fresh
    /// kill-on-close job and resume it. On error the caller kills the child.
    pub fn attach(process: HANDLE, pid: u32) -> Result<Job> {
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
            ensure!(
                AssignProcessToJobObject(handle, process) != 0,
                "Cannot attach child to owned process job"
            );
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
                if entry.th32OwnerProcessID == pid {
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
                job.terminate();
                anyhow::bail!("Cannot resume owned child");
            }
            Ok(job)
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
/// Most output kept per stream; the rest is read and discarded so the child
/// never blocks on a full pipe.
pub const MAX_CAPTURE: usize = 2_000_000;

/// A child process owned by async code: started in its own process group
/// (a kill-on-close Job object on Windows) so cancelling or dropping it kills
/// every descendant, not just the direct child.
pub struct AsyncChild {
    child: tokio::process::Child,
    resources: Vec<Box<dyn std::any::Any + Send + Sync>>,
    #[cfg(unix)]
    group: Option<i32>,
    #[cfg(windows)]
    job: Option<windows::Job>,
}

impl AsyncChild {
    /// Spawn `command` in a new process group.
    pub fn spawn(command: &mut tokio::process::Command) -> Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        {
            command.process_group(0);
            let child = command.spawn()?;
            let group = child.id().map(|pid| pid as i32);
            Ok(Self {
                child,
                resources: vec![],
                group,
            })
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
            command.creation_flags(CREATE_SUSPENDED);
            let mut child = command.spawn()?;
            let attached = match (child.raw_handle(), child.id()) {
                (Some(handle), Some(pid)) => windows::attach(handle as _, pid),
                _ => Err(anyhow::anyhow!("Child exited before it could be owned")),
            };
            match attached {
                Ok(job) => Ok(Self {
                    child,
                    resources: vec![],
                    job: Some(job),
                }),
                Err(error) => {
                    let _ = child.start_kill();
                    Err(error)
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            anyhow::bail!("Owned process execution unsupported on this platform")
        }
    }

    /// Own a child the caller already placed in its own session or process
    /// group (for example a PTY child that called `setsid`).
    #[cfg(unix)]
    pub fn from_process_group(child: tokio::process::Child) -> Self {
        let group = child.id().map(|pid| pid as i32);
        Self {
            child,
            resources: vec![],
            group,
        }
    }

    /// Keep `resource` alive as long as the child (for example a temporary
    /// sandbox profile).
    pub fn retain_resource(&mut self, resource: impl std::any::Any + Send + Sync) {
        self.resources.push(Box::new(resource));
    }

    /// Kill the whole process tree now. Idempotent; reaping happens in
    /// [`AsyncChild::wait`] or, after drop, in the background.
    pub fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            // SAFETY: plain syscall; a stale group id only yields ESRCH.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        let _ = self.child.start_kill();
    }
}

impl std::ops::Deref for AsyncChild {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for AsyncChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for AsyncChild {
    fn drop(&mut self) {
        // `kill_on_drop` makes tokio reap the direct child in the background.
        self.terminate();
    }
}

/// Receives raw output chunks as they arrive.
pub type ChunkCallback = std::sync::Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Read `reader` to the end into `sink`, keeping at most [`MAX_CAPTURE`]
/// bytes. Shared so a caller can keep what arrived if it stops waiting.
pub async fn drain_into(
    reader: Option<impl tokio::io::AsyncRead + Unpin>,
    sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    on_chunk: Option<ChunkCallback>,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return Ok(());
    };
    let mut buffer = [0u8; 8192];
    loop {
        let size = match reader.read(&mut buffer).await {
            Ok(size) => size,
            // A PTY master reports EIO once the last slave fd closes.
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(libc::EIO) => 0,
            Err(error) => return Err(error),
        };
        if size == 0 {
            return Ok(());
        }
        if let Some(on_chunk) = &on_chunk {
            on_chunk(&buffer[..size]);
        }
        let mut output = sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let keep = size.min(MAX_CAPTURE.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..keep]);
    }
}

/// How long to keep reading pipes after the process tree is gone. Only a
/// descendant that escaped the group (a daemon) can hold them open longer.
const PIPE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Collect a drain task's output, giving up after [`PIPE_GRACE`].
pub async fn finish_drain(
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) -> Result<Vec<u8>> {
    let abort = task.abort_handle();
    match tokio::time::timeout(PIPE_GRACE, task).await {
        Ok(Ok(result)) => result?,
        Ok(Err(error)) if error.is_panic() => anyhow::bail!("Output reader failed"),
        Ok(Err(_)) => {}
        Err(_) => abort.abort(),
    }
    Ok(std::mem::take(
        &mut *sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ))
}

/// How a supervised child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Exited,
    Cancelled,
    TimedOut,
}

/// Wait for `child` to exit, for `cancellation` to fire or for `deadline`,
/// whichever comes first. The process tree is killed on every path, so no
/// descendant outlives the call.
pub async fn supervise(
    child: &mut AsyncChild,
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
) -> Result<(std::process::ExitStatus, Ending)> {
    let ending = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(Ending::Cancelled),
        () = tokio::time::sleep_until(deadline) => Err(Ending::TimedOut),
        status = child.wait() => Ok(status?),
    };
    // Reap descendants even when the direct child exited on its own.
    child.terminate();
    Ok(match ending {
        Ok(status) => (status, Ending::Exited),
        Err(ending) => (child.wait().await?, ending),
    })
}

/// The deadline for a call: `timeout` from now, or the context's own
/// deadline if that comes first.
pub fn deadline_for(
    context: &ExecutionContext,
    timeout: std::time::Duration,
) -> tokio::time::Instant {
    let by_timeout = tokio::time::Instant::now() + timeout;
    context
        .deadline
        .map(tokio::time::Instant::from_std)
        .map_or(by_timeout, |deadline| deadline.min(by_timeout))
}

/// Bounded capture, concurrent pipe draining, cancellation and descendant
/// cleanup, without holding a thread while the child runs.
pub async fn run_owned_async(
    command: &mut tokio::process::Command,
    input: Vec<u8>,
    context: &ExecutionContext,
    timeout: std::time::Duration,
) -> Result<OwnedOutput> {
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;
    context.check()?;
    let deadline = deadline_for(context, timeout);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = AsyncChild::spawn(command)?;
    let stdin = child.stdin.take();
    let writer = tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&input).await;
        }
    });
    let out_sink = Arc::new(Mutex::new(Vec::new()));
    let err_sink = Arc::new(Mutex::new(Vec::new()));
    let out = tokio::spawn(drain_into(child.stdout.take(), out_sink.clone(), None));
    let err = tokio::spawn(drain_into(child.stderr.take(), err_sink.clone(), None));
    let (status, ending) = supervise(&mut child, &context.cancellation, deadline).await?;
    writer.abort();
    Ok(OwnedOutput {
        status,
        stdout: finish_drain(out, out_sink).await?,
        stderr: finish_drain(err, err_sink).await?,
        cancelled: ending == Ending::Cancelled,
        timed_out: ending == Ending::TimedOut,
    })
}

/// [`run_owned_async`] for standalone (non-async) callers.
pub fn run_owned(
    command: &mut Command,
    input: Vec<u8>,
    context: &ExecutionContext,
    timeout: std::time::Duration,
) -> Result<OwnedOutput> {
    let mut command = tokio::process::Command::from(std::mem::replace(command, Command::new("")));
    crate::runtime::block_on(run_owned_async(&mut command, input, context, timeout))
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
                let mut child = Command::new(std::env::current_exe().unwrap())
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
                let _ = child.wait();
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

/// Cancels the context's token at its deadline until dropped.
pub struct DeadlineGuard {
    task: tokio::task::JoinHandle<()>,
}
impl ExecutionContext {
    pub fn watch_deadline(&self) -> Result<Option<DeadlineGuard>> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        let token = self.cancellation.clone();
        let task = crate::runtime::runtime().spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            token.cancel();
        });
        Ok(Some(DeadlineGuard { task }))
    }
}
impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}
