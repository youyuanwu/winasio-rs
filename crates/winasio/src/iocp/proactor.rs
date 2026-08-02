// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The caller-driven completion backend.

use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;
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
/// ```compile_fail
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
/// is to await the operation and drive the proactor on the same thread — for
/// example inside a current-thread runtime, alternating between polling the
/// operation and polling the proactor:
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
    inner: Rc<ProactorInner>,
    /// Makes the type `!Send` and `!Sync`.
    _not_send: PhantomData<*const ()>,
}

pub(crate) struct ProactorInner {
    port: CompletionPort,
    /// Operations in flight, so shutdown can cancel them. Only touched from the
    /// owning thread; completion callbacks never reach it.
    pending: RefCell<HashMap<usize, CancelFn>>,
    /// Handles whose synchronous completions still produce a packet.
    no_skip: RefCell<Vec<isize>>,
}

/// Wakes a blocked [`Proactor::poll`] from another thread.
#[derive(Clone)]
pub struct Notify {
    port: std::sync::Arc<NotifyPort>,
}

pub struct NotifyPort(HANDLE);

// SAFETY: the port handle is usable from any thread and is only ever passed to
// `PostQueuedCompletionStatus`, which Windows serialises.
unsafe impl Send for NotifyPort {}
unsafe impl Sync for NotifyPort {}

impl Notify {
    /// Post the sentinel, so a waiting `poll` returns promptly.
    pub fn wake(&self) -> Result<()> {
        unsafe {
            windows::Win32::System::IO::PostQueuedCompletionStatus(self.port.0, 0, KEY_WAKEUP, None)
        }
    }
}

impl Proactor {
    /// Create a proactor with its own completion port.
    pub fn new() -> Result<Self> {
        Ok(Proactor {
            inner: Rc::new(ProactorInner {
                port: CompletionPort::new()?,
                pending: RefCell::new(HashMap::new()),
                no_skip: RefCell::new(Vec::new()),
            }),
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
        if !skips {
            self.inner.no_skip.borrow_mut().push(handle.0 as isize);
        }
        Ok(())
    }

    /// A handle that can wake a blocked [`Proactor::poll`] from another thread.
    pub fn notify(&self) -> Notify {
        Notify {
            port: std::sync::Arc::new(NotifyPort(self.inner.port.raw())),
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
                let cancel_key = key.clone();
                self.inner.pending.borrow_mut().insert(
                    optr as usize,
                    Box::new(move || {
                        let _ = cancel_key.cancel();
                    }),
                );
                Submit::pending(key, Some(self.inner.clone()), optr as usize)
            }
            Poll::Ready(result) => {
                // Completed inline. If the handle skips the port on success
                // there is no packet coming, so reclaim the reference now.
                // Otherwise a packet is still on its way and the reference
                // belongs to it.
                let handle_skips = !self.handle_lacks_skip(&key);
                if handle_skips || result.is_err() {
                    // SAFETY: matching leak above, and no completion will arrive.
                    unsafe { Key::<T>::unleak(optr) };
                    Submit::ready(key, result)
                } else {
                    let cancel_key = key.clone();
                    self.inner.pending.borrow_mut().insert(
                        optr as usize,
                        Box::new(move || {
                            let _ = cancel_key.cancel();
                        }),
                    );
                    Submit::pending(key, Some(self.inner.clone()), optr as usize)
                }
            }
        }
    }

    /// Whether the operation's handle lacked skip-on-success support.
    ///
    /// Conservative: without a way to ask the operation which handle it used,
    /// treat any registration that lacked the flag as applying.
    fn handle_lacks_skip<T: OpCode>(&self, _key: &Key<T>) -> bool {
        !self.inner.no_skip.borrow().is_empty()
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

        let mut delivered = 0usize;
        for entry in entries.iter().take(count) {
            if entry.lpCompletionKey == KEY_WAKEUP {
                continue;
            }
            if entry.lpCompletionKey != KEY_OPERATION {
                // Not ours. Another component may share this port.
                continue;
            }
            let optr: *mut OVERLAPPED = entry.lpOverlapped;
            if optr.is_null() {
                continue;
            }
            self.pending.borrow_mut().remove(&(optr as usize));

            let result = entry_result(entry);
            // SAFETY: the completion key established that this packet is ours,
            // so `optr` refers to a live operation allocation whose leaked
            // reference has not been reclaimed.
            if unsafe { dispatch_completion(optr, result) } {
                delivered += 1;
            }
        }
        Ok(delivered)
    }

    pub(crate) fn forget(&self, token: usize) {
        self.pending.borrow_mut().remove(&token);
    }

    /// Cancel everything in flight and drain their completions.
    fn shutdown(&self) {
        for cancel in self.pending.borrow().values() {
            cancel();
        }
        // Drain until the kernel has returned every outstanding operation.
        // Without this the port would close while Windows still holds pointers
        // into allocations we are about to release.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !self.pending.borrow().is_empty() {
            match self.poll(Some(Duration::from_millis(50))) {
                Ok(_) => {}
                Err(_) => break,
            }
            if std::time::Instant::now() > deadline {
                debug_assert!(false, "timed out draining completions at shutdown");
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

    #[test]
    fn proactor_is_not_send() {
        fn is_send<T: Send>() {}
        // Compile-time evidence lives in the `compile_fail` doctest on
        // `Proactor`; this only records the intent alongside it.
        is_send::<Notify>();
    }

    #[test]
    fn notify_wakes_a_blocked_poll() {
        let proactor = Proactor::new().unwrap();
        let notify = proactor.notify();

        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            notify.wake().unwrap();
        });

        // Would block for a long time without the wakeup.
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
