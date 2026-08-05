// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! TCP socket integration tests.
//!
//! Every test binds `:0` and reads the real port back. Nothing here hard-codes
//! a port: cargo runs integration binaries in parallel, and a fixed port would
//! make this suite fail depending on what else is running.
//!
//! Cases where the *backend's* teardown or completion delivery actually differ
//! get both an `_own_port` and a `_thread_pool` variant driven by one body, in
//! the style of `backends.rs`. The rest exercise classification, address
//! handling or option plumbing, which is backend-independent, and run on
//! `Proactor` alone to keep this binary's runtime inside the flakiness gate.

mod common;

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use winasio::iocp::{OpResult, Proactor, ThreadPool};
use winasio::net::{
    live_sockets, ReadOutcome, Shutdown, SocketError, TcpListener, TcpListenerOptions, TcpStream,
};

use common::drive_proactor;

/// A loopback address with an ephemeral port.
fn v4_any() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

fn v6_any() -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)
}

/// A blocking `std` client, run on a scoped thread.
///
/// The suite deliberately drives one side with `std::net` rather than pairing
/// two `winasio` sockets everywhere: if both ends were this crate's, a bug that
/// mis-encoded an address or mis-set an option could cancel out and the test
/// would still pass.
mod client {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};

    pub fn connect(addr: SocketAddr) -> TcpStream {
        TcpStream::connect(addr).expect("client connects")
    }

    pub fn send(stream: &mut TcpStream, bytes: &[u8]) {
        stream.write_all(bytes).expect("client writes");
        stream.flush().expect("client flushes");
    }

    /// Turn the next close into an RST rather than a FIN.
    pub fn set_abortive_close(stream: &TcpStream) {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::Networking::WinSock::{
            setsockopt, LINGER, SOCKET, SOL_SOCKET, SO_LINGER,
        };

        let linger = LINGER {
            l_onoff: 1,
            l_linger: 0,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&linger).cast::<u8>(),
                std::mem::size_of::<LINGER>(),
            )
        };
        // SAFETY: the socket is alive for the call and the option buffer is a
        // live `LINGER` of exactly the size Winsock expects.
        let rc = unsafe {
            setsockopt(
                SOCKET(stream.as_raw_socket() as usize),
                SOL_SOCKET,
                SO_LINGER,
                Some(bytes),
            )
        };
        assert_eq!(rc, 0, "setting SO_LINGER should succeed");
    }

    pub fn recv_exact(stream: &mut TcpStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        stream.read_exact(&mut buf).expect("client reads");
        buf
    }
}

// ---------------------------------------------------------------------------
// T1
// ---------------------------------------------------------------------------

/// T1 — FR-001, FR-008.
#[test]
fn listener_binds_and_reports_its_ephemeral_port() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");

    let local = listener.local_addr();
    assert!(local.is_ipv4());
    assert_ne!(
        local.port(),
        0,
        "binding port 0 must report back the port the system chose, \
         or a caller has no way to tell a client where to connect"
    );
    assert_eq!(local.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
}

// ---------------------------------------------------------------------------
// T2 — both backends
// ---------------------------------------------------------------------------

/// T2 — FR-003, FR-010, on the caller-driven backend.
#[test]
fn accept_and_connect_complete_on_own_port() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || client::connect(addr));

    let (stream, peer) = drive_proactor(&proactor, listener.accept()).expect("accept");
    assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(
        stream.peer_addr().expect("peer_addr"),
        peer,
        "the address reported by the accept and by the socket must agree"
    );

    drop(handle.join().expect("client thread"));
}

/// T2 — FR-003, FR-010, on the system thread pool.
#[test]
fn accept_and_connect_complete_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || client::connect(addr));

    let (stream, peer) = common::block_on(listener.accept()).expect("accept");
    assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(stream.peer_addr().expect("peer_addr"), peer);

    drop(handle.join().expect("client thread"));
}

/// T2b — FR-021, FR-022, `ConnectEx` on the system thread pool.
///
/// The other `ConnectEx` tests all drive a caller-owned `Proactor`. That is the
/// backend where the completion is dequeued by the same thread that submitted
/// it, so a `ConnectEx` that only ever completed inline, or one whose
/// completion was delivered on the wrong registration, could still pass. The
/// thread pool delivers on a pool thread, which is a genuinely different path
/// through `finish` and the `SO_UPDATE_CONNECT_CONTEXT` fix-up.
///
/// Both ends are crate sockets here — the connect *and* the accept — so the
/// whole handshake runs on pool threads.
#[test]
fn connect_completes_on_the_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let accepted = std::thread::spawn(move || {
        let (stream, peer) = common::block_on(listener.accept()).expect("accept");
        (stream, peer)
    });

    let client = common::block_on(TcpStream::connect(&ThreadPool, addr)).expect("connect");
    let (server, peer) = accepted.join().expect("accept thread");

    // `SO_UPDATE_CONNECT_CONTEXT` is what makes these queryable at all; before
    // it, `getsockname` on a `ConnectEx` socket fails with `WSAEINVAL`. So this
    // is not a redundant address assertion — it is the only cheap check that
    // the fix-up ran on the pool path.
    assert_eq!(client.peer_addr().expect("client peer_addr"), addr);
    assert_eq!(client.local_addr().expect("client local_addr"), peer);

    // And it must actually carry data, in both directions.
    let OpResult(written, _) = common::block_on(client.write(b"ping".to_vec()));
    assert_eq!(written.expect("client write"), 4);
    let OpResult(outcome, buf) = common::block_on(server.read(vec![0u8; 8]));
    assert_eq!(outcome.expect("server read"), ReadOutcome::Bytes(4));
    assert_eq!(&buf[..4], b"ping");
}

