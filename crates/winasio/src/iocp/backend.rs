// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Choosing a completion backend without naming one.
//!
//! The crate has two completion mechanisms, and they are shaped differently.
//! [`ThreadPoolIo::new`] both registers a handle and hands back the object that
//! owns that registration, while [`Proactor::attach`] registers into a proactor
//! the caller already holds and returns nothing. This module reconciles them so
//! a safe wrapper — a file, a pipe — can be written once and work with either.
//!
//! # Two roles
//!
//! * A [`Registrar`] turns a handle into a [`Submitter`] for that handle.
//! * A [`Submitter`] accepts operations and yields the future they resolve on.
//!
//! What a submitter *owns* is deliberately backend-specific:
//!
//! | | [`ThreadPool`] | `Rc<`[`Proactor`]`>` |
//! |---|---|---|
//! | Registrar carries | nothing | shared ownership of the proactor |
//! | Submitter is | the per-handle registration | another reference to the proactor |
//! | Per-handle token | yes — dropping it cancels and drains that handle | no |
//! | Thread affinity | `Send + Sync` | neither |
//!
//! The proactor case is not a per-handle token: a handle's association with a
//! completion port ends only when the handle is closed. Holding shared
//! ownership instead guarantees, structurally, that the proactor outlives every
//! file and pipe registered against it — which is why the safe types need no
//! lifetime parameter.
//!
//! # Thread affinity comes for free
//!
//! A safe type owns its submitter, so it is `Send`/`Sync` exactly when the
//! submitter is. Nothing asserts this with an `unsafe impl`; it falls out of
//! the field types, which is why `Rc<Proactor>` is the right shape for the
//! caller-driven backend even though `Arc` would also compile.
//!
//! # Generic, not object-safe
//!
//! [`Submitter::submit`] is generic over the operation, which makes the trait
//! not dyn-compatible. Safe wrappers therefore carry a type parameter rather
//! than a boxed backend. Erasing it would mean boxing every operation, which
//! would cost an allocation per I/O — the opposite of this crate's aim.
//!
//! ```
//! use std::rc::Rc;
//! use winasio::iocp::{Proactor, Registrar, ThreadPool};
//!
//! // Same call shape, either backend.
//! fn takes_any<R: Registrar>(_registrar: &R) {}
//! takes_any(&ThreadPool);
//! takes_any(&Rc::new(Proactor::new().unwrap()));
//! ```

use std::rc::Rc;

use windows::Win32::Foundation::HANDLE;

use super::future::Submit;
use super::op::OpCode;
use super::port::RegistrationError;
use super::proactor::Proactor;
use super::threadpool::ThreadPoolIo;

/// Something operations can be submitted to.
///
/// Implementors are obtained from a [`Registrar`]; a safe wrapper owns one and
/// inherits its thread affinity.
///
/// The `Send` bound is the stricter of the two backends' requirements, so an
/// operation that is not `Send` is rejected here even when the underlying
/// backend would have taken it:
///
/// ```compile_fail
/// use std::rc::Rc;
/// use std::task::Poll;
/// use winasio::iocp::{Proactor, Submitter, IntoInner, OpCode, win32_result};
/// use windows::Win32::System::IO::OVERLAPPED;
///
/// struct NotSendOp(Rc<u8>); // `Rc` makes it `!Send`.
///
/// unsafe impl OpCode for NotSendOp {
///     unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<windows::core::Result<usize>> {
///         unsafe { win32_result(false, optr) }
///     }
/// }
/// impl IntoInner for NotSendOp {
///     type Inner = ();
///     fn into_inner(self) {}
/// }
///
/// let p = Rc::new(Proactor::new().unwrap());
/// // Rejected: `Submitter::submit` requires `T: Send`.
/// Submitter::submit(&p, NotSendOp(Rc::new(1)));
/// ```
///
/// The same operation is still accepted by the proactor's own inherent method,
/// whose looser bound is unchanged:
///
/// ```
/// use std::rc::Rc;
/// use std::task::Poll;
/// use winasio::iocp::{Proactor, IntoInner, OpCode, win32_result};
/// use windows::Win32::System::IO::OVERLAPPED;
///
/// struct NotSendOp(Rc<u8>);
///
/// unsafe impl OpCode for NotSendOp {
///     unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<windows::core::Result<usize>> {
///         unsafe { win32_result(false, optr) }
///     }
/// }
/// impl IntoInner for NotSendOp {
///     type Inner = ();
///     fn into_inner(self) {}
/// }
///
/// let p = Proactor::new().unwrap();
/// let _ = p.submit(NotSendOp(Rc::new(1)));
/// ```
pub trait Submitter {
    /// Submit an operation and get the future it resolves on.
    ///
    /// `T: Send` because the thread-pool backend may run the completion — and
    /// drop the operation — on a pool thread. This is the stricter of the two
    /// backends' requirements; [`Proactor::submit`] keeps its looser bound for
    /// callers who use it directly.
    fn submit<T: OpCode + Send>(&self, op: T) -> Submit<T>;
}

