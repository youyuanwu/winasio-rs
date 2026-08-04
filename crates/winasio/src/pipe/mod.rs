// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Safe asynchronous named-pipe I/O.
//!
//! This module creates local Windows named pipes in overlapped byte or message
//! mode,
//! registers each handle with one of [`crate::iocp`]'s completion backends, and
//! exposes offsetless reads and writes that own their buffers until completion.
//! Pipe names are bare final components: builders compose the `\\.\pipe\...`
//! path themselves and reject remote or separator-bearing names before any
//! platform call is made.
//!
//! # Invariants and obligations
//!
//! * **Overlapped always.** [`ServerOptions`] and [`ClientOptions`] always add
//!   `FILE_FLAG_OVERLAPPED`; callers cannot opt out.
//! * **Register once.** Creating or connecting registers the handle
//!   immediately. A second registration is reported as
//!   [`SetupError::AlreadyRegistered`].
//! * **Byte mode by default.** Builders default to byte type and byte read
//!   mode. Server options can select message type, and client options can
//!   select message read mode; a message read that does not fit reports
//!   [`ReadOutcome::MoreData`] with the delivered byte count.
//! * **Access direction is runtime-checked.** Every connected pipe exposes both
//!   [`NamedPipe::read`] and [`NamedPipe::write`] regardless of the configured
//!   direction. An operation contrary to that direction resolves as the
//!   platform access failure and returns the caller's buffer.
//! * **The handle outlives operations created here.** A [`NamedPipe`] and every
//!   read or write it submits each hold a shared [`crate::iocp::Handle`].
//!   Dropping the owner cannot close a handle that one of its operations may
//!   still cancel through. This guarantee is deliberately bounded: operations a
//!   caller builds independently from [`NamedPipe::handle`] are outside it, must
//!   not outlive the `NamedPipe`, and are also cancelled by the pipe's drop
//!   because drop cancels all I/O on the handle.
//! * **Dropped operation futures are not cancel-safe.** Dropping a read, write,
//!   connect, or helper future before it resolves requests cancellation. Bytes
//!   already transferred are not undone, and the state owned by that operation
//!   — buffer and transferred count included — is not returned. Await the future
//!   if you need the buffer back.
//! * **Teardown is backend-specific.** Dropping a thread-pool-backed pipe first
//!   requests cancellation, then drops the per-handle registration token, which
//!   drains callbacks, and only then releases the owner's handle reference. A
//!   caller-driven pipe requests cancellation and returns without driving the
//!   proactor; the caller must keep their own proactor reference alive and keep
//!   polling it until outstanding records are reclaimed. If the pipe holds the
//!   last proactor reference, dropping that reference may drain and block.
//! * **Allocation budget.** A warmed single read or write with a caller-supplied
//!   buffer allocates exactly once on the thread-pool backend, for the operation
//!   record. The caller-driven backend allocates at most twice once warmed: the
//!   operation record plus amortised pending-operation bookkeeping.
//!   [`NamedPipe::read_to_end`] allocates for each submitted operation plus
//!   growth of its accumulator, and no per-iteration scratch buffer.
//!
//! # Typestate
//!
//! [`NamedPipeServer`] is an unconnected server instance. It exposes only
//! [`NamedPipeServer::connect`], which consumes the instance and resolves to a
//! connected [`NamedPipe`]. Only a connected pipe can read and write. Calling
//! [`NamedPipe::disconnect`] consumes the connected pipe and returns an
//! unconnected server instance for reuse.
//!
//! These transitions take the handle and registration out of the old value
//! before starting their work. That suppresses owner-drop cancellation for the
//! value being moved into its successor state, while an actually abandoned
//! owner still runs the full teardown sequence.
//!
//! `disconnect` requires exclusive ownership because it consumes the connected
//! pipe. If a program needs concurrent I/O, keep the pipe in an `Arc` only for
//! the connected lifetime, wait for those shared operations to finish, then
//! recover the sole owner (for example with `Arc::try_unwrap`) before
//! disconnecting and serving the next client.
//!
//! # Connect race
//!
//! Windows reports `ERROR_PIPE_CONNECTED` when a client opens the pipe after
//! [`CreateNamedPipeW`](windows::Win32::System::Pipes::CreateNamedPipeW) but
//! before `ConnectNamedPipe`. That is not a failure: the client is already
//! connected. The connect operation treats this condition as immediate success
//! and does not wait for a completion packet that Windows will never post.
//!
//! # Busy pipes
//!
//! The crate deliberately does not wait or retry when all instances are busy:
//! `WaitNamedPipeW` is synchronous and this module owns no runtime-agnostic
//! timer. Callers who want to wait should retry with their own runtime's timer,
//! treating [`SetupError::Busy`] as the retryable condition.
//!
//! ```no_run
//! # use std::thread;
//! # use std::time::Duration;
//! # use winasio::iocp::ThreadPool;
//! # use winasio::pipe::{ClientOptions, SetupError};
//! # fn connect_with_retry(name: &str) -> Result<(), SetupError> {
//! loop {
//!     match ClientOptions::new(name).connect(&ThreadPool) {
//!         Ok(_pipe) => return Ok(()),
//!         Err(SetupError::Busy) => thread::sleep(Duration::from_millis(10)),
//!         Err(e) => return Err(e),
//!     }
//! }
//! # }
//! ```

mod client;
mod connected;
mod name;
mod server;

pub use crate::fs::{ReadOutcome, SetupError};
pub use client::ClientOptions;
pub use connected::NamedPipe;
pub use name::MAX_NAME_COMPONENT_LEN;
pub use server::{AccessDirection, NamedPipeServer, PipeMode, ServerOptions};

/// A [`NamedPipeServer`] using the system thread-pool backend.
pub type ThreadPoolNamedPipeServer = NamedPipeServer<crate::iocp::ThreadPoolIo>;

/// A connected [`NamedPipe`] using the system thread-pool backend.
pub type ThreadPoolNamedPipe = NamedPipe<crate::iocp::ThreadPoolIo>;
