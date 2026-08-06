// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! An owned `HINTERNET`.

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::WinHttpCloseHandle;

/// An owned WinHTTP handle, closed exactly once when dropped.
pub(crate) struct Handle(*mut c_void);

// SAFETY: an `HINTERNET` is an opaque process-wide handle with no thread
// affinity. WinHTTP itself proves this: it invokes the status callback for a
// handle on arbitrary threads from its own pool, and — as measured — sometimes
// on whichever thread submitted the operation, so the handle is already being
// used from more than one thread by the platform before this crate does
// anything. `Handle` owns its value exclusively, so no other Rust code can be
// closing it concurrently, and `WinHttpCloseHandle` is itself safe to call from
// any thread.
unsafe impl Send for Handle {}

// SAFETY: as above, and additionally every method reachable through a `&Handle`
// only reads the pointer and passes it to a WinHTTP function. The complete set
// of such methods is small and each is safe to call concurrently:
//
// * `Session::connect` and `Connection::open_request` derive a new handle. Both
//   are documented as thread-safe and neither mutates the parent.
// * `Request::status_code`, `header` and `raw_headers` call
//   `WinHttpQueryHeaders`, which was measured to be fully synchronous even on
//   an async handle and reads a response buffer WinHTTP has already finished
//   writing.
// * `Session::set_timeouts` and `Request::set_timeouts` call
//   `WinHttpSetTimeouts`, which is a write, but of four independent scalars
//   that WinHTTP guards internally; concurrent calls can interleave to produce
//   either caller's values, which is the ordinary outcome of racing setters and
//   not memory-unsafe.
//
// Every operation that actually transfers data takes `&mut Request`, so the
// borrow checker — not this impl — is what prevents two transfers overlapping.
// Where that is bypassed by cloning a handle across threads, WinHTTP still
// refuses the second submission with `ERROR_WINHTTP_INCORRECT_HANDLE_STATE`
// rather than corrupting anything.
unsafe impl Sync for Handle {}

impl Handle {
    /// Take ownership of a raw handle.
    ///
    /// # Safety
    ///
    /// `raw` must be a valid `HINTERNET` that nothing else will close.
    pub(crate) unsafe fn from_raw(raw: *mut c_void) -> Self {
        Handle(raw)
    }

    pub(crate) fn as_raw(&self) -> *mut c_void {
        self.0
    }

    /// Close the handle now and consume the wrapper.
    ///
    /// Used on the construction failure paths, where the handle must be closed
    /// before its context has been handed to WinHTTP.
    pub(crate) fn close_now(self) {
        drop(self);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // Only inspect the failure. Reading thread error state after a
        // *successful* call returns whatever the thread last recorded, which
        // has nothing to do with this handle — the previous implementation had
        // this check inverted and asserted on the success path. The idiom here
        // matches `crate::sys::event`.
        //
        // A failure is not made fatal in release builds. By the time a handle
        // is being dropped there is nothing a caller could do about it, and a
        // panic in `Drop` during unwinding aborts the process.
        if let Err(e) = unsafe { WinHttpCloseHandle(self.0) } {
            debug_assert!(false, "failed to close a WinHTTP handle: {e}");
            // Keep the binding used in release builds too.
            let _ = e;
        }
        self.0 = std::ptr::null_mut();
    }
}
