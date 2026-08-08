// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The executor seam and the two built-in executors.
//!
//! `winasio-axum` delegates *how* concurrent request work runs to a
//! caller-supplied [`Executor`], so the crate itself depends on no async
//! runtime. The trait's shape mirrors [`hyper::rt::Executor`]: a single
//! `execute` method a foreign runtime user can satisfy in a few lines.
//!
//! # D1. Why `Executor` carries a `poll_progress` hook
//!
//! A *spawning* executor (one that hands each future to a runtime or a fresh
//! thread) needs only `execute`. A *current-thread* executor has nowhere to run
//! the futures it is handed unless the serve loop drives them, so the trait adds
//! one defaulted method, [`Executor::poll_progress`], that the loop polls each
//! turn. Its default returns [`Poll::Ready`] — a spawning executor has nothing
//! for the loop to drive — so a foreign impl stays a single method.
//!
//! **Rejected alternatives**: a bare single-method trait leaves a current-thread
//! executor undrivable; a second standalone "drive" trait forces boilerplate on
//! the common spawning case. The defaulted hook keeps foreign impls to one
//! method while making the current-thread executor drivable.
//!
//! [`hyper::rt::Executor`]: https://docs.rs/hyper/latest/hyper/rt/trait.Executor.html

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use futures_util::stream::{FuturesUnordered, StreamExt};

/// A boxed request-handling task the driver hands to an [`Executor`].
///
/// The output is `()` — a request task routes its own result (and any caught
/// panic) to the serve loop's error observer before returning, so the executor
/// never has to inspect it.
///
/// # D2. Why the task is `Send + 'static` for both executors
///
/// A single task type keeps the seam one uniform `Executor<RequestTask>` bound.
/// This means even [`CurrentThread`] — which runs everything on one thread and
/// could in principle drive `!Send` futures — requires `Send` tasks, slightly
/// stronger than strictly necessary. The alternative (a separate `!Send` task
/// type, doubling the seam and the request plumbing) was rejected because the
/// owned request future produced on the thread-pool backend, and `axum::Router`
/// itself, are already `Send`, so nothing is lost in practice.
pub type RequestTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// How concurrent request work is run.
///
/// Implement this for your runtime to plug it in without `winasio-axum` taking a
/// dependency on it. A spawning executor implements only [`execute`]; the
/// defaulted [`poll_progress`] suffices because the loop has nothing to drive.
///
/// ```
/// use winasio_axum::Executor;
///
/// // A tiny adapter for a runtime whose spawn takes any `Send + 'static` future.
/// struct MyRuntime;
/// impl<F: std::future::Future<Output = ()> + Send + 'static> Executor<F> for MyRuntime {
///     fn execute(&self, fut: F) {
///         // my_runtime::spawn(fut);
///         std::thread::spawn(move || futures::executor::block_on(fut));
///     }
///     // poll_progress defaulted: a spawning executor needs no loop driving.
/// }
/// ```
///
/// [`execute`]: Executor::execute
/// [`poll_progress`]: Executor::poll_progress
pub trait Executor<Fut> {
    /// Dispatch one task. A spawning executor hands `fut` to its runtime or a
    /// fresh thread; a current-thread executor stores it to drive in
    /// [`poll_progress`].
    ///
    /// [`poll_progress`]: Executor::poll_progress
    fn execute(&self, fut: Fut);

    /// Drive already-dispatched work that lives on the caller's thread.
    ///
    /// The serve loop polls this every turn so a current-thread executor makes
    /// progress on in-flight requests. The default returns [`Poll::Ready`] — a
    /// spawning executor has nothing for the loop to drive. The loop consumes
    /// this only to make progress and register wakers; it never awaits it to
    /// completion (there is no drain), so the `Ready`/`Pending` distinction
    /// cannot stall shutdown.
    fn poll_progress(&self, cx: &mut Context<'_>) -> Poll<()> {
        let _ = cx;
        Poll::Ready(())
    }
}

/// A current-thread executor: drives many in-flight request futures concurrently
/// on the caller's own thread, spawning nothing and requiring no runtime.
///
/// Concurrency comes from a [`FuturesUnordered`] the serve loop drains through
/// [`Executor::poll_progress`]; many request futures live in the set at once and
/// interleave as the loop polls. This is [`Send`] but not [`Sync`] (it holds a
/// [`RefCell`]) — correct, because it is used only on the single loop thread.
#[derive(Default)]
pub struct CurrentThread {
    tasks: RefCell<FuturesUnordered<RequestTask>>,
}