// ---------------------------------------------------------------------------
// T3 — both backends
// ---------------------------------------------------------------------------

/// T3 — FR-012, FR-013, on the caller-driven backend.
#[test]
fn round_trip_read_write_on_own_port() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || {
        let mut c = client::connect(addr);
        client::send(&mut c, b"ping");
        let echoed = client::recv_exact(&mut c, 4);
        assert_eq!(&echoed, b"pong");
    });

    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");

    let OpResult(outcome, buf) = drive_proactor(&proactor, stream.read(vec![0u8; 16]));
    assert_eq!(outcome.expect("read"), ReadOutcome::Bytes(4));
    assert_eq!(&buf[..4], b"ping");

    let OpResult(written, _) = drive_proactor(&proactor, stream.write(b"pong".to_vec()));
    assert_eq!(written.expect("write"), 4);

    handle.join().expect("client thread");
}

/// T3 — FR-012, FR-013, on the system thread pool.
#[test]
fn round_trip_read_write_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || {
        let mut c = client::connect(addr);
        client::send(&mut c, b"ping");
        let echoed = client::recv_exact(&mut c, 4);
        assert_eq!(&echoed, b"pong");
    });

    let (stream, _) = common::block_on(listener.accept()).expect("accept");

    let OpResult(outcome, buf) = common::block_on(stream.read(vec![0u8; 16]));
    assert_eq!(outcome.expect("read"), ReadOutcome::Bytes(4));
    assert_eq!(&buf[..4], b"ping");

    let OpResult(written, _) = common::block_on(stream.write(b"pong".to_vec()));
    assert_eq!(written.expect("write"), 4);

    handle.join().expect("client thread");
}

// ---------------------------------------------------------------------------
// T4
// ---------------------------------------------------------------------------

/// T4 — FR-005, FR-011, M7, M8, M26.
///
/// The `peer_addr` assertions are the observable consequence of
/// `SO_UPDATE_ACCEPT_CONTEXT` and `SO_UPDATE_CONNECT_CONTEXT`. Measured (M26):
/// without the accept-side update, `getpeername` on the accepted socket fails
/// with `WSAENOTCONN`, so dropping the update would surface here rather than as
/// a mysterious failure much later.
#[test]
fn peer_and_local_addresses_are_reported() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let listen_addr = listener.local_addr();

    // Two proactors, polled alternately, with each future dropped as soon as it
    // resolves — a completed future must not be polled again.
    //
    // Not because the handshake needs both sides running: TCP completes it into
    // the listener's backlog whether or not anything is accepting. It is that
    // each proactor only makes progress when *its* owner polls it, so a single
    // caller awaiting the connect to completion first would never drive the
    // accept, and vice versa.
    let connector = Rc::new(Proactor::new().expect("connector proactor"));
    let mut connecting = Box::pin(TcpStream::connect(&connector, listen_addr));
    let mut accepting = Box::pin(listener.accept());

    let mut accepted = None;
    let mut connected = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while accepted.is_none() || connected.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the connect and accept never met"
        );
        if accepted.is_none() {
            accepted = poll_once(&proactor, accepting.as_mut());
        }
        if connected.is_none() {
            connected = poll_once(&connector, connecting.as_mut());
        }
    }

    let (server, peer) = accepted.expect("accepted").expect("accept");
    let client = connected.expect("connected").expect("connect");

    assert_eq!(
        server.peer_addr().expect("server peer_addr"),
        client.local_addr().expect("client local_addr"),
        "each side's view of the other must match"
    );
    assert_eq!(
        client.peer_addr().expect("client peer_addr"),
        listen_addr,
        "the client's peer is the address it dialled"
    );
    assert_eq!(server.peer_addr().expect("server peer_addr"), peer);
    assert_eq!(
        server.local_addr().expect("server local_addr").port(),
        listen_addr.port(),
        "an accepted socket inherits the listener's local port"
    );
}

/// Poll a future once, driving the proactor if it is not ready.
fn poll_once<F: std::future::Future>(
    proactor: &Proactor,
    fut: std::pin::Pin<&mut F>,
) -> Option<F::Output> {
    use std::task::{Context, Poll};
    let mut cx = Context::from_waker(std::task::Waker::noop());
    match fut.poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => {
            let _ = proactor.poll(Some(Duration::from_millis(5)));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// T5 — both backends
// ---------------------------------------------------------------------------

/// T5 — FR-009. Eight accepts outstanding, eight clients.
#[test]
fn concurrent_accepts_all_complete_on_own_port() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    const N: usize = 8;
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    // Constructing the futures starts nothing — `accept` is an `async fn`, so no
    // socket exists and no `AcceptEx` is issued until the first poll.
    let baseline = winasio::iocp::live_operations();
    let mut pending: Vec<_> = (0..N).map(|_| Box::pin(listener.accept())).collect();

    // Post all eight *before* any client connects, so none of them can resolve
    // yet and the in-flight count is unambiguous. This is the assertion the
    // test turns on: an implementation that could hold only one accept at a
    // time would read 1 here. Without it the test would pass just as happily on
    // eight sequential accepts.
    for fut in pending.iter_mut() {
        assert!(
            poll_once(&proactor, fut.as_mut()).is_none(),
            "no client has connected, so no accept can resolve"
        );
    }
    assert_eq!(
        winasio::iocp::live_operations() - baseline,
        N,
        "all {N} accepts must be in flight at once"
    );

    let clients =
        std::thread::spawn(move || (0..N).map(|_| client::connect(addr)).collect::<Vec<_>>());

    let mut done = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !pending.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out with {} accepts outstanding",
            pending.len()
        );
        pending.retain_mut(|fut| match poll_once(&proactor, fut.as_mut()) {
            Some(result) => {
                done.push(result.expect("accept"));
                false
            }
            None => true,
        });
    }

    assert_eq!(done.len(), N);
    let mut ports: Vec<u16> = done.iter().map(|(_, peer)| peer.port()).collect();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), N, "each accept must yield a distinct peer");

    drop(clients.join().expect("client thread"));
}

