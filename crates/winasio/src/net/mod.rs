// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Asynchronous TCP sockets.
//!
//! [`TcpListener`] accepts connections with `AcceptEx`; [`TcpStream`] connects
//! with `ConnectEx` and transfers with `WSARecv` / `WSASend`. Both are generic
//! over the completion backend, so the same code runs on an owned
//! [`Proactor`](crate::iocp::Proactor) or on the system thread pool.
//!
//! # Invariants and obligations
//!
//! * **Overlapped always.** Every socket is created with `WSA_FLAG_OVERLAPPED`;
//!   callers cannot opt out, and a socket handed in from elsewhere is not
//!   accepted.
//! * **Register once.** [`TcpStream::connect`] and [`TcpListener::bind`]
//!   register their socket immediately, and each accepted socket is registered
//!   exactly once before it becomes a [`TcpStream`]. A second registration is
//!   reported as [`SocketError::AlreadyRegistered`].
//! * **The socket outlives operations created here.** A [`TcpStream`] and every
//!   read, write or connect it submits each hold a clone of the same
//!   [`Socket`]. Dropping the owner cannot close a socket that one of its
//!   operations may still cancel through.
//! * **Dropped operation futures are not cancel-safe.** Dropping a read, write,
//!   connect or accept future before it resolves requests cancellation. Bytes
//!   already transferred are not undone, and the state owned by that operation
//!   — the buffer, or the half-built accepted socket — is not returned. Await
//!   the future if you need the buffer back. A dropped accept does not leak the
//!   socket it was building: the operation owns it and closes it when the
//!   driver finally releases the record.
//! * **Teardown is backend-specific.** Dropping a thread-pool-backed stream
//!   cancels outstanding I/O and drains callbacks before releasing the socket.
//!   A caller-driven stream requests cancellation and returns without driving
//!   the proactor; the caller must keep polling their proactor until the
//!   outstanding records are reclaimed.
//! * **Closing is not graceful, and this differs from `fs` and `pipe`.**
//!   Dropping a [`TcpStream`] calls `closesocket`, which is an abrupt close.
//!   A file or a pipe has no equivalent of a half-open connection, so their
//!   drop is the whole story; a socket does. If the peer must be able to tell
//!   "I finished sending" from "I vanished", call
//!   [`TcpStream::shutdown`] with [`Shutdown::Write`] and read until
//!   [`ReadOutcome::ClosedPeer`] before dropping. The crate will not do this
//!   for you, because a graceful close can block and a `Drop` that blocks is
//!   worse than one that is abrupt.
//! * **`closesocket`, not `CloseHandle`, and no [`crate::iocp::Handle`].** A
//!   socket is closed exactly once, by the last [`Socket`] clone to drop, with
//!   `closesocket`. `iocp::Handle` is not reused for sockets even though a
//!   `SOCKET` can be passed to `HANDLE`-typed APIs: its drop calls
//!   `CloseHandle`, which for a socket skips the Winsock layer's own teardown
//!   and leaks provider state. The type is therefore separate on purpose, not
//!   for want of factoring.
//!
//! # The generic asymmetry, and why it is not an oversight
//!
//! [`TcpStream`] is generic over a [`Submitter`](crate::iocp::Submitter);
//! [`TcpListener`] is generic over a [`Registrar`](crate::iocp::Registrar).
//! They differ because their jobs differ. A stream only ever starts operations
//! on a socket it already owns, which is all a submitter can do. A listener
//! *manufactures* sockets — every `AcceptEx` needs a fresh one, and that socket
//! must be registered before it can be used — and no submitter can hand back a
//! registrar. The bound is written `R: Registrar + Clone` because `Registrar`
//! has no `Clone` supertrait and each accepted stream needs its own submitter.
//!
//! # Ownership rules
//!
//! A socket owned by this module is closed with `closesocket`, never
//! `CloseHandle`, and closed exactly once, by the last [`Socket`] clone to
//! drop. Operations hold their own clone for the whole of their life, so a
//! completion arriving after the owning stream was dropped cannot name a socket
//! the system has already recycled.
//!
//! # Dual-stack addresses
//!
//! A listener created with `only_v6` cleared accepts IPv4 connections on its
//! IPv6 socket and reports those peers as v4-mapped
//! [`SocketAddr::V6`](std::net::SocketAddr::V6) values such as
//! `[::ffff:127.0.0.1]:1234`. The crate does not un-map them: the mapping is
//! how the platform describes what happened, and discarding it would hide that
//! the connection arrived on a v6 socket. Callers wanting an IPv4 form can use
//! [`std::net::Ipv6Addr::to_ipv4_mapped`].
//!
//! # Example
//!
//! An echo server on the system thread pool. The client is a blocking
//! [`std::net::TcpStream`] on another thread, so the example needs no executor
//! beyond whatever is already awaiting it.
//!
//! ```no_run
//! # use winasio::iocp::{OpResult, ThreadPool};
//! # use winasio::net::{ReadOutcome, Shutdown, TcpListener};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let listener = TcpListener::bind(&ThreadPool, "127.0.0.1:0".parse()?)?;
//! let addr = listener.local_addr();
//!
//! let client = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
//!     use std::io::{Read, Write};
//!     let mut c = std::net::TcpStream::connect(addr)?;
//!     c.write_all(b"ping")?;
//!     // Half-close, so the server's read loop sees an end rather than
//!     // blocking for a request that will never come.
//!     c.shutdown(std::net::Shutdown::Write)?;
//!     let mut echoed = Vec::new();
//!     c.read_to_end(&mut echoed)?;
//!     Ok(echoed)
//! });
//!
//! let (stream, _peer) = listener.accept().await?;
//! let mut buf = vec![0u8; 64];
//! loop {
//!     let OpResult(outcome, returned) = stream.read(buf).await;
//!     buf = returned;
//!     match outcome? {
//!         ReadOutcome::ClosedPeer => break,
//!         ReadOutcome::Bytes(n) => {
//!             let OpResult(written, _) = stream.write(buf[..n].to_vec()).await;
//!             written?;
//!         }
//!         other => panic!("a socket cannot report {other:?}"),
//!     }
//! }
//! // Send our own FIN, or the client's `read_to_end` never returns.
//! stream.shutdown(Shutdown::Write)?;
//!
//! assert_eq!(client.join().expect("client thread")?, b"ping");
//! # Ok(())
//! # }
//! ```
//!
//! # Out of scope
//!
//! UDP, `WSARecvFrom` / `WSASendTo`, `WSARecvMsg`, `TransmitFile` and vectored
//! I/O.

