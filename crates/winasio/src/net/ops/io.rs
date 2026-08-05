// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! `WSARecv` / `WSASend` operations.
//!
//! These mirror [`crate::iocp::ops::stream`]'s `ReadHandle` / `WriteHandle`,
//! including the idempotent `record_completion` pattern, and differ in two
//! ways.
//!
//! The first is the byte-count out-parameter: it is `None`. `WSARecv`'s
//! `lpNumberOfBytesRecvd` is documented as required for a non-overlapped call
//! and optional otherwise, which was worth measuring rather than trusting.
//! Measured (M24): null is accepted on both the inline and the pending path,
//! and `OVERLAPPED::InternalHigh` is filled either way. Passing `None` is what
//! lets these operations honour [`OpCode`]'s contract — every pointer handed to
//! Windows is derived from `&mut self` — for a count that would otherwise have
//! to live on `operate`'s stack.
//!
//! The second is classification: a socket's end of stream is a *successful*
//! zero-byte read, not `ERROR_HANDLE_EOF`, and the same disconnection reaches
//! the caller under two different spellings depending on which path resolved
//! it. See [`super::super::outcome`].

use std::task::Poll;

use windows::core::{Result, PSTR};
use windows::Win32::Networking::WinSock::{WSARecv, WSASend, WSABUF};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::fs::ReadOutcome;
use crate::iocp::ops::sys::checked_u32_len;
use crate::iocp::{win32_result, IntoInner, IoBuf, IoBufMut, OpCode};

use super::super::outcome::classify_socket_read;
use super::super::socket::Socket;

/// A `WSABUF` that can travel with the operation that owns it.
///
/// Winsock requires the descriptor array to stay valid for the whole of a
/// pending overlapped call, so it cannot live on `operate`'s stack — it has to
/// be a field. `WSABUF`'s data pointer is a `PSTR`, which is `!Send`, and that
/// would make every operation holding one unspawnable on a multi-threaded
/// executor.
#[repr(transparent)]
struct OwnedWsaBuf(WSABUF);

// SAFETY: the raw pointer inside is not an independent capability — it always
// points into the buffer field of the same operation, which moves with it and
// which is itself `Send` (the `OpCode` impls below require `B: Send`). Nothing
// outside the operation can reach it, so moving the operation to another thread
// moves the pointer and its referent together. This is the same reasoning that
// lets `RawOp` be `Send` despite the raw pointers in `OVERLAPPED`.
unsafe impl Send for OwnedWsaBuf {}

impl OwnedWsaBuf {
    fn zeroed() -> Self {
        OwnedWsaBuf(WSABUF::default())
    }
}

/// Receive into a caller-owned buffer.
pub(crate) struct RecvSocket<B: IoBufMut> {
    socket: Socket,
    buffer: B,
    /// The descriptor the kernel is given.
    ///
    /// Held in the operation rather than on `operate`'s stack. `WSARecv` only
    /// reads it during the call, so a stack copy would be sound — keeping it
    /// here is what makes the "every pointer derived from `&mut self`" rule
    /// mechanical instead of a case-by-case argument.
    wsabuf: OwnedWsaBuf,
    /// How many bytes were asked for.
    ///
    /// Load-bearing, not decoration: the classifier cannot tell a graceful
    /// close from a caller who passed an empty buffer without it.
    requested: usize,
    /// The in/out flags slot `WSARecv` is given.
    ///
    /// It lives here for the same reason `wsabuf` does. `WSARecv` documents
    /// this as an in/out parameter, and for a pending overlapped receive the
    /// kernel may write to it after `operate` has returned — at which point a
    /// stack slot would be someone else's frame.
    flags: u32,
    outcome: Option<ReadOutcome>,
}

impl<B: IoBufMut> RecvSocket<B> {
    pub(crate) fn new(socket: Socket, buffer: B) -> Self {
        RecvSocket {
            socket,
            buffer,
            wsabuf: OwnedWsaBuf::zeroed(),
            requested: 0,
            flags: 0,
            outcome: None,
        }
    }