/// T5 — FR-009, on the system thread pool.
#[test]
fn concurrent_accepts_all_complete_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    const N: usize = 8;
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    // `accept` is an `async fn`, so these futures are inert until polled:
    // awaiting them one at a time would submit the second `AcceptEx` only after
    // the first had resolved, and a listener that could hold just one accept in
    // flight would pass. They are therefore all polled once, with no client yet
    // connected — so none can resolve — and the in-flight count is checked
    // before anything is allowed to complete.
    let baseline = winasio::iocp::live_operations();
    let mut pending: Vec<_> = (0..N).map(|_| Box::pin(listener.accept())).collect();
    common::block_on(std::future::poll_fn(|cx| {
        for fut in pending.iter_mut() {
            assert!(
                fut.as_mut().poll(cx).is_pending(),
                "no client has connected, so no accept can resolve"
            );
        }
        Poll::Ready(())
    }));
    assert_eq!(
        winasio::iocp::live_operations() - baseline,
        N,
        "all {N} accepts must be in flight at once"
    );

    let clients =
        std::thread::spawn(move || (0..N).map(|_| client::connect(addr)).collect::<Vec<_>>());

    let accepted = common::block_on(async {
        let mut out = Vec::new();
        std::future::poll_fn(|cx| {
            pending.retain_mut(|fut| match fut.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    out.push(result.expect("accept"));
                    false
                }
                Poll::Pending => true,
            });
            if pending.is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        out
    });

    assert_eq!(accepted.len(), N);
    let mut ports: Vec<u16> = accepted.iter().map(|(_, peer)| peer.port()).collect();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), N, "each accept must yield a distinct peer");
    drop(clients.join().expect("client thread"));
}

// ---------------------------------------------------------------------------
// T6
// ---------------------------------------------------------------------------

/// T6 — FR-029.
///
/// Reached from `ERROR_CONNECTION_REFUSED` (1225) on the completion packet, not
/// `WSAECONNREFUSED`: `ConnectEx` on a registered socket reports the refusal
/// through the port, having already been through `RtlNtStatusToDosError`.
/// Classifying only the Winsock spelling would leave this a `Win32` error.
#[test]
fn connect_to_a_closed_port_is_refused() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));

    // Bind and immediately drop a listener to obtain a port nothing is on.
    let dead = {
        let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
        listener.local_addr()
    };

    let result = drive_proactor(&proactor, TcpStream::connect(&proactor, dead));
    match result {
        Err(SocketError::ConnectionRefused) => {}
        Err(other) => panic!("expected ConnectionRefused, got {other:?}"),
        Ok(_) => panic!("connected to a port with no listener"),
    }
}

// ---------------------------------------------------------------------------
// T7 — both backends
// ---------------------------------------------------------------------------

/// T7 — FR-018. A graceful FIN must report `ClosedPeer`, not `Bytes(0)`.
#[test]
fn read_after_peer_closes_reports_closed_peer_on_own_port() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || {
        let c = client::connect(addr);
        // Close without sending: the server's first read sees only the FIN.
        drop(c);
    });

    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");
    let OpResult(outcome, _) = drive_proactor(&proactor, stream.read(vec![0u8; 16]));
    assert_eq!(
        outcome.expect("read"),
        ReadOutcome::ClosedPeer,
        "a graceful close reported as Bytes(0) would spin read_to_end forever"
    );

    handle.join().expect("client thread");
}

/// T7 — FR-018, on the system thread pool.
#[test]
fn read_after_peer_closes_reports_closed_peer_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || drop(client::connect(addr)));

    let (stream, _) = common::block_on(listener.accept()).expect("accept");
    let OpResult(outcome, _) = common::block_on(stream.read(vec![0u8; 16]));
    assert_eq!(outcome.expect("read"), ReadOutcome::ClosedPeer);

    handle.join().expect("client thread");
}

// ---------------------------------------------------------------------------
// T8
// ---------------------------------------------------------------------------

/// T8 — the non-termination hazard.
///
/// `read_to_end` loops until the stream ends. If a graceful close were reported
/// as `Bytes(0)` it would loop forever; the deadline inside `drive_proactor`
/// turns that into a failure rather than a hung test binary.
#[test]
fn read_to_end_terminates_when_the_peer_closes() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || {
        let mut c = client::connect(addr);
        client::send(&mut c, b"hello world");
        drop(c);
    });

    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");
    let result = drive_proactor(&proactor, stream.read_to_end(Vec::new()));
    let (outcome, buffer, transferred) = result.into_parts();
    outcome.expect("read_to_end");
    assert_eq!(transferred, 11);
    assert_eq!(&buffer, b"hello world");

    handle.join().expect("client thread");
}

// ---------------------------------------------------------------------------
// T9
// ---------------------------------------------------------------------------

