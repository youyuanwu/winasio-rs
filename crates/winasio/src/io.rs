// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Whole-payload I/O helpers shared by files and pipes.
//!
//! The public result types in this module are returned by helper methods on
//! [`crate::fs::File`] and [`crate::pipe::NamedPipe`]. The helper loops
//! themselves are defined once here, so files and pipes share the same
//! progress-accounting and failure classification.

use std::future::Future;

use windows::core::Error;
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_NETNAME_DELETED, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
};

use crate::fs::ReadOutcome;
use crate::iocp::{IoBuf, IoBufMut, OpResult, Submitter};

const HELPER_CHUNK: usize = 4096;

/// Failure categories reported by whole-payload helpers.
#[derive(Debug)]
pub enum TransferError {
    /// The stream ended before a fixed-size read could be completed.
    UnexpectedEof,
    /// A stream peer closed its end while the helper was still transferring.
    ClosedPeer,
    /// Any other platform failure.
    Win32(Error),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::UnexpectedEof => write!(f, "unexpected end of stream"),
            TransferError::ClosedPeer => write!(f, "peer closed the stream"),
            TransferError::Win32(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TransferError {}

/// Result of a whole-payload helper.
///
/// The buffer and transferred count are present on both success and failure.
#[derive(Debug)]
#[must_use = "the caller's buffer and transfer count are returned here"]
pub struct TransferResult<B> {
    /// Whether the helper finished the requested transfer.
    pub result: Result<(), TransferError>,
    /// The caller's buffer.
    pub buffer: B,
    /// Bytes transferred before the helper stopped.
    pub transferred: usize,
}

impl<B> TransferResult<B> {
    fn success(buffer: B, transferred: usize) -> Self {
        TransferResult {
            result: Ok(()),
            buffer,
            transferred,
        }
    }

    fn failure(buffer: B, transferred: usize, error: TransferError) -> Self {
        TransferResult {
            result: Err(error),
            buffer,
            transferred,
        }
    }

    /// Split into result, buffer, and transferred count.
    pub fn into_parts(self) -> (Result<(), TransferError>, B, usize) {
        (self.result, self.buffer, self.transferred)
    }
}

pub(crate) trait WholePayloadIo {
    fn read_once<B>(
        &self,
        position: u64,
        buffer: B,
    ) -> impl Future<Output = OpResult<ReadOutcome, B>>
    where
        B: IoBufMut + Send;

    fn write_once<B>(&self, position: u64, buffer: B) -> impl Future<Output = OpResult<usize, B>>
    where
        B: IoBuf + Send;

    fn advance_position(position: &mut u64, transferred: usize);
}

impl<S: Submitter> WholePayloadIo for crate::fs::File<S> {
    fn read_once<B>(
        &self,
        position: u64,
        buffer: B,
    ) -> impl Future<Output = OpResult<ReadOutcome, B>>
    where
        B: IoBufMut + Send,
    {
        self.read_at(position, buffer)
    }

    fn write_once<B>(&self, position: u64, buffer: B) -> impl Future<Output = OpResult<usize, B>>
    where
        B: IoBuf + Send,
    {
        self.write_at(position, buffer)
    }

    fn advance_position(position: &mut u64, transferred: usize) {
        *position = position.saturating_add(transferred as u64);
    }
}

impl<S: Submitter> WholePayloadIo for crate::pipe::NamedPipe<S> {
    fn read_once<B>(
        &self,
        _position: u64,
        buffer: B,
    ) -> impl Future<Output = OpResult<ReadOutcome, B>>
    where
        B: IoBufMut + Send,
    {
        self.read(buffer)
    }

    fn write_once<B>(&self, _position: u64, buffer: B) -> impl Future<Output = OpResult<usize, B>>
    where
        B: IoBuf + Send,
    {
        self.write(buffer)
    }

    fn advance_position(_position: &mut u64, _transferred: usize) {}
}

pub(crate) async fn write_all<T, B>(io: &T, mut position: u64, mut buffer: B) -> TransferResult<B>
where
    T: WholePayloadIo,
    B: IoBuf + Send,
{
    let target = buffer.bytes_init();
    let mut transferred = 0;
    while transferred < target {
        let tail = TailBuf::new(buffer, transferred, HELPER_CHUNK);
        let OpResult(result, tail) = io.write_once(position, tail).await;
        buffer = tail.into_inner();
        match result {
            Ok(0) => {
                return TransferResult::failure(buffer, transferred, TransferError::ClosedPeer);
            }
            Ok(n) => {
                transferred += n;
                T::advance_position(&mut position, n);
            }
            Err(e) => {
                return TransferResult::failure(buffer, transferred, classify_platform_error(e));
            }
        }
    }
    TransferResult::success(buffer, transferred)
}

pub(crate) async fn read_exact<T, B>(io: &T, mut position: u64, mut buffer: B) -> TransferResult<B>
where
    T: WholePayloadIo,
    B: IoBufMut + Send,
{
    let target = buffer.bytes_total();
    let mut transferred = 0;
    while transferred < target {
        let tail = TailBuf::new(buffer, transferred, HELPER_CHUNK);
        let OpResult(result, tail) = io.read_once(position, tail).await;
        buffer = tail.into_inner();
        match result {
            // A zero-length transfer is not end-of-stream. On a message-mode
            // pipe it is a real, empty message; the caller asked for a specific
            // number of bytes, so consuming one simply means this iteration
            // made no progress and the next read continues. Only the outcomes
            // that genuinely mean "there will be no more data" cut the loop
            // short. Progress is guaranteed because `TailBuf` always presents
            // non-empty spare capacity, so `Bytes(0)` reflects the peer, not a
            // buffer with no room.
            Ok(ReadOutcome::Bytes(0)) => {}
            // These are different conditions and FR-033 requires the caller be
            // able to tell them apart: a file simply ended, versus a peer that
            // went away mid-transfer. Collapsing both into `UnexpectedEof` means
            // a caller matching on `ClosedPeer` never sees a pipe closure.
            Ok(ReadOutcome::Eof) => {
                return TransferResult::failure(buffer, transferred, TransferError::UnexpectedEof);
            }
            Ok(ReadOutcome::ClosedPeer) => {
                return TransferResult::failure(buffer, transferred, TransferError::ClosedPeer);
            }
            Ok(ReadOutcome::Bytes(n) | ReadOutcome::MoreData(n)) => {
                // Defence in depth: clamp to what was actually asked for before
                // using this as the next window's offset. The operation clamps
                // before `set_init`, so a platform over-report cannot publish
                // uninitialised bytes there -- but this offset feeds the *next*
                // `TailBuf`, and an unclamped value would move the window past
                // the initialised region and expose those bytes through a safe
                // slice. Windows cannot currently report more than the requested
                // length, so this is unreachable; it is cheap insurance against
                // that ever changing.
                let n = n.min(target - transferred);
                transferred += n;
                T::advance_position(&mut position, n);
            }
            Err(e) => {
                return TransferResult::failure(buffer, transferred, classify_platform_error(e));
            }
        }
    }
    TransferResult::success(buffer, transferred)
}

pub(crate) async fn read_to_end<T>(
    io: &T,
    mut position: u64,
    mut buffer: Vec<u8>,
) -> TransferResult<Vec<u8>>
where
    T: WholePayloadIo,
{
    let mut transferred = 0;
    loop {
        if buffer.len() == buffer.capacity() {
            buffer.reserve(HELPER_CHUNK);
        }
        let previous_len = buffer.len();
        let tail = TailBuf::new(buffer, previous_len, HELPER_CHUNK);
        let OpResult(result, tail) = io.read_once(position, tail).await;
        buffer = tail.into_inner();
        match result {
            Ok(ReadOutcome::Eof | ReadOutcome::ClosedPeer) => {
                return TransferResult::success(buffer, transferred);
            }
            Ok(ReadOutcome::Bytes(0)) => {
                // This was submitted with non-empty spare capacity, so it is
                // not a self-induced zero-capacity read that could spin. A
                // zero byte result is therefore a real zero-length pipe message
                // and the next iteration awaits another operation. A peer can
                // keep sending empty messages forever, just as it can keep
                // sending non-empty data forever, but the helper is not looping
                // locally without completed I/O.
            }
            Ok(ReadOutcome::Bytes(n) | ReadOutcome::MoreData(n)) => {
                transferred += n;
                T::advance_position(&mut position, n);
            }
            Err(e) => {
                return TransferResult::failure(buffer, transferred, classify_platform_error(e));
            }
        }
    }
}

fn classify_platform_error(error: Error) -> TransferError {
    if error.code() == ERROR_BROKEN_PIPE.to_hresult()
        || error.code() == ERROR_NO_DATA.to_hresult()
        || error.code() == ERROR_PIPE_NOT_CONNECTED.to_hresult()
        || error.code() == ERROR_NETNAME_DELETED.to_hresult()
    {
        TransferError::ClosedPeer
    } else {
        TransferError::Win32(error)
    }
}

struct TailBuf<B> {
    inner: B,
    offset: usize,
    limit: usize,
}

impl<B> TailBuf<B> {
    fn new(inner: B, offset: usize, limit: usize) -> Self {
        TailBuf {
            inner,
            offset,
            limit,
        }
    }

    fn into_inner(self) -> B {
        self.inner
    }
}

unsafe impl<B: IoBuf> IoBuf for TailBuf<B> {
    fn stable_ptr(&self) -> *const u8 {
        self.inner.stable_ptr().wrapping_add(self.offset)
    }

    fn bytes_init(&self) -> usize {
        self.inner
            .bytes_init()
            .saturating_sub(self.offset)
            .min(self.limit)
    }
}

unsafe impl<B: IoBufMut> IoBufMut for TailBuf<B> {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.inner.stable_mut_ptr().wrapping_add(self.offset)
    }

    fn bytes_total(&self) -> usize {
        self.inner
            .bytes_total()
            .saturating_sub(self.offset)
            .min(self.limit)
    }

    /// # Safety
    ///
    /// The first `len` bytes of this tail must have been initialised by the
    /// completed operation, and `len` must not exceed this tail's total length.
    unsafe fn set_init(&mut self, len: usize) {
        let len = len.min(self.bytes_total());
        // SAFETY: `len` was reported for this tail buffer and then clamped to
        // this tail's writable capacity, so publishing `offset + len` stays
        // within the underlying buffer's total capacity.
        unsafe { self.inner.set_init(self.offset + len) };
    }
}
