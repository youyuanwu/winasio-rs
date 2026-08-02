// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The caller-driven completion backend.

use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::{OVERLAPPED, OVERLAPPED_ENTRY};

use super::future::Submit;
use super::op::OpCode;
use super::port::{entry_result, CompletionPort, RegistrationError, KEY_OPERATION, KEY_WAKEUP};
use super::raw::{dispatch_completion, Key};

/// How many completions to retrieve per wait.
const BATCH: usize = 64;

/// Cancels an in-flight operation at shutdown.
type CancelFn = Box<dyn Fn()>;

/// A completion port plus the operations in flight on it.
///
/// # Threading
///
/// `Proactor` is deliberately **not** [`Send`]. Submission and
/// [`poll`](Proactor::poll) happen on the thread that owns it, which keeps the
/// pending set lock-free. Use the thread-pool backend where completions must be
/// delivered without a driver thread, such as under a multi-threaded runtime.
///
/// The [`Submit`] futures it produces *are* `Send` when the operation is, so
/// only the driver is thread-bound.
///
/// ```compile_fail,E0277
/// # use winasio::iocp::Proactor;
/// fn requires_send<T: Send>(_: T) {}
/// let proactor = Proactor::new().unwrap();
/// // Fails to compile: a Proactor is bound to the thread that created it.
/// requires_send(proactor);
/// ```
///
/// # Driving it
///
/// Nothing completes unless [`poll`](Proactor::poll) runs. The canonical shape
/// is to await the operation and drive the proactor on the same thread,
/// alternating between the two:
///
/// ```text
/// let proactor = Proactor::new()?;
/// proactor.attach(handle)?;
/// let mut op = proactor.submit(ReadAt::new(handle, 0, buffer));
///
/// loop {
///     if let Some(done) = poll_once(&mut op) {
///         break done;
///     }
///     proactor.poll(Some(Duration::from_millis(5)))?;
/// }
/// ```
///
/// See `crates/winasio-tests/tests/iocp.rs` for a working driver.
pub struct Proactor {
    inner: ProactorInner,
    /// Makes the type `!Send` and `!Sync`.
    _not_send: PhantomData<*const ()>,
}

struct ProactorInner {
    port: Arc<CompletionPort>,
    /// Operations in flight, so shutdown can cancel them. Only touched from the
    /// owning thread; completion callbacks never reach it.
    pending: RefCell<HashMap<usize, CancelFn>>,
    /// Per handle: does an inline success skip the completion port?
    skips_on_success: RefCell<HashMap<isize, bool>>,
}

/// Wakes a blocked [`Proactor::poll`] from another thread.
///
/// Holds a reference to the port, so waking after the proactor is dropped is
/// harmless rather than a use-after-close.
#[derive(Clone)]
pub struct Notify {
    port: Arc<CompletionPort>,
}

impl Notify {
    /// Post the sentinel, so a waiting `poll` returns promptly.
    pub fn wake(&self) -> Result<()> {
        self.port.wake()
    }
}

impl Proactor {
    /// Create a proactor with its own completion port.
    pub fn new() -> Result<Self> {
        Ok(Proactor {
            inner: ProactorInner {
                port: Arc::new(CompletionPort::new()?),
                pending: RefCell::new(HashMap::new()),
                skips_on_success: RefCell::new(HashMap::new()),
            },
            _not_send: PhantomData,
        })
    }

    /// Associate a handle with this proactor.
    ///
    /// A handle may be registered with exactly one completion mechanism, for
    /// its whole lifetime. A second attempt — with either backend, in either
    /// order — fails with [`RegistrationError::AlreadyRegistered`].
    pub fn attach(&self, handle: HANDLE) -> std::result::Result<(), RegistrationError> {
        let skips = self.inner.port.attach(handle)?;
        self.inner
            .skips_on_success
            .borrow_mut()
            .insert(handle.0 as isize, skips);
        Ok(())
    }

    /// A handle that can wake a blocked [`Proactor::poll`] from another thread.
    pub fn notify(&self) -> Notify {
        Notify {
            port: Arc::clone(&self.inner.port),
        }
    }

