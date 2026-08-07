// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! `AF_UNIX` socket integration tests.
//!
//! Modelled on `sockets.rs`, with one difference that shapes the whole file:
//! a Unix socket's address is a **file that the platform leaves behind**.
//! `bind` creates it and `closesocket` does not remove it, so a path reused
//! across tests — or across runs — fails the second `bind` with
//! `WSAEADDRINUSE`, which is a genuinely baffling failure to debug from the
//! error alone. Every test therefore takes its path from [`SocketPath`], which
//! makes it unique per binary, test, process and call, and removes it on drop
//! whatever happened in between.
//!
//! Cases where the *backend's* teardown or completion delivery actually differ
//! get both an `_own_port` and a `_thread_pool` variant driven by one body, as
//! in `sockets.rs`. The rest are backend-independent and run on `Proactor`
//! alone to keep this binary's runtime inside the flakiness gate.
//!
//! Nothing here is allowed to pass vacuously. Where a test could in principle
//! skip — because the environment might not support something — it asserts
//! that it did not skip instead.

mod common;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use winasio::iocp::{OpResult, Proactor, ThreadPool};
use winasio::net::{
    live_sockets, ReadOutcome, Shutdown, SocketError, UnixListener, UnixListenerOptions,
    UnixSocketAddr, UnixSocketAddrError, UnixStream, UNIX_PATH_MAX,
};

use common::drive_proactor;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// A socket path that is unique and cleans up after itself.
///
/// Uniqueness has four parts because all four can collide: the binary name
/// (cargo runs integration binaries in parallel), the test name (cargo runs
/// tests within a binary in parallel), the process id (a previous run may
/// still be alive, and a leftover from a *dead* one certainly is), and a
/// counter (one test may want two).
///
/// The `Drop` is the load-bearing half. Windows does not remove a bound
/// `AF_UNIX` path when the socket closes, so without this a single failing
/// test would poison every later run of the same test with `WSAEADDRINUSE` —
/// an error that says nothing about the real problem. It runs on the panic
/// path too, which is exactly when it is most needed.
struct SocketPath {
    path: PathBuf,
}

impl SocketPath {
    fn new(test_name: &str) -> SocketPath {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let binary = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "unknown".to_string());
        let mut path = std::env::temp_dir();
        path.push(format!(
            "winasio-{}-{}-{}-{}.sock",
            sanitize(&binary),
            sanitize(test_name),
            std::process::id(),
            n
        ));
        // A leftover from a crashed earlier run would make `bind` fail. The
        // whole point of this type is that a test never has to think about
        // that, so clear it here as well as on drop.
        let _ = std::fs::remove_file(&path);
        SocketPath { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn addr(&self) -> UnixSocketAddr {
        UnixSocketAddr::from_pathname(&self.path).expect("a temp path fits in sun_path")
    }

    fn exists(&self) -> bool {
        self.path.exists()
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// A path of exactly `UNIX_PATH_MAX` bytes, with **no room for a terminator**.
///
/// Measured: such a path binds, and `getsockname` reports it back with no NUL
/// anywhere in the slot. It is the case a naive `CStr`-style decoder reads off
/// the end of, so it gets its own test — and this helper builds it precisely
/// rather than approximately, because a 107-byte path would prove nothing.
///
/// Returns `None` if the temp directory is so deep that the padding would not
/// fit; the caller asserts on that rather than skipping.
fn full_slot_path(discriminator: u64) -> Option<PathBuf> {
    let mut dir = std::env::temp_dir();
    // A trailing separator would be doubled below.
    if dir.as_os_str().to_string_lossy().ends_with('\\') {
        dir = PathBuf::from(dir.as_os_str().to_string_lossy().trim_end_matches('\\'));
    }
    let prefix = format!("{}\\w{}-", dir.display(), discriminator);
    let padding = UNIX_PATH_MAX.checked_sub(prefix.len())?;
    if padding == 0 {
        return None;
    }
    let full = format!("{prefix}{}", "p".repeat(padding));
    assert_eq!(full.len(), UNIX_PATH_MAX, "the helper must be exact");
    Some(PathBuf::from(full))
}

/// Drop a path built by [`full_slot_path`], which has no [`SocketPath`] guard.
struct RawPathGuard(PathBuf);

impl Drop for RawPathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------
// U1 — bind and address reporting
// ---------------------------------------------------------------------------

/// Binding creates the file and reports the path back.
///
/// The file's existence is asserted because it is the fact the whole cleanup
/// story rests on: if `bind` did *not* create a file, `unlink_stale` and the
/// `SocketPath` guard would both be solving a problem that does not exist, and
/// this suite would be quietly testing nothing.
#[test]
fn bind_creates_the_file_and_reports_the_path() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("bind_creates_the_file");
    let proactor = Rc::new(Proactor::new().expect("proactor"));

    assert!(!path.exists(), "the path must start clean");
    let listener = UnixListener::bind(&proactor, &path.addr()).expect("bind");

    assert!(
        path.exists(),
        "binding an AF_UNIX socket must create a real file; if this ever stops \
         being true, the stale-path handling in this crate is solving nothing"
    );
    assert_eq!(listener.local_addr().as_pathname(), Some(path.path()));
    assert!(!listener.local_addr().is_unnamed());
}

/// Closing the listener leaves the file, and the crate does not hide it.
///
/// This is the platform behaviour the module documents and refuses to paper
/// over, so it is asserted rather than assumed. If Windows ever started
/// removing the file, this test fails and the documentation — and
/// `unlink_stale`'s rationale — would need revisiting.
#[test]
fn dropping_the_listener_leaves_the_file_behind() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("drop_leaves_the_file");
    let proactor = Rc::new(Proactor::new().expect("proactor"));

    let listener = UnixListener::bind(&proactor, &path.addr()).expect("bind");
    assert!(path.exists());
    drop(listener);

    assert!(
        path.exists(),
        "closesocket does not unlink an AF_UNIX path; the crate must not \
         pretend otherwise, because a Drop that deleted the file could delete \
         one another process had since rebound"
    );
}

// ---------------------------------------------------------------------------
// U2 — accept and connect, both backends
// ---------------------------------------------------------------------------

/// Accept and connect complete on the caller-driven backend.
#[test]
fn accept_and_connect_complete_on_own_port() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("accept_connect_own");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let connecting = UnixStream::connect(&proactor, &addr);
    let accepting = listener.accept();

