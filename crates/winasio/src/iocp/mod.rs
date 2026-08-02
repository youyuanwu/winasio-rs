// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Runtime-agnostic infrastructure for Windows overlapped I/O.
//!
//! This module turns any Windows API that takes an `OVERLAPPED` into something
//! awaitable, without depending on a particular async runtime and without
//! requiring changes to this crate for each new API.
//!
//! # Defining an operation
//!
//! Implement [`OpCode`]. The operation owns whatever state Windows touches —
//! a byte buffer, a caller-allocated structure, or both — and gets it back when
//! the operation completes. There is deliberately **no buffer trait bound** on
//! [`OpCode`], because many Windows APIs fill a struct rather than a byte slice;
//! [`IoBuf`]/[`IoBufMut`] are conveniences for the cases that are byte slices.
//!
//! ```no_run
//! use std::task::Poll;
//! use winasio::iocp::{win32_result, IntoInner, OpCode};
//! use windows::Win32::Foundation::HANDLE;
//! use windows::Win32::System::IO::OVERLAPPED;
//!
//! struct MyOp {
//!     handle: isize,          // a Send-able stand-in for HANDLE
//!     buffer: Vec<u8>,        // owned for the operation's lifetime
//! }
//!
//! unsafe impl OpCode for MyOp {
//!     unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<windows::core::Result<usize>> {
//!         // Call the overlapped API here, deriving every pointer from `self`.
//!         unsafe { win32_result(false, optr) }
//!     }
//! }
//!
//! impl IntoInner for MyOp {
//!     type Inner = Vec<u8>;
//!     fn into_inner(self) -> Vec<u8> { self.buffer }
//! }
//! ```
//!
//! # Choosing a backend
//!
//! A handle is registered with exactly one completion mechanism, permanently.
//! Pick per handle:
//!
//! | | [`Proactor`] (own port) | [`ThreadPoolIo`] (system-managed) |
//! |---|---|---|
//! | Who drives completions | you, via [`Proactor::poll`] | the Win32 thread pool |
//! | Thread affinity | `!Send`; submit and poll on one thread | `Send + Sync` |
//! | Suits | single-threaded loops, tight control over when work is processed | multi-threaded runtimes, or anywhere you do not want a driver |
//! | Batching | yes, many completions per wait | no |
//! | Shutdown | deterministic drain | cancel-and-drain on drop |
//!
//! Registering a handle twice — with either backend, in either order — fails
//! with [`RegistrationError::AlreadyRegistered`].
//!
//! The [`Submit`] futures both backends produce are [`Send`] whenever the
//! operation is, so only the [`Proactor`] itself is thread-bound.
//!
//! # Ownership and cancellation
//!
//! Submitting an operation transfers its state to the driver. If the awaiting
//! future is dropped before completion:
//!
//! * cancellation is requested immediately;
//! * **the state is not returned to the caller** — the buffer is lost;
//! * its memory is retained until Windows delivers the completion, which is
//!   normally sub-millisecond but is unbounded on a stalled handle;
//! * only then is the allocation released.
//!
//! This is inherent to completion-based I/O rather than a design choice: the
//! kernel may still be writing into the buffer, so releasing it any earlier
//! would be a use-after-free. Callers who need a buffer back must await the
//! operation rather than dropping it.

mod buf;
mod future;
mod op;
pub mod ops;
mod port;
mod proactor;
mod raw;
mod threadpool;

pub use buf::{BufResult, IoBuf, IoBufMut};
pub use future::Submit;
pub use op::{win32_result, IntoInner, OpCode, OpType};
pub use ops::{ReadAt, SendHandle, WaitForHandle, WriteAt};
pub use port::RegistrationError;
pub use proactor::{Notify, Proactor};
pub use threadpool::ThreadPoolIo;

#[cfg(any(test, feature = "test-util"))]
pub use raw::live_operations;
