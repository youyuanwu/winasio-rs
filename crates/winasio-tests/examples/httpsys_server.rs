// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

// A complete HTTP.sys server, written entirely in safe code.
//
// Run it directly:
//
// ```text
// cargo run -p winasio-tests --example httpsys_server
// ```
//
// then `curl http://localhost:12367/example/anything`.
//
// The crate deliberately leaves the accept loop to the caller, so this is what
// one looks like. It is also compiled and executed by the test suite, which is
// what keeps it honest.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use windows::core::{Result, HSTRING};

use winasio::httpsys::{
    HttpInitializer, Method, ReceiveError, RequestQueue, Response, ResponseHeader, ServerSession,
    UrlGroup,
};

/// Serve `count` requests, then stop. Pass `usize::MAX` to serve forever.
///
/// There is no `unsafe` anywhere in this function: reading the request and
/// composing the reply are entirely safe operations.
pub fn run_server(port: u16, path: &str, count: usize) -> Result<()> {
    let _http = HttpInitializer::new()?;
    let session = ServerSession::new()?;
    let group = UrlGroup::new(&session)?;

    let queue = Arc::new(RequestQueue::new()?);
    queue.bind_url_group(&group)?;
    group.add_url(&HSTRING::from(format!("http://localhost:{port}/{path}/")))?;

    println!("listening on http://localhost:{port}/{path}/");

    let mut served = 0usize;
    while served < count {
        let request = match block_on(queue.receive()) {
            Ok(r) => r,
            // Did not fit even after retrying. Clear it, or the next receive
            // would return the very same request forever.
            Err(ReceiveError::TooLarge { id, .. }) => {
                eprintln!("rejecting an over-large request");
                let _ = block_on(queue.reject(id));
                continue;
            }
            // The queue was closed, or the operation was cancelled.
            Err(ReceiveError::Failed(_)) => break,
        };

        // Read the request. Every accessor borrows from the request's own
        // buffer, so none of this allocates.
        let target = request.target().unwrap_or("<not utf-8>").to_string();
        let peer = request
            .peer_address()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".into());
        println!(
            "{} {target} from {peer}",
            String::from_utf8_lossy(request.method().as_bytes())
        );

        // Read a body, if there is one.
        let body = if request.has_more_body() {
            block_on(queue.read_body_to_end(request.id(), 64 * 1024)).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Compose a reply. Values that are compile-time constants cost nothing.
        let mut reply = Response::new(200);
        reply
            .set_reason(&b"OK"[..])
            .set_header(
                ResponseHeader::CONTENT_TYPE,
                &b"text/plain; charset=utf-8"[..],
            )
            .add_header(&b"X-Powered-By"[..], &b"winasio"[..]);

        match request.method() {
            Method::Get => {
                reply.add_body(format!("you asked for {target}\n").into_bytes());
            }
            Method::Post => {
                reply.add_body(format!("received {} bytes\n", body.len()).into_bytes());
            }
            other => {
                let name = String::from_utf8_lossy(other.as_bytes()).into_owned();
                reply
                    .set_status(405)
                    .set_reason(&b"Method Not Allowed"[..])
                    .add_body(format!("{name} is not supported\n").into_bytes());
            }
        }

        // The reply comes back whether or not the send succeeded.
        let outcome = block_on(queue.send(request.id(), reply));
        if let Err(e) = outcome.0 {
            eprintln!("send failed: {e}");
        }

        served += 1;
    }

    queue.close()?;
    Ok(())
}

/// A minimal executor, so the example depends on no async runtime.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |d| RawWaker::new(d, &VTABLE),
        |d| unsafe { (*(d as *const AtomicBool)).store(true, Ordering::SeqCst) },
        |d| unsafe { (*(d as *const AtomicBool)).store(true, Ordering::SeqCst) },
        |_| {},
    );

    let flag = AtomicBool::new(true);
    let raw = RawWaker::new(&flag as *const AtomicBool as *const (), &VTABLE);
    // SAFETY: the vtable only touches the `AtomicBool`, which outlives the waker
    // because the future is driven to completion before this returns.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = fut;
    // SAFETY: `fut` lives on this stack frame and is never moved again.
    let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };

    loop {
        if flag.swap(false, Ordering::SeqCst) {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[allow(dead_code)]
fn main() {
    if let Err(e) = run_server(12367, "example", usize::MAX) {
        eprintln!("server failed: {e}");
        std::process::exit(1);
    }
}