    // Both futures are driven on one thread, so neither may block the other.
    let (accepted, connected) = drive_proactor(&proactor, async {
        futures_join(accepting, connecting).await
    });
    let (server, peer) = accepted.expect("accept");
    let client = connected.expect("connect");

    assert_eq!(server.local_addr().expect("server local"), addr);
    assert_eq!(client.peer_addr().expect("client peer"), addr);
    assert!(
        peer.is_unnamed(),
        "a client that binds the wildcard is seen as unnamed; see UnixListener::accept"
    );
}

/// The same, on the system thread pool.
#[test]
fn accept_and_connect_complete_on_thread_pool() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("accept_connect_pool");
    let addr = path.addr();
    let listener = UnixListener::bind(&ThreadPool, &addr).expect("bind");

    let (accepted, connected) = common::block_on(async {
        futures_join(listener.accept(), UnixStream::connect(&ThreadPool, &addr)).await
    });
    let (server, peer) = accepted.expect("accept");
    let client = connected.expect("connect");

    assert_eq!(server.local_addr().expect("server local"), addr);
    assert_eq!(client.peer_addr().expect("client peer"), addr);
    assert!(peer.is_unnamed());
}

/// A minimal two-future join, so a single-threaded proactor can drive both
/// halves of a connection without a futures dependency.
async fn futures_join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    use std::task::Poll;

    let mut a = Box::pin(a);
    let mut b = Box::pin(b);
    let mut ra = None;
    let mut rb = None;
    std::future::poll_fn(move |cx| {
        if ra.is_none() {
            if let Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        if ra.is_some() && rb.is_some() {
            Poll::Ready((ra.take().unwrap(), rb.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// U3 — the unnamed peer
// ---------------------------------------------------------------------------

/// An accepted peer is unnamed, and that is reported accurately.
///
/// This is the one address case with no TCP counterpart. `ConnectEx` refuses
/// an unbound socket, so a client must bind something; the empty path is the
/// only choice that creates no file, and it is what this crate binds. The
/// server therefore sees a peer with no path at all.
///
/// The test asserts on all three ways that address can be looked at — the one
/// `accept` returns, the one `peer_addr` reports on the server side, and the
/// one `local_addr` reports on the client side — because a bug that produced a
/// plausible-looking empty value in one of them and garbage in another would
/// otherwise slip through.
#[test]
fn an_accepted_peer_is_unnamed_and_says_so() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("unnamed_peer");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let (accepted, connected) = drive_proactor(&proactor, async {
        futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await
    });
    let (server, peer) = accepted.expect("accept");
    let client = connected.expect("connect");

    assert!(peer.is_unnamed(), "accept reported {peer:?}");
    assert_eq!(peer.as_pathname(), None);
    assert!(peer.as_bytes().is_empty());
    assert_eq!(peer, UnixSocketAddr::unnamed());

    let from_server = server.peer_addr().expect("server sees the peer");
    assert!(
        from_server.is_unnamed(),
        "peer_addr reported {from_server:?}"
    );
    assert_eq!(from_server, peer);

    let from_client = client.local_addr().expect("client sees itself");
    assert!(
        from_client.is_unnamed(),
        "the client bound the wildcard, so its own address is unnamed too, \
         reported {from_client:?}"
    );
}

// ---------------------------------------------------------------------------
// U4 — payload round trip, both backends
// ---------------------------------------------------------------------------

/// A payload crosses in both directions on the caller-driven backend.
#[test]
fn round_trip_read_write_on_own_port() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("round_trip_own");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let echoed = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        let (server, _) = accepted.expect("accept");
        let client = connected.expect("connect");

        let (sent, _, n) = client
            .write_all(b"ping over a path".to_vec())
            .await
            .into_parts();
        sent.expect("client write");
        assert_eq!(n, 16);

        let (result, buf, got) = server.read_exact(vec![0u8; 16]).await.into_parts();
        result.expect("server read_exact");
        assert_eq!(got, 16);

        let (sent, _, _) = server.write_all(buf.clone()).await.into_parts();
        sent.expect("server write");

        let (result, back, got) = client.read_exact(vec![0u8; 16]).await.into_parts();
        result.expect("client read_exact");
        assert_eq!(got, 16);
        back
    });

    assert_eq!(&echoed, b"ping over a path");
}

/// The same, on the system thread pool.
#[test]
fn round_trip_read_write_on_thread_pool() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("round_trip_pool");
    let addr = path.addr();
    let listener = UnixListener::bind(&ThreadPool, &addr).expect("bind");

    let echoed = common::block_on(async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&ThreadPool, &addr)).await;
        let (server, _) = accepted.expect("accept");
        let client = connected.expect("connect");

        let (sent, _, _) = client
            .write_all(b"ping over a path".to_vec())
            .await
            .into_parts();
        sent.expect("client write");

        let (result, buf, got) = server.read_exact(vec![0u8; 16]).await.into_parts();
        result.expect("server read_exact");
        assert_eq!(got, 16);

        let (sent, _, _) = server.write_all(buf).await.into_parts();
        sent.expect("server write");

        let (result, back, _) = client.read_exact(vec![0u8; 16]).await.into_parts();
        result.expect("client read_exact");
        back
    });

    assert_eq!(&echoed, b"ping over a path");
}

