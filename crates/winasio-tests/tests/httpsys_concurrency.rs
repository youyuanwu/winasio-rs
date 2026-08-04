// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Phase 7: concurrency and shutdown.
//!
//! Covers SC-021 (many requests with several receives outstanding), SC-025 (a
//! listener driven from several threads) and SC-020 (outstanding operations
//! return to zero after close).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::{block_on, send_raw, Server};
use winasio::httpsys::{ReceiveConfig, RequestQueue, Response, ResponseHeader};
use winasio::iocp::ThreadPoolIo;

const PORT: u16 = 12365;

/// Serve requests until the queue closes, answering each with its own target.
fn serve_loop(queue: Arc<RequestQueue<ThreadPoolIo>>, served: Arc<AtomicUsize>) {
    loop {
        let request = match block_on(queue.receive()) {
            Ok(r) => r,
            // The queue was closed, or the operation was cancelled.
            Err(_) => return,
        };
        let mut reply = Response::new(200);
        reply
            .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
            .add_body(request.raw_target().to_vec());
        if block_on(queue.send(request.id(), reply)).0.is_ok() {
            served.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// SC-025: a listener driven from two threads, each serving distinct requests.
///
/// The operating system forbids *concurrent sends on one request identifier*,
/// which is why each thread owns the requests it receives end to end.
#[test]
fn a_listener_serves_correctly_from_several_threads() {
    let _guard = common::serial();
    let server = match Server::start(PORT, "conc", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };
    let served = Arc::new(AtomicUsize::new(0));

    let workers: Vec<_> = (0..2)
        .map(|_| {
            let q = server.queue().clone();
            let s = served.clone();
            std::thread::spawn(move || serve_loop(q, s))
        })
        .collect();

    const REQUESTS: usize = 40;
    let clients: Vec<_> = (0..REQUESTS)
        .map(|i| std::thread::spawn(move || send_raw(PORT, "GET", &format!("conc/{i}"), &[], &[])))
        .collect();

    let mut answered = 0;
    for (i, c) in clients.into_iter().enumerate() {
        if let Some(raw) = c.join().expect("client thread") {
            let (_, _, body) = common::parse_response(&raw);
            assert_eq!(
                String::from_utf8_lossy(&body),
                format!("/conc/{i}"),
                "each request must get its own answer"
            );
            answered += 1;
        }
    }
    assert_eq!(answered, REQUESTS, "every request must be answered");

    // Closing through the shared handle is what unblocks the workers: their
    // pending receives resolve with an error once the queue is gone.
    server.queue().close().expect("close");
    for w in workers {
        let _ = w.join();
    }
    assert_eq!(served.load(Ordering::SeqCst), REQUESTS);
}

/// SC-021: a sustained series of requests with several receives outstanding at
/// once, each delivered to exactly one receive.
#[test]
#[ignore = "long-running soak; run with --ignored"]
fn many_requests_with_several_receives_outstanding() {
    let _guard = common::serial();
    let server = match Server::start(PORT + 1, "soak", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };
    let served = Arc::new(AtomicUsize::new(0));

    // Four receives outstanding, as SC-021 requires.
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let q = server.queue().clone();
            let s = served.clone();
            std::thread::spawn(move || serve_loop(q, s))
        })
        .collect();

    const REQUESTS: usize = 1000;
    let mut answered = 0usize;
    // Eight client threads, each issuing a share of the requests.
    let clients: Vec<_> = (0..8)
        .map(|t| {
            std::thread::spawn(move || {
                let mut ok = 0;
                for i in 0..REQUESTS / 8 {
                    let target = format!("soak/{t}-{i}");
                    if let Some(raw) = send_raw(PORT + 1, "GET", &target, &[], &[]) {
                        let (_, _, body) = common::parse_response(&raw);
                        if String::from_utf8_lossy(&body) == format!("/{target}") {
                            ok += 1;
                        }
                    }
                }
                ok
            })
        })
        .collect();

    for c in clients {
        answered += c.join().expect("client thread");
    }

    assert_eq!(
        answered, REQUESTS,
        "every request must be answered exactly once with its own target"
    );

    server.queue().close().expect("close");
    for w in workers {
        let _ = w.join();
    }
    assert_eq!(served.load(Ordering::SeqCst), REQUESTS);
}

/// SC-020: closing a queue with receives outstanding drains them, leaving no
/// operation alive in the I/O layer.
#[test]
fn closing_with_operations_outstanding_drains_them() {
    // `live_operations` is process-global, so no other test in this binary may
    // have work in flight while it is being observed.
    let _guard = common::serial();
    let before = winasio::iocp::live_operations();

    {
        let server = match Server::start(PORT + 2, "drain", ReceiveConfig::default()) {
            Some(s) => s,
            None => return,
        };

        // Several receives outstanding, none of which will ever be satisfied.
        let workers: Vec<_> = (0..3)
            .map(|_| {
                let q = server.queue().clone();
                std::thread::spawn(move || {
                    let _ = block_on(q.receive());
                })
            })
            .collect();

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            winasio::iocp::live_operations() > before,
            "receives should be in flight"
        );

        // Closing cancels and drains. This is the only way to stop workers
        // blocked in `receive`, and it works through the shared `Arc`.
        server.queue().close().expect("close");
        for w in workers {
            let _ = w.join();
        }
        drop(server);
    }

    // The drain is synchronous with the close, so this needs no polling loop.
    assert_eq!(
        winasio::iocp::live_operations(),
        before,
        "no operation may outlive the queue it was submitted to"
    );
}
