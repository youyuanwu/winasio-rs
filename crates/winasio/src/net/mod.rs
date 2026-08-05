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