// ---------------------------------------------------------------------------
// U5 — stale files and the opt-in unlink
// ---------------------------------------------------------------------------

/// A leftover file makes `bind` fail, and it fails as `AddressInUse`.
///
/// The variant matters as much as the failure. `WSAEADDRINUSE` is what the
/// platform reports for a stale *file*, which is not what the name suggests —
/// nothing is using the address — so a caller acting on the error needs the
/// classification to be stable. This pins it.
#[test]
fn a_stale_file_makes_bind_fail_with_address_in_use() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("stale_bind_fails");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();

    // A real bound-and-closed socket, not a hand-made file: the point is that
    // the platform's own leftover is what blocks the rebind.
    let first = UnixListener::bind(&proactor, &addr).expect("first bind");
    drop(first);
    assert!(path.exists(), "the file must survive the close");

    let err = UnixListener::bind(&proactor, &addr).expect_err(
        "binding over a stale file must fail; if this ever succeeds, the crate \
         is silently reusing an address it does not own",
    );
    assert!(
        matches!(err, SocketError::AddressInUse),
        "a stale path classifies as AddressInUse, got {err:?}"
    );
}

/// The opt-in removes the stale file and the bind then succeeds.
///
/// Paired deliberately with the test above: together they show the option
/// changes the outcome, which neither shows alone. A test that only asserted
/// "with the option, bind succeeds" would pass even if the option did nothing
/// and no stale file had ever existed.
#[test]
fn the_opt_in_unlinks_a_stale_file() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("stale_unlink_opt_in");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();

    let first = UnixListener::bind(&proactor, &addr).expect("first bind");
    drop(first);
    assert!(path.exists(), "the file must survive the close");

    let mut options = UnixListenerOptions::new();
    options.unlink_stale(true);
    let second =
        UnixListener::bind_with(&proactor, &addr, &options).expect("bind with unlink_stale");

    assert_eq!(second.local_addr(), &addr);
    assert!(
        path.exists(),
        "the *new* bind recreates the file it just removed"
    );
}

/// The option is off unless asked for.
///
/// The default is a documented promise, so it is checked through the public
/// path — a fresh `UnixListenerOptions` handed to `bind_with` — rather than by
/// reading the field, which a unit test already does.
#[test]
fn the_default_options_do_not_unlink() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("default_no_unlink");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();

    let first = UnixListener::bind(&proactor, &addr).expect("first bind");
    drop(first);

    let err = UnixListener::bind_with(&proactor, &addr, &UnixListenerOptions::new())
        .expect_err("the default must not unlink");
    assert!(matches!(err, SocketError::AddressInUse), "got {err:?}");
}

/// The opt-in is harmless when there is nothing to remove.
#[test]
fn the_opt_in_is_a_no_op_when_the_path_is_clean() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("unlink_nothing_there");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    assert!(!path.exists(), "the path must start clean");

    let mut options = UnixListenerOptions::new();
    options.unlink_stale(true);
    let listener = UnixListener::bind_with(&proactor, &addr, &options)
        .expect("a missing file is not an error");
    assert_eq!(listener.local_addr(), &addr);
}

// ---------------------------------------------------------------------------
// U6 — connect refused
// ---------------------------------------------------------------------------

/// Connecting to a path with no listener is refused.
///
/// Measured, and worth pinning because it is not the obvious answer: the
/// failure is really "no such file", but the platform reports
/// `WSAECONNREFUSED` — **inline**, not as a pending operation that later
/// fails. The variant is asserted rather than merely "some error", since
/// `ConnectionRefused` is what a caller will branch on and a drift to
/// `Win32(ERROR_FILE_NOT_FOUND)` would be a silent breaking change.
#[test]
fn connect_to_an_unbound_path_is_refused() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("connect_refused");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    assert!(!path.exists(), "nothing may be listening on this path");

    let err = drive_proactor(&proactor, UnixStream::connect(&proactor, &addr))
        .expect_err("connecting to a path with no listener must fail");
    assert!(
        matches!(err, SocketError::ConnectionRefused),
        "a path with no listener is refused, not reported as a missing file; got {err:?}"
    );
}