/// T9 — FR-018, FR-019.
///
/// An abrupt loss of the connection (RST) must be reported as an **error**,
/// not as [`ReadOutcome::ClosedPeer`].
///
/// This is the distinction the crate is careful about. A graceful close is a
/// FIN: the peer said "I have sent everything", every byte it sent arrived,
/// and stopping is correct. A reset says the opposite — the connection died,
/// and whatever was in flight is gone. Both surface at a `WSARecv` as "no more
/// data", so it is tempting to fold them together, and the earlier
/// implementation did. The consequence was silent data loss: `read_to_end`
/// returned `Ok(())` with a truncated buffer and no way for the caller to
/// tell. See `read_to_end_after_a_reset_fails_rather_than_truncating` below,
/// which is the test that would have caught it.
///
/// The connection is accepted *before* the reset is triggered. Resetting first
/// makes the `AcceptEx` itself fail with `ConnectionAborted` — which is correct
/// behaviour, but tests the accept path rather than the read path.
#[test]
fn read_after_peer_resets_reports_an_error() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let c = client::connect(addr);
        // `TcpStream::set_linger` is still unstable, so set `SO_LINGER`
        // directly. A zero timeout makes the close abortive.
        client::set_abortive_close(&c);
        rx.recv().expect("the server signals once it has accepted");
        drop(c);
    });

    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");
    tx.send(()).expect("signal the client to reset");
    handle.join().expect("client thread");

    // The reset may not have arrived yet; read until it does, bounded.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let err = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the reset never turned into a read error"
        );
        let OpResult(outcome, _) = drive_proactor(&proactor, stream.read(vec![0u8; 16]));
        match outcome {
            Err(e) => break e,
            Ok(ReadOutcome::Bytes(0)) => continue,
            Ok(ReadOutcome::ClosedPeer) => {
                panic!("a reset must be an error: reporting it as ClosedPeer tells the caller the stream ended cleanly")
            }
            Ok(other) => panic!("unexpected outcome {other:?} from a reset connection"),
        }
    };
    assert!(
        matches!(
            winasio::net::SocketError::from_win32(err),
            winasio::net::SocketError::ConnectionAborted
        ),
        "a reset classifies as a lost connection, so callers can tell it from an unrelated fault"
    );
}

/// T9b — FR-018, FR-019, and the reason FR-018 draws the line where it does.
///
/// `read_to_end` on a connection that is reset mid-stream must **fail**. It
/// must not return `Ok(())` with a short buffer.
///
/// This is the falsifiable form of the distinction above, and it is the test
/// that matters: `read_after_peer_resets_reports_an_error` checks the
/// classifier in isolation, but a caller never sees the classifier — it sees
/// `read_to_end`. If a reset were treated as a graceful close, this test would
/// observe a successful call that quietly dropped the tail of the stream,
/// which is exactly the failure mode that is impossible to debug in the field.
#[test]
fn read_to_end_after_a_reset_fails_rather_than_truncating() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let mut c = client::connect(addr);
        client::set_abortive_close(&c);
        // Send a prefix, then reset. A caller reading to the end must not be
        // told that this prefix was the whole message.
        client::send(&mut c, b"prefix");
        rx.recv().expect("the server signals once it has accepted");
        drop(c);
    });

    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");
    tx.send(()).expect("signal the client to reset");
    handle.join().expect("client thread");

    let transfer = drive_proactor(&proactor, stream.read_to_end(Vec::new()));
    if transfer.result.is_ok() {
        panic!(
            "read_to_end returned Ok on a reset connection, silently truncating the stream to {} bytes",
            transfer.buffer.len()
        );
    }
}

// ---------------------------------------------------------------------------
// T10 — Proactor-only by construction
// ---------------------------------------------------------------------------

/// T10 — M4. An inline send must queue no completion packet.
///
/// **`Proactor`-only by construction, not by omission.**
/// `unclaimed_completions()` and `pending_count()` are inherent `Proactor`
/// methods with no thread-pool equivalent, and there is no way to observe an
/// unclaimed packet on the pool: its completions are dispatched by the system.
///
/// The instrument matters. `Proactor::poll` returns the number of completions
/// *delivered*, not dequeued — a packet for an operation that already resolved
/// inline is silently discarded — so a test that asserted `poll() == 0` alone
/// would pass whether or not the packet existed. `unclaimed_completions()`
/// counts the packets that were dequeued and had nothing to deliver to, which
/// is exactly the quantity `SetFileCompletionNotificationModes` suppresses.
///
/// Falsified: with `skip_notification_on_inline_success` stubbed out, this test
/// fails. See `.paw/work/tcp-sockets/reviews/falsification.md`.
#[test]
fn an_inline_send_queues_no_completion_packet() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || {
        let mut c = client::connect(addr);
        // Read the payload so the connection stays open until the test is done.
        let _ = client::recv_exact(&mut c, 4);
        c
    });

    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");

    // Drain anything the accept left behind before measuring.
    while proactor.poll(Some(Duration::from_millis(5))).expect("poll") > 0 {}
    let before = proactor.unclaimed_completions();

    // A small send on a fresh connection completes inline: it fits in the
    // socket's send buffer and never has to wait for the network.
    //
    // That it *did* complete inline is asserted, not assumed. `write` submits
    // eagerly, so one poll resolving proves the inline path was taken. Without
    // this guard the test would still pass if the send went pending — the
    // packet would be claimed by the normal path, leaving `unclaimed` and
    // `pending_count` at zero — which is precisely the "passes for the wrong
    // reason forever" failure the rest of this test exists to avoid.
    let mut writing = Box::pin(stream.write(b"ping".to_vec()));
    let OpResult(written, _) = poll_once(&proactor, writing.as_mut())
        .expect("a 4-byte send on a fresh connection must complete inline");
    assert_eq!(written.expect("write"), 4);

    // Give a packet, if one were queued, every chance to arrive.
    let delivered = proactor
        .poll(Some(Duration::from_millis(200)))
        .expect("poll");

    assert_eq!(
        proactor.unclaimed_completions(),
        before,
        "an inline success must queue no packet; one was dequeued with nothing \
         to deliver it to, which means the skip mode was not applied"
    );
    assert_eq!(
        proactor.pending_count(),
        0,
        "no operation should still be outstanding"
    );
    assert_eq!(delivered, 0, "nothing was waiting for a completion");

    drop(handle.join().expect("client thread"));
}

