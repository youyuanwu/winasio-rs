// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Integration tests for `winasio_axum`, the concurrent axum driver over
//! HTTP.sys.
//!
//! # Ports
//!
//! This binary owns 12420..=12440 — one per test, because several tests shut
//! their listener down and a shared prefix would race (see `util_server.rs`).
//!
//! # Why every test drives `block_on`
//!
//! `winasio-axum` promises it needs no runtime. `common::block_on` is a bare
//! park-and-retry loop with no worker threads and no reactor, so if any part of
//! the driver quietly needed one, none of this would pass.
//!
//! # Why the concurrency tests must fail under sequential serving
//!
//! `SC-008`/`FR-014` demand proof that the executors do something a sequential
//! `serve_one` could not. The `CurrentThread` and `ThreadPerRequest` tests below
//! use a rendezvous (a shared counter, a `Barrier`) that only completes when
//! several requests are in flight at once. Served one at a time, they would
//! never complete and `block_on`'s 30-second deadline would fire — a loud
//! failure, not a hang. Two of them are hand-falsified in the plan's manual
//! verification.

mod common;

use std::convert::Infallible;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use winasio::httpsys::MIN_CAPACITY;
use winasio::iocp::ThreadPool;
use winasio_axum::{serve, CurrentThread, Executor, RequestTask, ThreadPerRequest};
use winasio_util::tower_service::Service;
use winasio_util::{
    AcceptError, ConnectionInfo, IncomingBody, ReceiveConfig, ServeError, Server, ServerSession,
};

const PORT: u16 = 12420;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Bind a listener, or `None` when the machine will not let us (no URL
/// reservation), so tests skip rather than fail. Mirrors `util_server.rs::start`.
fn start<'a>(session: &'a ServerSession, port: u16, path: &str) -> Option<Server<'a>> {
    start_with(session, port, path, ReceiveConfig::default())
}

/// As [`start`], but with an explicit receive configuration (for the oversized
/// test, which needs a queue too small for a padded request).
fn start_with<'a>(
    session: &'a ServerSession,
    port: u16,
    path: &str,
    config: ReceiveConfig,
) -> Option<Server<'a>> {
    match Server::builder(session)
        .url(&format!("http://localhost:{port}/{path}/"))
        .receive_config(config)
        .build(&ThreadPool)
    {
        Ok(server) => Some(server),
        Err(error) => {
            eprintln!(
                "skipping: cannot bind http://localhost:{port}/{path}/: {error} \
                 (a URL reservation may be needed)"
            );
            None
        }
    }
}

/// Drive a [`Serve`](winasio_axum::Serve) — or any [`IntoFuture`] — to completion
/// on the bare park loop. [`Serve`](winasio_axum::Serve) is `IntoFuture`, not
/// `Future`, so it needs the `into_future()` step that `common::block_on` (which
/// takes a `Future`) does not do for us.
fn drive<F: IntoFuture>(future: F) -> F::Output {
    common::block_on(future.into_future())
}

