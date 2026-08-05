// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The `ConnectEx` operation.
//!
//! `ConnectEx` is the only asynchronous connect Winsock offers, and it comes
//! with two requirements that are easy to miss because neither produces a
//! plausible error message when skipped:
//!
//! * the socket must already be **bound** — otherwise the call fails with
//!   `WSAEINVAL`, which reads like a bad argument rather than a missing step;
//! * on success the socket is left in a half-initialised state until
//!   `SO_UPDATE_CONNECT_CONTEXT` is applied, so `getpeername`, `shutdown` and
//!   the socket's own options misbehave in ways that look like unrelated bugs.
//!
//! The bind is done by the caller before submitting (it is fallible and belongs
//! with the other setup failures); the context update is done here, on the
//! completion path, so no caller can forget it.

use std::task::Poll;

use windows::core::{Error, Result, HRESULT};
use windows::Win32::Networking::WinSock::WSAENOTCONN;
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::iocp::{win32_result, IntoInner, OpCode};

use super::super::addr::SockAddrBytes;
use super::super::ext::extensions;
use super::super::socket::Socket;

/// Connect an already-bound socket to `addr`.
pub(crate) struct ConnectSocket {
    socket: Socket,
    /// The destination, owned by the operation.
    ///
    /// `ConnectEx` reads it for the duration of a *pending* call, not just
    /// during the call itself, so it cannot live on `operate`'s stack.
    addr: SockAddrBytes,
    /// The `lpdwBytesSent` slot.
    ///
    /// Ignored by `ConnectEx` when no send buffer is supplied, but owned here
    /// anyway: a stack slot would be a pointer not derived from `&mut self`,
    /// and the point of the rule is that it needs no per-call argument.
    bytes_sent: u32,
    /// Whether the context update ran and what it said.
    ///
    /// `on_complete` is explicitly documented as "not guaranteed to run exactly
    /// once", so this is both the result and the idempotency latch.
    updated: Option<Result<()>>,
}

impl ConnectSocket {
    pub(crate) fn new(socket: Socket, addr: SockAddrBytes) -> Self {
        ConnectSocket {
            socket,
            addr,
            bytes_sent: 0,
            updated: None,
        }
    }

    /// The connect result, with the context update folded in.
    ///
    /// A connect that succeeded at the transport level but whose context update
    /// failed is reported as a failure: handing back a socket on which
    /// `peer_addr` lies is worse than refusing it.
    pub(crate) fn finish(self, result: Result<usize>) -> Result<()> {
        result?;
        match self.updated {
            Some(update) => update,
            // Unreachable in practice — the driver calls `on_complete` on every
            // path that resolves an operation — but this arm exists precisely
            // as insurance against that changing, and insurance that returns
            // `Ok` pays out to the wrong party: the caller would get a
            // `TcpStream` whose `peer_addr`, `shutdown` and socket options all
            // misbehave, because `SO_UPDATE_CONNECT_CONTEXT` never ran.
            //
            // `AcceptSocket::finish` refuses in the same situation. The two
            // must agree.
            None => Err(Error::from_hresult(HRESULT::from_win32(
                WSAENOTCONN.0 as u32,
            ))),
        }
    }

    fn record_completion(&mut self, result: &Result<usize>) {
        if self.updated.is_some() || result.is_err() {
            return;
        }
        self.updated = Some(self.socket.update_connect_context());
    }
}

impl IntoInner for ConnectSocket {
    type Inner = ();

    fn into_inner(self) {}
}

// SAFETY: `operate` derives the socket from `self.socket` and the address
// pointer from `self.addr`, both reached through `&mut self`, so the address
// storage outlives a pending `ConnectEx`. The byte-count out-parameter is null.
unsafe impl OpCode for ConnectSocket {
    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let connect_ex = match extensions() {
            Ok(ext) => ext.connect_ex,
            Err(e) => return Poll::Ready(Err(e)),
        };

        // SAFETY: the pointer came from `WSAIoctl`'s extension lookup on a
        // socket of this family, the address storage is owned by this
        // operation, and `optr` is this operation's own `OVERLAPPED`.
        let started = unsafe {
            connect_ex(
                self.socket.raw(),
                self.addr.as_ptr(),
                self.addr.len(),
                std::ptr::null(),
                0,
                &mut self.bytes_sent,
                optr,
            )
        };
        // No Windows call may occur between `ConnectEx` and `win32_result`.
        let result = unsafe { win32_result(started.as_bool(), optr) };
        if let Poll::Ready(ref ready) = result {
            self.record_completion(ready);
        }
        result
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        // SAFETY: `optr` is the same pointer passed to `operate`; the socket is
        // kept alive by this operation's own `Socket` clone.
        unsafe { CancelIoEx(self.socket.as_handle(), Some(optr)) }
    }

    unsafe fn on_complete(&mut self, result: &Result<usize>) {
        self.record_completion(result);
    }
}