    /// Submit an operation.
    ///
    /// If it completes inline the result is available immediately; otherwise the
    /// returned future resolves when the completion arrives.
    pub fn submit<T: OpCode>(&self, op: T) -> Submit<T> {
        let key = Key::new(op);

        // Leak the kernel's reference *before* starting the operation. A
        // completion can be delivered before the initiating call returns, and
        // would otherwise find no reference to reclaim.
        let optr = key.leak();

        // SAFETY: called once, before the operation is in flight.
        let started = unsafe { key.operate() };

        match started {
            Poll::Pending => {
                self.track(optr, &key);
                Submit::pending(key)
            }
            Poll::Ready(Err(e)) => {
                // A failed start queues no packet.
                // SAFETY: matches the leak above; nothing will reclaim it.
                unsafe { Key::<T>::unleak(optr) };
                Submit::ready(key, Err(e))
            }
            Poll::Ready(Ok(n)) => {
                if self.packet_follows_inline_success(&key) {
                    // The handle does not skip the port, so a packet is still
                    // coming and owns the leaked reference.
                    self.track(optr, &key);
                    Submit::pending(key)
                } else {
                    // No packet will arrive. Run the completion hook here, since
                    // nothing else will, then reclaim the reference.
                    let result = Ok(n);
                    key.on_complete_inline(&result);
                    // SAFETY: matches the leak above; no completion will arrive.
                    unsafe { Key::<T>::unleak(optr) };
                    Submit::ready(key, result)
                }
            }
        }
    }

    fn track<T: OpCode>(&self, optr: *mut OVERLAPPED, key: &Key<T>) {
        let cancel_key = key.clone();
        self.inner.pending.borrow_mut().insert(
            optr as usize,
            Box::new(move || {
                let _ = cancel_key.cancel();
            }),
        );
    }

    /// Whether a completion packet will follow an inline success.
    ///
    /// Determined per handle from what `SetFileCompletionNotificationModes`
    /// reported at registration. An operation that does not report its handle is
    /// assumed to skip the port, which is the documented contract on
    /// [`OpCode::handle`].
    fn packet_follows_inline_success<T: OpCode>(&self, key: &Key<T>) -> bool {
        match key.handle() {
            Some(h) => !self
                .inner
                .skips_on_success
                .borrow()
                .get(&(h.0 as isize))
                .copied()
                .unwrap_or(false),
            None => false,
        }
    }

    /// Retrieve and dispatch available completions.
    ///
    /// Returns how many were delivered. With `timeout` of `None` this blocks
    /// until at least one packet arrives or [`Notify::wake`] is called.
    pub fn poll(&self, timeout: Option<Duration>) -> Result<usize> {
        self.inner.poll(timeout)
    }

    /// Number of operations still in flight.
    pub fn pending_count(&self) -> usize {
        self.inner.pending.borrow().len()
    }
}

impl ProactorInner {
    fn poll(&self, timeout: Option<Duration>) -> Result<usize> {
        let mut entries = [OVERLAPPED_ENTRY::default(); BATCH];
        let count = self.port.poll(&mut entries, timeout)?;

        // Collect first, so no `RefCell` borrow is live while a waker runs —
        // an inline executor could otherwise re-enter this proactor.
        let mut ready: Vec<(*mut OVERLAPPED, Result<usize>)> = Vec::new();
        {
            let mut pending = self.pending.borrow_mut();
            for entry in entries.iter().take(count) {
                if entry.lpCompletionKey == KEY_WAKEUP {
                    continue;
                }
                if entry.lpCompletionKey != KEY_OPERATION {
                    // Not ours; another component may share this port.
                    continue;
                }
                let optr: *mut OVERLAPPED = entry.lpOverlapped;
                if optr.is_null() {
                    continue;
                }
                pending.remove(&(optr as usize));
                ready.push((optr, entry_result(entry)));
            }
        }

        let mut delivered = 0usize;
        for (optr, result) in ready {
            // SAFETY: the completion key established that this packet is ours,
            // so `optr` refers to a live operation allocation whose leaked
            // reference has not been reclaimed.
            if unsafe { dispatch_completion(optr, result) } {
                delivered += 1;
            }
        }
        Ok(delivered)
    }

    /// Cancel everything in flight and drain their completions.
    fn shutdown(&self) {
        // Take the closures out before calling them, so no borrow is live.
        let cancels: Vec<CancelFn> = self.pending.borrow_mut().drain().map(|(_, c)| c).collect();
        let outstanding = cancels.len();
        for cancel in &cancels {
            cancel();
        }
        if outstanding == 0 {
            return;
        }

        // Drain the cancellations. Without this the port would close while
        // Windows still holds pointers into allocations about to be released.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut drained = 0usize;
        while drained < outstanding {
            match self.poll(Some(Duration::from_millis(50))) {
                Ok(n) => drained += n,
                Err(_) => break,
            }
            if std::time::Instant::now() > deadline {
                // Cannot panic here: this runs in `Drop`.
                eprintln!(
                    "winasio: timed out draining {} outstanding IOCP completion(s) at shutdown",
                    outstanding - drained
                );
                break;
            }
        }
    }
}