/// Fire one raw request from a background thread, returning the raw reply.
fn request(
    port: u16,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::thread::JoinHandle<Option<Vec<u8>>> {
    let method = method.to_string();
    let target = target.to_string();
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let body = body.to_vec();
    std::thread::spawn(move || common::send_raw(port, &method, &target, &headers, &body))
}

/// A waker that unparks this thread, for driving a future by hand with real
/// wakeups (unlike `Waker::noop`, which cannot deliver an async completion).
struct ParkWaker(std::thread::Thread);

impl Wake for ParkWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Poll `fut` on this thread until `stop()` holds or 15 seconds pass, parking
/// between polls so genuine async completions can wake it. Returns whether
/// `stop()` was observed. Used by the tests that must *drop* a serve future
/// rather than run it to completion.
fn drive_until<F: Future + ?Sized>(mut fut: Pin<&mut F>, stop: impl Fn() -> bool) -> bool {
    let waker = Waker::from(Arc::new(ParkWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let _ = fut.as_mut().poll(&mut cx);
        if stop() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Concurrency that a sequential serve could not achieve
// ---------------------------------------------------------------------------

#[test]
fn current_thread_serves_requests_that_only_complete_concurrently() {
    // FR-014 / SC-001. N handlers each announce arrival and then wait until all
    // N have arrived. Only if the executor keeps all N in flight at once does
    // the rendezvous complete; served one at a time it would deadlock and
    // `block_on`'s deadline would fire.
    const N: usize = 4;
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT, "cc") else {
        return;
    };
    let shutdown = server.shutdown_handle();

    let arrived = Arc::new(AtomicUsize::new(0));
    let router = {
        let arrived = Arc::clone(&arrived);
        axum::Router::new().route(
            "/cc/rendezvous",
            axum::routing::get(move || {
                let arrived = Arc::clone(&arrived);
                async move {
                    arrived.fetch_add(1, Ordering::SeqCst);
                    std::future::poll_fn(move |cx| {
                        if arrived.load(Ordering::SeqCst) == N {
                            Poll::Ready(())
                        } else {
                            // Self-wake: a bare executor has no timer, so nudging
                            // keeps this future eligible until the last arrival.
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                    })
                    .await;
                    "done"
                }
            }),
        )
    };

    let mut replies = Vec::new();
    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| drive(serve(&server, router, CurrentThread::new())));

        let clients: Vec<_> = (0..N)
            .map(|_| request(PORT, "GET", "cc/rendezvous", &[], b""))
            .collect();
        for client in clients {
            replies.push(client.join().unwrap());
        }

        // Every request has been answered; stop the loop.
        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });

    assert_eq!(replies.len(), N);
    for reply in &replies {
        let raw = reply.as_ref().expect("a reply");
        assert_eq!(common::parse_response(raw).2, b"done");
    }
}

#[test]
fn thread_per_request_runs_handlers_in_parallel() {
    // SC-002. N handlers cross a `Barrier(N)` — impossible unless N threads run
    // at once — and record their `ThreadId`. Distinct ids prove real parallelism.
    const N: usize = 4;
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 1, "tp") else {
        return;
    };
    let shutdown = server.shutdown_handle();

    let barrier = Arc::new(Barrier::new(N));
    let ids = Arc::new(Mutex::new(Vec::<ThreadId>::new()));
    let router = {
        let barrier = Arc::clone(&barrier);
        let ids = Arc::clone(&ids);
        axum::Router::new().route(
            "/tp/rendezvous",
            axum::routing::get(move || {
                let barrier = Arc::clone(&barrier);
                let ids = Arc::clone(&ids);
                async move {
                    // Blocking, deliberately: on `ThreadPerRequest` each handler
                    // owns its thread, so blocking it is fine and the barrier can
                    // only release when all N threads exist.
                    barrier.wait();
                    ids.lock().unwrap().push(std::thread::current().id());
                    "done"
                }
            }),
        )
    };

    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| drive(serve(&server, router, ThreadPerRequest::new())));

        let clients: Vec<_> = (0..N)
            .map(|_| request(PORT + 1, "GET", "tp/rendezvous", &[], b""))
            .collect();
        for client in clients {
            let raw = client.join().unwrap().expect("a reply");
            assert_eq!(common::parse_response(&raw).2, b"done");
        }

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });

    let ids = ids.lock().unwrap().clone();
    assert_eq!(ids.len(), N, "every handler ran");
    let distinct: std::collections::HashSet<ThreadId> = ids.into_iter().collect();
    assert_eq!(distinct.len(), N, "each handler ran on a distinct thread");
}

// ---------------------------------------------------------------------------
// The executor is the caller's, plugged through the trait
// ---------------------------------------------------------------------------

