// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A connected `AF_UNIX` stream.

use std::future::Future;
use std::net::Shutdown;

use crate::fs::ReadOutcome;
use crate::iocp::{IntoInner, IoBuf, IoBufMut, OpResult, Registrar, Submitter};

use super::addr::SockAddrBytes;
use super::error::SocketError;
use super::ops::connect::ConnectSocket;
use super::ops::io::{RecvSocket, SendSocket};
use super::socket::Socket;
use super::unix_addr::UnixSocketAddr;

struct Inner<S> {
    socket: Socket,
    submitter: S,
}

/// A connected `AF_UNIX` stream registered with a completion backend.
///
/// The `AF_UNIX` counterpart of [`super::TcpStream`], and generic over the
/// *submitter* for the same reason: a stream only ever starts operations on a
/// socket it already owns.
///
/// Dropping the stream requests cancellation of anything still in flight and
/// then closes the socket once the last reference to it goes away — including
/// references held by operations the kernel has not finished with.
///
/// # It behaves like [`super::TcpStream`], and that was checked rather than assumed
///
/// The transfer path is *literally* the same code: `WSARecv` and `WSASend`
/// through the same operations, classified by the same `classify_socket_read`
/// and the same [`crate::io`] helpers. What was not obvious is whether the
/// platform gives those shared paths the same *inputs* for `AF_UNIX`, because
/// a stream transport with no RST would have quietly broken the module's
/// central promise — that a clean end of stream and a lost connection are
/// different results. It was measured, family against family, on identical
/// scenarios:
///
/// | the peer... | TCP reports | `AF_UNIX` reports |
/// |---|---|---|
/// | called `shutdown(SD_SEND)` | `Ok(0)` → [`ReadOutcome::ClosedPeer`] | the same |
/// | closed with nothing left unread | `Ok(0)` → [`ReadOutcome::ClosedPeer`] | the same |
/// | closed with data still unread | `WSAECONNRESET` → `Err` | the same |
/// | closed with `SO_LINGER {1, 0}` | `WSAECONNRESET` → `Err` | the same |
///
/// and on the completion path the reset arrives as `STATUS_CONNECTION_RESET`,
/// translated to `ERROR_NETNAME_DELETED`, for both families alike — which is
/// already in the crate's classification tables.
///
/// So `AF_UNIX` on Windows *does* have an abortive close; it is simply not
/// what a plain `closesocket` performs, and it is not what a plain
/// `closesocket` performs on TCP either. The invariant holds unchanged, and
/// [`UnixStream::read_to_end`] and [`UnixStream::read_exact`] are offered with
/// the **same** contract as their TCP counterparts: a peer that vanishes
/// mid-stream fails them rather than handing back a silently truncated buffer.
///
/// # What genuinely differs from TCP
///
/// * **The address is a path, and the peer's is usually unnamed.** See
///   [`UnixSocketAddr`] and [`super::UnixListener::accept`].
/// * **Binding leaves a file behind, and closing does not remove it.** That is
///   the listener's problem rather than the stream's — see
///   [`super::UnixListenerOptions::unlink_stale`] — but it is why a connect
///   here binds the *unnamed* address, which creates no file, rather than
///   something a caller would have to clean up.
/// * **A connection to a path with no listener is refused inline.** Measured:
///   `WSAECONNREFUSED`, not `ERROR_FILE_NOT_FOUND` and not a pending
///   operation, even though the failure is really "no such file". It therefore
///   reaches the caller as [`SocketError::ConnectionRefused`], the same
///   variant TCP produces, which is the more useful answer of the two.
pub struct UnixStream<S: Submitter> {
    inner: Option<Inner<S>>,
}