impl Drop for Proactor {
    fn drop(&mut self) {
        self.inner.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iocp::buf::BufResult;
    use crate::iocp::op::IntoInner;
    use crate::iocp::ops::ReadAt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Completes inline without ever touching Windows, so the synchronous path
    /// can be tested deterministically. Real file I/O almost never completes
    /// inline, which is why this needs a synthetic operation.
    struct InlineOp {
        outcome: std::result::Result<usize, windows::core::Error>,
        completed_with: Option<usize>,
        buffer: Vec<u8>,
    }

    static INLINE_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);

    unsafe impl OpCode for InlineOp {
        // No handle reported: the driver must assume no packet follows.
        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            Poll::Ready(self.outcome.clone())
        }
        unsafe fn on_complete(&mut self, result: &Result<usize>) {
            INLINE_COMPLETIONS.fetch_add(1, Ordering::SeqCst);
            self.completed_with = result.as_ref().ok().copied();
        }
    }

    impl IntoInner for InlineOp {
        type Inner = (Vec<u8>, Option<usize>);
        fn into_inner(self) -> Self::Inner {
            (self.buffer, self.completed_with)
        }
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        // These futures are always immediately ready.
        let mut fut = Box::pin(fut);
        let waker = noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("expected an inline completion"),
        }
    }

    fn noop_waker() -> std::task::Waker {
        use std::task::{RawWaker, RawWakerVTable, Waker};
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    #[test]
    fn submit_future_is_send_when_the_operation_is() {
        fn is_send<T: Send>() {}
        // Load-bearing: `Submit` must stay `Send` so it can be awaited on a
        // multi-threaded runtime. Holding a non-`Send` backend handle here
        // would silently break the thread-pool backend and httpsys.
        is_send::<Submit<ReadAt<Vec<u8>>>>();
    }

    #[test]
    fn inline_success_yields_one_result_and_runs_the_completion_hook() {
        let before = INLINE_COMPLETIONS.load(Ordering::SeqCst);
        let proactor = Proactor::new().unwrap();

        let submitted = proactor.submit(InlineOp {
            outcome: Ok(17),
            completed_with: None,
            buffer: vec![1, 2, 3],
        });
        assert!(
            submitted.is_ready(),
            "an inline op resolves without polling"
        );
        assert_eq!(
            proactor.pending_count(),
            0,
            "nothing is tracked when no packet is expected"
        );

        let out: BufResult<usize, InlineOp> = block_on(submitted);
        let (result, (buf, seen)) = out.into_inner_parts();
        assert_eq!(result.unwrap(), 17, "the transferred count survives");
        assert_eq!(buf, vec![1, 2, 3], "the buffer comes back");
        assert_eq!(
            seen,
            Some(17),
            "on_complete must run even though no packet arrives"
        );
        assert_eq!(
            INLINE_COMPLETIONS.load(Ordering::SeqCst),
            before + 1,
            "exactly one completion"
        );

        // No packet should be delivered afterwards.
        let n = proactor.poll(Some(Duration::from_millis(20))).unwrap();
        assert_eq!(n, 0, "an inline completion must not also queue a packet");
    }

    #[test]
    fn inline_failure_returns_the_operation_state() {
        let proactor = Proactor::new().unwrap();
        let err = windows::core::Error::from_hresult(windows::core::HRESULT(-2147024809));

        let submitted = proactor.submit(InlineOp {
            outcome: Err(err),
            completed_with: None,
            buffer: vec![9, 9],
        });
        assert_eq!(proactor.pending_count(), 0);

        let out: BufResult<usize, InlineOp> = block_on(submitted);
        let (result, (buf, _)) = out.into_inner_parts();
        assert!(result.is_err(), "the failure surfaces");
        assert_eq!(buf, vec![9, 9], "state comes back on failure too");

        let n = proactor.poll(Some(Duration::from_millis(20))).unwrap();
        assert_eq!(n, 0, "a failed start queues no packet");
    }

    #[test]
    fn notify_is_send_and_outlives_the_proactor() {
        fn is_send<T: Send>() {}
        is_send::<Notify>();

        let notify = {
            let proactor = Proactor::new().unwrap();
            proactor.notify()
        };
        // The port is kept alive by the Notify, so this is not a use-after-close.
        notify.wake().unwrap();
    }

    #[test]
    fn notify_wakes_a_blocked_poll() {
        let proactor = Proactor::new().unwrap();
        let notify = proactor.notify();

        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            notify.wake().unwrap();
        });

        let started = std::time::Instant::now();
        proactor.poll(Some(Duration::from_secs(10))).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "poll should return as soon as the sentinel arrives"
        );
        handle.join().unwrap();
    }

    #[test]
    fn poll_with_no_work_times_out() {
        let proactor = Proactor::new().unwrap();
        let n = proactor.poll(Some(Duration::from_millis(20))).unwrap();
        assert_eq!(n, 0);
    }
}
