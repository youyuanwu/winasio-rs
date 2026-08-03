// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! SC-027: throughput and latency for a minimal request/response cycle.
//!
//! Run with `cargo bench -p winasio-tests`.
//!
//! The benchmark needs a bindable URL. Where it cannot get one it reports that
//! and exits cleanly rather than hanging -- which matters because CI's
//! `--all-targets` run executes bench binaries in test mode.

use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use criterion::{criterion_group, criterion_main, Criterion};
use windows::core::HSTRING;

use winasio::httpsys::{
    HttpInitializer, RequestQueue, Response, ResponseHeader, ServerSession, UrlGroup,
};

const PORT: u16 = 12366;

fn block_on<F: Future>(fut: F) -> F::Output {
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |d| RawWaker::new(d, &VTABLE),
        |d| unsafe { (*(d as *const AtomicBool)).store(true, Ordering::SeqCst) },
        |d| unsafe { (*(d as *const AtomicBool)).store(true, Ordering::SeqCst) },
        |_| {},
    );
    let flag = AtomicBool::new(true);
    let raw = RawWaker::new(&flag as *const AtomicBool as *const (), &VTABLE);
    // SAFETY: the vtable only touches the `AtomicBool`, which outlives the waker.
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
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}

/// One client request, returning once the reply has been read.
fn client_roundtrip(port: u16) -> Option<usize> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok()?;
    let req =
        format!("GET /bench/x HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    if buf.is_empty() {
        return None;
    }
    Some(buf.len())
}

fn bench_request_response(c: &mut Criterion) {
    let Ok(_http) = HttpInitializer::new() else {
        eprintln!("skipping benchmark: cannot initialise HTTP.sys");
        return;
    };
    let Ok(session) = ServerSession::new() else {
        eprintln!("skipping benchmark: cannot create a session");
        return;
    };
    let Ok(group) = UrlGroup::new(&session) else {
        eprintln!("skipping benchmark: cannot create a URL group");
        return;
    };
    let Ok(queue) = RequestQueue::new() else {
        eprintln!("skipping benchmark: cannot create a queue");
        return;
    };
    let queue = Arc::new(queue);
    if queue.bind_url_group(&group).is_err() {
        eprintln!("skipping benchmark: cannot bind the group");
        return;
    }
    let url = HSTRING::from(format!("http://localhost:{PORT}/bench/"));
    if let Err(e) = group.add_url(&url) {
        eprintln!("skipping benchmark: cannot bind {url}: {e} (a URL reservation may be needed)");
        return;
    }

    // A worker serving requests for the duration of the benchmark.
    let worker_queue = queue.clone();
    let worker = std::thread::spawn(move || {
        while let Ok(request) = block_on(worker_queue.receive()) {
            let mut reply = Response::new(200);
            reply
                .set_reason(&b"OK"[..])
                .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
                .add_body(&b"ok"[..]);
            if block_on(worker_queue.send(request.id(), reply)).0.is_err() {
                break;
            }
        }
    });

    // Confirm the pair actually works before measuring; otherwise every sample
    // would time out.
    if client_roundtrip(PORT).is_none() {
        eprintln!("skipping benchmark: the listener did not answer");
        let _ = queue.close();
        let _ = worker.join();
        return;
    }

    let mut bench_group = c.benchmark_group("httpsys");
    bench_group.sample_size(50);
    bench_group.bench_function("minimal_request_response", |b| {
        b.iter(|| {
            client_roundtrip(PORT).expect("round trip");
        })
    });
    bench_group.finish();

    let _ = queue.close();
    let _ = worker.join();
}

criterion_group!(benches, bench_request_response);
criterion_main!(benches);
