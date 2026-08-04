// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Operations on a request queue.

pub(crate) mod body;
pub(crate) mod cancel;
pub(crate) mod receive;
pub(crate) mod send;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::HttpServer::HttpCloseRequestQueue;

use crate::iocp::SendHandle;

use super::error::check;

/// Shared owner of a request-queue handle.
///
/// Cloning is a reference-count bump and performs no allocation, so submitted
/// operations can keep the queue alive until the kernel is finished with them.
/// `Send` and `Sync` are derived from [`SendHandle`], whose thread-agnostic
/// guarantee is already audited.
#[derive(Clone, Debug)]
pub(crate) struct QueueHandle(Arc<Owned>);

#[derive(Debug)]
struct Owned {
    handle: SendHandle,
    closed: AtomicBool,
}

impl Owned {
    fn close(&self) -> Result<()> {
        let handle = self.handle.0;
        if handle.is_invalid() {
            return Ok(());
        }
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        check(unsafe { HttpCloseRequestQueue(handle) })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // Ignored: a panic in `Drop` aborts during unwinding.
        let _ = self.close();
    }
}

impl QueueHandle {
    /// Take ownership of a raw request-queue handle.
    ///
    /// # Safety
    ///
    /// * `handle` must be a valid HTTP.sys request queue handle currently owned
    ///   by the caller.
    /// * Ownership transfers here: the handle must not be closed by anything
    ///   else, and nothing else may alias it as an owner.
    /// * This is deliberately not a safe constructor. A safe one would let a
    ///   caller adopt the same raw handle twice and double-close it.
    pub(crate) unsafe fn from_raw(handle: HANDLE) -> Self {
        QueueHandle(Arc::new(Owned {
            handle: SendHandle(handle),
            closed: AtomicBool::new(false),
        }))
    }

    /// The underlying request-queue handle, borrowed for a call.
    pub(crate) fn raw(&self) -> HANDLE {
        self.0.handle.0
    }

    /// How many references currently share this handle.
    #[cfg(test)]
    pub(crate) fn ref_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }

    /// Release this reference, closing now only if it is the last one.
    pub(crate) fn release(self) -> Result<()> {
        match Arc::try_unwrap(self.0) {
            Ok(owned) => owned.close(),
            Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpsys::init::VERSION;
    use windows::Win32::Foundation::GetHandleInformation;
    use windows::Win32::Networking::HttpServer::HttpCreateRequestQueue;

    fn is_send_sync<T: Send + Sync>() {}

    fn new_raw_queue() -> Option<HANDLE> {
        let mut handle = HANDLE::default();
        let code = unsafe {
            HttpCreateRequestQueue(
                VERSION,
                windows::core::PCWSTR::null(),
                None,
                None,
                &mut handle,
            )
        };
        check(code).ok()?;
        Some(handle)
    }

    fn handle_is_valid(handle: HANDLE) -> bool {
        let mut flags = 0u32;
        unsafe { GetHandleInformation(handle, &mut flags) }.is_ok()
    }

    #[test]
    fn queue_handle_is_send_and_sync_by_derivation() {
        is_send_sync::<QueueHandle>();
    }

    #[test]
    fn clone_shares_the_same_raw_handle() {
        // SAFETY: an invalid handle is owned by nobody and is never closed.
        let handle = unsafe { QueueHandle::from_raw(HANDLE::default()) };
        let clone = handle.clone();
        assert_eq!(handle.raw(), clone.raw());
        assert_eq!(handle.ref_count(), 2);
        drop(clone);
        assert_eq!(handle.ref_count(), 1);
    }

    #[test]
    fn last_reference_closes_the_queue() {
        let Some(raw) = new_raw_queue() else {
            return;
        };
        // SAFETY: HTTP.sys returned a newly owned request queue handle.
        let handle = unsafe { QueueHandle::from_raw(raw) };
        handle.release().expect("last reference closes");
        assert!(!handle_is_valid(raw));
    }

    #[test]
    fn clone_kept_alive_across_release_defers_the_close() {
        let Some(raw) = new_raw_queue() else {
            return;
        };
        // SAFETY: HTTP.sys returned a newly owned request queue handle.
        let handle = unsafe { QueueHandle::from_raw(raw) };
        let clone = handle.clone();

        handle.release().expect("release with a clone succeeds");
        assert_eq!(clone.ref_count(), 1);
        assert!(handle_is_valid(raw));

        drop(clone);
        assert!(!handle_is_valid(raw));
    }
}
