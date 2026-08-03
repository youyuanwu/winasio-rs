// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Shared ownership of a kernel handle.
//!
//! A [`Handle`] is held by the safe type that owns it *and* by every operation
//! that type submits. The kernel handle is closed when the last of those
//! references goes away — never before.
//!
//! # Why this exists
//!
//! Dropping an in-flight operation's future requests cancellation, and
//! cancellation is issued through the handle. If a safe wrapper closed its
//! handle at drop time, an operation future dropped *afterwards* would cancel
//! through a closed handle — and Windows recycles handle values, so that call
//! could land on an unrelated object opened in the meantime. Sharing ownership
//! removes the hazard rather than documenting it: the handle a late
//! cancellation uses is, by construction, still the handle that operation was
//! started on.
//!
//! # What it does not cover
//!
//! The guarantee extends to operations created *through* the safe types. An
//! operation a caller builds from a raw handle borrowed out of a safe type is
//! outside it and must not outlive that type.

use std::fmt;
use std::sync::Arc;

use windows::Win32::Foundation::{CloseHandle, HANDLE};

use super::ops::SendHandle;

/// Shared owner of a kernel handle.
///
/// Cloning is a reference-count bump and performs no allocation, so an
/// operation can hold one without affecting the crate's per-operation
/// allocation budget.
///
/// `Send` and `Sync` are derived rather than asserted: the only field is
/// [`SendHandle`], whose thread-agnostic guarantee is already audited.
#[derive(Clone)]
pub struct Handle(Arc<Owned>);

struct Owned(SendHandle);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0 .0.is_invalid() {
            // Nothing can still be using it: every operation held a clone.
            let _ = unsafe { CloseHandle(self.0 .0) };
        }
    }
}

impl Handle {
    /// Take ownership of a raw handle.
    ///
    /// # Safety
    ///
    /// * `handle` must be a valid kernel handle currently owned by the caller.
    /// * Ownership transfers here: the handle must not be closed by anything
    ///   else, and nothing else may alias it.
    /// * This is deliberately not a safe constructor. A safe one would let a
    ///   caller build an owning wrapper around a borrowed handle and
    ///   double-close it, bypassing the contract on the safe types' own
    ///   handle-adopting constructors.
    pub unsafe fn from_raw(handle: HANDLE) -> Self {
        Handle(Arc::new(Owned(SendHandle(handle))))
    }

    /// The underlying kernel handle.
    ///
    /// Borrowed, not transferred: it stays valid only while this `Handle` — or
    /// another clone of it — is alive.
    pub fn raw(&self) -> HANDLE {
        self.0 .0 .0
    }

    /// How many references currently share this handle.
    ///
    /// Test support: lets a test observe that operations really do hold one.
    #[cfg(any(test, feature = "test-util"))]
    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Handle").field(&self.raw().0).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_send_sync<T: Send + Sync>() {}

    #[test]
    fn handle_is_send_and_sync_without_any_unsafe_impl() {
        // Derived from `SendHandle`, not asserted here. If this ever fails, the
        // fix is the field's type, never a new `unsafe impl` on `Handle`.
        is_send_sync::<Handle>();
    }

    #[test]
    fn clone_shares_the_same_raw_handle() {
        // A pseudo-handle: valid to hold and compare, and closing it is a no-op
        // that Windows tolerates, so no real resource is involved.
        let h = unsafe { Handle::from_raw(HANDLE(-1isize as *mut _)) };
        let c = h.clone();
        assert_eq!(h.raw(), c.raw());
        assert_eq!(h.ref_count(), 2, "the clone shares one allocation");
        drop(c);
        assert_eq!(h.ref_count(), 1);
    }

    #[test]
    fn invalid_handle_is_not_closed() {
        // `Handle::default`-like construction must not attempt a close, so a
        // failed open can be represented without a spurious CloseHandle.
        let h = unsafe { Handle::from_raw(HANDLE::default()) };
        assert!(h.raw().is_invalid());
        drop(h);
    }
}