// ---------------------------------------------------------------------------
// T11
// ---------------------------------------------------------------------------

/// T11 — FR-026, FR-029.
///
/// Asserts the `SkipModeUnsupported` *variant*, not merely that an error
/// occurred: the whole point of the variant is that it is distinguishable from
/// `AlreadyRegistered`, which carries the same `ERROR_INVALID_PARAMETER`.
#[test]
fn a_socket_refusing_the_skip_mode_is_refused_registration() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    use winasio::iocp::RefuseSkipMode;

    let proactor = Rc::new(Proactor::new().expect("proactor"));

    // The connect path: registration happens before the connect is submitted.
    {
        let _refuse = RefuseSkipMode::new();
        let result = drive_proactor(&proactor, TcpStream::connect(&proactor, v4_any()));
        match result {
            Err(SocketError::SkipModeUnsupported(_)) => {}
            Err(other) => panic!("expected SkipModeUnsupported, got {other:?}"),
            Ok(_) => panic!("registration should have been refused"),
        }
    }

    // The listener path: `bind_with` registers the listening socket.
    {
        let _refuse = RefuseSkipMode::new();
        match TcpListener::bind(&proactor, v4_any()) {
            Err(SocketError::SkipModeUnsupported(_)) => {}
            Err(other) => panic!("expected SkipModeUnsupported, got {other:?}"),
            Ok(_) => panic!("registration should have been refused"),
        }
    }

    // The accept path registers a *second* socket after the operation
    // completes, which is a different call site from either of the above. It is
    // also the only one where the failure happens with a socket already in
    // hand: FR-027 makes registration the last fallible step precisely so that
    // this socket is closed on the way out. Reporting the right error while
    // leaking the accepted socket would satisfy the match below and still be a
    // bug, so the socket count is asserted too.
    {
        let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
        let addr = listener.local_addr();
        let handle = std::thread::spawn(move || client::connect(addr));

        // Taken after the listener exists, so it counts only the accepted
        // socket that the failing registration is supposed to clean up.
        let sockets_before = winasio::net::live_sockets();
        let result = {
            let _refuse = RefuseSkipMode::new();
            drive_proactor(&proactor, listener.accept())
        };
        match result {
            Err(SocketError::SkipModeUnsupported(_)) => {}
            Err(other) => panic!("expected SkipModeUnsupported from accept, got {other:?}"),
            Ok(_) => panic!("the accepted socket's registration should have been refused"),
        }
        assert_eq!(
            winasio::net::live_sockets(),
            sockets_before,
            "a refused registration must close the socket AcceptEx already created"
        );
        drop(handle.join().expect("client thread"));
    }
}

// ---------------------------------------------------------------------------
// T12 — both backends
// ---------------------------------------------------------------------------

/// T12 — FR-007. A dropped accept future must leak no socket.
///
/// `live_sockets()` is the direct instrument: it counts the sockets this crate
/// owns. `live_operations()` is a secondary signal — an operation that outlived
/// its cancellation would still be holding a `Socket` clone, so the two should
/// return to baseline together.
#[test]
fn dropping_an_accept_future_leaks_no_socket_on_own_port() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));

    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let baseline = live_sockets();
    let ops_baseline = winasio::iocp::live_operations();

    {
        let mut accepting = Box::pin(listener.accept());
        // Start it — the accept socket exists once `operate` has run.
        assert!(poll_once(&proactor, accepting.as_mut()).is_none());
        assert!(
            live_sockets() > baseline,
            "the accept must have created a socket, or this test proves nothing"
        );
    }

    // The cancellation packet has to be dequeued before the operation, and with
    // it the accept socket, is released.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while live_sockets() > baseline || winasio::iocp::live_operations() > ops_baseline {
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped accept leaked: {} sockets and {} operations above baseline",
            live_sockets() - baseline,
            winasio::iocp::live_operations() - ops_baseline
        );
        let _ = proactor.poll(Some(Duration::from_millis(5)));
    }
}

