// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Waiting on signalable handles, expressed as an operation.
//!
//! Events, processes, mutexes and timers are not I/O, so they never produce a
//! completion packet of their own. They are driven by the Win32 wait
//! infrastructure instead. Wrapping them as an [`OpCode`] means callers learn
//! one model: the same submission, the same cancellation semantics, and the same
//! ownership rules as for reads and writes.
//!
//! # How the completion is delivered
//!
//! `RegisterWaitForSingleObject` invokes a callback on a pool thread when the
//! handle signals. That callback posts the operation's `OVERLAPPED` to the
//! proactor's completion port, so event completions flow through exactly the
//! same path as I/O and no separate wake mechanism exists.
//!
//! # Cancellation does not block
//!
//! Dropping the future calls `UnregisterWaitEx` with no completion event, which
//! returns immediately rather than waiting for the callback. The blocking form —
//! passing `INVALID_HANDLE_VALUE` — would stall whichever thread happened to
//! drop the future.
//!
//! Because unregistering means the callback will *not* run, cancellation must
//! post the terminal completion itself; otherwise nothing would ever release the
//! operation. Exactly one of the callback and the cancellation does so, decided
//! by an atomic exchange.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::task::Poll;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    RegisterWaitForSingleObject, UnregisterWaitEx, INFINITE, WT_EXECUTEONLYONCE,
};
use windows::Win32::System::IO::{PostQueuedCompletionStatus, OVERLAPPED};

use crate::iocp::op::{IntoInner, OpCode, OpType};
use crate::iocp::port::KEY_OPERATION;

/// Shared between the wait callback and the operation.
struct WaitContext {
    port: HANDLE,
    overlapped: *mut OVERLAPPED,
    /// Ensures the terminal completion is posted exactly once, whether by the
    /// callback or by cancellation.
    posted: AtomicBool,
}

// SAFETY: both handles are opaque identifiers passed straight back to Windows,
// and `overlapped` is only ever forwarded to `PostQueuedCompletionStatus`.
unsafe impl Send for WaitContext {}
unsafe impl Sync for WaitContext {}

impl WaitContext {
    /// Post the operation's completion, if nobody has yet.
    ///
    /// Returns whether this call was the one that posted.
    fn post_once(&self) -> bool {
        if self.posted.swap(true, Ordering::AcqRel) {
            return false;
        }
        let _ = unsafe {
            PostQueuedCompletionStatus(
                self.port,
                0,
                KEY_OPERATION,
                Some(self.overlapped as *const _),
            )
        };
        true
    }
}

/// Invoked on a pool thread when the waited handle signals.
///
/// # Safety
///
/// `context` must be a pointer produced by `Arc::into_raw` for a
/// [`WaitContext`], whose reference this call consumes.
unsafe extern "system" fn wait_callback(context: *mut core::ffi::c_void, timed_out: bool) {
    if context.is_null() {
        return;
    }
    // Reclaim the reference leaked at registration. `WT_EXECUTEONLYONCE` means
    // this runs at most once.
    let ctx = unsafe { Arc::from_raw(context as *const WaitContext) };

    // The wait is registered with an infinite timeout, so a timeout here would
    // mean the registration was misconfigured.
    debug_assert!(!timed_out, "an infinite wait cannot time out");

    ctx.post_once();
}

/// Waits for a handle to become signalled.
///
/// The handle is borrowed, not owned: this does not close it. It must stay valid
/// until the operation reaches a terminal state, which is part of [`OpCode`]'s
/// safety contract for [`OpType::Event`].
///
/// A wait belongs to the proactor that will drive it, because the wait callback
/// must post its completion to that proactor's port.
pub struct WaitForHandle {
    target: HANDLE,
    port: HANDLE,
    /// The registration handle from `RegisterWaitForSingleObject`.
    wait: AtomicIsize,
    ctx: Option<Arc<WaitContext>>,
}

// SAFETY: `HANDLE` is a raw pointer but a kernel handle is thread-agnostic.
unsafe impl Send for WaitForHandle {}
unsafe impl Sync for WaitForHandle {}

impl WaitForHandle {
    /// Wait for `target` to signal, completing through `proactor`.
    pub fn new(proactor: &crate::iocp::Proactor, target: HANDLE) -> Self {
        WaitForHandle {
            target,
            port: proactor.port_handle(),
            wait: AtomicIsize::new(0),
            ctx: None,
        }
    }

    /// Release the wait registration.
    ///
    /// Returns `true` if the callback is guaranteed not to run, meaning the
    /// caller is responsible for the terminal completion and for the reference
    /// the callback would have reclaimed.
    fn unregister(&self) -> bool {
        let raw = self.wait.swap(0, Ordering::AcqRel);
        if raw == 0 {
            return false;
        }
        // `None`: do not block waiting for the callback.
        unsafe { UnregisterWaitEx(HANDLE(raw as *mut core::ffi::c_void), None) }.is_ok()
    }
}

unsafe impl OpCode for WaitForHandle {
    fn op_type(&self) -> OpType {
        OpType::Event(self.target)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let ctx = Arc::new(WaitContext {
            port: self.port,
            overlapped: optr,
            posted: AtomicBool::new(false),
        });
        // One reference for the callback; Windows owns it until it fires.
        let leaked = Arc::into_raw(Arc::clone(&ctx));
        self.ctx = Some(ctx);

        let mut wait = HANDLE::default();
        let registered = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                self.target,
                Some(wait_callback),
                Some(leaked as *const core::ffi::c_void),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };

        match registered {
            Ok(()) => {
                self.wait.store(wait.0 as isize, Ordering::Release);
                Poll::Pending
            }
            Err(e) => {
                // The callback will never run, so reclaim its reference.
                drop(unsafe { Arc::from_raw(leaked) });
                self.ctx = None;
                Poll::Ready(Err(e))
            }
        }
    }

    unsafe fn cancel(&mut self, _optr: *mut OVERLAPPED) -> Result<()> {
        let callback_will_not_run = self.unregister();
        let Some(ctx) = self.ctx.as_ref() else {
            return Ok(());
        };

        if callback_will_not_run {
            // Nothing else will terminate this operation, so post it here.
            // Winning the exchange also proves the callback never posted, so
            // its leaked reference is ours to release.
            if ctx.post_once() {
                let raw = Arc::as_ptr(ctx);
                // SAFETY: the callback is guaranteed not to run and therefore
                // will not reclaim the reference leaked in `operate`.
                drop(unsafe { Arc::from_raw(raw) });
            }
        }
        Ok(())
    }

    unsafe fn on_complete(&mut self, _result: &Result<usize>) {
        // The wait has fired; drop the registration so the handle is not held.
        self.unregister();
    }
}

impl IntoInner for WaitForHandle {
    type Inner = ();
    fn into_inner(self) {}
}
