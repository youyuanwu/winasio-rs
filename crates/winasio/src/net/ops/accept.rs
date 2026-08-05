// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The `AcceptEx` operation.
//!
//! `AcceptEx` is unlike the other operations here in that the caller must
//! supply the accepted socket up front — Winsock does not create it. It also
//! writes both endpoint addresses into a caller-supplied buffer in a
//! provider-defined layout that only `GetAcceptExSockaddrs` can decode, and it
//! leaves the accepted socket half-initialised until
//! `SO_UPDATE_ACCEPT_CONTEXT` is applied.
//!
//! # The address buffer
//!
//! Each address slot must be at least `sizeof(SOCKADDR_STORAGE) + 16`. The
//! sixteen extra bytes are not padding-by-superstition — the provider is
//! entitled to write into them — but the reason to get this right is worse
//! than a failed call: measured (M36), `AcceptEx` **accepts** a buffer sized to
//! `SOCKADDR_STORAGE` alone and returns `WSA_IO_PENDING` exactly as usual. The
//! undersizing is not diagnosed at submission, so it does not fail loudly; it
//! is a silent out-of-bounds write waiting for a completion. That is why the
//! constant is asserted in a test rather than merely commented.
//!
//! (An earlier version of this comment claimed the call was rejected. It is
//! not. The claim was inherited from the documented contract instead of being
//! measured, which is the same mistake this crate has now made three times.)
//!
//! `dwReceiveDataLength` is zero, which is what makes this a pure
//! accept: a non-zero value would make the operation wait for the client's
//! first send, turning a completed TCP handshake into an unbounded wait that a
//! silent client can hold open indefinitely.

use std::net::SocketAddr;
use std::task::Poll;

use windows::core::Result;
use windows::Win32::Networking::WinSock::{ADDRESS_FAMILY, SOCKADDR, SOCKADDR_STORAGE};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::iocp::{win32_result, IntoInner, OpCode};

use super::super::addr::decode_raw;
use super::super::ext::extensions;
use super::super::socket::Socket;

/// The size of one address slot in the `AcceptEx` output buffer.
const ADDR_SLOT: usize = std::mem::size_of::<SOCKADDR_STORAGE>() + 16;

/// What a completed accept produced.
pub(crate) struct AcceptedParts {
    pub(crate) socket: Socket,
    pub(crate) peer: SocketAddr,
}

/// Accept one connection onto a caller-supplied socket.
pub(crate) struct AcceptSocket {
    listener: Socket,
    accepted: Socket,
    /// The provider's address output. Written by the kernel for the whole of a
    /// pending call, so it is owned by the operation.
    buffer: Box<[u8; ADDR_SLOT * 2]>,
    /// The `lpdwBytesReceived` slot. Zero for a pure accept, but the parameter
    /// is not optional, and a stack slot would be a pointer the operation
    /// cannot vouch for.
    received: u32,
    /// The decoded addresses, or the failure that stopped them being decoded.
    ///
    /// Filled on the completion path, while the operation still owns the buffer
    /// the addresses point into. Doing it later is not possible: the pointers
    /// `GetAcceptExSockaddrs` hands back are *into* `buffer`.
    outcome: Option<Result<(SocketAddr, SocketAddr)>>,
}

impl AcceptSocket {
    pub(crate) fn new(listener: Socket, accepted: Socket) -> Self {
        AcceptSocket {
            listener,
            accepted,
            buffer: Box::new([0u8; ADDR_SLOT * 2]),
            received: 0,
            outcome: None,
        }
    }

    /// The accepted socket, ready to use, with both endpoint addresses.
    pub(crate) fn finish(self, result: Result<usize>) -> Result<AcceptedParts> {
        result?;
        // The local address is decoded too, and discarded: it is reachable
        // afterwards through `getsockname`, and requiring it to decode is a
        // check that the provider's buffer layout was what we assumed.
        let (_local, peer) = match self.outcome {
            Some(outcome) => outcome?,
            // Unreachable in practice — the driver calls `on_complete` on every
            // path that resolves an operation. Returning a fabricated address
            // would be worse than failing.
            None => return Err(windows::core::Error::from_thread()),
        };
        Ok(AcceptedParts {
            socket: self.accepted,
            peer,
        })
    }

    fn record_completion(&mut self, result: &Result<usize>) {
        // `on_complete` is documented as "not guaranteed to run exactly once".
        if self.outcome.is_some() || result.is_err() {
            return;
        }
        self.outcome = Some(self.decode());
    }

