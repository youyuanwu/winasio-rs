// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The request queue -- the listener requests arrive on and replies go out on.

use windows::core::{Error, Result};
use windows::Win32::Foundation::{ERROR_INVALID_HANDLE, HANDLE};
use windows::Win32::Networking::HttpServer::{
    HttpCloseRequestQueue, HttpCreateRequestQueue, HttpServerBindingProperty, HTTP_BINDING_INFO,
    HTTP_PROPERTY_FLAGS,
};

use crate::iocp::{OpCode, Submit, ThreadPoolIo};

use super::error::check;
use super::init::VERSION;
use super::session::UrlGroup;

/// A listener.
///
/// Completions are delivered by the Win32 thread pool rather than a
/// caller-driven proactor: a queue is normally shared across tasks on a
/// multi-threaded runtime, and [`Proactor`](crate::iocp::Proactor) is `!Send`.
///
/// Closing is idempotent and reports failure as a value; dropping closes and
/// discards any error, because a panic in `Drop` aborts during unwinding.
pub struct RequestQueue {
    handle: HANDLE,
    /// `None` once closed. Dropping the registration cancels and drains
    /// outstanding operations, which is why it is released *before* the handle.
    io: Option<ThreadPoolIo>,
}

impl std::fmt::Debug for RequestQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestQueue")
            .field("handle", &self.handle)
            .field("open", &self.io.is_some())
            .finish()
    }
}

// SAFETY: a request-queue handle is thread-agnostic, and `ThreadPoolIo` is
// already `Send + Sync`. `HANDLE` is only `!Send` because it is a raw pointer.
unsafe impl Send for RequestQueue {}
unsafe impl Sync for RequestQueue {}

impl RequestQueue {
    /// Create an anonymous request queue and register it for completions.
    pub fn new() -> Result<RequestQueue> {
        let mut handle = HANDLE::default();
        let code = unsafe {
            HttpCreateRequestQueue(VERSION, windows::core::PCWSTR::null(), None, None, &mut handle)
        };
        check(code)?;
        debug_assert!(!handle.is_invalid());

        match ThreadPoolIo::new(handle) {
            Ok(io) => Ok(RequestQueue {
                handle,
                io: Some(io),
            }),
            Err(e) => {
                // Do not leak the queue if registration fails.
                let _ = unsafe { HttpCloseRequestQueue(handle) };
                Err(Error::from(e))
            }
        }
    }

    /// Direct traffic for `group`'s URLs to this queue.
    pub fn bind_url_group(&self, group: &UrlGroup) -> Result<()> {
        let info = HTTP_BINDING_INFO {
            // `Present` bit: the binding is being set rather than cleared.
            Flags: HTTP_PROPERTY_FLAGS { _bitfield: 1 },
            RequestQueueHandle: self.handle,
        };
        unsafe {
            group.set_property(
                HttpServerBindingProperty,
                std::ptr::addr_of!(info) as *const core::ffi::c_void,
                std::mem::size_of::<HTTP_BINDING_INFO>() as u32,
            )
        }
    }

    /// The underlying handle, for operations built against this queue.
    // Consumed by the operations added in later phases.
    #[allow(dead_code)]
    pub(crate) fn handle(&self) -> HANDLE {
        self.handle
    }

    /// Submit an operation, failing rather than panicking once closed.
    // Consumed by the operations added in later phases.
    #[allow(dead_code)]
    pub(crate) fn submit<T: OpCode + Send>(&self, op: T) -> Result<Submit<T>> {
        match self.io.as_ref() {
            Some(io) => Ok(io.submit(op)),
            None => Err(Error::from_hresult(ERROR_INVALID_HANDLE.to_hresult())),
        }
    }

    /// Close the queue, draining outstanding operations first.
    ///
    /// Idempotent: closing an already-closed queue succeeds and does nothing.
    pub fn close(&mut self) -> Result<()> {
        if self.io.is_none() && self.handle.is_invalid() {
            return Ok(());
        }
        // Release the registration first. Dropping it cancels and drains
        // in-flight operations, so the kernel no longer holds pointers into
        // them by the time the handle goes away.
        self.io = None;

        let handle = std::mem::take(&mut self.handle);
        if handle.is_invalid() {
            return Ok(());
        }
        check(unsafe { HttpCloseRequestQueue(handle) })
    }
}

impl Drop for RequestQueue {
    fn drop(&mut self) {
        // Ignored: a panic in `Drop` aborts during unwinding.
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;
    use windows::Win32::System::IO::OVERLAPPED;

    struct NeverRuns;

    unsafe impl OpCode for NeverRuns {
        fn handle(&self) -> Option<HANDLE> {
            None
        }
        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            unreachable!("a closed queue must reject the submission before operating")
        }
    }

    /// FR-010: submitting to a closed queue returns an error value rather than
    /// panicking. Exercised in-crate because `submit` is not public and the
    /// public operations do not exist until later phases.
    #[test]
    fn submitting_to_a_closed_queue_is_an_error() {
        let mut queue = match RequestQueue::new() {
            Ok(q) => q,
            // No HTTP service available; nothing to assert.
            Err(_) => return,
        };
        queue.close().expect("first close succeeds");
        assert!(queue.submit(NeverRuns).is_err());
    }

    /// FR-011: closing twice succeeds.
    #[test]
    fn closing_twice_succeeds() {
        let mut queue = match RequestQueue::new() {
            Ok(q) => q,
            Err(_) => return,
        };
        queue.close().expect("first close succeeds");
        queue.close().expect("second close is a no-op");
    }

    /// SC-004: a queue holding an unusable handle must drop without panicking.
    #[test]
    fn dropping_a_queue_with_an_invalid_handle_does_not_panic() {
        let bogus = RequestQueue {
            handle: HANDLE(usize::MAX as *mut core::ffi::c_void),
            io: None,
        };
        drop(bogus);
    }
}