impl CurrentThread {
    /// Create an empty current-thread executor.
    pub fn new() -> Self {
        Self {
            tasks: RefCell::new(FuturesUnordered::new()),
        }
    }
}

impl Executor<RequestTask> for CurrentThread {
    fn execute(&self, fut: RequestTask) {
        self.tasks.borrow_mut().push(fut);
    }

    fn poll_progress(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut tasks = self.tasks.borrow_mut();
        loop {
            match tasks.poll_next_unpin(cx) {
                // A task finished; drain any others that are also ready.
                Poll::Ready(Some(())) => continue,
                // The set is empty: nothing in flight.
                Poll::Ready(None) => return Poll::Ready(()),
                // Work in flight, none ready right now.
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// A thread-spawning executor: runs each request on a fresh [`std::thread`],
/// giving real parallelism.
///
/// Each task is driven to completion by a private, runtime-free `block_on`
/// (an owned park-based waker, no timeout). A zero-sized, [`Send`] + [`Sync`]
/// unit — it tracks nothing, because spawned threads finish on their own.
///
/// **Obligation**: because spawned threads are untracked, a task (and thus its
/// error-observer callback) may still be running *after* the serve loop that
/// dispatched it has returned. A caller needing "all work finished" must
/// coordinate that itself; this executor offers no join handle.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadPerRequest;

impl ThreadPerRequest {
    /// Create a thread-spawning executor.
    pub fn new() -> Self {
        Self
    }
}

impl Executor<RequestTask> for ThreadPerRequest {
    fn execute(&self, fut: RequestTask) {
        std::thread::spawn(move || block_on(fut));
    }
    // poll_progress defaulted: nothing lives on the loop thread to drive.
}

/// A waker that unparks a specific thread when woken.
struct ThreadWaker {
    thread: std::thread::Thread,
}

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.thread.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.thread.unpark();
    }
}

/// Drive a future to completion on the current thread, parking between polls.
///
/// A minimal, dependency-free, runtime-free executor used by
/// [`ThreadPerRequest`]. Unlike the deadline-bound, stack-address-waker helper
/// used in the integration-test harness, this uses a heap-owned ([`Arc`]) waker
/// and has no timeout, making it safe for arbitrary, long-lived request work.
/// The [`std::thread::park`]/`unpark` pair handles the wake/park race: an
/// `unpark` that arrives before `park` leaves a token that makes the next `park`
/// return immediately.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = Waker::from(Arc::new(ThreadWaker {
        thread: std::thread::current(),
    }));
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// A future that returns `Pending` once (waking itself) before completing.
    /// Proves a driver actually re-polls on wake rather than busy-spinning.
    struct YieldOnce {
        yielded: bool,
    }

    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.yielded {
                Poll::Ready(())
            } else {
                self.yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn block_on_repolls_after_a_wake() {
        // If `block_on` did not re-poll after a wake it would park forever; a
        // returning call proves the owned waker drives the future to completion.
        block_on(YieldOnce { yielded: false });
    }

    #[test]
    fn current_thread_interleaves_many_futures_and_would_deadlock_if_sequential() {
        // N tasks each announce arrival, then spin (self-waking) until all N have
        // arrived. Run concurrently, all N are in flight and reach the rendezvous.
        // Run one-at-a-time to completion (sequential), task 0 waits for an
        // arrival count that never comes -- a deadlock the watchdog catches.
        const N: usize = 4;
        let exec = CurrentThread::new();
        let arrived = Arc::new(AtomicUsize::new(0));
        for _ in 0..N {
            let arrived = Arc::clone(&arrived);
            exec.execute(Box::pin(async move {
                arrived.fetch_add(1, Ordering::SeqCst);
                std::future::poll_fn(move |cx| {
                    if arrived.load(Ordering::SeqCst) == N {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
            }));
        }

        // Drive the executor on a watchdog thread so a regression times out
        // loudly instead of hanging the suite. `CurrentThread` is `Send`.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            block_on(poll_fn(|cx| exec.poll_progress(cx)));
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the current-thread executor must interleave all {N} futures; \
             a timeout means they were driven sequentially"
        );
    }

    #[test]
    fn thread_per_request_runs_on_another_thread() {
        let main_id = std::thread::current().id();
        let (tx, rx) = mpsc::channel();
        ThreadPerRequest::new().execute(Box::pin(async move {
            let _ = tx.send(std::thread::current().id());
        }));
        let task_id = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the spawned task must run");
        assert_ne!(task_id, main_id, "the task must run on a distinct thread");
    }

    #[test]
    fn default_poll_progress_is_ready() {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(ThreadPerRequest::new().poll_progress(&mut cx).is_ready());
    }
}
