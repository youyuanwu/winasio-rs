// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A listening `AF_UNIX` socket.

use crate::iocp::{OpResult, Registrar, Submitter};

use super::error::SocketError;
use super::ops::accept::AcceptSocket;
use super::socket::Socket;
use super::unix_addr::UnixSocketAddr;
use super::unix_stream::UnixStream;

/// Options for [`UnixListener::bind_with`].
///
/// Fields are private with setter methods, matching [`super::TcpListenerOptions`]
/// and [`crate::pipe::ServerOptions`]: a new option can then be added without
/// breaking callers who constructed this by hand.
#[derive(Debug, Clone, Copy)]
pub struct UnixListenerOptions {
    backlog: u32,
    unlink_stale: bool,
}

impl UnixListenerOptions {
    /// Options matching the platform defaults.
    pub fn new() -> Self {
        UnixListenerOptions {
            // The conventional "as much as the system allows" value; Windows
            // clamps it to the provider's maximum. Same default as
            // `TcpListenerOptions`.
            backlog: 128,
            // Off. See `unlink_stale`.
            unlink_stale: false,
        }
    }

    /// Set the `listen` backlog.
    ///
    /// Saturated into the platform's `i32` rather than truncated, exactly as
    /// [`super::TcpListenerOptions::backlog`].
    pub fn backlog(&mut self, backlog: u32) -> &mut Self {
        self.backlog = backlog;
        self
    }

    /// Delete a pre-existing file at the socket path before binding.
    ///
    /// **Off by default, and that default is the important part.**
    ///
    /// Binding an `AF_UNIX` socket creates a real file, and — measured —
    /// `closesocket` does *not* remove it. A path left behind by a previous
    /// run makes the next `bind` fail with `WSAEADDRINUSE`, which reaches the
    /// caller as [`SocketError::AddressInUse`]. Cleaning that up is the
    /// caller's job, because the crate cannot tell a leftover from a socket
    /// another live process is serving right now: the file looks the same
    /// either way, and unlinking the second one silently steals the address
    /// from a working server that goes on running, accepting nothing, with no
    /// error anywhere.
    ///
    /// That is why this is opt-in and why nothing enables it implicitly. It is
    /// the same judgement the crate makes about graceful close in `Drop`: a
    /// risky recovery that the caller can perform, and can decide is safe,
    /// does not get performed on the caller's behalf.
    ///
    /// Turning it on removes the file if one exists — and only then; a missing
    /// file is not an error — before `bind`. A file that exists and cannot be
    /// removed fails the bind with that removal error rather than the more
    /// confusing `WSAEADDRINUSE` that would follow. Setting it on a listener
    /// bound to the unnamed address does nothing, since there is no file.
    ///
    /// It closes no race: two processes can still both unlink and both bind.
    /// It is a convenience for the single-owner case — a test, or a service
    /// that knows it is the only writer of that path — not a lock.
    pub fn unlink_stale(&mut self, unlink_stale: bool) -> &mut Self {
        self.unlink_stale = unlink_stale;
        self
    }
}

impl Default for UnixListenerOptions {
    fn default() -> Self {
        UnixListenerOptions::new()
    }
}

/// A listening `AF_UNIX` socket registered with a completion backend.
///
/// The `AF_UNIX` counterpart of [`super::TcpListener`], and generic over a
/// [`Registrar`] for the same reason: every accept needs a fresh socket, and a
/// fresh socket must be registered before anything can be submitted on it. See
/// the [module docs](super) for why the two socket types differ in this.
///
/// **Field order is load-bearing**, exactly as it is for [`super::TcpListener`]:
/// the submitter's drop cancels and drains outstanding I/O on the listening
/// socket, so it has to run while that socket is still open.
///
/// # The bound file is not removed on drop
///
/// Dropping a listener closes its socket and leaves the file on disk, because
/// that is what the platform does and the crate does not paper over it. See
/// [`UnixListenerOptions::unlink_stale`] for why cleanup is not done
/// implicitly, and note that removing the file in `Drop` would be worse still:
/// a `Drop` runs on the panic path too, so a crash mid-request would delete
/// the address out from under an unrelated process that had since rebound it.
pub struct UnixListener<R: Registrar + Clone> {
    /// The listener's own submitter, from registering the listening socket.
    /// Declared first so it is dropped first. See above.
    io: R::Io,
    /// Kept so each accepted socket can be registered with the same backend.
    registrar: R,
    socket: Socket,
    /// The bound address, cached at construction.
    local: UnixSocketAddr,
}