/// Connecting to a path that exists but is an ordinary file is also refused.
///
/// The interesting half of the case above: here the file *is* there, so a
/// naive implementation that checked for existence would proceed.
#[test]
fn connect_to_a_plain_file_is_refused() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("connect_plain_file");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    std::fs::write(path.path(), b"not a socket").expect("create a plain file");
    assert!(path.exists());

    let err = drive_proactor(&proactor, UnixStream::connect(&proactor, &path.addr()))
        .expect_err("a plain file is not a listener");
    assert!(matches!(err, SocketError::ConnectionRefused), "got {err:?}");
}

// ---------------------------------------------------------------------------
// U7 — end of stream, half-close, half-open
// ---------------------------------------------------------------------------

/// A peer's `shutdown(Write)` is seen as a clean end of stream, and the
/// other direction keeps working.
///
/// Half-open is the property this checks, and it is checked in both
/// directions: the server reads to the end *and then* writes, and the client
/// receives that write. A transport that tore the whole connection down on a
/// one-way shutdown would fail the second half while passing the first.
#[test]
fn half_close_ends_one_direction_and_leaves_the_other_open() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("half_close");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let reply = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        let (server, _) = accepted.expect("accept");
        let client = connected.expect("connect");

        let (sent, _, _) = client.write_all(b"request".to_vec()).await.into_parts();
        sent.expect("client write");
        client.shutdown(Shutdown::Write).expect("client half-close");

        // The server reads to the end. Without the half-close this would hang.
        let (result, request, n) = server.read_to_end(Vec::new()).await.into_parts();
        result.expect("read_to_end after a half-close is a clean end of stream");
        assert_eq!(&request[..n], b"request");

        // The other direction is still open: this is the half-open property.
        let (sent, _, _) = server.write_all(b"response".to_vec()).await.into_parts();
        sent.expect("the server->client direction survives the client's shutdown");
        server.shutdown(Shutdown::Write).expect("server half-close");

        let (result, reply, n) = client.read_to_end(Vec::new()).await.into_parts();
        result.expect("client read_to_end");
        reply[..n].to_vec()
    });

    assert_eq!(
        &reply, b"response",
        "the client must still receive after having shut down its own send side"
    );
}

/// A read after the peer closes reports `ClosedPeer`, not an error.
#[test]
fn read_after_peer_closes_reports_closed_peer() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("closed_peer");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let outcome = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        let (server, _) = accepted.expect("accept");
        drop(connected.expect("connect"));

        let OpResult(outcome, _) = server.read(vec![0u8; 16]).await;
        outcome
    });

    assert!(
        matches!(outcome, Ok(ReadOutcome::ClosedPeer)),
        "an ordinary close with nothing unread is a clean end of stream, got {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// U8 — the reset distinction, which AF_UNIX turns out to keep
// ---------------------------------------------------------------------------

/// Set `SO_LINGER {1, 0}` on a `winasio` socket, making its next close abortive.
///
/// `sockets.rs` does this to a `std::net::TcpStream`; there is no `std` type
/// for an `AF_UNIX` socket on Windows, so it is applied to the crate's own
/// [`winasio::net::Socket`] instead.
fn set_abortive_close(socket: &winasio::net::Socket) {
    use windows::Win32::Networking::WinSock::{setsockopt, LINGER, SOL_SOCKET, SO_LINGER};

    let linger = LINGER {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: a live `LINGER` of exactly the size Winsock expects, read only
    // for the duration of the call.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(&linger).cast::<u8>(),
            std::mem::size_of::<LINGER>(),
        )
    };
    // SAFETY: the socket is alive for the call.
    let rc = unsafe { setsockopt(socket.raw(), SOL_SOCKET, SO_LINGER, Some(bytes)) };
    assert_eq!(
        rc, 0,
        "AF_UNIX must accept SO_LINGER; if it ever stops, the whole premise \
         that AF_UNIX has an abortive close needs re-measuring"
    );
}

