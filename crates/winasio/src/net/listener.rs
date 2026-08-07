// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A listening TCP socket.

use std::net::SocketAddr;

use crate::iocp::{OpResult, Registrar, Submitter};

use super::addr::{family_of, unsupported_family};
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
/// `Clone` is required on `R` because the listener keeps a clone of the
/// registrar for its whole life, so that every later accept can register its
/// socket with the same backend. [`Registrar`] does not require `Clone`
/// itself, so the bound is stated here.
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

        if addr.is_ipv6() {
            // Set unconditionally, not only when clearing it. `only_v6(true)`
            // otherwise means "leave the machine's default alone", which is not
            // what the option says and not what a caller who wrote it wants.
            // M19 measured the default as 1 on this machine, but the option is
            // documented as setting the behaviour, not as agreeing with a
            // default that a system policy is free to change.
            socket
                .set_only_v6(options.only_v6)
                .map_err(SocketError::from_win32)?;
        }

        socket.bind_to(addr).map_err(SocketError::from_win32)?;
        socket
            .listen_on(backlog_argument(options.backlog))
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
        let accepted =
            Socket::new_overlapped(family_of(&self.local)).map_err(SocketError::from_win32)?;

        let op = AcceptSocket::new(self.socket.clone(), accepted);
        let OpResult(result, op) = self.io.submit(op).await;

        // `finish` applies `SO_UPDATE_ACCEPT_CONTEXT` and copies both addresses
        // out of the provider's buffer before it is dropped.
        let parts = op.finish(result).map_err(SocketError::from_win32)?;

        // The operation hands back encoded bytes rather than a `SocketAddr`,
        // so that it need know nothing about address families — see the
        // accept operation's docs. This is where the family knowledge lives:
        // a listener created for an IP address decodes an IP peer, and a
        // storage that decodes as neither is refused rather than invented.
        let peer = parts
            .peer
            .to_socket_addr()
            .ok_or_else(|| SocketError::from_win32(unsupported_family()))?;

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

        Ok((TcpStream::from_parts(parts.socket, io), peer))
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

/// The `listen` argument for a requested backlog.
///
/// `listen` takes an `i32`; the option is a `u32` so that callers need not
/// think about the signedness of a queue length. Saturating rather than
/// wrapping matters: `as i32` alone would turn a large request into a negative
/// number, and a negative backlog is not an error — Winsock reinterprets it,
/// so an oversized request would silently become a small queue.
fn backlog_argument(requested: u32) -> i32 {
    requested.min(i32::MAX as u32) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_requested_backlog_reaches_listen_unchanged() {
        assert_eq!(backlog_argument(0), 0);
        assert_eq!(backlog_argument(4), 4);
        assert_eq!(backlog_argument(TcpListenerOptions::new().backlog), 128);
    }

    #[test]
    fn an_oversized_backlog_saturates_rather_than_going_negative() {
        assert_eq!(backlog_argument(i32::MAX as u32), i32::MAX);
        assert_eq!(backlog_argument(u32::MAX), i32::MAX);
        assert!(
            backlog_argument(u32::MAX) > 0,
            "a wrapped backlog would be negative, which Winsock silently \
             reinterprets instead of rejecting"
        );
    }

    #[test]
    fn the_builder_records_what_it_was_given() {
        let mut options = TcpListenerOptions::new();
        options.backlog(4).only_v6(false);
        assert_eq!(options.backlog, 4);
        assert!(!options.only_v6);
    }
}
