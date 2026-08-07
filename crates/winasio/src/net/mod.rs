// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Asynchronous stream sockets: TCP, and `AF_UNIX`.
//!
//! [`TcpListener`] and [`UnixListener`] accept connections with `AcceptEx`;
//! [`TcpStream`] and [`UnixStream`] connect with `ConnectEx` and transfer with
//! `WSARecv` / `WSASend`. All four are generic over the completion backend, so
//! the same code runs on an owned [`Proactor`](crate::iocp::Proactor) or on
//! the system thread pool.
//!
//! The two families share every operation below the address: the same accept,
//! connect, send and receive code, the same classification, the same
//! ownership rules. What differs is what an address is, and that is confined
//! to [`UnixSocketAddr`] and the two Unix types' constructors.
//!
//! # Invariants and obligations
//!
//! These hold for both families unless a bullet says otherwise.
//!
//! * **Overlapped always.** Every socket is created with `WSA_FLAG_OVERLAPPED`;
//!   callers cannot opt out, and a socket handed in from elsewhere is not
//!   accepted.
//! * **Register once.** [`TcpStream::connect`], [`TcpListener::bind`],
//!   [`UnixStream::connect`] and [`UnixListener::bind`] register their socket
//!   immediately, and each accepted socket is registered exactly once before
//!   it becomes a stream. A second registration is reported as
//!   [`SocketError::AlreadyRegistered`].
//! * **The socket outlives operations created here.** A stream and every
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
//! * **A clean end of stream and a lost connection are different results.**
//!   [`ReadOutcome::ClosedPeer`] means the peer finished sending: the stream
//!   ended and you have all of it. A connection that is *reset* resolves the
//!   read as an `Err` instead, never as `ClosedPeer`, and the whole-payload
//!   helpers ([`TcpStream::read_to_end`], [`read_exact`](TcpStream::read_exact),
//!   and their [`UnixStream`] counterparts) fail rather than returning what
//!   they managed to collect. This distinction is deliberate and is the one
//!   thing in this module worth reading twice: the alternative is a
//!   `read_to_end` that returns `Ok` with a silently truncated buffer, which
//!   the caller has no way to detect. To act on the difference, classify the
//!   error with [`SocketError::from_win32`] — `read` and `write` resolve to a
//!   raw [`windows::core::Error`], the same as files and pipes.
//!
//!   **This holds for `AF_UNIX` too, and it was checked rather than assumed.**
//!   See [below](#af_unix-keeps-the-reset-distinction).
//! * **Closing is not graceful, and this differs from `fs` and `pipe`.**
//!   Dropping a stream calls `closesocket`, which is an abrupt close.
//!   A file or a pipe has no equivalent of a half-open connection, so their
//!   drop is the whole story; a socket does — for both families. If the peer
//!   must be able to tell "I finished sending" from "I vanished", call
//!   [`shutdown`](TcpStream::shutdown) with [`Shutdown::Write`] and read until
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
//! * **A bound `AF_UNIX` path is a file, and nothing here removes it.**
//!   `bind` creates it, `closesocket` leaves it, and a leftover makes the next
//!   `bind` fail with [`SocketError::AddressInUse`]. Cleanup is the caller's,
//!   with [`UnixListenerOptions::unlink_stale`] available as an explicit
//!   opt-in. See that method for why it is not the default.
//!
//! # `AF_UNIX` keeps the reset distinction
//!
//! It is reasonable to expect that it would not. `AF_UNIX` is a local
//! transport with no wire and no TCP state machine, so "there is no RST here"
//! is a plausible thing to believe — and if it were true, the invariant above
//! could not be delivered for [`UnixStream`]: a peer that vanished mid-stream
//! would be indistinguishable from one that finished, and `read_to_end` would
//! be free to return `Ok` with a truncated buffer. That is the same class of
//! defect this crate fought in [`crate::winhttp`], where it added its own
//! `Content-Length` accounting to produce a `TruncatedBody` error.
//!
//! It was measured, both families against identical scenarios, rather than
//! reasoned about:
//!
//! | the peer... | TCP | `AF_UNIX` |
//! |---|---|---|
//! | `shutdown(SD_SEND)`, then reads | `Ok(0)` → [`ReadOutcome::ClosedPeer`] | identical |
//! | `closesocket`, nothing left unread | `Ok(0)` → [`ReadOutcome::ClosedPeer`] | identical |
//! | `closesocket`, data still unread | `WSAECONNRESET` → `Err` | identical |
//! | `closesocket` under `SO_LINGER {1, 0}` | `WSAECONNRESET` → `Err` | identical |
//!
//! On the completion path both families deliver the reset as
//! `STATUS_CONNECTION_RESET`, which `RtlNtStatusToDosError` renders as
//! `ERROR_NETNAME_DELETED` — a code already in this module's classification
//! table and in [`crate::io`]'s.
//!
//! So `AF_UNIX` *does* have an abortive close. What it does not have is one
//! that a plain `closesocket` performs — and neither does TCP. The two rows
//! that look like "`AF_UNIX` loses data silently" are the rows where TCP does
//! the same thing, for the same reason: an ordinary close is a graceful close.
//!
//! The consequence is that [`UnixStream::read_to_end`] and
//! [`UnixStream::read_exact`] are offered with the **same** contract as their
//! TCP counterparts, not a weakened one. Had the measurement gone the other
//! way they would not have been offered at all, because a `read_to_end` that
//! cannot report truncation is worse than no `read_to_end`.
//!
//! # The generic asymmetry, and why it is not an oversight
//!
//! [`TcpStream`] and [`UnixStream`] are generic over a
//! [`Submitter`](crate::iocp::Submitter); [`TcpListener`] and [`UnixListener`]
//! are generic over a [`Registrar`](crate::iocp::Registrar).
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
//! # `AF_UNIX` addresses
//!
//! An address is a filesystem path or nothing at all — see [`UnixSocketAddr`],
//! whose docs carry the measurements the encoding rests on. Two consequences
//! are worth knowing before writing any code against these types:
//!
//! * **An accepted peer is normally unnamed.** `ConnectEx` refuses an unbound
//!   socket, so [`UnixStream::connect`] binds the empty path first — which
//!   creates no file and leaves nothing to clean up. The server therefore sees
//!   a peer with no path. That is the accurate answer, not a missing one; a
//!   server that needs to know who connected must say so in its protocol.
//! * **A relative path works, and resolves against the process working
//!   directory.** Measured. That makes it a poor choice for anything
//!   long-lived, since the address then depends on mutable process state.
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
//!             // `write_all`, not `write`: a single write may transfer fewer
//!             // bytes than it was given, and an echo that drops the rest is
//!             // not an echo.
//!             let (sent, _, _) = stream.write_all(buf[..n].to_vec()).await.into_parts();
//!             sent?;
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
//! # `AF_UNIX` example
//!
//! The same shape, over a path. Both ends are this crate's, so the whole
//! exchange runs on the thread pool. Note the two things a TCP version does
//! not have to say: the path has to be unique and is the caller's to remove,
//! and the accepted peer is unnamed.
//!
//! ```no_run
//! # use winasio::iocp::{OpResult, ThreadPool};
//! # use winasio::net::{ReadOutcome, Shutdown, UnixListener, UnixSocketAddr, UnixStream};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let mut path = std::env::temp_dir();
//! path.push(format!("winasio-example-{}.sock", std::process::id()));
//! let addr = UnixSocketAddr::from_pathname(&path)?;
//!
//! // `bind` does not remove a stale file, and neither does dropping the
//! // listener. This is the caller's cleanup; `UnixListenerOptions` has an
//! // explicit opt-in for doing it before the bind instead.
//! let listener = UnixListener::bind(&ThreadPool, &addr)?;
//!
//! let client = UnixStream::connect(&ThreadPool, &addr).await?;
//! let (server, peer) = listener.accept().await?;
//! // The client bound the unnamed address, so this is what the server sees.
//! assert!(peer.is_unnamed());
//!
//! let (sent, _, _) = client.write_all(b"ping".to_vec()).await.into_parts();
//! sent?;
//! // Without this the server's `read_to_end` waits for a request that will
//! // never come.
//! client.shutdown(Shutdown::Write)?;
//!
//! let (result, echoed, n) = server.read_to_end(Vec::new()).await.into_parts();
//! result?;
//! assert_eq!(&echoed[..n], b"ping");
//!
//! drop(listener);
//! let _ = std::fs::remove_file(&path);
//! # Ok(())
//! # }
//! ```
//!
//! # Out of scope
//!
//! UDP, `WSARecvFrom` / `WSASendTo`, `WSARecvMsg`, `TransmitFile` and vectored
//! I/O. For `AF_UNIX` specifically: datagram sockets, which the platform does
//! not offer — `SOCK_DGRAM` on `AF_UNIX` was measured to fail with
//! `WSAEAFNOSUPPORT` — along with socket-pair helpers and any peer-credential
//! analogue of `SO_PEERCRED`.