/// **The headline test of this suite.** `read_to_end` on an `AF_UNIX`
/// connection that is reset mid-stream must fail, not truncate.
///
/// This work started from the belief that `AF_UNIX` on Windows has no reset —
/// that a peer vanishing mid-stream would be indistinguishable from one that
/// finished, making the module's central invariant undeliverable here and
/// `read_to_end` a truncation hazard. Measurement said otherwise: `SO_LINGER
/// {1, 0}` is accepted on an `AF_UNIX` socket and produces a genuine abortive
/// close, delivered on the completion path as `STATUS_CONNECTION_RESET`, which
/// the crate already classifies as a lost connection.
///
/// So `UnixStream::read_to_end` is offered with the same contract as
/// `TcpStream::read_to_end`, and this is the test that keeps that promise
/// honest. It is the exact counterpart of
/// `read_to_end_after_a_reset_fails_rather_than_truncating` in `sockets.rs`,
/// and it fails loudly if the platform ever behaves the way this work
/// originally assumed it did.
#[test]
fn read_to_end_after_a_reset_fails_rather_than_truncating() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("reset_no_truncate");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let (result, collected) = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        let (server, _) = accepted.expect("accept");
        let client = connected.expect("connect");

        // A prefix, then an abortive close. A caller reading to the end must
        // not be told that this prefix was the whole message.
        let (sent, _, _) = client.write_all(b"prefix".to_vec()).await.into_parts();
        sent.expect("client write");
        set_abortive_close(client.socket());
        drop(client);

        let transfer = server.read_to_end(Vec::new()).await;
        let (result, buffer, _) = transfer.into_parts();
        (result, buffer)
    });

    if let Ok(()) = result {
        panic!(
            "read_to_end returned Ok on a reset AF_UNIX connection, silently \
             truncating the stream to {} bytes. AF_UNIX does have an abortive \
             close on Windows; if this platform no longer delivers one, the \
             module docs and read_to_end's contract must both change.",
            collected.len()
        );
    }
    // Assert *why* it failed, not merely that it did. Without this the test
    // would pass if `read_to_end` broke for an unrelated reason — a bad
    // handle, a cancelled operation — and would go on passing after the reset
    // detection it exists to guard had been removed.
    let err = result.expect_err("checked above");
    // `TransferError::ClosedPeer` is the *abrupt* variant, which reads oddly
    // until you notice that a clean end of stream never reaches here at all:
    // `read_to_end` returns `Ok` for that. Reaching the error path with this
    // variant is precisely "the connection was lost mid-transfer", which is
    // what a reset is.
    assert!(
        matches!(err, winasio::io::TransferError::ClosedPeer),
        "a reset must surface as a lost connection, not as an unrelated \
         platform fault; got {err:?}"
    );
    // Whatever did arrive is handed back rather than discarded, and is a
    // genuine prefix of what was sent rather than garbage.
    //
    // Note what this does *not* claim: that the prefix arrives at all. Under
    // `SO_LINGER {1, 0}` the queued bytes are usually discarded with the
    // connection, so `collected` is normally empty and an assertion that it
    // was non-empty would be flaky. The useful invariant is the weaker,
    // true one — nothing is invented, and nothing beyond what was sent
    // appears — so that is what is asserted.
    assert!(
        b"prefix".starts_with(collected.as_slice()),
        "the salvaged bytes must be a prefix of what was sent, got {collected:?}"
    );
}

/// A failed whole-payload read still hands back what it collected.
///
/// The companion to the reset test above, which cannot check this: an
/// abortive close normally discards the queued bytes, so there is nothing to
/// salvage and asserting there was would be flaky. This provokes the same
/// *shape* of failure deterministically — `read_exact` asking for more than
/// the peer will ever send, then a half-close — so the buffer and the count
/// are both observable.
///
/// It matters because `TransferResult` returning the buffer on the error path
/// is the whole reason a caller can act on a truncated stream at all. If it
/// silently dropped the bytes, every test above would still pass.
#[test]
fn a_failed_read_exact_still_returns_what_it_collected() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("partial_salvage");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let (result, buffer, transferred) = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        let (server, _) = accepted.expect("accept");
        let client = connected.expect("connect");

        let (sent, _, _) = client.write_all(b"half".to_vec()).await.into_parts();
        sent.expect("client write");
        // A clean half-close, so the failure is unambiguously "the stream
        // ended early" rather than anything to do with a reset.
        client.shutdown(Shutdown::Write).expect("half-close");

        // Ask for ten bytes when only four will ever arrive.
        server.read_exact(vec![0u8; 10]).await.into_parts()
    });

    let err = result.expect_err("read_exact cannot be satisfied and must fail");
    assert!(
        matches!(err, winasio::io::TransferError::ClosedPeer),
        "the stream ended before the request could be filled; got {err:?}"
    );
    assert_eq!(
        transferred, 4,
        "the count must report what actually arrived, not zero and not ten"
    );
    assert_eq!(
        &buffer[..transferred],
        b"half",
        "the collected bytes must be handed back intact on the error path, or a \
         caller has no way to salvage a truncated stream"
    );
}

/// The classifier's view of the same event, in isolation.
///
/// `read_to_end_after_a_reset_fails_rather_than_truncating` is what a caller
/// actually experiences; this checks the underlying read reports a *lost
/// connection* rather than a clean end, which is what makes that possible.
#[test]
fn a_reset_is_an_error_not_a_closed_peer() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("reset_is_error");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let err = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        let (server, _) = accepted.expect("accept");
        let client = connected.expect("connect");

        // Data the closer never read is the other way to provoke a reset;
        // using it here rather than SO_LINGER covers the second measured path.
        let (sent, _, _) = client.write_all(b"unread".to_vec()).await.into_parts();
        sent.expect("client write");
        let (sent, _, _) = server
            .write_all(b"never read by the client".to_vec())
            .await
            .into_parts();
        sent.expect("server write");
        drop(client);

        // Drain whatever did arrive, then find the failure.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "the reset never turned into a read error"
            );
            let OpResult(outcome, _) = server.read(vec![0u8; 64]).await;
            match outcome {
                Err(e) => break e,
                Ok(ReadOutcome::Bytes(_)) => continue,
                Ok(ReadOutcome::ClosedPeer) => panic!(
                    "closing with data still unread must be a reset, not a clean end; \
                     reporting it as ClosedPeer tells the caller the stream finished"
                ),
                Ok(other) => panic!("unexpected outcome {other:?}"),
            }
        }
    });

    assert!(
        matches!(SocketError::from_win32(err), SocketError::ConnectionAborted),
        "a reset classifies as a lost connection so a caller can tell it from \
         an unrelated fault"
    );
}