impl<R: Registrar + Clone> UnixListener<R> {
    /// Bind and listen on `addr` with the default options.
    ///
    /// A pre-existing file at the path makes this fail with
    /// [`SocketError::AddressInUse`]; see
    /// [`UnixListenerOptions::unlink_stale`].
    ///
    /// A path whose **directory does not exist** fails with a code worth
    /// naming here, because its own name will mislead you: measured, it is
    /// `WSAENETDOWN` (10050) — "a socket operation encountered a dead
    /// network" — for a bind that involves no network at all. It is *not*
    /// `WSAEADDRNOTAVAIL` (10049), which is what the behaviour deserves. It
    /// reaches the caller as [`SocketError::Win32`] with the raw code intact,
    /// rather than being reclassified into something more sensible-sounding:
    /// the crate does not restate the platform's claims in words the platform
    /// did not use, and `WSAENETDOWN` is a code TCP can produce too, so
    /// remapping it here would quietly change what TCP reports.
    pub fn bind(registrar: &R, addr: &UnixSocketAddr) -> Result<Self, SocketError> {
        Self::bind_with(registrar, addr, &UnixListenerOptions::new())
    }

    /// Bind and listen on `addr`.
    pub fn bind_with(
        registrar: &R,
        addr: &UnixSocketAddr,
        options: &UnixListenerOptions,
    ) -> Result<Self, SocketError> {
        // Protocol zero, not `IPPROTO_TCP`: measured, `AF_UNIX` with
        // `IPPROTO_TCP` fails `WSAEPROTONOSUPPORT`.
        let socket = Socket::new_overlapped_unix().map_err(SocketError::from_win32)?;

        if options.unlink_stale {
            unlink_stale_path(addr)?;
        }

        socket
            .bind_bytes(&super::addr::SockAddrBytes::from_unix_addr(addr))
            .map_err(SocketError::from_win32)?;
        socket
            .listen_on(backlog_argument(options.backlog))
            .map_err(SocketError::from_win32)?;

        // Read the address back rather than trusting the argument, matching
        // `TcpListener`, where it is what reports the ephemeral port. There is
        // no `AF_UNIX` equivalent of an ephemeral port, so this is a
        // consistency check rather than a discovery — but it is also the only
        // thing that would catch the kernel storing something other than what
        // was asked for, which for a fixed 108-byte field is not unthinkable.
        let local = socket.local_unix_addr().map_err(SocketError::from_win32)?;

        // Registration last among the fallible setup steps: until it succeeds
        // `socket` is a plain local whose `Drop` closes it.
        let io = registrar.register(socket.as_handle())?;

        Ok(UnixListener {
            socket,
            io,
            registrar: registrar.clone(),
            local,
        })
    }

    /// The address the listener is bound to.
    pub fn local_addr(&self) -> &UnixSocketAddr {
        &self.local
    }

    /// The listening socket.
    pub fn socket(&self) -> &Socket {
        &self.socket
    }

    /// Accept one connection.
    ///
    /// If the returned future is dropped before resolving, cancellation is
    /// requested on the listener and the half-built socket is closed.
    ///
    /// **The peer address is very often unnamed.** A client must be bound
    /// before `ConnectEx` — the same rule as TCP — and the natural thing to
    /// bind it to is the empty path, which is what [`UnixStream::connect`]
    /// does and what `std`'s Unix client does. Such a peer reports
    /// [`UnixSocketAddr::is_unnamed`]. That is not a failure and not a missing
    /// address: it is the accurate answer, and a caller that needs to know who
    /// connected must carry that in the protocol rather than in the address.
    pub async fn accept(&self) -> Result<(UnixStream<R::Io>, UnixSocketAddr), SocketError> {
        // `AcceptEx` does not create the socket; the caller must supply one of
        // the listener's family, unbound and unconnected.
        let accepted = Socket::new_overlapped_unix().map_err(SocketError::from_win32)?;

        let op = AcceptSocket::new(self.socket.clone(), accepted);
        let OpResult(result, op) = self.io.submit(op).await;

        // `finish` applies `SO_UPDATE_ACCEPT_CONTEXT` and copies both addresses
        // out of the provider's buffer before it is dropped.
        let parts = op.finish(result).map_err(SocketError::from_win32)?;

        // The operation is family-agnostic and hands back encoded bytes; this
        // is where the `AF_UNIX` knowledge lives. A storage that is not
        // `AF_UNIX` is refused rather than invented — it would mean the
        // provider answered an `AF_UNIX` accept with something else.
        let peer = parts
            .peer
            .to_unix_addr()
            .ok_or_else(|| SocketError::from_win32(super::addr::unsupported_family()))?;

        // LOAD-BEARING ORDERING, as in `TcpListener::accept`: registering is
        // the last fallible step, so no error path can leave a registered
        // socket that nothing owns.
        let io = self.registrar.register(parts.socket.as_handle())?;

        Ok((UnixStream::from_parts(parts.socket, io), peer))
    }
}

