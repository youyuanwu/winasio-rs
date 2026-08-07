// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Whole-payload I/O helpers shared by files, pipes and sockets.
//!
//! The public result types in this module are returned by helper methods on
//! [`crate::fs::File`], [`crate::pipe::NamedPipe`] and
//! [`crate::net::TcpStream`]. The helper loops themselves are defined once
//! here, so all three share the same progress-accounting and failure
//! classification.

use std::future::Future;

use windows::core::{Error, HRESULT};
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_CONNECTION_ABORTED, ERROR_NETNAME_DELETED, ERROR_NO_DATA,
    ERROR_PIPE_NOT_CONNECTED, ERROR_UNEXP_NET_ERR,
};
use windows::Win32::Networking::WinSock::{
    WSAECONNABORTED, WSAECONNRESET, WSAEDISCON, WSAENETRESET, WSAESHUTDOWN,
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

impl<S: Submitter> WholePayloadIo for crate::net::TcpStream<S> {
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

    /// A socket has no cursor, so there is no position to advance.
    fn advance_position(_position: &mut u64, _transferred: usize) {}
}

impl<S: Submitter> WholePayloadIo for crate::net::UnixStream<S> {
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

    /// A socket has no cursor, so there is no position to advance.
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

/// Map a raw platform error onto the transfer-level vocabulary.
///
/// The list is longer than it looks like it needs to be because the same
/// condition reaches this function under different names depending on the
/// transport *and* on which path resolved the operation. A socket
/// disconnection is `WSAECONNRESET` when it fails inline and
/// `ERROR_NETNAME_DELETED` when it arrives on a completion packet, having gone
/// through `RtlNtStatusToDosError`. Recognising only one spelling would make
/// `read_to_end` return a `Win32` error for a perfectly ordinary close, and
/// only on whichever path the test did not exercise.
fn classify_platform_error(error: Error) -> TransferError {
    // Pipe spellings, plus the eight ways a TCP disconnection can be named.
    const CLOSED: [u32; 11] = [
        ERROR_BROKEN_PIPE.0,
        ERROR_NO_DATA.0,
        ERROR_PIPE_NOT_CONNECTED.0,
        // Socket failures that arrived on a completion packet, having been
        // translated by `RtlNtStatusToDosError`.
        ERROR_NETNAME_DELETED.0,
        ERROR_CONNECTION_ABORTED.0,
        ERROR_UNEXP_NET_ERR.0,
        // The same conditions when the call failed inline and Winsock's own
        // numbering survived.
        WSAECONNRESET.0 as u32,
        WSAECONNABORTED.0 as u32,
        WSAENETRESET.0 as u32,
        WSAESHUTDOWN.0 as u32,
        WSAEDISCON.0 as u32,
    ];

    if CLOSED
        .iter()
        .any(|code| error.code() == HRESULT::from_win32(*code))
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
    fn as_uninit(&mut self) -> &mut crate::iocp::UninitSlice {
        let offset = self.offset;
        let limit = self.limit;
        let whole = self.inner.as_uninit();
        // Bounds-checked rather than pointer arithmetic. An offset past the end
        // must yield an empty tail, not a panic -- `read_exact` walks the offset
        // forward until the buffer is full. `saturating_add` is load-bearing:
        // plain `+` would overflow-panic in debug once `offset + limit` passed
        // `usize::MAX`.
        let start = offset.min(whole.len());
        let end = start.saturating_add(limit).min(whole.len());
        whole.slice_mut(start, end)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(code: u32) -> TransferError {
        classify_platform_error(Error::from_hresult(HRESULT::from_win32(code)))
    }

    #[test]
    fn every_way_a_peer_can_vanish_is_recognised() {
        // The table exists because the same disconnection reaches this function
        // under a different name depending on the transport and on whether the
        // operation failed inline or on a completion packet. A missing entry
        // does not crash — it quietly turns an orderly close into a `Win32`
        // error on one code path only, which is exactly the kind of bug that
        // survives a passing test suite.
        for code in [
            ERROR_BROKEN_PIPE.0,
            ERROR_NO_DATA.0,
            ERROR_PIPE_NOT_CONNECTED.0,
            ERROR_NETNAME_DELETED.0,
            ERROR_CONNECTION_ABORTED.0,
            ERROR_UNEXP_NET_ERR.0,
            WSAECONNRESET.0 as u32,
            WSAECONNABORTED.0 as u32,
            WSAENETRESET.0 as u32,
            WSAESHUTDOWN.0 as u32,
            WSAEDISCON.0 as u32,
        ] {
            assert!(
                matches!(classify(code), TransferError::ClosedPeer),
                "code {code} should classify as a closed peer"
            );
        }
    }

    #[test]
    fn an_unrelated_failure_stays_a_win32_error() {
        // A control. Without it the table above could be satisfied by a
        // classifier that called everything a closed peer.
        use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
        assert!(matches!(
            classify(ERROR_ACCESS_DENIED.0),
            TransferError::Win32(_)
        ));
    }

    #[test]
    fn a_cancelled_transfer_is_not_a_closed_peer() {
        // Cancellation is the caller's own doing. Reporting it as a peer
        // closure would tell `read_to_end` the stream ended cleanly and hand
        // back a truncated buffer with no error.
        use windows::Win32::Foundation::ERROR_OPERATION_ABORTED;
        assert!(matches!(
            classify(ERROR_OPERATION_ABORTED.0),
            TransferError::Win32(_)
        ));
    }
}
