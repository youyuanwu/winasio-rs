// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Phase 7: the allocation budget.
//!
//! Covers SC-016 (reading metadata allocates nothing), SC-017 (a reply of
//! constants allocates nothing) and SC-018 (an end-to-end serve stays within
//! FR-027's budget and does not grow with header count or body size).
//!
//! # Why the counter is thread-scoped
//!
//! A `#[global_allocator]` sees the whole process. The client runs on its own
//! thread and allocates freely while the measured region is open, so a global
//! counter would measure the test harness rather than the code under test.
//! Counting is therefore per-thread, and only the measuring thread turns it on.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::{block_on, Server};
use winasio::httpsys::{ReceiveConfig, Request, Response, ResponseHeader};

const PORT: u16 = 12364;

thread_local! {
    // `const` initialisers do not allocate, which matters: a lazily-initialised
    // thread local would allocate from inside the allocator.
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }
}

fn note() {
    // `try_with`: during thread-local destruction the cell is gone, and
    // panicking out of the allocator would abort.
    let armed = ARMED.try_with(|a| a.get()).unwrap_or(false);
    if armed {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `f` with allocation counting armed on this thread.
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let value = f();
    ARMED.with(|a| a.set(false));
    (value, COUNT.with(|c| c.get()))
}

/// SC-016: reading a request's raw target and ten header values as bytes
/// performs no allocation.
#[test]
fn reading_request_metadata_does_not_allocate() {
    let server = match Server::start(PORT, "alloc", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    let headers: Vec<(String, String)> = (0..10)
        .map(|i| (format!("X-H{i}"), format!("value-{i}")))
        .collect();
    let borrowed: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    server.client_request("GET", "alloc/target", &borrowed, &[]);
    let request = block_on(server.queue().receive()).expect("receive");

    // Warm up: the I/O layer's live-operation set is a hash set that grows on
    // first use, and a cold first pass would measure that rather than this code.
    read_everything(&request);

    let (total, allocations) = measure(|| read_everything(&request));

    assert!(total > 0, "the request should have been read");
    assert_eq!(
        allocations, 0,
        "reading the target and ten headers must not allocate"
    );
}

/// Touch the raw target and every unrecognised header value, as bytes.
fn read_everything(request: &Request) -> usize {
    let mut total = request.raw_target().len();
    for (name, value) in request.unknown_headers() {
        total += name.len() + value.len();
    }
    total
}

/// SC-017: a reply built entirely from compile-time constants -- reason phrase,
/// ten recognised header values, and unrecognised headers within the inline
/// capacity -- performs no allocation.
#[test]
fn building_a_constant_reply_does_not_allocate() {
    // Warm up any lazily-initialised machinery first.
    drop(build_constant_reply());

    let (reply, allocations) = measure(build_constant_reply);

    assert_eq!(reply.status(), 200);
    assert_eq!(
        allocations, 0,
        "a reply of constants must not allocate; got {allocations}"
    );
}

fn build_constant_reply() -> Response {
    let mut r = Response::new(200);
    r.set_reason(&b"OK"[..])
        .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
        .set_header(ResponseHeader::CACHE_CONTROL, &b"no-cache"[..])
        .set_header(ResponseHeader::CONNECTION, &b"close"[..])
        .set_header(ResponseHeader::DATE, &b"Sat, 02 Aug 2026 00:00:00 GMT"[..])
        .set_header(ResponseHeader::ETAG, &b"\"abc\""[..])
        .set_header(ResponseHeader::LOCATION, &b"/elsewhere"[..])
        .set_header(ResponseHeader::SERVER, &b"winasio"[..])
        .set_header(ResponseHeader::VARY, &b"Accept"[..])
        .set_header(ResponseHeader::AGE, &b"0"[..])
        .set_header(ResponseHeader::ACCEPT_RANGES, &b"bytes"[..])
        // Within INLINE_UNKNOWN_HEADERS, so still allocation-free.
        .add_header(&b"X-One"[..], &b"1"[..])
        .add_header(&b"X-Two"[..], &b"2"[..])
        .add_body(&b"constant body"[..]);
    r
}

/// The documented increments beyond the nominal path.
///
/// FR-027 states what an inline-capacity spill adds. That figure is measured
/// here rather than asserted in prose -- it was originally documented as one and
/// is actually two, because the reply's overflow storage and the contiguous
/// descriptor array HTTP.sys requires are separate allocations.
#[test]
fn the_documented_increments_are_accurate() {
    let server = match Server::start(PORT + 2, "increments", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    // Warm-up passes, discarded.
    serve_with_reply(&server, 2);
    serve_with_reply(&server, 12);

    let inline = serve_with_reply(&server, 2);
    let spilled = serve_with_reply(&server, 12);

    println!(
        "inline={inline} spilled={spilled} increment={}",
        spilled - inline
    );

    assert_eq!(inline, 3, "the nominal path is three allocations");
    assert_eq!(
        spilled - inline,
        2,
        "exceeding INLINE_UNKNOWN_HEADERS costs two: the overflow storage and \
         the contiguous descriptor array. FR-027 and the module documentation \
         must state two, not one"
    );
}

/// Serve one request, replying with `headers` unrecognised headers.
fn serve_with_reply(server: &Server, headers: usize) -> usize {
    let client = server.request("GET", "increments", &[], &[]);
    std::thread::sleep(std::time::Duration::from_millis(150));

    let names: [&'static [u8]; 12] = [
        b"X-A", b"X-B", b"X-C", b"X-D", b"X-E", b"X-F", b"X-G", b"X-H", b"X-I", b"X-J", b"X-K",
        b"X-L",
    ];

    let (_, allocations) = measure(|| {
        let request = block_on(server.queue().receive()).expect("receive");
        let mut reply = Response::new(200);
        for name in names.iter().take(headers) {
            reply.add_header(*name, &b"v"[..]);
        }
        reply.add_body(&b"ok"[..]);
        block_on(server.queue().send(request.id(), reply))
            .0
            .expect("send");
    });

    let _ = client.join();
    allocations
}

/// SC-018: an end-to-end serve stays within FR-027's budget, and the count does
/// not grow with the number of headers read and set, nor with body size at a
/// fixed number of body operations.
#[test]
fn an_end_to_end_serve_stays_within_the_allocation_budget() {
    let server = match Server::start(PORT + 1, "budget", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    // Warm-up pass, discarded.
    serve_once(&server, 2, 16);

    let baseline = serve_once(&server, 2, 16);
    let more_headers = serve_once(&server, 4, 16);
    let bigger_body = serve_once(&server, 2, 32);

    println!(
        "allocations: baseline={baseline} more_headers={more_headers} bigger_body={bigger_body}"
    );

    assert!(
        baseline <= 4,
        "FR-027 budgets four allocations for a serve with no retry and an \
         inline-capacity reply; measured {baseline}"
    );
    assert_eq!(
        baseline, 3,
        "the expected three are the receive operation record, the request \
         metadata buffer and the send operation record. A change here means \
         something new started allocating on the serve path"
    );
    assert_eq!(
        baseline, more_headers,
        "doubling the headers read and set must not change the count"
    );
    assert_eq!(
        baseline, bigger_body,
        "doubling the body at a fixed operation count must not change the count"
    );
}

/// Serve one request, returning how many allocations the measured thread made.
fn serve_once(server: &Server, header_count: usize, body_len: usize) -> usize {
    let headers: Vec<(String, String)> = (0..header_count)
        .map(|i| (format!("X-R{i}"), format!("v{i}")))
        .collect();
    let borrowed: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let client = server.request("GET", "budget", &borrowed, &[]);
    // Let the request reach the queue before the measured region opens, so the
    // client's own work is not counted even in wall-clock terms.
    std::thread::sleep(std::time::Duration::from_millis(150));

    // A body built outside the measured region: FR-027 budgets the metadata
    // cycle, and a caller-provided body is the caller's allocation.
    let body: &'static [u8] = &[b'x'; 64];
    let body = &body[..body_len];

    let (_, allocations) = measure(|| {
        let request = block_on(server.queue().receive()).expect("receive");

        // Read metadata: the target and every unrecognised header.
        let mut seen = request.raw_target().len();
        for (name, value) in request.unknown_headers() {
            seen += name.len() + value.len();
        }
        std::hint::black_box(seen);

        let mut reply = Response::new(200);
        reply
            .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
            .set_header(ResponseHeader::SERVER, &b"winasio"[..])
            .add_body(body);

        block_on(server.queue().send(request.id(), reply))
            .0
            .expect("send");
    });

    let _ = client.join();
    allocations
}