/// T12 — FR-007, on the system thread pool.
#[test]
fn dropping_an_accept_future_leaks_no_socket_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();

    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let baseline = live_sockets();

    {
        let mut accepting = Box::pin(listener.accept());
        use std::future::Future;
        use std::task::{Context, Poll};
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(accepting.as_mut().poll(&mut cx), Poll::Pending));
        assert!(live_sockets() > baseline);
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while live_sockets() > baseline {
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped accept leaked {} sockets",
            live_sockets() - baseline
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------
// T13 — both backends
// ---------------------------------------------------------------------------

/// T13 — FR-014, all three directions.
#[test]
fn shutdown_is_seen_by_the_peer_on_own_port() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();
    let addr2 = addr;

    let handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut c = client::connect(addr);
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).expect("client read to end");
        buf
    });

    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");
    let OpResult(written, _) = drive_proactor(&proactor, stream.write(b"bye".to_vec()));
    assert_eq!(written.expect("write"), 3);

    // `Shutdown::Write` sends FIN, which is what ends the client's read.
    stream.shutdown(Shutdown::Write).expect("shutdown write");
    assert_eq!(handle.join().expect("client thread"), b"bye");

    // `Shutdown::Read` is asserted by its effect, not merely by returning
    // `Ok`. Accepting the call proves nothing: mapping every direction to
    // `SD_SEND`, or making `Read` a no-op, would leave that passing. After
    // `SD_RECEIVE` the socket's receive side is torn down, so a subsequent
    // read must report the peer as closed rather than blocking forever.
    let peer = std::thread::spawn(move || {
        let mut c = client::connect(addr2);
        client::send(&mut c, b"unread");
        c
    });
    let (other, _) = drive_proactor(&proactor, listener.accept()).expect("accept");
    other.shutdown(Shutdown::Read).expect("shutdown read");
    let OpResult(outcome, _) = drive_proactor(&proactor, other.read(vec![0u8; 16]));
    // Measured: Windows fails the read with `WSAECONNABORTED` (0x80072745)
    // rather than completing it with zero bytes. Note what that means for the
    // classifier — this is an *error*, not `ClosedPeer`, and rightly so: the
    // receive side was torn down locally, the peer did nothing, and there are
    // six unread bytes sitting in the buffer that the caller will never see.
    // Calling that a graceful close would tell `read_to_end` the stream ended
    // cleanly.
    //
    // The assertion is on the failure, not on `Ok(_)`: a `Read` shutdown that
    // was silently a no-op would return the six bytes and fail here.
    let err = outcome.expect_err("a read after SD_RECEIVE must not succeed");
    assert!(
        matches!(
            winasio::net::SocketError::from_win32(err),
            winasio::net::SocketError::ConnectionAborted
        ),
        "a read after SD_RECEIVE reports the connection as aborted"
    );
    drop(peer.join().expect("peer thread"));

    // `Both` is accepted on an already half-closed socket.
    stream.shutdown(Shutdown::Both).expect("shutdown both");
}

/// T13 — FR-014, on the system thread pool.
#[test]
fn shutdown_is_seen_by_the_peer_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut c = client::connect(addr);
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).expect("client read to end");
        buf
    });

    let (stream, _) = common::block_on(listener.accept()).expect("accept");
    let OpResult(written, _) = common::block_on(stream.write(b"bye".to_vec()));
    assert_eq!(written.expect("write"), 3);

    stream.shutdown(Shutdown::Write).expect("shutdown write");
    assert_eq!(handle.join().expect("client thread"), b"bye");
}

// ---------------------------------------------------------------------------
// T14 — both backends
// ---------------------------------------------------------------------------

const BIG: usize = 1024 * 1024;

/// T14 — FR-016. A payload far larger than any socket buffer.
///
/// One MiB is chosen to be several times the default send buffer, so the
/// transfer certainly needs more than one `WSASend` and more than one
/// `WSARecv`. A helper that returned after its first partial transfer would
/// fail here and nowhere else.
#[test]
fn write_all_and_read_exact_move_a_large_payload_on_own_port() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let payload: Vec<u8> = (0..BIG).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();

    let handle = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let mut c = client::connect(addr);
        let mut buf = vec![0u8; BIG];
        c.read_exact(&mut buf).expect("client reads the payload");
        c.write_all(&buf).expect("client echoes the payload");
        c.flush().expect("flush");
        buf
    });

    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");
    let result = drive_proactor(&proactor, stream.write_all(payload));
    let (outcome, _, transferred) = result.into_parts();
    outcome.expect("write_all");
    assert_eq!(transferred, BIG);

    // Read the echo back, so `read_exact` is exercised on this backend too.
    // Both helpers are resubmission loops and the two backends deliver
    // completions differently, which is exactly where a loop breaks.
    let result = drive_proactor(&proactor, stream.read_exact(vec![0u8; BIG]));
    let (outcome, buffer, transferred) = result.into_parts();
    outcome.expect("read_exact");
    assert_eq!(transferred, BIG);
    assert_eq!(buffer, expected);

    assert_eq!(handle.join().expect("client thread"), expected);
}

/// T14 — FR-016, receive side, on the system thread pool.
#[test]
fn write_all_and_read_exact_move_a_large_payload_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let payload: Vec<u8> = (0..BIG).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();

    let handle = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let mut c = client::connect(addr);
        c.write_all(&payload).expect("client writes the payload");
        c.flush().expect("flush");
        let mut echoed = vec![0u8; BIG];
        c.read_exact(&mut echoed).expect("client reads the echo");
        echoed
    });

    let (stream, _) = common::block_on(listener.accept()).expect("accept");
    let result = common::block_on(stream.read_exact(vec![0u8; BIG]));
    let (outcome, buffer, transferred) = result.into_parts();
    outcome.expect("read_exact");
    assert_eq!(transferred, BIG);
    assert_eq!(buffer, expected);

    // Echo it back, so `write_all` is exercised on this backend too.
    let result = common::block_on(stream.write_all(buffer));
    let (outcome, _, transferred) = result.into_parts();
    outcome.expect("write_all");
    assert_eq!(transferred, BIG);

    assert_eq!(handle.join().expect("client thread"), expected);
}

// ---------------------------------------------------------------------------
// T15 / T16
// ---------------------------------------------------------------------------