// ---------------------------------------------------------------------------
// U9 — the inline completion path
// ---------------------------------------------------------------------------

/// An inline `AF_UNIX` send queues no completion packet.
///
/// **`Proactor`-only by construction**, as in `sockets.rs`:
/// `unclaimed_completions()` has no thread-pool equivalent.
///
/// This matters more for `AF_UNIX` than for TCP. Measured, `AF_UNIX` sends and
/// receives complete inline essentially always — there is no network to wait
/// for — so the mandatory skip mode is not an optimisation here but the
/// dominant path. If it were ever not applied to an `AF_UNIX` socket, every
/// transfer would leave a stray packet behind.
///
/// That the send *did* complete inline is asserted, not assumed. Without that
/// guard the test would pass with the skip mode removed, because a pending
/// send's packet is claimed by the normal path and never counted as unclaimed.
#[test]
fn an_inline_send_queues_no_completion_packet() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("inline_send");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let (server, client) = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        (accepted.expect("accept").0, connected.expect("connect"))
    });

    // Drain anything the accept and connect left behind before measuring.
    while proactor.poll(Some(Duration::from_millis(5))).expect("poll") > 0 {}
    let before = proactor.unclaimed_completions();

    let mut writing = Box::pin(server.write(b"ping".to_vec()));
    let OpResult(written, _) = poll_once(writing.as_mut()).expect(
        "a 4-byte AF_UNIX send must complete inline; if it went pending, this \
         test would prove nothing about the skip mode",
    );
    assert_eq!(written.expect("write"), 4);

    let delivered = proactor
        .poll(Some(Duration::from_millis(200)))
        .expect("poll");

    assert_eq!(
        proactor.unclaimed_completions(),
        before,
        "an inline success must queue no packet; one was dequeued with nothing \
         to deliver it to, which means the skip mode was not applied to the \
         AF_UNIX socket"
    );
    assert_eq!(proactor.pending_count(), 0);
    assert_eq!(delivered, 0);

    drop(client);
}

/// An inline `AF_UNIX` *receive* likewise queues no packet.
///
/// The send case above is the one `sockets.rs` covers for TCP. The receive
/// case is worth its own test here because on `AF_UNIX` it is the common one:
/// data written by a local peer is already there when the read is submitted,
/// so a receive with data waiting resolves immediately rather than pending.
#[test]
fn an_inline_receive_queues_no_completion_packet() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("inline_recv");
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let addr = path.addr();
    let listener = UnixListener::bind(&proactor, &addr).expect("bind");

    let (server, client) = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        let (server, _) = accepted.expect("accept");
        let client = connected.expect("connect");
        let (sent, _, _) = client.write_all(b"waiting".to_vec()).await.into_parts();
        sent.expect("client write");
        (server, client)
    });

    while proactor.poll(Some(Duration::from_millis(5))).expect("poll") > 0 {}
    let before = proactor.unclaimed_completions();

    let mut reading = Box::pin(server.read(vec![0u8; 32]));
    let OpResult(outcome, buf) = poll_once(reading.as_mut())
        .expect("a receive with data already queued must complete inline on AF_UNIX");
    assert!(
        matches!(outcome, Ok(ReadOutcome::Bytes(7))),
        "got {outcome:?}"
    );
    assert_eq!(&buf[..7], b"waiting");

    let delivered = proactor
        .poll(Some(Duration::from_millis(200)))
        .expect("poll");
    assert_eq!(
        proactor.unclaimed_completions(),
        before,
        "an inline receive must queue no packet"
    );
    assert_eq!(proactor.pending_count(), 0);
    assert_eq!(delivered, 0);

    drop(client);
}

/// Poll a future once. Returns `None` if it went pending.
///
/// Deliberately does **not** drive the proactor: the whole point is to
/// distinguish an operation that resolved without a completion packet from one
/// that needed one, and polling the port would erase the difference.
fn poll_once<F: Future>(mut fut: std::pin::Pin<&mut F>) -> Option<F::Output> {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => Some(v),
        std::task::Poll::Pending => None,
    }
}

// ---------------------------------------------------------------------------
// U10 — path encoding edge cases, end to end
// ---------------------------------------------------------------------------

