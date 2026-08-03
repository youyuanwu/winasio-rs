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
//! * **Dropped operation futures are not cancel-safe.** Dropping a read, write,
//!   or helper future before it resolves requests cancellation. Bytes already
//!   transferred are not undone, and neither the buffer nor any transferred
//!   count is returned. Await the future if you need the buffer back.
//! * **Teardown is backend-specific.** Dropping a thread-pool-backed file first
//!   requests cancellation, then drops the per-handle registration token, which
//!   drains callbacks, and only then releases the owner's handle reference. A
//!   caller-driven file requests cancellation and returns without driving the
//!   proactor; the caller must keep their own proactor reference alive and keep
//!   polling it until outstanding records are reclaimed. If the file holds the
//!   last proactor reference, dropping that reference may drain and block.
//! * **Allocation budget.** A warmed single read or write with a caller-supplied
//!   buffer allocates exactly once on the thread-pool backend, for the operation
//!   record. The caller-driven backend allocates at most twice once warmed: the
//!   operation record plus amortised pending-operation bookkeeping.
//!   [`File::read_to_end`] allocates for each submitted operation plus growth of
//!   its accumulator, and no per-iteration scratch buffer.
//!
//! The thread-pool aliases below are conveniences for the common backend:
//! callers can write [`ThreadPoolFile`] instead of `File<ThreadPoolIo>`.

mod error;
mod file;
mod options;
/// Read outcome classification shared by file and pipe reads.
pub mod outcome;
/// Test-only helpers for exercising teardown paths.
#[cfg(feature = "test-util")]
pub mod test_util;

pub use error::SetupError;
pub use file::File;
pub use options::OpenOptions;
pub use outcome::ReadOutcome;

/// A [`File`] using the system thread-pool backend.
pub type ThreadPoolFile = File<crate::iocp::ThreadPoolIo>;