/// T15 — FR-017, the read half.
#[test]
fn a_dropped_read_future_returns_no_buffer_and_leaks_nothing() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || client::connect(addr));
    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");

    let baseline = winasio::iocp::live_operations();
    {
        // The peer sends nothing, so this read cannot complete.
        let mut reading = Box::pin(stream.read(vec![0u8; 64]));
        assert!(poll_once(&proactor, reading.as_mut()).is_none());
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while winasio::iocp::live_operations() > baseline {
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped read leaked {} operations",
            winasio::iocp::live_operations() - baseline
        );
        let _ = proactor.poll(Some(Duration::from_millis(5)));
    }

    drop(handle.join().expect("client thread"));
}

/// T16 — FR-017, the write half.
///
/// A write is harder to leave pending than a read: a small send completes
/// inline. The buffer is therefore large enough to overrun the socket's send
/// buffer with a peer that never reads.
#[test]
fn a_dropped_write_future_returns_no_buffer_and_leaks_nothing() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || client::connect(addr));
    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");

    let baseline = winasio::iocp::live_operations();
    {
        // The write that gets dropped must actually be *pending*. A send that
        // completes inline has nothing to cancel, so dropping it exercises none
        // of the teardown path and the drain loop below becomes a no-op that
        // passes unconditionally. Windows decides that on send-buffer space, so
        // rather than assume a large enough buffer pends, keep sending — and
        // letting each inline send stand — until one does not resolve. The peer
        // never reads, so the socket's send buffer fills and this terminates.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "no write ever pended, so the drop was never measured"
            );
            let mut writing = Box::pin(stream.write(vec![0u8; 1024 * 1024]));
            if poll_once(&proactor, writing.as_mut()).is_none() {
                // Pending: drop it here, which is the case under test.
                break;
            }
        }
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while winasio::iocp::live_operations() > baseline {
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped write leaked {} operations",
            winasio::iocp::live_operations() - baseline
        );
        let _ = proactor.poll(Some(Duration::from_millis(5)));
    }

    drop(handle.join().expect("client thread"));
}

/// T15 — FR-017, the read half, on the system thread pool.
///
/// Worth having separately from the own-port variant rather than assumed from
/// it: drop-time teardown is the largest behavioural difference between the two
/// backends. The thread pool's registration drop cancels and then waits for
/// callbacks to drain, while the proactor's returns immediately and leaves the
/// draining to the caller's `poll`. A leak that only the pool exhibits would be
/// invisible to the test above.
#[test]
fn a_dropped_read_future_returns_no_buffer_and_leaks_nothing_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || client::connect(addr));
    let (stream, _) = common::block_on(listener.accept()).expect("accept");

    let baseline = winasio::iocp::live_operations();
    {
        // The peer sends nothing, so this read cannot resolve.
        let mut reading = Box::pin(stream.read(vec![0u8; 64]));
        let polled = common::block_on(std::future::poll_fn(|cx| {
            Poll::Ready(reading.as_mut().poll(cx).is_ready())
        }));
        assert!(!polled, "a read with no data available must not resolve");
    }

    await_operations_drained(baseline, "read");
    drop(handle.join().expect("client thread"));
}

/// T16 — FR-017, the write half, on the system thread pool.
#[test]
fn a_dropped_write_future_returns_no_buffer_and_leaks_nothing_on_thread_pool() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let listener = TcpListener::bind(&ThreadPool, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || client::connect(addr));
    let (stream, _) = common::block_on(listener.accept()).expect("accept");

    let baseline = winasio::iocp::live_operations();
    {
        // As in the own-port variant: keep sending until one send genuinely
        // pends, and drop that one. Dropping an inline completion would measure
        // nothing.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "no write ever pended, so the drop was never measured"
            );
            let mut writing = Box::pin(stream.write(vec![0u8; 1024 * 1024]));
            let ready = common::block_on(std::future::poll_fn(|cx| {
                Poll::Ready(writing.as_mut().poll(cx).is_ready())
            }));
            if !ready {
                break;
            }
        }
    }

    await_operations_drained(baseline, "write");
    drop(handle.join().expect("client thread"));
}