impl<R: Registrar + Clone> std::fmt::Debug for UnixListener<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixListener")
            .field("socket", &self.socket)
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

impl<R: Registrar + Clone> Drop for UnixListener<R> {
    fn drop(&mut self) {
        // Ask the kernel to abandon any accept still in flight before the
        // submitter goes away, exactly as `TcpListener::drop` does. The socket
        // is not closed here: the `Socket` clones held by in-flight accepts
        // keep it alive until their completions arrive.
        //
        // The bound file is deliberately not removed. See the type docs.
        let _ = self.socket.cancel_all();
    }
}

/// Remove a stale socket file, if one is there.
///
/// A missing file is success: the option asks for the path to be free, and it
/// already is. Any other failure is reported, because binding afterwards would
/// fail with `WSAEADDRINUSE` and hide the real reason — a permission problem,
/// say, or a directory in the way.
fn unlink_stale_path(addr: &UnixSocketAddr) -> Result<(), SocketError> {
    let Some(path) = addr.as_pathname() else {
        // The unnamed address has no file. Nothing to do, and certainly not an
        // error: an option that panicked or failed on the wildcard would make
        // `unlink_stale(true)` unsafe to set unconditionally.
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SocketError::from_win32(windows::core::Error::from(e))),
    }
}

/// The `listen` argument for a requested backlog.
///
/// Saturating rather than wrapping, for the reason spelled out in
/// [`super::listener`]: `as i32` alone would turn a large request into a
/// negative number, which Winsock reinterprets instead of rejecting.
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
        assert_eq!(backlog_argument(UnixListenerOptions::new().backlog), 128);
    }

    #[test]
    fn an_oversized_backlog_saturates_rather_than_going_negative() {
        assert_eq!(backlog_argument(u32::MAX), i32::MAX);
        assert!(backlog_argument(u32::MAX) > 0);
    }

    #[test]
    fn stale_unlinking_is_off_by_default() {
        // The user-facing decision, asserted rather than assumed. A default of
        // `true` would silently delete a path another process is serving.
        assert!(!UnixListenerOptions::new().unlink_stale);
        assert!(!UnixListenerOptions::default().unlink_stale);
    }

    #[test]
    fn the_builder_records_what_it_was_given() {
        let mut options = UnixListenerOptions::new();
        options.backlog(7).unlink_stale(true);
        assert_eq!(options.backlog, 7);
        assert!(options.unlink_stale);
    }

    #[test]
    fn unlinking_a_path_that_is_not_there_is_not_an_error() {
        // Otherwise the very first run of a service with the option set would
        // fail, which is the case it exists to serve.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "winasio_absent_{}_{}.sock",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        assert!(!path.exists(), "the test needs the path to be absent");
        let addr = UnixSocketAddr::from_pathname(&path).expect("build");
        unlink_stale_path(&addr).expect("an absent path is already free");
    }

    #[test]
    fn unlinking_removes_a_file_that_is_there() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "winasio_stale_{}_{}.sock",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, b"not really a socket").expect("create the stale file");
        assert!(path.exists(), "the test needs the path to be present");
        let addr = UnixSocketAddr::from_pathname(&path).expect("build");
        unlink_stale_path(&addr).expect("remove");
        assert!(!path.exists(), "the file must be gone");
    }

    #[test]
    fn unlinking_the_unnamed_address_does_nothing_and_succeeds() {
        // `unlink_stale(true)` must be safe to set unconditionally, including
        // on a listener that binds no file at all.
        unlink_stale_path(&UnixSocketAddr::unnamed()).expect("nothing to unlink");
    }
}