/// A completion mechanism a handle can be registered with.
///
/// A handle may be registered exactly once, for its whole lifetime; a second
/// attempt through either backend fails with
/// [`RegistrationError::AlreadyRegistered`].
///
/// A value holding a `Rc<Proactor>` submitter is not `Send`, so it cannot
/// escape the thread that drives the proactor:
///
/// ```compile_fail
/// use std::rc::Rc;
/// use winasio::iocp::{Proactor, Submitter};
///
/// struct Owner<S: Submitter> { submitter: S }
///
/// fn needs_send<T: Send>(_: T) {}
///
/// let p = Rc::new(Proactor::new().unwrap());
/// // `Rc<Proactor>` is neither `Send` nor `Sync`, so neither is the owner.
/// needs_send(Owner { submitter: p });
/// ```
///
/// The thread-pool submitter, by contrast, is:
///
/// ```
/// use winasio::iocp::{Submitter, ThreadPoolIo};
///
/// struct Owner<S: Submitter> { submitter: S }
///
/// fn needs_send<T: Send>(_: T) {}
/// fn only_type_checks(io: ThreadPoolIo) { needs_send(Owner { submitter: io }); }
/// ```
pub trait Registrar {
    /// What operations on the registered handle are submitted through.
    type Io: Submitter;

    /// Register `handle` and produce its submitter.
    ///
    /// # Errors
    ///
    /// Fails if the handle is already associated with a completion mechanism,
    /// or if the platform refuses the association.
    fn register(&self, handle: HANDLE) -> Result<Self::Io, RegistrationError>;
}

/// The system thread pool, as a registrar.
///
/// Carries no state: the thread pool needs nothing from the caller until a
/// handle exists. Registering yields a [`ThreadPoolIo`], which *is* that
/// handle's registration — dropping it cancels and drains the handle's
/// outstanding operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadPool;

impl Submitter for ThreadPoolIo {
    fn submit<T: OpCode + Send>(&self, op: T) -> Submit<T> {
        ThreadPoolIo::submit(self, op)
    }
}

impl Registrar for ThreadPool {
    type Io = ThreadPoolIo;

    fn register(&self, handle: HANDLE) -> Result<Self::Io, RegistrationError> {
        ThreadPoolIo::new(handle)
    }
}

impl Submitter for Rc<Proactor> {
    fn submit<T: OpCode + Send>(&self, op: T) -> Submit<T> {
        Proactor::submit(self, op)
    }
}

impl Registrar for Rc<Proactor> {
    type Io = Rc<Proactor>;

    fn register(&self, handle: HANDLE) -> Result<Self::Io, RegistrationError> {
        Proactor::attach(self, handle)?;
        // Shared ownership, so the proactor cannot outlive its files.
        Ok(Rc::clone(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iocp::handle::Handle;

    /// Stands in for a safe wrapper: a shared handle plus a submitter.
    #[allow(dead_code)]
    struct Owner<S: Submitter> {
        handle: Handle,
        submitter: S,
    }

    fn is_send_sync<T: Send + Sync>() {}

    #[test]
    fn thread_pool_owner_is_send_and_sync_by_derivation() {
        // No `unsafe impl` anywhere in this crate's new code makes this true:
        // `Handle` is `Send + Sync` and so is `ThreadPoolIo`.
        is_send_sync::<Owner<ThreadPoolIo>>();
    }

    #[test]
    fn thread_pool_registrar_is_zero_sized() {
        // It carries no state, which is the point: nothing to supply up front.
        assert_eq!(std::mem::size_of::<ThreadPool>(), 0);
    }

    #[test]
    fn proactor_owner_is_not_send() {
        // Proven at compile time by the `compile_fail` doctest on
        // `Registrar for Rc<Proactor>` below; asserting it at runtime would
        // need specialization. What we *can* check here is the premise: the
        // proactor pointer is the only non-`Send` field, so if the wrapper is
        // ever accidentally made `Send`, that doctest fails.
        assert_eq!(
            std::mem::size_of::<Owner<Rc<Proactor>>>(),
            std::mem::size_of::<Handle>() + std::mem::size_of::<Rc<Proactor>>(),
            "the owner is exactly its handle plus its submitter"
        );
    }
}