#[test]
fn a_caller_supplied_executor_receives_the_request_work() {
    // P3 / SC-003. A foreign `Executor` impl (not one of the built-ins) drives
    // the request work; the loop dispatches through it.
    #[derive(Clone)]
    struct CountingExecutor {
        dispatched: Arc<AtomicUsize>,
    }

    impl Executor<RequestTask> for CountingExecutor {
        fn execute(&self, task: RequestTask) {
            self.dispatched.fetch_add(1, Ordering::SeqCst);
            // A spawning executor: drive to completion on a fresh thread with a
            // bare `block_on`, no runtime.
            std::thread::spawn(move || futures::executor::block_on(task));
        }
    }

    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 2, "plug") else {
        return;
    };
    let shutdown = server.shutdown_handle();

    let dispatched = Arc::new(AtomicUsize::new(0));
    let router = axum::Router::new().route("/plug/hello", axum::routing::get(|| async { "hello" }));
    let executor = CountingExecutor {
        dispatched: Arc::clone(&dispatched),
    };

    const REQUESTS: usize = 3;
    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| drive(serve(&server, router, executor)));

        for _ in 0..REQUESTS {
            let raw = request(PORT + 2, "GET", "plug/hello", &[], b"")
                .join()
                .unwrap()
                .expect("a reply");
            assert_eq!(common::parse_response(&raw).2, b"hello");
        }

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });

    assert_eq!(
        dispatched.load(Ordering::SeqCst),
        REQUESTS,
        "each request must have been dispatched through the caller's executor"
    );
}

// ---------------------------------------------------------------------------
// The loop keeps going through recoverable trouble
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_request_is_recovered_from_and_the_loop_continues() {
    // FR-006 / SC-004. A padded request cannot fit the deliberately small queue
    // and is discarded below as `RequestTooLarge`; the observer sees it and a
    // subsequent normal request on the same server still returns 200 — proof the
    // loop continued rather than spun or aborted.
    let session = ServerSession::new().unwrap();
    // Room for a small request but not a padded one; MIN_CAPACITY alone is the
    // bare structure with no room for even a URL (httpsys_receive.rs).
    let config = ReceiveConfig {
        initial_capacity: MIN_CAPACITY + 1024,
        max_retries: 0,
    };
    let Some(server) = start_with(&session, PORT + 3, "big", config) else {
        return;
    };
    let shutdown = server.shutdown_handle();

    let oversized_seen = Arc::new(AtomicUsize::new(0));
    let router = axum::Router::new().route("/big/small", axum::routing::get(|| async { "ok" }));
    let observer = {
        let oversized_seen = Arc::clone(&oversized_seen);
        move |error: ServeError| {
            if matches!(
                error,
                ServeError::Accept(AcceptError::RequestTooLarge { .. })
            ) {
                oversized_seen.fetch_add(1, Ordering::SeqCst);
            }
        }
    };

    std::thread::scope(|scope| {
        let serve_thread =
            scope.spawn(|| drive(serve(&server, router, CurrentThread::new()).on_error(observer)));

        // A header far larger than the 1 KiB of slack in the queue.
        let padding = "p".repeat(4096);
        let oversized = request(PORT + 3, "GET", "big/small", &[("X-Pad", &padding)], b"");
        // The oversized request is discarded with no reply; do not wait on it.
        let _ = oversized.join().unwrap();

        // A normal request must still be answered.
        let raw = request(PORT + 3, "GET", "big/small", &[], b"")
            .join()
            .unwrap()
            .expect("a reply after the oversized one");
        let (status, _headers, body) = common::parse_response(&raw);
        assert!(status.contains("200"), "got {status:?}");
        assert_eq!(body, b"ok");

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });

    assert!(
        oversized_seen.load(Ordering::SeqCst) >= 1,
        "the observer must have been told about the oversized request"
    );
}

#[test]
fn a_panicking_handler_is_isolated_under_current_thread() {
    panicking_handler_is_isolated(PORT + 4, "panicct", Which::Ct);
}

#[test]
fn a_panicking_handler_is_isolated_under_thread_per_request() {
    panicking_handler_is_isolated(PORT + 5, "panictp", Which::Tp);
}

enum Which {
    Ct,
    Tp,
}