mod addr;
mod error;
mod ext;
mod init;
mod listener;
mod ops;
mod outcome;
mod socket;
mod stream;
mod unix_addr;
mod unix_listener;
mod unix_stream;

pub use error::SocketError;
pub use listener::{TcpListener, TcpListenerOptions};
pub use socket::Socket;
pub use stream::TcpStream;
pub use unix_addr::{UnixSocketAddr, UnixSocketAddrError, UNIX_PATH_MAX};
pub use unix_listener::{UnixListener, UnixListenerOptions};
pub use unix_stream::UnixStream;

/// Re-exported so a caller need not reach into [`std::net`] for the one
/// argument [`TcpStream::shutdown`] and [`UnixStream::shutdown`] take.
pub use std::net::Shutdown;

/// The read outcomes a socket reports, shared with files and pipes.
///
/// A socket never produces [`ReadOutcome::Eof`] (that is a file's end of file)
/// or [`ReadOutcome::MoreData`] (that is a message-mode pipe). It reports
/// [`ReadOutcome::ClosedPeer`] where a file would report `Eof`. This is the
/// same for both address families.
pub use crate::fs::ReadOutcome;

/// A [`TcpStream`] on the system thread pool.
pub type ThreadPoolTcpStream = TcpStream<crate::iocp::ThreadPoolIo>;

/// A [`TcpListener`] on the system thread pool.
///
/// Named for the *registrar*, not the submitter: the listener is what
/// registers each accepted socket, so it is `ThreadPool` that appears here
/// while [`ThreadPoolTcpStream`] names `ThreadPoolIo`.
pub type ThreadPoolTcpListener = TcpListener<crate::iocp::ThreadPool>;

/// A [`UnixStream`] on the system thread pool.
pub type ThreadPoolUnixStream = UnixStream<crate::iocp::ThreadPoolIo>;

/// A [`UnixListener`] on the system thread pool.
///
/// Named for the *registrar*, not the submitter, for the same reason
/// [`ThreadPoolTcpListener`] is: the listener registers each accepted socket,
/// so `ThreadPool` appears here while [`ThreadPoolUnixStream`] names
/// `ThreadPoolIo`.
pub type ThreadPoolUnixListener = UnixListener<crate::iocp::ThreadPool>;

#[cfg(any(test, feature = "test-util"))]
pub use socket::{live_sockets, socket_guard};