/// Wait for the operation count to fall back to `baseline`.
///
/// Nothing is driven here: on the thread pool the cancellation callback runs on
/// a pool thread, so the test only has to wait for it. A leak shows up as the
/// count never coming back down.
fn await_operations_drained(baseline: usize, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while winasio::iocp::live_operations() > baseline {
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped {what} leaked {} operations",
            winasio::iocp::live_operations() - baseline
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ---------------------------------------------------------------------------
// T17 / T18
// ---------------------------------------------------------------------------

/// Whether this machine has a usable IPv6 loopback.
///
/// Decided with `std`, deliberately: the point is to skip only for a genuine
/// platform limitation, using an instrument that cannot be broken by the code
/// under test.
fn ipv6_loopback_available() -> bool {
    std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// T17 — FR-028, M27. IPv6 end to end.
#[test]
fn an_ipv6_listener_round_trips() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    // Only a machine genuinely without IPv6 may skip, and that is decided by
    // `std`, independently of the code under test. Catching every bind error
    // here would turn an address-conversion, `IPV6_V6ONLY`, registration or
    // bind regression into a silent pass of the mandatory IPv6 coverage.
    if !ipv6_loopback_available() {
        eprintln!("skipping: no IPv6 loopback on this machine");
        return;
    }
    let listener = TcpListener::bind(&proactor, v6_any()).expect("bind v6");
    let addr = listener.local_addr();
    assert!(addr.is_ipv6());

    let handle = std::thread::spawn(move || {
        let mut c = client::connect(addr);
        client::send(&mut c, b"v6");
        c
    });

    let (stream, peer) = drive_proactor(&proactor, listener.accept()).expect("accept");
    assert!(peer.is_ipv6());

    let OpResult(outcome, buf) = drive_proactor(&proactor, stream.read(vec![0u8; 8]));
    assert_eq!(outcome.expect("read"), ReadOutcome::Bytes(2));
    assert_eq!(&buf[..2], b"v6");

    drop(handle.join().expect("client thread"));
}

/// T18 — D10, FR-028, M28.
///
/// A dual-stack listener reports an IPv4 peer as a v4-mapped `SocketAddr::V6`.
/// The assertion is deliberately on the mapped form: un-mapping it in the crate
/// would discard the fact that the connection arrived on a v6 socket.
#[test]
fn a_dual_stack_listener_accepts_an_ipv4_client() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let mut options = TcpListenerOptions::new();
    options.only_v6(false);
    let unspecified = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0);
    if !ipv6_loopback_available() {
        eprintln!("skipping: no IPv6 loopback on this machine");
        return;
    }
    let listener =
        TcpListener::bind_with(&proactor, unspecified, &options).expect("bind dual-stack");
    let port = listener.local_addr().port();
    let v4_target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    let handle = std::thread::spawn(move || client::connect(v4_target));

    let (_stream, peer) = drive_proactor(&proactor, listener.accept()).expect("accept");
    let SocketAddr::V6(v6) = peer else {
        panic!("a dual-stack listener reports peers as V6, got {peer:?}");
    };
    assert_eq!(
        v6.ip().to_ipv4_mapped(),
        Some(Ipv4Addr::LOCALHOST),
        "an IPv4 client on a dual-stack listener is a v4-mapped V6 address"
    );

    drop(handle.join().expect("client thread"));
}

// ---------------------------------------------------------------------------
// T19 / T20
// ---------------------------------------------------------------------------

/// T19 — FR-002, smoke only.
///
/// Deliberately weak, and labelled so. Windows clamps and rounds the backlog
/// inside the provider, and exposes no way to read back the value it settled
/// on, so nothing observable here distinguishes `backlog(4)` from the default:
/// deleting the option's plumbing entirely would leave this test passing. It
/// checks that the option is accepted and does not break `bind`. The part that
/// *is* falsifiable — that the requested value reaches `listen`, including the
/// `i32::MAX` saturation — lives in the `backlog_argument` unit tests in
/// `net::listener`, which is where it can actually be observed.
#[test]
fn a_custom_backlog_is_accepted() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let mut options = TcpListenerOptions::new();
    options.backlog(4);
    let listener = TcpListener::bind_with(&proactor, v4_any(), &options).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || client::connect(addr));
    let (_stream, _peer) = drive_proactor(&proactor, listener.accept()).expect("accept");
    drop(handle.join().expect("client thread"));
}

/// T20 — FR-018's zero-length exception.
///
/// Measured in probe round 5, because the first version of this test was
/// written from prose that contradicted its own citation and duly timed out:
///
/// * on an **idle** connection a zero-length `WSARecv` goes pending (M30) and
///   completes with 0 bytes only when data arrives (M31);
/// * with data **already queued** it completes inline with 0 bytes (M33);
/// * and it is non-destructive either way — the bytes are still there for the
///   next receive (M32).
///
/// The exception matters in all three cases: what varies is when `Ok(0)`
/// appears, not whether it does. This test takes the deterministic one — data
/// already queued — so it cannot race.
#[test]
fn a_zero_length_read_reports_bytes_zero_not_closed_peer() {
    // The socket and operation counters are process-global and cargo runs
    // these tests concurrently, so the whole binary serialises on one lock.
    // It also makes the suite deterministic, which the flakiness gate needs.
    let _guard = winasio::net::socket_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let listener = TcpListener::bind(&proactor, v4_any()).expect("bind");
    let addr = listener.local_addr();

    let handle = std::thread::spawn(move || {
        let mut c = client::connect(addr);
        client::send(&mut c, b"queued");
        c
    });
    let (stream, _) = drive_proactor(&proactor, listener.accept()).expect("accept");

    // Wait until the bytes are actually in the receive buffer, so the
    // zero-length read below takes the inline path rather than pending.
    wait_until_readable(&proactor, &stream);

    let OpResult(outcome, _) = drive_proactor(&proactor, stream.read(Vec::new()));
    assert_eq!(
        outcome.expect("read"),
        ReadOutcome::Bytes(0),
        "the connection is open and has data; only the request was empty"
    );

    // Non-destructive: the queued bytes survived the zero-length receive.
    let OpResult(outcome, buf) = drive_proactor(&proactor, stream.read(vec![0u8; 16]));
    assert_eq!(outcome.expect("read"), ReadOutcome::Bytes(6));
    assert_eq!(&buf[..6], b"queued");

    drop(handle.join().expect("client thread"));
}

/// Block until the stream has data waiting, without consuming it.
///
/// A zero-length read is exactly the readiness probe for this — that is what
/// the pattern is for — so this uses one, bounded by a deadline.
fn wait_until_readable<S: winasio::iocp::Submitter>(proactor: &Proactor, stream: &TcpStream<S>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut probing = Box::pin(stream.read(Vec::new()));
        if let Some(OpResult(outcome, _)) = poll_once(proactor, probing.as_mut()) {
            outcome.expect("zero-length probe");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the peer's bytes never arrived"
        );
    }
}