/// A path filling the whole 108-byte slot, with no NUL terminator, works.
///
/// This is the case a `CStr`-style decoder reads off the end of. Measured:
/// such a path binds, and `getsockname` reports back a slot with no NUL
/// anywhere. The test builds the path to exactly `UNIX_PATH_MAX` bytes and
/// asserts the length, so a change in the temp directory's depth cannot
/// silently turn this into a 90-byte path that proves nothing.
///
/// If the environment makes an exact-length path impossible, the test
/// **fails** rather than skipping.
#[test]
fn a_path_filling_the_slot_with_no_terminator_round_trips() {
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));

    let path = full_slot_path(std::process::id() as u64).expect(
        "the temp directory is too deep to build a 108-byte path; this test \
         must not be skipped silently, so it fails instead",
    );
    assert_eq!(
        path.as_os_str().len(),
        UNIX_PATH_MAX,
        "the whole point is that the path fills the slot exactly"
    );
    let _cleanup = RawPathGuard(path.clone());
    let _ = std::fs::remove_file(&path);

    let addr =
        UnixSocketAddr::from_pathname(&path).expect("a 108-byte path is exactly the maximum");
    assert_eq!(addr.as_bytes().len(), UNIX_PATH_MAX);

    let listener = UnixListener::bind(&proactor, &addr).expect("bind a full-slot path");
    assert!(path.exists(), "the file must be created at the full path");
    assert_eq!(
        listener.local_addr().as_pathname(),
        Some(path.as_path()),
        "getsockname must report a slot with no terminator back as the whole path"
    );
    assert_eq!(listener.local_addr().as_bytes().len(), UNIX_PATH_MAX);

    // And it is usable, not merely bindable.
    let peer = drive_proactor(&proactor, async {
        let (accepted, connected) =
            futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
        let (_server, peer) = accepted.expect("accept over a full-slot path");
        let client = connected.expect("connect to a full-slot path");
        assert_eq!(client.peer_addr().expect("peer_addr"), addr);
        peer
    });
    assert!(peer.is_unnamed());
}

/// A non-ASCII path round-trips byte for byte.
///
/// `sun_path` is UTF-8 bytes, and the `windows` crate types it as `[i8; 108]`,
/// so every byte above 0x7F crosses a signedness boundary twice on the way
/// out and back. Measured to survive; this checks the crate's own encoding
/// does too, comparing **bytes** rather than paths so that a lossy conversion
/// somewhere in the middle cannot be masked by a forgiving `PathBuf`
/// comparison.
#[test]
fn a_non_ascii_path_round_trips_byte_for_byte() {
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));

    let mut path = std::env::temp_dir();
    path.push(format!(
        "winasio-\u{00e9}\u{4e2d}\u{6587}-{}.sock",
        std::process::id()
    ));
    let _cleanup = RawPathGuard(path.clone());
    let _ = std::fs::remove_file(&path);

    let addr = UnixSocketAddr::from_pathname(&path).expect("build");
    let expected = path.as_os_str().to_string_lossy().into_owned().into_bytes();
    assert!(
        expected.iter().any(|b| *b >= 0x80),
        "the test path must actually contain non-ASCII bytes, or this proves nothing"
    );
    assert_eq!(addr.as_bytes(), expected.as_slice());

    let listener = UnixListener::bind(&proactor, &addr).expect("bind a non-ASCII path");
    assert!(path.exists(), "the expected file must appear on disk");
    assert_eq!(
        listener.local_addr().as_bytes(),
        expected.as_slice(),
        "getsockname must return the same bytes that were sent in"
    );
    assert_eq!(listener.local_addr(), &addr);
}

/// A relative path binds, and resolves against the process working directory.
///
/// Documented behaviour, so it is pinned. It also makes clear *why* the docs
/// call it a poor choice: the address depends on mutable process state.
#[test]
fn a_relative_path_resolves_against_the_working_directory() {
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));

    let name = format!("winasio-rel-{}.sock", std::process::id());
    let cwd = std::env::current_dir().expect("cwd");
    let absolute = cwd.join(&name);
    let _cleanup = RawPathGuard(absolute.clone());
    let _ = std::fs::remove_file(&absolute);

    let addr = UnixSocketAddr::from_pathname(&name).expect("build");
    let listener = UnixListener::bind(&proactor, &addr).expect("bind a relative path");

    assert!(
        absolute.exists(),
        "a relative path must resolve against the process working directory, \
         so the file should appear at {}",
        absolute.display()
    );
    // The address itself is reported back exactly as given — the platform
    // stores the bytes, not the resolution.
    assert_eq!(listener.local_addr().as_pathname(), Some(Path::new(&name)));
}

/// Binding into a directory that does not exist fails — with a code whose
/// *name* is actively misleading.
///
/// This is one the brief that started this work got wrong, and it is worth
/// pinning precisely for that reason. The number is 10050, which is
/// **`WSAENETDOWN`** ("a socket operation encountered a dead network"), not
/// `WSAEADDRNOTAVAIL` (10049) as the name would suggest — and there is, of
/// course, no network involved in an `AF_UNIX` bind at all.
///
/// The crate does not invent a friendlier classification for it. Mapping
/// `WSAENETDOWN` to something Unix-flavoured would change what TCP reports for
/// the same code, which is a separate decision from this one; and mapping it
/// to `AddressNotAvailable` would be asserting a fact about the address that
/// the platform did not state. So it surfaces as
/// [`SocketError::Win32`] with the raw code intact, and this test pins both
/// halves — the code, so a platform change is noticed, and the classification,
/// so a caller can rely on being able to see the code at all.
#[test]
fn binding_into_a_missing_directory_fails_with_the_raw_platform_code() {
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));

    let mut path = std::env::temp_dir();
    path.push(format!("winasio-no-such-dir-{}", std::process::id()));
    path.push("x.sock");
    assert!(!path.exists());

    let addr = UnixSocketAddr::from_pathname(&path).expect("build");
    let err = UnixListener::bind(&proactor, &addr).expect_err("the directory does not exist");

    let SocketError::Win32(raw) = &err else {
        panic!(
            "a missing directory is left unclassified so the caller can see the \
             real code; got {err:?}"
        );
    };
    assert_eq!(
        raw.code().0 as u32 & 0xFFFF,
        10050,
        "measured: WSAENETDOWN (10050), despite the name. Not WSAEADDRNOTAVAIL \
         (10049). If this changed, the module docs saying so are now wrong."
    );
}

