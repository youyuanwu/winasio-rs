// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The system-managed completion backend.
//!
//! Uses the Win32 thread pool's I/O objects, so completions arrive on pool
//! threads with no driver loop. This is the backend for multi-threaded runtimes,
//! where nothing would call [`Proactor::poll`](super::Proactor::poll).
//!
//! # The start/cancel protocol
//!
//! Windows requires `StartThreadpoolIo` before *each* operation is initiated —
//! omitting it "will cause the thread pool to ignore an I/O operation when it
//! completes and will cause memory corruption". If the operation then fails to
//! start, or completes inline on a handle that skips the completion port,
//! `CancelThreadpoolIo` must balance it, or the pending-I/O count leaks and
//! teardown blocks forever.
//!
//! # Teardown
//!
//! `WaitForThreadpoolIoCallbacks` alone is not enough. Passing `true` cancels
//! queued callbacks, stranding the references they are responsible for
//! reclaiming; passing `false` blocks until outstanding operations finish, which
//! may be never. So the registration cancels the handle's I/O first, then waits
//! with `false` so every callback runs, then closes.

use std::sync::Arc;
use std::task::Poll;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::SetFileCompletionNotificationModes;
use windows::Win32::System::Threading::{
    CancelThreadpoolIo, CloseThreadpoolIo, CreateThreadpoolIo, StartThreadpoolIo,
    WaitForThreadpoolIoCallbacks, PTP_CALLBACK_INSTANCE, PTP_IO,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use super::future::Submit;
use super::op::OpCode;
use super::port::RegistrationError;
use super::raw::{dispatch_completion, Key};

/// See `port.rs`; not exported by the `windows` crate.
const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: u8 = 0x1;
const FILE_SKIP_SET_EVENT_ON_HANDLE: u8 = 0x2;

/// A handle registered with the Win32 thread pool.
///
/// Unlike [`Proactor`](super::Proactor) this is [`Send`] and [`Sync`]:
/// completions arrive on pool threads and wake their futures directly, so it can
/// be shared across a multi-threaded runtime and needs no driver.
///
/// Clone it freely; the registration is released when the last clone drops, at
/// which point outstanding operations are cancelled and drained.
#[derive(Clone)]
pub struct ThreadPoolIo {
    inner: Arc<Registration>,
}

struct Registration {
    handle: HANDLE,
    io: PTP_IO,
    /// Whether an inline success skips the completion notification.
    skips_on_success: bool,
}

// SAFETY: `HANDLE` and `PTP_IO` are opaque kernel/pool identifiers usable from
// any thread; Windows serialises access to both.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

/// The thread pool's I/O completion callback.
///
/// # Safety
///
/// Invoked by Windows for an operation started under a registration created by
/// [`ThreadPoolIo::new`]. `overlapped` is the pointer that operation was given.
unsafe extern "system" fn io_callback(
    _instance: PTP_CALLBACK_INSTANCE,
    _context: *mut core::ffi::c_void,
    overlapped: *mut core::ffi::c_void,
    io_result: u32,
    bytes_transferred: usize,
    _io: PTP_IO,
) {
    let optr = overlapped as *mut OVERLAPPED;
    if optr.is_null() {
        return;
    }

    let result = if io_result == 0 {
        Ok(bytes_transferred)
    } else {
        Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::WIN32_ERROR(io_result).to_hresult(),
        ))
    };

    // SAFETY: this callback fires only for operations started through this
    // registration, so `optr` refers to a live operation allocation whose
    // leaked reference has not been reclaimed. No state outside the operation
    // itself is touched, so nothing here depends on the registration still
    // being alive.
    unsafe { dispatch_completion(optr, result) };
}

impl ThreadPoolIo {
    /// Register a handle with the Win32 thread pool.
    ///
    /// A handle may be registered with exactly one completion mechanism, for its
    /// whole lifetime. A second attempt — with either backend, in either order —
    /// fails with [`RegistrationError::AlreadyRegistered`].
    pub fn new(handle: HANDLE) -> std::result::Result<Self, RegistrationError> {
        // No context is passed: the callback needs nothing but the OVERLAPPED,
        // which keeps the registration's lifetime independent of in-flight
        // callbacks.
        let io = unsafe { CreateThreadpoolIo(handle, Some(io_callback), None, None) }
            .map_err(RegistrationError::from_association)?;

        let skips_on_success = unsafe {
            SetFileCompletionNotificationModes(
                handle,
                FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE,
            )
        }
        .is_ok();

        Ok(ThreadPoolIo {
            inner: Arc::new(Registration {
                handle,
                io,
                skips_on_success,
            }),
        })
    }

    /// The registered handle.
    pub fn handle(&self) -> HANDLE {
        self.inner.handle
    }

    /// Whether inline successes on this handle skip the completion callback.
    pub fn skips_on_success(&self) -> bool {
        self.inner.skips_on_success
    }

    /// Submit an operation.
    ///
    /// The returned future resolves when the thread pool delivers the
    /// completion; the caller does not poll anything.
    ///
    /// `T: Send` because the completion callback runs `on_complete` and may drop
    /// the operation on a pool thread.
    pub fn submit<T: OpCode + Send>(&self, op: T) -> Submit<T> {
        let key = Key::new(op);
        let optr = key.leak();

        // Required before the operation is initiated. Omitting it makes the
        // pool ignore the completion and corrupt memory.
        unsafe { StartThreadpoolIo(self.inner.io) };

        // SAFETY: called once, before the operation is in flight.
        let started = unsafe { key.operate() };

        match started {
            Poll::Pending => Submit::pending(key),
            Poll::Ready(Err(e)) => {
                // The operation never started, so no callback will run.
                unsafe { CancelThreadpoolIo(self.inner.io) };
                // SAFETY: matches the leak above; nothing will reclaim it.
                unsafe { Key::<T>::unleak(optr) };
                Submit::ready(key, Err(e))
            }
            Poll::Ready(Ok(n)) => {
                if self.inner.skips_on_success {
                    // No callback will run for an inline success on this handle.
                    unsafe { CancelThreadpoolIo(self.inner.io) };
                    let result = Ok(n);
                    key.on_complete_inline(&result);
                    // SAFETY: matches the leak above; no callback will arrive.
                    unsafe { Key::<T>::unleak(optr) };
                    Submit::ready(key, result)
                } else {
                    // A callback is still coming and owns the leaked reference.
                    Submit::pending(key)
                }
            }
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // Cancel anything still outstanding so its callback runs promptly.
        // Waiting without cancelling can block indefinitely.
        let _ = unsafe { CancelIoEx(self.handle, None) };
        // `false`: let queued callbacks run, so every leaked reference is
        // reclaimed before the pool object goes away. `true` would cancel them
        // and strand those references.
        unsafe { WaitForThreadpoolIoCallbacks(self.io, false) };
        unsafe { CloseThreadpoolIo(self.io) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threadpool_io_is_send_and_sync() {
        fn is_send_sync<T: Send + Sync>() {}
        // The whole point of this backend: usable from a multi-threaded runtime.
        is_send_sync::<ThreadPoolIo>();
    }
}
