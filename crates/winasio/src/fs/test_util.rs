// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Test-only helpers for exercising file teardown paths.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crate::iocp::{IoBuf, IoBufMut, Registrar};
use crate::pipe::{ClientOptions, NamedPipe, ServerOptions};

use super::SetupError;

/// A pending-read pipe pair for teardown tests.
pub type PendingReadPair<S> = (NamedPipe<S>, PendingReadPeer<S>);

/// The client side of a pipe used to keep a server read pending.
pub struct PendingReadPeer<S: crate::iocp::Submitter> {
    _client: NamedPipe<S>,
}

/// Create a registered handle whose reads remain pending until cancelled.
///
/// The returned pipe is the server end of a byte-mode named pipe. The peer is
/// kept open but never writes, so a read submitted through the pipe is genuinely
/// in flight rather than an immediate closed-peer condition.
pub fn pending_read_file<R: Registrar>(
    registrar: &R,
) -> Result<PendingReadPair<R::Io>, SetupError> {
    let name = unique_pipe_name();
    let server = ServerOptions::new(name.clone()).create(registrar)?;
    let client = ClientOptions::new(name).connect(registrar)?;
    let server = expect_ready(server.connect())?.map_err(SetupError::from_windows)?;
    Ok((server, PendingReadPeer { _client: client }))
}

fn expect_ready<F>(future: F) -> Result<F::Output, SetupError>
where
    F: Future,
{
    let mut future = Box::pin(future);
    let mut cx = Context::from_waker(Waker::noop());
    match Pin::as_mut(&mut future).poll(&mut cx) {
        Poll::Ready(output) => Ok(output),
        Poll::Pending => Err(SetupError::Win32(windows::core::Error::from_hresult(
            windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult(),
        ))),
    }
}

fn unique_pipe_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("winasio_fs_pending_{pid:x}_{n:x}")
}

/// A mutable buffer that records when it is dropped.
pub struct DropProbeBuf {
    bytes: Vec<u8>,
    drops: Arc<AtomicUsize>,
}

impl DropProbeBuf {
    /// Allocate a buffer with `capacity` and increment `drops` on drop.
    pub fn with_capacity(capacity: usize, drops: Arc<AtomicUsize>) -> Self {
        DropProbeBuf {
            bytes: Vec::with_capacity(capacity),
            drops,
        }
    }
}

impl Drop for DropProbeBuf {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

// SAFETY: the buffer is a `Vec<u8>`, whose allocation address is stable across
// moves, and `bytes_init` never exceeds the allocation length.
unsafe impl IoBuf for DropProbeBuf {
    fn stable_ptr(&self) -> *const u8 {
        self.bytes.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.bytes.len()
    }
}

// SAFETY: the mutable pointer is the same `Vec<u8>` allocation reported by
// `stable_ptr`; capacity bounds the writable region; and `set_init` delegates to
// `Vec::set_len` after checking that bound.
unsafe impl IoBufMut for DropProbeBuf {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    fn bytes_total(&self) -> usize {
        self.bytes.capacity()
    }

    unsafe fn set_init(&mut self, len: usize) {
        assert!(len <= self.bytes.capacity());
        // SAFETY: the caller guarantees the first `len` bytes were initialised.
        unsafe { self.bytes.set_len(len) };
    }
}