    /// Apply the context update and decode both addresses.
    ///
    /// Order matters. `SO_UPDATE_ACCEPT_CONTEXT` first: until it runs, the
    /// accepted socket has no inherited state. Measured on the accepted socket
    /// with no update applied: `getsockname` fails with `WSAEINVAL` (M34) and
    /// `getpeername` fails with `WSAENOTCONN` (M35). M26 adds that passing the
    /// wrong listener yields `WSAEFAULT` and leaves `getpeername` still
    /// failing.
    ///
    /// Note that this differs from the connect side, where `getsockname`
    /// succeeds before `SO_UPDATE_CONNECT_CONTEXT` (M8, probe 20) — which is
    /// why the two were measured separately rather than assumed alike.
    fn decode(&mut self) -> Result<(SocketAddr, SocketAddr)> {
        self.accepted.update_accept_context(&self.listener)?;

        let get_addrs = extensions()?.get_accept_ex_sockaddrs;

        let mut local_ptr: *mut SOCKADDR = std::ptr::null_mut();
        let mut local_len: i32 = 0;
        let mut peer_ptr: *mut SOCKADDR = std::ptr::null_mut();
        let mut peer_len: i32 = 0;

        // SAFETY: the buffer is the one `AcceptEx` filled, with the same slot
        // sizes it was given, and the four out-parameters are live locals. The
        // pointers written back point into `self.buffer`, which is still owned
        // here.
        unsafe {
            get_addrs(
                self.buffer.as_ptr().cast(),
                0,
                ADDR_SLOT as u32,
                ADDR_SLOT as u32,
                &mut local_ptr,
                &mut local_len,
                &mut peer_ptr,
                &mut peer_len,
            )
        };

        // SAFETY: both pointers were produced by `GetAcceptExSockaddrs` from
        // the buffer this operation still owns, with the lengths it reported.
        let local = unsafe { decode_raw(local_ptr, local_len) };
        // SAFETY: as above.
        let peer = unsafe { decode_raw(peer_ptr, peer_len) };

        match (local, peer) {
            (Some(local), Some(peer)) => Ok((local, peer)),
            // A family this crate does not encode. Refusing is better than
            // handing back a socket whose addresses are invented.
            _ => Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::ERROR_INVALID_PARAMETER.to_hresult(),
            )),
        }
    }

    /// The family the accepted socket must be created with.
    pub(crate) fn family_for(listener_addr: &SocketAddr) -> ADDRESS_FAMILY {
        super::super::addr::family_of(listener_addr)
    }
}

impl IntoInner for AcceptSocket {
    type Inner = ();

    fn into_inner(self) {}
}

// SAFETY: `operate` derives both sockets and the output buffer from `&mut
// self`, and the byte-count slot too. All of them outlive the pinned operation,
// which is what `AcceptEx` requires of a pending call.
unsafe impl OpCode for AcceptSocket {
    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let accept_ex = match extensions() {
            Ok(ext) => ext.accept_ex,
            Err(e) => return Poll::Ready(Err(e)),
        };

        // SAFETY: the listener is listening, the accepted socket is a fresh
        // unbound socket of the same family, the buffer is two slots of exactly
        // the length declared, and `optr` is this operation's own `OVERLAPPED`.
        let started = unsafe {
            accept_ex(
                self.listener.raw(),
                self.accepted.raw(),
                self.buffer.as_mut_ptr().cast(),
                // Zero: a pure accept. See the module docs.
                0,
                ADDR_SLOT as u32,
                ADDR_SLOT as u32,
                &mut self.received,
                optr,
            )
        };
        // No Windows call may occur between `AcceptEx` and `win32_result`.
        let result = unsafe { win32_result(started.as_bool(), optr) };
        if let Poll::Ready(ref ready) = result {
            self.record_completion(ready);
        }
        result
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        // Cancellation targets the *listener*: that is the handle `AcceptEx`
        // was issued on and the one the completion is associated with.
        //
        // SAFETY: `optr` is the same pointer passed to `operate`; both sockets
        // are kept alive by this operation's own clones.
        unsafe { CancelIoEx(self.listener.as_handle(), Some(optr)) }
    }

    unsafe fn on_complete(&mut self, result: &Result<usize>) {
        self.record_completion(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_slot_leaves_room_for_the_providers_sixteen_bytes() {
        // Sizing a slot to `SOCKADDR_STORAGE` alone is the classic `AcceptEx`
        // mistake, and measurement (M36) makes it worse than folklore says:
        // the call does not fail, it returns `WSA_IO_PENDING` like any other
        // and the undersizing only shows up as memory the provider was
        // entitled to write and we did not reserve. Nothing at runtime will
        // catch it, so this assertion is the only thing standing between the
        // constant and a later "simplification".
        assert_eq!(ADDR_SLOT, std::mem::size_of::<SOCKADDR_STORAGE>() + 16);
        assert!(ADDR_SLOT >= std::mem::size_of::<SOCKADDR_STORAGE>() + 16);
    }
}
