// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Safe asynchronous file I/O.
//!
//! This module opens ordinary Windows file handles in overlapped mode, registers
//! them with one of [`crate::iocp`]'s completion backends, and exposes
//! positional reads and writes that own their buffers until completion.
//!
//! # Invariants and obligations
//!
//! * **Overlapped always.** [`OpenOptions`] always adds
//!   `FILE_FLAG_OVERLAPPED`; callers cannot opt out. Handles adopted with
//!   [`File::from_handle`] must already have been opened for overlapped I/O.
//! * **Register once.** Opening and adoption register the handle immediately.
//!   A handle associated with an I/O completion port or thread-pool object stays
//!   associated for its lifetime, so a second registration is reported as
//!   [`SetupError::AlreadyRegistered`].
//! * **The handle outlives operations created here.** A [`File`] and every read
//!   or write it submits each hold a shared [`Handle`](crate::iocp::Handle).
//!   Dropping the file cannot close a handle that one of its operations may
//!   still cancel through. This guarantee is deliberately bounded: operations a
//!   caller builds independently from [`File::handle`] are outside it, must not
//!   outlive the `File`, and are also cancelled by the file's drop because drop
//!   cancels all I/O on the handle.
//! * **Dropped operation futures are not cancel-safe.** Dropping a read or write
//!   future before it resolves requests cancellation, but the buffer and any
//!   transferred count are not returned. Await the future if you need the buffer
//!   back.
//! * **Teardown is backend-specific.** Dropping a thread-pool-backed file first
//!   requests cancellation, then drops the per-handle registration token, which
//!   drains callbacks, and only then releases the owner's handle reference. A
//!   caller-driven file requests cancellation and returns without driving the
//!   proactor; the caller must keep their own proactor reference alive and keep
//!   polling it until outstanding records are reclaimed. If the file holds the
//!   last proactor reference, dropping that reference may drain and block.
//!
//! The thread-pool aliases below are conveniences for the common backend:
//! callers can write [`ThreadPoolFile`] instead of `File<ThreadPoolIo>`.

mod error;
mod file;
mod options;
pub mod outcome;

pub use error::SetupError;
pub use file::File;
pub use options::OpenOptions;
pub use outcome::ReadOutcome;

/// A [`File`] using the system thread-pool backend.
pub type ThreadPoolFile = File<crate::iocp::ThreadPoolIo>;