mod addr;
mod error;
mod ext;
mod init;
mod listener;
mod ops;
mod outcome;
mod socket;
mod stream;

pub use error::SocketError;
pub use listener::{TcpListener, TcpListenerOptions};
pub use socket::Socket;
pub use stream::TcpStream;

/// Re-exported so a caller need not reach into [`std::net`] for the one
/// argument [`TcpStream::shutdown`] takes.
pub use std::net::Shutdown;

/// The read outcomes a socket reports, shared with files and pipes.
///
/// A socket never produces [`ReadOutcome::Eof`] (that is a file's end of file)
/// or [`ReadOutcome::MoreData`] (that is a message-mode pipe). It reports
/// [`ReadOutcome::ClosedPeer`] where a file would report `Eof`.
pub use crate::fs::ReadOutcome;

/// A [`TcpStream`] on the system thread pool.
pub type ThreadPoolTcpStream = TcpStream<crate::iocp::ThreadPoolIo>;

/// A [`TcpListener`] on the system thread pool.
///
/// Named for the *registrar*, not the submitter: the listener is what
/// registers each accepted socket, so it is `ThreadPool` that appears here
/// while [`ThreadPoolTcpStream`] names `ThreadPoolIo`.
pub type ThreadPoolTcpListener = TcpListener<crate::iocp::ThreadPool>;

#[cfg(any(test, feature = "test-util"))]
pub use socket::{live_sockets, socket_guard};