/// An over-long path is refused at construction, before any syscall.
///
/// The rejection carries the measured length and the limit, so a caller can
/// see by how much. Truncating instead would bind a *different*, valid path,
/// which is the worst available outcome.
#[test]
fn an_over_long_path_is_refused_before_any_syscall() {
    let long = format!("C:\\{}", "x".repeat(UNIX_PATH_MAX));
    let err = UnixSocketAddr::from_pathname(&long).expect_err("too long for sun_path");
    assert!(
        matches!(err, UnixSocketAddrError::PathTooLong { len, max }
            if len == long.len() && max == UNIX_PATH_MAX),
        "got {err:?}"
    );
    // And the message names the numbers, not just the category.
    let text = err.to_string();
    assert!(text.contains(&long.len().to_string()), "message: {text}");
    assert!(text.contains(&UNIX_PATH_MAX.to_string()), "message: {text}");
}

// ---------------------------------------------------------------------------
// U11 — leak checks
// ---------------------------------------------------------------------------

/// A completed exchange leaks no sockets.
///
/// `live_sockets()` is process-global, which is why the whole binary
/// serialises on `socket_guard()`.
#[test]
fn a_completed_exchange_leaks_no_sockets() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("no_socket_leak");
    let before = live_sockets();

    {
        let proactor = Rc::new(Proactor::new().expect("proactor"));
        let addr = path.addr();
        let listener = UnixListener::bind(&proactor, &addr).expect("bind");
        drive_proactor(&proactor, async {
            let (accepted, connected) =
                futures_join(listener.accept(), UnixStream::connect(&proactor, &addr)).await;
            let (server, _) = accepted.expect("accept");
            let client = connected.expect("connect");
            let (sent, _, _) = client.write_all(b"x".to_vec()).await.into_parts();
            sent.expect("write");
            let (result, _, n) = server.read_exact(vec![0u8; 1]).await.into_parts();
            result.expect("read");
            assert_eq!(n, 1);
        });
    }

    assert_eq!(
        live_sockets(),
        before,
        "every socket in the exchange — listener, accepted, connected — must be closed"
    );
}

/// Dropping an accept future leaks no socket.
///
/// `AcceptEx` needs a socket created up front, so a dropped accept has one to
/// account for. The proactor is polled until the cancelled operation's record
/// is reclaimed, which is when the socket can finally close.
#[test]
fn dropping_an_accept_future_leaks_no_socket() {
    let _guard = winasio::net::socket_guard();
    let path = SocketPath::new("dropped_accept");
    let before = live_sockets();

    {
        let proactor = Rc::new(Proactor::new().expect("proactor"));
        let listener = UnixListener::bind(&proactor, &path.addr()).expect("bind");

        {
            let mut accepting = Box::pin(listener.accept());
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(
                accepting.as_mut().poll(&mut cx).is_pending(),
                "an accept with no client waiting must be pending, or this test \
                 drops a future that already resolved and proves nothing"
            );
            assert_eq!(
                live_sockets(),
                before + 2,
                "the listener and the half-built accepted socket"
            );
        }

        // Drive until the cancellation is reclaimed.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while live_sockets() > before + 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the cancelled accept never released its socket"
            );
            let _ = proactor.poll(Some(Duration::from_millis(5)));
        }
    }

    assert_eq!(live_sockets(), before);
}

// ---------------------------------------------------------------------------
// U12 — the exhaustive-match proof
// ---------------------------------------------------------------------------

/// `UnixSocketAddrError` is matchable exhaustively from outside the crate.
///
/// `#[non_exhaustive]` has no effect within the defining crate, so this proof
/// has to live here. It is the observable half of a deliberate decision: the
/// ways a path can fail to fit a fixed 108-byte UTF-8 field are closed by the
/// platform, so callers are allowed to match all of them and get a compile
/// error if that ever stops being true — rather than being forced into a
/// `_ =>` arm that would silently swallow a new case.
#[test]
fn the_address_error_can_be_matched_exhaustively_from_outside_the_crate() {
    fn describe(e: UnixSocketAddrError) -> &'static str {
        match e {
            UnixSocketAddrError::PathTooLong { .. } => "too long",
            UnixSocketAddrError::NotUtf8 => "not utf-8",
            UnixSocketAddrError::InteriorNul { .. } => "interior nul",
        }
    }

    assert_eq!(
        describe(UnixSocketAddrError::PathTooLong {
            len: 200,
            max: UNIX_PATH_MAX
        }),
        "too long"
    );
    assert_eq!(describe(UnixSocketAddrError::NotUtf8), "not utf-8");
    assert_eq!(
        describe(UnixSocketAddrError::InteriorNul { position: 3 }),
        "interior nul"
    );
}