/// FR-006. A handler that panics must not take the loop down: the observer is
/// told, its own peer gets nothing (the `Responder` is dropped), and a following
/// request is still answered. This is the test that fails if the per-task
/// `catch_unwind` wrap is removed — under `CurrentThread` the panic would unwind
/// the loop instead of being caught.
fn panicking_handler_is_isolated(port: u16, path: &str, which: Which) {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, port, path) else {
        return;
    };
    let shutdown = server.shutdown_handle();

    let panics_seen = Arc::new(AtomicUsize::new(0));
    let boom = format!("/{path}/boom");
    let ok = format!("/{path}/ok");
    let router = axum::Router::new()
        .route(
            &boom,
            axum::routing::get(|| async {
                panic!("handler blew up");
                #[allow(unreachable_code)]
                "unreachable"
            }),
        )
        .route(&ok, axum::routing::get(|| async { "still serving" }));
    let observer = {
        let panics_seen = Arc::clone(&panics_seen);
        move |error: ServeError| {
            // A caught panic is reported as `Service` carrying a `HandlerPanic`.
            if let ServeError::Service(source) = &error {
                if source
                    .downcast_ref::<winasio_axum::HandlerPanic>()
                    .is_some()
                {
                    panics_seen.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    };

    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| match which {
            Which::Ct => drive(serve(&server, router, CurrentThread::new()).on_error(observer)),
            Which::Tp => drive(serve(&server, router, ThreadPerRequest::new()).on_error(observer)),
        });

        // The panicking request: it gets no reply, by design.
        let _ = request(port, "GET", &format!("{path}/boom"), &[], b"")
            .join()
            .unwrap();

        // A normal request after the panic must still be answered.
        let raw = request(port, "GET", &format!("{path}/ok"), &[], b"")
            .join()
            .unwrap()
            .expect("a reply after the panic");
        assert_eq!(common::parse_response(&raw).2, b"still serving");

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });

    assert!(
        panics_seen.load(Ordering::SeqCst) >= 1,
        "the observer must have been told about the caught panic"
    );
}

// ---------------------------------------------------------------------------
// Backpressure: an unready service does not dequeue
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct NeverReady {
    polls: Arc<AtomicUsize>,
}

impl Service<Request<IncomingBody>> for NeverReady {
    type Response = Response<Full<Bytes>>;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn call(&mut self, _request: Request<IncomingBody>) -> Self::Future {
        unreachable!("a service that is never ready must never be called");
    }
}