    fn record_completion(&mut self, result: &Result<usize>) {
        self.outcome = classify_socket_read(result, self.requested);
        if let Ok(n) = result {
            let n = (*n).min(self.buffer.bytes_total());
            // SAFETY: Windows reported initialising `n` bytes of the buffer
            // this operation has owned for the whole call. The value is clamped
            // to the buffer's capacity before publication.
            unsafe { self.buffer.set_init(n) };
        }
    }

    /// Convert the low-level byte count into the safe read outcome.
    pub(crate) fn finish(self, result: Result<usize>) -> (Result<ReadOutcome>, B) {
        let outcome = match (result, self.outcome) {
            (_, Some(outcome)) => Ok(outcome),
            (Ok(n), None) => Ok(ReadOutcome::Bytes(n)),
            (Err(e), None) => Err(e),
        };
        (outcome, self.buffer)
    }
}

impl<B: IoBufMut> IntoInner for RecvSocket<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.buffer
    }
}

// SAFETY: `operate` derives the socket from `self.socket`, the descriptor from
// `self.wsabuf`, and the data pointer inside it from `self.buffer` — each
// reached through `&mut self`, so all of them outlive the pinned operation. No
// byte-count pointer is passed at all.
unsafe impl<B: IoBufMut + Send> OpCode for RecvSocket<B> {
    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let slice = self.buffer.as_uninit();
        let len = match checked_u32_len(slice.len()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        self.requested = slice.len();
        self.wsabuf = OwnedWsaBuf(WSABUF {
            len,
            buf: PSTR(slice.as_mut_ptr().cast::<u8>()),
        });

        self.flags = 0;
        // SAFETY: the descriptor, the storage it points at and the flags slot
        // are all owned by this operation, whose allocation is retained until
        // completion — nothing here is borrowed from `operate`'s frame, which
        // is gone by the time a pending `WSARecv` completes. `None` for the
        // count is measured-safe (M24); the count is recovered from
        // `InternalHigh` by `win32_result`, or from the completion packet.
        let started = unsafe {
            WSARecv(
                self.socket.raw(),
                std::slice::from_mut(&mut self.wsabuf.0),
                None,
                &mut self.flags,
                Some(optr),
                None,
            )
        };
        // No Windows call may occur between `WSARecv` and `win32_result`.
        let result = unsafe { win32_result(started == 0, optr) };
        if let Poll::Ready(ref ready) = result {
            self.record_completion(ready);
        }
        result
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        // SAFETY: `optr` is the same pointer passed to `operate`; the socket is
        // kept alive by this operation's own `Socket` clone, so a late
        // cancellation cannot name a closed socket.
        unsafe { CancelIoEx(self.socket.as_handle(), Some(optr)) }
    }

    unsafe fn on_complete(&mut self, result: &Result<usize>) {
        self.record_completion(result);
    }
}

/// Send from a caller-owned buffer.
pub(crate) struct SendSocket<B: IoBuf> {
    socket: Socket,
    buffer: B,
    wsabuf: OwnedWsaBuf,
}

impl<B: IoBuf> SendSocket<B> {
    pub(crate) fn new(socket: Socket, buffer: B) -> Self {
        SendSocket {
            socket,
            buffer,
            wsabuf: OwnedWsaBuf::zeroed(),
        }
    }
}

impl<B: IoBuf> IntoInner for SendSocket<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.buffer
    }
}

// SAFETY: as for `RecvSocket` — socket, descriptor and payload pointer are all
// reached through `&mut self`, and no count pointer is passed.
unsafe impl<B: IoBuf + Send> OpCode for SendSocket<B> {
    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let len = match checked_u32_len(self.buffer.bytes_init()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        self.wsabuf = OwnedWsaBuf(WSABUF {
            len,
            buf: PSTR(self.buffer.stable_ptr() as *mut u8),
        });

        // SAFETY: the descriptor points at initialised bytes owned by this
        // operation, whose allocation is retained until completion.
        let started = unsafe {
            WSASend(
                self.socket.raw(),
                std::slice::from_ref(&self.wsabuf.0),
                None,
                0,
                Some(optr),
                None,
            )
        };
        // No Windows call may occur between `WSASend` and `win32_result`.
        unsafe { win32_result(started == 0, optr) }
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        // SAFETY: as for `RecvSocket::cancel`.
        unsafe { CancelIoEx(self.socket.as_handle(), Some(optr)) }
    }
}
