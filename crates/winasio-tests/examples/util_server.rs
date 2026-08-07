// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

// A `tower::Service` served over HTTP.sys, with no runtime and no `unsafe`.
//
// Run it directly:
//
//     cargo run -p winasio-tests --example util_server
//
// then `curl http://localhost:12368/util/anything`.
//
// This is the counterpart of `httpsys_server.rs`: the same job, one layer up.
// Where that example composes a platform `Response` by hand, this one takes an
// `http::Response<B>` from a `tower::Service` and lets the crate do the
// conversion and the framing.
//
// Two things are deliberately on display.
//
// * **No runtime.** `httpsys_server.rs` reaches for tokio just to have
//   something to await on. This one uses `futures::executor::block_on`, which
//   is a bare park-and-retry loop: no worker threads, no reactor, no reliance
//   on any executor's behaviour. The crate spawns nothing, so nothing is needed.
// * **No accept loop policy.** `Server::serve` drives requests one at a time,
//   which is all a single-threaded caller wants. A caller who wants concurrency
//   uses `Server::accept` and moves each `Accepted` wherever it likes; the
//   crate takes no position, because spawning is the caller's business.
//
// It is also compiled and executed by the test suite, which is what keeps it
// honest.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use winasio::iocp::ThreadPool;
use winasio_util::tower_service::Service;
use winasio_util::{Error, IncomingBody, ServerSession};

/// The application. An ordinary `tower::Service`, so an `axum::Router` or a
/// `tower-http` layer would slot in unchanged.
///
/// `Clone` because that is how a concurrent caller shares a service across
/// tasks: `poll_ready` reserves capacity on one clone and `call` spends it.
/// Nothing here has capacity to reserve, so readiness is immediate.
#[derive(Clone, Default)]
pub struct Echo {
    served: usize,
}

impl Service<Request<IncomingBody>> for Echo {
    type Response = Response<Full<Bytes>>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Always ready. A service with a connection pool or a rate limit would
        // return `Pending` here, and this crate would stop pulling requests out
        // of the kernel queue until it was ready -- which is backpressure that
        // actually does something.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<IncomingBody>) -> Self::Future {
        self.served += 1;
        let served = self.served;
        Box::pin(async move {
            let target = request.uri().to_string();
            let method = request.method().clone();

            // The connection details HTTP.sys knows travel in the extensions,
            // because `http::Request` has nowhere else to put them.
            let peer = request
                .extensions()
                .get::<winasio_util::ConnectionInfo>()
                .and_then(|info| info.peer_address.as_ref())
                .map(|address| address.to_string())
                .unwrap_or_else(|| "unknown".into());
            println!("{method} {target} from {peer}");

            // `IncomingBody` is an `http_body::Body`, so the usual combinators
            // work. Collecting is a choice this handler makes; a streaming
            // handler would poll frames instead.
            let body = request.into_body().collect().await;

            let mut reply = match (&method, body) {
                (_, Err(e)) => text(
                    StatusCode::BAD_REQUEST,
                    format!("could not read the body: {e}\n"),
                ),
                (&Method::GET, _) => text(StatusCode::OK, format!("you asked for {target}\n")),
                (&Method::POST, Ok(body)) => text(
                    StatusCode::OK,
                    format!("received {} bytes\n", body.to_bytes().len()),
                ),
                (other, _) => text(
                    StatusCode::METHOD_NOT_ALLOWED,
                    format!("{other} is not supported\n"),
                ),
            };
            // An ordinary header. `Content-Length` is not set here: the crate
            // derives it from the body, and would check it against this one if
            // it were.
            reply
                .headers_mut()
                .insert("x-served", served.to_string().parse().unwrap());
            Ok(reply)
        })
    }
}

fn text(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    let mut reply = Response::new(Full::new(Bytes::from(body)));
    *reply.status_mut() = status;
    reply
        .headers_mut()
        .insert("content-type", "text/plain; charset=utf-8".parse().unwrap());
    reply
}

/// Serve `count` requests, then stop. Pass `usize::MAX` to serve forever.
///
/// There is no `unsafe` anywhere in this file.
pub async fn run_server(port: u16, path: &str, count: usize) -> Result<(), Error> {
    // The session owns the subsystem initialisation, because HTTP.sys will not
    // create a session before `HttpInitialize` has run.
    let session = ServerSession::new()?;
    let server = winasio_util::Server::builder(&session)
        .url(&format!("http://localhost:{port}/{path}/"))
        .build(&ThreadPool)?;

    println!("listening on http://localhost:{port}/{path}/");

    let mut service = Echo::default();
    let mut served = 0usize;
    while served < count {
        match server.serve_one(&mut service).await {
            Ok(()) => served += 1,
            // The queue was closed underneath us. Not a fault; stop.
            Err(e) if e.is_queue_closed() => break,
            // An over-large request has already been discarded by the layer
            // below, so carrying on is safe and does not spin.
            Err(Error::RequestTooLarge { .. }) => eprintln!("discarded an over-large request"),
            Err(e) => eprintln!("serving failed: {e}"),
        }
    }

    server.shutdown()?;
    Ok(())
}

#[allow(dead_code)]
fn main() {
    // No runtime. `block_on` parks the thread and retries; the crate needs
    // nothing more.
    if let Err(e) = futures::executor::block_on(run_server(12368, "util", usize::MAX)) {
        eprintln!("server failed: {e}");
        std::process::exit(1);
    }
}