#[test]
fn an_unready_service_never_dequeues_the_request() {
    // FR-007 / SC-005. The *observable* backpressure proof, mirroring
    // `util_server.rs::readiness_is_awaited_before_a_request_is_accepted`: run the
    // concurrent loop with a permanently-unready service and poll it a while, then
    // drop it and answer the same request with a plain `serve_one` on the *same*
    // server. That the second serve answers proves the loop never pulled the
    // request out of the kernel queue.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 6, "bp") else {
        return;
    };

    let client = request(PORT + 6, "GET", "bp/x", &[], b"");
    std::thread::sleep(Duration::from_millis(400));

    let polls = Arc::new(AtomicUsize::new(0));
    {
        let service = NeverReady {
            polls: Arc::clone(&polls),
        };
        let mut serving = serve(&server, service, CurrentThread::new()).into_future();
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        for _ in 0..50 {
            assert!(
                serving.as_mut().poll(&mut cx).is_pending(),
                "an unready service must never complete a serve"
            );
        }
        // Dropped without ever having been ready — its borrow of the server ends.
    }
    assert!(
        polls.load(Ordering::SeqCst) >= 50,
        "readiness must actually have been polled, got {}",
        polls.load(Ordering::SeqCst)
    );

    // The request is still queued, because it was never accepted.
    let mut ready_service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ready"))))
    });
    common::block_on(server.serve_one(&mut ready_service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    assert_eq!(common::parse_response(&raw).2, b"ready");
}

#[test]
fn the_readiness_gate_consumes_exactly_one_reservation() {
    // FR-007 (planning-review C2). A service with a *single* permit: a fresh
    // clone acquires it in `poll_ready`, and `Accepted::serve`'s re-poll on that
    // same clone must NOT need a second permit. The handler is therefore invoked
    // exactly once, proving one reservation is consumed across the gate and the
    // re-poll — not two.
    #[derive(Clone)]
    struct SinglePermit {
        permit: Arc<AtomicBool>,
        acquired: bool,
        calls: Arc<AtomicUsize>,
    }

    impl Service<Request<IncomingBody>> for SinglePermit {
        type Response = Response<Full<Bytes>>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Infallible>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            if self.acquired {
                // Idempotent: an already-ready clone stays ready without taking
                // another permit.
                return Poll::Ready(Ok(()));
            }
            if self.permit.swap(false, Ordering::SeqCst) {
                self.acquired = true;
                Poll::Ready(Ok(()))
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }

        fn call(&mut self, _request: Request<IncomingBody>) -> Self::Future {
            assert!(self.acquired, "call without a reservation");
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(Response::new(Full::new(Bytes::from_static(b"served")))))
        }
    }

    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 7, "permit") else {
        return;
    };

    let done = Arc::new(AtomicBool::new(false));
    let reply = Arc::new(Mutex::new(None));
    let client = {
        let done = Arc::clone(&done);
        let reply = Arc::clone(&reply);
        std::thread::spawn(move || {
            let raw = common::send_raw(PORT + 7, "GET", "permit/x", &[], b"");
            *reply.lock().unwrap() = raw;
            done.store(true, Ordering::SeqCst);
        })
    };
    std::thread::sleep(Duration::from_millis(400));

    let calls = Arc::new(AtomicUsize::new(0));
    {
        let service = SinglePermit {
            permit: Arc::new(AtomicBool::new(true)),
            acquired: false,
            calls: Arc::clone(&calls),
        };
        let mut serving = serve(&server, service, CurrentThread::new()).into_future();
        // Drive until the one round-trip completes, then drop the loop (whose
        // readiness gate would otherwise spin on the now-exhausted permit).
        assert!(
            drive_until(serving.as_mut(), || done.load(Ordering::SeqCst)),
            "the single permitted request should have completed"
        );
    }
    client.join().unwrap();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one handler call for one permit"
    );
    let raw = reply.lock().unwrap().clone().expect("a reply");
    assert_eq!(common::parse_response(&raw).2, b"served");
}

// ---------------------------------------------------------------------------
// Error policy and shutdown
// ---------------------------------------------------------------------------

#[test]
fn a_failing_service_yields_500_and_notifies_the_observer() {
    // SC-004. A service that errors instead of responding: the peer gets a
    // bodiless 500 (framed by winasio-util) and the observer sees `Service`.
    #[derive(Debug)]
    struct Boom;
    impl std::fmt::Display for Boom {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "boom")
        }
    }
    impl std::error::Error for Boom {}

    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 8, "fail") else {
        return;
    };
    let shutdown = server.shutdown_handle();

    let service_errors = Arc::new(AtomicUsize::new(0));
    let observer = {
        let service_errors = Arc::clone(&service_errors);
        move |error: ServeError| {
            if matches!(error, ServeError::Service(_)) {
                service_errors.fetch_add(1, Ordering::SeqCst);
            }
        }
    };
    let service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Err::<Response<Full<Bytes>>, Boom>(Boom)
    });

    std::thread::scope(|scope| {
        let serve_thread =
            scope.spawn(|| drive(serve(&server, service, CurrentThread::new()).on_error(observer)));

        let raw = request(PORT + 8, "GET", "fail/x", &[], b"")
            .join()
            .unwrap()
            .expect("a reply");
        let (status, _headers, body) = common::parse_response(&raw);
        assert!(status.contains("500"), "got {status:?}");
        assert!(body.is_empty(), "a 500 from the fallback carries no body");

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });

    assert!(
        service_errors.load(Ordering::SeqCst) >= 1,
        "the observer must have seen the service error"
    );
}

