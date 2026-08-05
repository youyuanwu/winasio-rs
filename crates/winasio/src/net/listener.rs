// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A listening TCP socket.

use std::net::SocketAddr;

use crate::iocp::{OpResult, Registrar, Submitter};

use super::addr::family_of;
use super::error::SocketError;
use super::ops::accept::AcceptSocket;
use super::socket::Socket;
use super::stream::TcpStream;

/// Options for [`TcpListener::bind_with`].
///
/// Fields are private with setter methods, matching
/// [`crate::pipe::ServerOptions`]: a new option can then be added without
/// breaking callers who constructed this by hand.
#[derive(Debug, Clone, Copy)]
pub struct TcpListenerOptions {
    backlog: u32,
    only_v6: bool,
}

impl TcpListenerOptions {
    /// Options matching the platform defaults.
    pub fn new() -> Self {
        TcpListenerOptions {
            // The conventional "as much as the system allows" value; Windows
            // clamps it to the provider's maximum.
            backlog: 128,
            // Matches the Windows default, so `new` changes nothing about how a
            // v6 listener behaves.
            only_v6: true,
        }
    }

    /// Set the `listen` backlog.
    ///
    /// Saturated into the platform's `i32` rather than truncated: a caller
    /// asking for more than the platform can express should get the largest
    /// backlog available, not a wrapped-around small one.
    pub fn backlog(&mut self, backlog: u32) -> &mut Self {
        self.backlog = backlog;
        self
    }

    /// Set whether an IPv6 listener refuses IPv4 connections.
    ///
    /// Windows defaults this to `true`. Clearing it makes the listener
    /// dual-stack, and IPv4 peers are then reported as v4-mapped
    /// [`SocketAddr::V6`] — see the [module docs](super).
    ///
    /// Ignored for an IPv4 listener.
    pub fn only_v6(&mut self, only_v6: bool) -> &mut Self {
        self.only_v6 = only_v6;
        self
    }
}

impl Default for TcpListenerOptions {
    fn default() -> Self {
        TcpListenerOptions::new()
    }
}

/// A listening TCP socket registered with a completion backend.
///
/// Generic over a [`Registrar`], not a `Submitter`: every accept needs a fresh
/// socket, and a fresh socket must be registered before anything can be
/// submitted on it. See the [module docs](super) for why the two socket types
/// differ in this.
///
/// `Clone` is required on `R` because each accepted [`TcpStream`] takes its own
/// submitter; [`Registrar`] does not require `Clone` itself, so the bound is
/// stated here.
///
/// **Field order is load-bearing.** Fields drop in declaration order, and the
/// submitter's drop cancels and drains outstanding I/O on the listening socket
/// — so it has to run while that socket is still open. Closing first would
/// leave the drain calling `CancelIoEx` on a descriptor the system is free to
/// have handed to someone else. `TcpStream` states the same order explicitly in
/// its `Drop`; here the declaration order is the statement, hence this note.
pub struct TcpListener<R: Registrar + Clone> {
    /// The listener's own submitter, from registering the listening socket.
    /// Declared first so it is dropped first. See above.
    io: R::Io,
    /// Kept so each accepted socket can be registered with the same backend.
    /// This is the reason the type is registrar-generic at all.
    registrar: R,
    socket: Socket,
    /// The bound address, cached at construction.
    ///
    /// Not just a convenience: the family it names decides what family each
    /// accepted socket is created with, and reading it back per accept would
    /// mean a syscall that can fail in the middle of an accept loop.
    local: SocketAddr,
}

impl<R: Registrar + Clone> TcpListener<R> {
    /// Bind and listen on `addr` with the default options.
    pub fn bind(registrar: &R, addr: SocketAddr) -> Result<Self, SocketError> {
        Self::bind_with(registrar, addr, &TcpListenerOptions::new())
    }

    /// Bind and listen on `addr`.
    pub fn bind_with(
        registrar: &R,
        addr: SocketAddr,
        options: &TcpListenerOptions,
    ) -> Result<Self, SocketError> {
        let socket = Socket::new_overlapped(family_of(&addr)).map_err(SocketError::from_win32)?;

        if addr.is_ipv6() && !options.only_v6 {
            socket.set_only_v6(false).map_err(SocketError::from_win32)?;
        }

        socket.bind_to(addr).map_err(SocketError::from_win32)?;
        socket
            .listen_on(options.backlog.min(i32::MAX as u32) as i32)
            .map_err(SocketError::from_win32)?;

        let local = socket.local_addr().map_err(SocketError::from_win32)?;

        // Registration last among the fallible setup steps: until it succeeds
        // `socket` is a plain local whose `Drop` closes it.
        let io = registrar.register(socket.as_handle())?;

        Ok(TcpListener {
            socket,
            io,
            registrar: registrar.clone(),
            local,
        })
    }

    /// The address the listener is bound to.
    ///
    /// Reflects the port the system chose when the caller asked for port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The listening socket.
    pub fn socket(&self) -> &Socket {
        &self.socket
    }

    /// Accept one connection.
    ///
    /// If the returned future is dropped before resolving, cancellation is
    /// requested on the listener and the half-built socket is closed.
    pub async fn accept(&self) -> Result<(TcpStream<R::Io>, SocketAddr), SocketError> {
        // `AcceptEx` does not create the socket; the caller must supply one of
        // the listener's family, unbound and unconnected.
        let accepted = Socket::new_overlapped(AcceptSocket::family_for(&self.local))
            .map_err(SocketError::from_win32)?;

        let op = AcceptSocket::new(self.socket.clone(), accepted);
        let OpResult(result, op) = self.io.submit(op).await;

        // `finish` applies `SO_UPDATE_ACCEPT_CONTEXT` and decodes both
        // addresses out of the provider's buffer before it is dropped.
        let parts = op.finish(result).map_err(SocketError::from_win32)?;

        // LOAD-BEARING ORDERING — this is the whole of FR-027's mitigation, and
        // there is deliberately no guard object.
        //
        // Registering the accepted socket is the last fallible step. Everything
        // that could fail — the accept itself, the context update, the address
        // decode — has already happened, so the window in which a registered
        // socket exists with nothing owning it is empty by construction: the
        // only statement between `register` and the `TcpStream` that owns the
        // result is infallible. Registering earlier would open that window on
        // every error path and would then need a guard to close it.
        let io = self.registrar.register(parts.socket.as_handle())?;

        Ok((TcpStream::from_parts(parts.socket, io), parts.peer))
    }
}

impl<R: Registrar + Clone> Drop for TcpListener<R> {
    fn drop(&mut self) {
        // Ask the kernel to abandon any accept still in flight before the
        // submitter goes away, exactly as `TcpStream::drop` does. The socket is
        // not closed here: the `Socket` clones held by in-flight accepts keep
        // it alive until their completions arrive, which is what stops a late
        // completion naming a recycled socket.
        //
        // The submitter and the registrar are then dropped by field order,
        // before the socket. See the note on the struct.
        let _ = self.socket.cancel_all();
    }
}
