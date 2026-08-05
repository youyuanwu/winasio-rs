// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A connected TCP stream.

use std::future::Future;
use std::net::{Shutdown, SocketAddr};

use crate::fs::ReadOutcome;
use crate::iocp::{IntoInner, IoBuf, IoBufMut, OpResult, Registrar, Submitter};

use super::addr::{family_of, wildcard_for, SockAddrBytes};
use super::error::SocketError;
use super::ops::connect::ConnectSocket;
use super::ops::io::{RecvSocket, SendSocket};
use super::socket::Socket;

struct Inner<S> {
    socket: Socket,
    submitter: S,
}

/// A connected TCP stream registered with a completion backend.
///
/// Generic over the *submitter*, like [`crate::fs::File`]: a stream only ever
/// starts operations on a socket it already owns. Contrast
/// [`super::TcpListener`], which must manufacture registered sockets and so
/// needs a [`Registrar`].
///
/// Dropping the stream requests cancellation of anything still in flight and
/// then closes the socket once the last reference to it goes away — including
/// references held by operations the kernel has not finished with.
pub struct TcpStream<S: Submitter> {
    inner: Option<Inner<S>>,
}

impl<S: Submitter> TcpStream<S> {
    pub(crate) fn from_parts(socket: Socket, submitter: S) -> Self {
        TcpStream {
            inner: Some(Inner { socket, submitter }),
        }
    }

    fn open(&self) -> &Inner<S> {
        self.inner
            .as_ref()
            .expect("the stream is only torn down in `Drop`")
    }

    /// The socket this stream owns.
    pub fn socket(&self) -> &Socket {
        &self.open().socket
    }

    /// The local address the connection is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, SocketError> {
        self.open()
            .socket
            .local_addr()
            .map_err(SocketError::from_win32)
    }

    /// The address of the peer.
    ///
    /// Available because `SO_UPDATE_CONNECT_CONTEXT` (for a connected stream)
    /// or `SO_UPDATE_ACCEPT_CONTEXT` (for an accepted one) has already been
    /// applied; without that this would fail with `WSAENOTCONN`.
    ///
    /// A dual-stack listener reports an IPv4 peer as a v4-mapped
    /// [`SocketAddr::V6`]. The crate does not un-map it: doing so would lose
    /// the fact that the connection arrived on a v6 socket, and callers that
    /// care can un-map themselves.
    pub fn peer_addr(&self) -> Result<SocketAddr, SocketError> {
        self.open()
            .socket
            .peer_addr()
            .map_err(SocketError::from_win32)
    }

    /// Shut down one or both directions of the connection.
    ///
    /// This is `shutdown(2)`, not a close: it sends FIN (or stops receiving),
    /// and the socket stays alive until the stream is dropped. A peer blocked
    /// in a read sees a graceful close.
    pub fn shutdown(&self, how: Shutdown) -> Result<(), SocketError> {
        self.open()
            .socket
            .shutdown_dir(how)
            .map_err(SocketError::from_win32)
    }

    /// Start a read.
    ///
    /// The buffer is moved into the operation and handed back when it resolves,
    /// so a dropped future cannot leave the kernel writing into storage the
    /// caller has reclaimed.
    ///
    /// If the returned future is dropped before resolving, cancellation is
    /// requested and the buffer is not returned.
    pub fn read<B>(&self, buffer: B) -> impl Future<Output = OpResult<ReadOutcome, B>>
    where
        B: IoBufMut + Send,
    {
        let open = self.open();
        let submitted = open
            .submitter
            .submit(RecvSocket::new(open.socket.clone(), buffer));
        async move {
            let OpResult(result, op) = submitted.await;
            let (result, buffer) = op.finish(result);
            OpResult(result, buffer)
        }
    }

    /// Start a write.
    ///
    /// A successful write may transfer fewer bytes than the buffer holds; use
    /// [`TcpStream::write_all`] when the whole payload must go out.
    ///
    /// If the returned future is dropped before resolving, cancellation is
    /// requested and the buffer is not returned.
    pub fn write<B>(&self, buffer: B) -> impl Future<Output = OpResult<usize, B>>
    where
        B: IoBuf + Send,
    {
        let open = self.open();
        let submitted = open
            .submitter
            .submit(SendSocket::new(open.socket.clone(), buffer));
        async move {
            let OpResult(result, op) = submitted.await;
            OpResult(result, op.into_inner())
        }
    }

    /// Read until the buffer is full, the peer closes, or a read fails.
    pub fn read_exact<B>(
        &self,
        buffer: B,
    ) -> impl Future<Output = crate::io::TransferResult<B>> + '_
    where
        B: IoBufMut + Send,
    {
        crate::io::read_exact(self, 0, buffer)
    }

    /// Read until the peer closes, growing the buffer as needed.
    pub fn read_to_end(
        &self,
        buffer: Vec<u8>,
    ) -> impl Future<Output = crate::io::TransferResult<Vec<u8>>> + '_ {
        crate::io::read_to_end(self, 0, buffer)
    }

    /// Write the whole buffer, submitting as many sends as it takes.
    pub fn write_all<B>(&self, buffer: B) -> impl Future<Output = crate::io::TransferResult<B>> + '_
    where
        B: IoBuf + Send,
    {
        crate::io::write_all(self, 0, buffer)
    }
}

impl<S: Submitter> TcpStream<S> {
    /// Connect to `addr` over a socket registered with `registrar`.
    ///
    /// The socket is bound to the wildcard address of the destination's family
    /// first. That is not a convenience: `ConnectEx` fails with `WSAEINVAL` on
    /// an unbound socket, and the error names nothing that would lead you here.
    pub async fn connect<R>(registrar: &R, addr: SocketAddr) -> Result<Self, SocketError>
    where
        R: Registrar<Io = S>,
    {
        let socket = Socket::new_overlapped(family_of(&addr)).map_err(SocketError::from_win32)?;

        socket
            .bind_to(wildcard_for(&addr))
            .map_err(SocketError::from_win32)?;

        // Registration is the last fallible step before the connect itself, so
        // a failure here cannot leave a registered socket that nothing owns:
        // `socket` is still local and its `Drop` closes it.
        let submitter = registrar.register(socket.as_handle())?;

        let op = ConnectSocket::new(socket.clone(), SockAddrBytes::from_socket_addr(addr));
        let OpResult(result, op) = submitter.submit(op).await;
        op.finish(result).map_err(SocketError::from_win32)?;

        Ok(TcpStream::from_parts(socket, submitter))
    }
}

impl<S: Submitter> Drop for TcpStream<S> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            // Ask the kernel to abandon anything still in flight before the
            // submitter goes away. The socket itself is not closed here — the
            // `Socket` clones held by in-flight operations keep it alive until
            // their completions arrive, which is what stops a late completion
            // naming a recycled socket.
            let _ = inner.socket.cancel_all();
            drop(inner.submitter);
            drop(inner.socket);
        }
    }
}

impl<S: Submitter> std::fmt::Debug for TcpStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpStream")
            .field("socket", &self.open().socket)
            .finish_non_exhaustive()
    }
}
