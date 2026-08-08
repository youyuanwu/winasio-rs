// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

// An `axum::Router` served over HTTP.sys *concurrently*, with no async runtime
// and no `unsafe`.
//
// Run it directly:
//
//     cargo run -p winasio-tests --example axum_server
//
// then `curl http://localhost:12421/axum/greet`.
//
// This is the axum counterpart of `util_server.rs`. Where that example drives
// requests one at a time with `Server::serve_one`, this one hands them to
// `winasio_axum::serve`, which accepts in a loop and dispatches each request to
// an `Executor` — here `CurrentThread`, which interleaves many in-flight
// requests on this one thread while spawning nothing.
//
// Two things are deliberately on display.
//
// * **No runtime.** The whole thing runs on `futures::executor::block_on`, a
//   bare park-and-retry loop. `winasio-axum` depends on no runtime, and neither
//   does a real `axum::Router` with axum's default features off.
// * **Routes carry HTTP.sys's prefix.** HTTP.sys delivers request URIs with the
//   registered prefix included, so the routes are written `/axumex/greet`, not
//   `/greet`. See the crate documentation for the rationale.
//
// It is also compiled and executed by the test suite, which is what keeps it
// honest.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use winasio::iocp::ThreadPool;
use winasio_axum::{serve, CurrentThread};
use winasio_util::ServerSession;

/// Build the application. An ordinary `axum::Router`, so extractors, layers and
/// the rest of the axum ecosystem work unchanged.
fn app(path: &str) -> Router {
    Router::new()
        .route(
            &format!("/{path}/greet"),
            get(|| async { "hello from axum over HTTP.sys\n" }),
        )
        .route(
            &format!("/{path}/echo"),
            post(|body: String| async move { format!("you sent {} bytes\n", body.len()) }),
        )
}

/// Serve `count` requests concurrently, then stop.
///
/// The serve loop runs until the queue closes, so a small monitor thread closes
/// it once `count` requests have been answered. Closing is delayed a moment so
/// the final reply has flushed — the loop abandons in-flight work on shutdown by
/// design, and this example does not want to abandon the very reply it counted.
///
/// There is no `unsafe` anywhere in this file.
pub async fn run_server(
    port: u16,
    path: &str,
    count: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session = ServerSession::new()?;
    let server = winasio_util::Server::builder(&session)
        .url(&format!("http://localhost:{port}/{path}/"))
        .build(&ThreadPool)?;

    println!("listening on http://localhost:{port}/{path}/");

    // Every answered request bumps this; the monitor stops the server when it
    // reaches `count`.
    let served = Arc::new(AtomicUsize::new(0));
    let shutdown = server.shutdown_handle();
    let monitor = {
        let served = Arc::clone(&served);
        std::thread::spawn(move || {
            while served.load(Ordering::SeqCst) < count {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // Let the last reply flush before the queue closes.
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = shutdown.shutdown();
        })
    };

    // Count a request as served the moment its handler runs, via a tiny tower
    // wrapper around the router. `on_error` reports the rare recoverable failure
    // without any logging dependency.
    let counting = CountServed {
        inner: app(path),
        served: Arc::clone(&served),
    };
    serve(&server, counting, CurrentThread::new())
        .on_error(|error| eprintln!("request failed: {error}"))
        .await?;

    monitor.join().expect("monitor thread");
    Ok(())
}

/// A `tower::Service` that counts each request it forwards to `inner`.
#[derive(Clone)]
struct CountServed<S> {
    inner: S,
    served: Arc<AtomicUsize>,
}

impl<S, B> winasio_util::tower_service::Service<http::Request<B>> for CountServed<S>
where
    S: winasio_util::tower_service::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        self.served.fetch_add(1, Ordering::SeqCst);
        self.inner.call(request)
    }
}

#[allow(dead_code)]
fn main() {
    // No runtime: a bare `block_on`, which is the whole claim.
    if let Err(e) = futures::executor::block_on(run_server(12421, "axum", usize::MAX)) {
        eprintln!("server failed: {e}");
        std::process::exit(1);
    }
}
