//! The process-wide async runtime and cancellation-aware helpers.
//!
//! All network and subprocess I/O runs on one multi-threaded tokio runtime
//! instead of a runtime per client. Synchronous callers enter it through
//! [`block_on`] / [`block_on_cancellable`]; async code uses
//! [`sleep_cancellable`] and [`with_cancellation`] so a turn stops the moment
//! its [`CancellationToken`] fires, without polling.

use crate::CancellationToken;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

/// Returned when a [`CancellationToken`] fires before the work finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Matches the message the transport has always used, which callers
        // and tests recognise as an interruption rather than a failure.
        f.write_str("Turn interrupted")
    }
}

impl std::error::Error for Cancelled {}

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// The shared runtime, created on first use.
///
/// # Panics
/// Panics if the OS refuses to create the runtime's threads, which leaves
/// the process unable to do any I/O.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(
                std::thread::available_parallelism()
                    .map_or(2, |n| n.get())
                    .clamp(2, 4),
            )
            .thread_name("codingbuddy-io")
            .enable_all()
            .build()
            // invariant: creating a handful of threads only fails when the
            // process is out of resources.
            .expect("failed to start the async runtime")
    })
}

/// Run `future` to completion from synchronous code.
///
/// Safe to call from inside a runtime thread too: the future then runs on a
/// helper thread instead of nesting `block_on`, which tokio forbids.
pub fn block_on<F>(future: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(|| runtime().block_on(future))
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    } else {
        runtime().block_on(future)
    }
}

/// Race `future` against `token`. The future is dropped (and so cancelled)
/// as soon as the token fires.
pub async fn with_cancellation<F: Future>(
    token: Option<&CancellationToken>,
    future: F,
) -> Result<F::Output, Cancelled> {
    let Some(token) = token else {
        return Ok(future.await);
    };
    if token.is_cancelled() {
        return Err(Cancelled);
    }
    tokio::select! {
        biased;
        () = token.cancelled() => Err(Cancelled),
        value = future => Ok(value),
    }
}

/// [`block_on`] that gives up as soon as `token` is cancelled.
pub fn block_on_cancellable<F>(
    token: Option<&CancellationToken>,
    future: F,
) -> Result<F::Output, Cancelled>
where
    F: Future + Send,
    F::Output: Send,
{
    block_on(with_cancellation(token, future))
}

/// Sleep for `duration`, waking early with `Err(Cancelled)` on cancellation.
/// Use this for retry backoff instead of `thread::sleep`.
pub async fn sleep_cancellable(
    duration: Duration,
    token: Option<&CancellationToken>,
) -> Result<(), Cancelled> {
    with_cancellation(token, tokio::time::sleep(duration)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn cancel_wakes_a_waiting_sleep_immediately() {
        let token = CancellationToken::new();
        let canceller = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            canceller.cancel();
        });
        let started = Instant::now();
        let result = block_on(sleep_cancellable(Duration::from_secs(30), Some(&token)));
        assert_eq!(result, Err(Cancelled));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn cancelling_a_parent_wakes_a_child_waiter() {
        let parent = CancellationToken::new();
        let child = parent.child_token().child_token();
        let canceller = parent.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            canceller.cancel();
        });
        let result = block_on_cancellable(Some(&child), async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            "finished"
        });
        assert_eq!(result, Err(Cancelled));
    }

    #[test]
    fn child_cancel_does_not_reach_parent() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
        assert_eq!(block_on_cancellable(Some(&parent), async { 7 }), Ok(7));
    }

    #[test]
    fn already_cancelled_token_short_circuits() {
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            block_on_cancellable(Some(&token), async { 1 }),
            Err(Cancelled)
        );
        assert_eq!(block_on_cancellable(None, async { 1 }), Ok(1));
    }

    #[test]
    fn block_on_works_from_inside_the_runtime() {
        let value = runtime().block_on(async { block_on(async { 41 + 1 }) });
        assert_eq!(value, 42);
    }
}