impl<S: Submitter> UnixStream<S> {
    pub(crate) fn from_parts(socket: Socket, submitter: S) -> Self {
        UnixStream {
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
    ///
    /// Unnamed for a stream produced by [`UnixStream::connect`], which binds
    /// the empty path; the listener's path for an accepted one.
    pub fn local_addr(&self) -> Result<UnixSocketAddr, SocketError> {
        self.open()
            .socket
            .local_unix_addr()
            .map_err(SocketError::from_win32)
    }

    /// The address of the peer.
    ///
    /// Available because `SO_UPDATE_CONNECT_CONTEXT` (for a connected stream)
    /// or `SO_UPDATE_ACCEPT_CONTEXT` (for an accepted one) has already been
    /// applied; without that this fails with `WSAENOTCONN`, which was measured
    /// for `AF_UNIX` separately rather than inherited from the TCP result.
    ///
    /// Frequently [`UnixSocketAddr::is_unnamed`] on the server side. That is
    /// the accurate answer rather than a missing one — see
    /// [`super::UnixListener::accept`].
    pub fn peer_addr(&self) -> Result<UnixSocketAddr, SocketError> {
        self.open()
            .socket
            .peer_unix_addr()
            .map_err(SocketError::from_win32)
    }

    /// Shut down one or both directions of the connection.
    ///
    /// This is `shutdown(2)`, not a close: it sends the end-of-stream marker
    /// (or stops receiving), and the socket stays alive until the stream is
    /// dropped. Measured for `AF_UNIX`: the peer's next read resolves as
    /// [`ReadOutcome::ClosedPeer`], and the *other* direction keeps working —
    /// half-open is real here, not simulated.
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
    ///
    /// Note that on `AF_UNIX` this very often completes **inline** — measured,
    /// a receive with data already waiting returns success immediately rather
    /// than pending. That is exactly the case the crate's mandatory
    /// inline-success skip mode exists to handle, and it is exercised
    /// constantly here rather than occasionally as it is on TCP.
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
    /// [`UnixStream::write_all`] when the whole payload must go out.
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
    ///
    /// Carries the same contract as [`super::TcpStream::read_exact`], and that
    /// is a measured claim rather than an inherited one: see the type docs.
    /// A peer that is cut off mid-stream fails this with
    /// [`TransferError::ClosedPeer`](crate::io::TransferError::ClosedPeer)
    /// rather than resolving `Ok` on a partly-filled buffer.
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
    ///
    /// Offered on the same terms as [`super::TcpStream::read_to_end`]. This is
    /// the method the module's central invariant is really about, so it is
    /// worth being explicit: a peer that finishes cleanly resolves this `Ok`
    /// with everything it sent, and a peer that is cut off resolves it `Err`
    /// with the bytes that did arrive still in
    /// [`TransferResult::buffer`](crate::io::TransferResult::buffer) and the
    /// count in `transferred`. The two are distinguishable. On a transport
    /// where they were not, this method would be a truncation hazard and would
    /// not be offered at all.
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

impl<S: Submitter> UnixStream<S> {
    /// Connect to `addr` over a socket registered with `registrar`.
    ///
    /// The socket is bound to the **unnamed** address first. That is not a
    /// convenience: measured, `ConnectEx` on an unbound `AF_UNIX` socket fails
    /// with `WSAEINVAL` — the same rule, and the same unhelpful error, as TCP.
    /// The unnamed address is the right thing to bind because it creates no
    /// file, so a client leaves nothing on disk for anyone to clean up. The
    /// cost is that the server sees this peer as unnamed; see
    /// [`super::UnixListener::accept`].
    ///
    /// A path with nothing listening on it fails with
    /// [`SocketError::ConnectionRefused`].
    pub async fn connect<R>(registrar: &R, addr: &UnixSocketAddr) -> Result<Self, SocketError>
    where
        R: Registrar<Io = S>,
    {
        let socket = Socket::new_overlapped_unix().map_err(SocketError::from_win32)?;

        // The `AF_UNIX` wildcard: an empty `sun_path`. Measured to bind
        // successfully and to create nothing on disk.
        socket
            .bind_bytes(&SockAddrBytes::from_unix_addr(&UnixSocketAddr::unnamed()))
            .map_err(SocketError::from_win32)?;

        // Registration is the last fallible step before the connect itself, so
        // a failure here cannot leave a registered socket that nothing owns:
        // `socket` is still local and its `Drop` closes it.
        let submitter = registrar.register(socket.as_handle())?;

        let op = ConnectSocket::new(socket.clone(), SockAddrBytes::from_unix_addr(addr));
        let OpResult(result, op) = submitter.submit(op).await;
        op.finish(result).map_err(SocketError::from_win32)?;

        Ok(UnixStream::from_parts(socket, submitter))
    }
}

impl<S: Submitter> Drop for UnixStream<S> {
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

impl<S: Submitter> std::fmt::Debug for UnixStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixStream")
            .field("socket", &self.open().socket)
            .finish_non_exhaustive()
    }
}