#[test]
fn shutdown_from_another_thread_ends_the_loop_cleanly() {
    // SC-004 (the `is_queue_closed` path). A `ShutdownHandle` used from another
    // thread closes the queue, and the loop returns `Ok(())`.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 9, "stop") else {
        return;
    };
    let shutdown = server.shutdown_handle();
    let router = axum::Router::new().route("/stop/x", axum::routing::get(|| async { "ok" }));

    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| drive(serve(&server, router, CurrentThread::new())));

        // Give the loop time to reach its first `accept`, then stop it.
        std::thread::sleep(Duration::from_millis(300));
        shutdown.shutdown().unwrap();

        let result = serve_thread.join().unwrap();
        assert!(
            result.is_ok(),
            "a clean shutdown must return Ok, got {result:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// Peer address and the ConnectInfo recipe (M2 / M2c re-verification)
// ---------------------------------------------------------------------------

#[test]
fn the_peer_address_is_reachable_through_connection_info() {
    // FR-015 / SC-006. Without any axum feature, a handler reads the peer address
    // from `winasio_util::ConnectionInfo` in the request extensions.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 10, "ci") else {
        return;
    };
    let shutdown = server.shutdown_handle();
    let router = axum::Router::new().route(
        "/ci/peer",
        axum::routing::get(|info: axum::Extension<ConnectionInfo>| async move {
            match info.0.peer_address {
                Some(address) => format!("peer {address}"),
                None => "peer unknown".to_string(),
            }
        }),
    );

    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| drive(serve(&server, router, CurrentThread::new())));

        let raw = request(PORT + 10, "GET", "ci/peer", &[], b"")
            .join()
            .unwrap()
            .expect("a reply");
        let (status, _headers, body) = common::parse_response(&raw);
        assert!(status.contains("200"), "got {status:?}");
        let body = String::from_utf8_lossy(&body);
        assert!(body.starts_with("peer "), "got {body:?}");
        assert!(
            body.contains("127.0.0.1") || body.contains("::1"),
            "the loopback peer address should be present, got {body:?}"
        );

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });
}

/// Inserts `axum::extract::ConnectInfo(addr)` into the request extensions from
/// the peer address `winasio-util` already put there — the recipe M2 proves.
#[derive(Clone)]
struct InsertConnectInfo<S> {
    inner: S,
}

impl<S, B> Service<Request<B>> for InsertConnectInfo<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        if let Some(info) = request.extensions().get::<ConnectionInfo>().copied() {
            let address = info
                .peer_address
                .unwrap_or_else(|| "127.0.0.1:0".parse().unwrap());
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(address));
        }
        self.inner.call(request)
    }
}

#[test]
fn the_axum_connect_info_extractor_works_when_the_recipe_is_applied() {
    // SC-006 / M2. With `ConnectInfo(addr)` inserted, axum's own
    // `ConnectInfo<SocketAddr>` extractor is satisfied and the handler returns 200.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 11, "ci2") else {
        return;
    };
    let shutdown = server.shutdown_handle();
    let router = axum::Router::new().route(
        "/ci2/peer",
        axum::routing::get(
            |axum::extract::ConnectInfo(address): axum::extract::ConnectInfo<SocketAddr>| async move {
                format!("axum peer {address}")
            },
        ),
    );
    let service = InsertConnectInfo { inner: router };

    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| drive(serve(&server, service, CurrentThread::new())));

        let raw = request(PORT + 11, "GET", "ci2/peer", &[], b"")
            .join()
            .unwrap()
            .expect("a reply");
        let (status, _headers, body) = common::parse_response(&raw);
        assert!(status.contains("200"), "got {status:?}");
        assert!(
            String::from_utf8_lossy(&body).starts_with("axum peer "),
            "got {:?}",
            String::from_utf8_lossy(&body)
        );

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });
}

#[test]
fn the_axum_connect_info_extractor_500s_without_the_recipe() {
    // SC-006 / M2c (the control). The identical extractor without the insertion
    // returns axum's own 500 "Missing request extension", so the M2 test above is
    // meaningful rather than vacuous.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 12, "ci3") else {
        return;
    };
    let shutdown = server.shutdown_handle();
    let router = axum::Router::new().route(
        "/ci3/peer",
        axum::routing::get(
            |axum::extract::ConnectInfo(address): axum::extract::ConnectInfo<SocketAddr>| async move {
                format!("axum peer {address}")
            },
        ),
    );

    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| drive(serve(&server, router, CurrentThread::new())));

        let raw = request(PORT + 12, "GET", "ci3/peer", &[], b"")
            .join()
            .unwrap()
            .expect("a reply");
        let (status, _headers, _body) = common::parse_response(&raw);
        assert!(
            status.contains("500"),
            "the extractor should 500 without the inserted ConnectInfo, got {status:?}"
        );

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });
}

// ---------------------------------------------------------------------------
// An axum Router, served concurrently, routes as itself
// ---------------------------------------------------------------------------

#[test]
fn an_axum_router_served_concurrently_routes_its_own_hits_and_misses() {
    // P1 scenario 2. Two routes and a miss, through `serve` + `CurrentThread`;
    // the router — not the crate — decides the 200s and the 404.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 13, "router") else {
        return;
    };
    let shutdown = server.shutdown_handle();
    let router = axum::Router::new()
        .route("/router/greet", axum::routing::get(|| async { "hello" }))
        .route("/router/other", axum::routing::get(|| async { "nope" }));

    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| drive(serve(&server, router, CurrentThread::new())));

        let greet = request(PORT + 13, "GET", "router/greet", &[], b"")
            .join()
            .unwrap()
            .expect("a reply");
        assert_eq!(common::parse_response(&greet).2, b"hello");

        let other = request(PORT + 13, "GET", "router/other", &[], b"")
            .join()
            .unwrap()
            .expect("a reply");
        assert_eq!(common::parse_response(&other).2, b"nope");

        let miss = request(PORT + 13, "GET", "router/nothing", &[], b"")
            .join()
            .unwrap()
            .expect("a reply");
        let (status, _headers, body) = common::parse_response(&miss);
        assert!(status.contains("404"), "got {status:?}");
        assert!(body.is_empty());

        shutdown.shutdown().unwrap();
        serve_thread.join().unwrap().expect("serve returned Ok");
    });
}

// ---------------------------------------------------------------------------
// The runnable example
// ---------------------------------------------------------------------------

/// The example server, compiled into this test binary so it can be run.
#[allow(dead_code)]
mod example {
    include!("../examples/axum_server.rs");
}

const EXAMPLE_PORT: u16 = 12440;

#[test]
fn the_example_server_serves_axum_concurrently_with_no_runtime() {
    let server = std::thread::spawn(|| {
        futures::executor::block_on(example::run_server(EXAMPLE_PORT, "axumex", 2))
    });

    std::thread::sleep(Duration::from_millis(600));

    let Some(get) = common::send_raw(EXAMPLE_PORT, "GET", "axumex/greet", &[], &[]) else {
        eprintln!("skipping: the example could not bind {EXAMPLE_PORT}");
        return;
    };
    let (status, _headers, body) = common::parse_response(&get);
    assert!(status.contains("200"), "got {status:?}");
    assert!(
        String::from_utf8_lossy(&body).contains("hello from axum"),
        "body was {:?}",
        String::from_utf8_lossy(&body)
    );

    let post = common::send_raw(EXAMPLE_PORT, "POST", "axumex/echo", &[], b"twelve bytes").unwrap();
    let (_, _, body) = common::parse_response(&post);
    assert!(
        String::from_utf8_lossy(&body).contains("you sent 12 bytes"),
        "the example should read the body, got {:?}",
        String::from_utf8_lossy(&body)
    );

    server.join().expect("example thread").expect("example ran");
}

#[test]
fn the_example_server_contains_no_unsafe() {
    let source = include_str!("../examples/axum_server.rs");
    for (n, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !code.contains("unsafe"),
            "examples/axum_server.rs:{} uses `unsafe`: {line}",
            n + 1
        );
    }
}
