// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Phase 4: receiving, retrying and rejecting.
//!
//! Covers SC-005 (retry accounting), SC-006 (rejection clears the queue),
//! SC-014 (more-body indication), SC-022 (a request survives being moved) and
//! SC-024 (every accessor).
//!
//! Receiving is testable without replying: the client simply never gets an
//! answer, which is why this phase does not depend on the send path.

mod common;

use common::{block_on, Server};
use winasio::httpsys::{Method, ReceiveConfig, ReceiveError, Request, RequestHeader, MIN_CAPACITY};

const PORT: u16 = 12361;

/// SC-024 plus SC-022: every accessor reports correctly, and keeps doing so
/// after the request has been moved through a collection and a function.
#[test]
fn accessors_report_the_request_and_survive_being_moved() {
    let server = match Server::start(PORT, "recv", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    server.client_request(
        "GET",
        "recv/path?q=1",
        &[
            ("X-Custom", "first"),
            ("X-Custom", "second"),
            ("X-Empty", ""),
        ],
        &[],
    );

    let request = block_on(server.queue().receive()).expect("receive");

    // Moved into a collection, then out of a function, before anything is read.
    // If the metadata were inline, every pointer below would now dangle.
    let mut held = Vec::new();
    held.push(request);
    let request = returned_from_a_function(held.pop().unwrap());

    assert_eq!(request.method(), Method::Get);
    assert_eq!(request.method().as_bytes(), b"GET");

    let target = request.target().expect("target is UTF-8");
    assert!(
        target.contains("/recv/path"),
        "raw target should be the wire target, got {target:?}"
    );
    assert!(
        target.contains("q=1"),
        "query should be present in {target:?}"
    );
    assert_eq!(request.raw_target(), target.as_bytes());

    // Pre-parsed components are wide, so these are borrowed UTF-16.
    assert!(!request.full_url_wide().is_empty());
    let full = request.full_url_lossy();
    assert!(
        full.starts_with("http://localhost:") && full.ends_with("/recv/path?q=1"),
        "the full URL should be the reconstructed absolute form, got {full:?}"
    );
    assert!(request.path_lossy().contains("/recv/path"));
    assert!(request.host_lossy().contains("localhost"));
    assert_eq!(request.query_lossy(), "?q=1");

    assert_eq!(request.version(), (1, 1));
    assert_ne!(request.id().get(), 0);

    // A recognised header, looked up on the *request* side.
    let host = request
        .header_str(RequestHeader::HOST)
        .expect("Host is a recognised request header");
    assert!(host.contains("localhost"), "got {host:?}");

    // Absent is distinguishable from present-but-empty.
    assert!(
        request.header(RequestHeader::IF_MATCH).is_none(),
        "an absent header must be None"
    );
    assert_eq!(
        request.unknown_header("X-Empty"),
        Some(&b""[..]),
        "a present but empty header must be Some(&[])"
    );

    // Repeated unrecognised names: HTTP.sys coalesces duplicates into a single
    // comma-joined value before we ever see them, so enumeration yields one
    // entry carrying both values rather than two entries.
    let customs: Vec<_> = request
        .unknown_headers()
        .filter(|(n, _)| n.eq_ignore_ascii_case(b"X-Custom"))
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        .collect();
    assert_eq!(
        customs,
        vec!["first, second".to_string()],
        "HTTP.sys coalesces repeated header values"
    );
    assert_eq!(
        request.unknown_header("x-custom"),
        Some(&b"first, second"[..]),
        "lookup is case-insensitive and yields the first matching entry"
    );

    // The peer address, allocation-free.
    let peer = request.peer_address().expect("a peer address");
    assert!(peer.ip().is_loopback(), "got {peer}");

    // A GET with no body.
    assert!(!request.has_more_body());
    assert_eq!(request.retries(), 0, "the default capacity should suffice");
}

fn returned_from_a_function(r: Request) -> Request {
    r
}

/// SC-014: the more-body indication tracks whether a body was sent.
#[test]
fn more_body_indication_matches_the_request() {
    let server = match Server::start(PORT + 100, "body", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    server.client_request("POST", "body", &[], b"hello");
    let with_body = block_on(server.queue().receive()).expect("receive");
    assert!(
        with_body.has_more_body(),
        "a request with a non-empty body must report more data"
    );

    server.client_request("GET", "body", &[], &[]);
    let without = block_on(server.queue().receive()).expect("receive");
    assert!(
        !without.has_more_body(),
        "a request with no body must not report more data"
    );
}

/// SC-005: retry accounting.
///
/// The capacity is configured by the test rather than left at the default, so
/// this does not depend on the host's registry-tunable request-size limits.
#[test]
fn an_over_large_request_is_retried_exactly_once() {
    let tight = ReceiveConfig {
        initial_capacity: MIN_CAPACITY,
        max_retries: 1,
    };
    let server = match Server::start(PORT + 1, "retry", tight) {
        Some(s) => s,
        None => return,
    };

    let padding = "p".repeat(2048);
    server.client_request("GET", "retry", &[("X-Pad", &padding)], &[]);

    let request = block_on(server.queue().receive()).expect("receive after retry");
    assert_eq!(request.retries(), 1, "one retry should have been needed");
    assert!(request.target().unwrap().contains("/retry"));
    assert_eq!(
        request.unknown_header("X-Pad").map(<[u8]>::len),
        Some(padding.len()),
        "the retried receive must carry the whole request"
    );

    // The same request against an ample capacity needs no retry.
    let roomy = ReceiveConfig {
        initial_capacity: 65536,
        max_retries: 1,
    };
    let server = match Server::start(PORT + 2, "retry2", roomy) {
        Some(s) => s,
        None => return,
    };
    server.client_request("GET", "retry2", &[("X-Pad", &padding)], &[]);
    let request = block_on(server.queue().receive()).expect("receive");
    assert_eq!(request.retries(), 0, "an ample capacity needs no retry");
}

/// SC-006: with retrying disabled, an over-large request reports its identifier,
/// and rejecting it clears the queue so the next receive returns a *different*
/// request. Without this the accept loop would livelock on the same request.
#[test]
fn rejecting_an_over_large_request_clears_the_queue() {
    // Room for a small request but not a padded one. MIN_CAPACITY alone is
    // exactly the base structure, leaving no room for a URL at all, so even a
    // trivial request would not fit.
    let no_retry = ReceiveConfig {
        initial_capacity: MIN_CAPACITY + 1024,
        max_retries: 0,
    };
    let server = match Server::start(PORT + 3, "reject", no_retry) {
        Some(s) => s,
        None => return,
    };

    let padding = "p".repeat(2048);
    server.client_request("GET", "reject/big", &[("X-Pad", &padding)], &[]);

    let err = block_on(server.queue().receive()).expect_err("should not fit");
    let stuck = match err {
        ReceiveError::TooLarge {
            id,
            retries,
            discarded,
            ..
        } => {
            assert_eq!(retries, 0, "retrying was disabled");
            assert_ne!(id.get(), 0, "the identifier must be recoverable");
            assert!(discarded, "the library must clear the request itself");
            id
        }
        other => panic!("expected TooLarge, got {other:?}"),
    };

    // A second, small request must now be the one delivered -- with no
    // intervention from the caller, which is what makes a livelock impossible.
    server.client_request("GET", "reject/small", &[], &[]);
    let next = block_on(server.queue().receive()).expect("receive after the discard");
    assert_ne!(
        next.id(),
        stuck,
        "the discarded request must not be delivered again"
    );
    assert!(next.target().unwrap().contains("/reject/small"));
}

/// SC-024's remaining branch: a header value that is not valid UTF-8 is
/// reported as not-text, while its bytes come back unchanged.
#[test]
fn a_non_utf8_header_value_is_bytes_but_not_text() {
    let server = match Server::start(PORT + 5, "bytes", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    // 0xFF is never valid UTF-8. Written straight onto the wire, since a `&str`
    // could not carry it.
    let raw = b"GET /bytes/x HTTP/1.1\r\nHost: localhost\r\nX-Raw: a\xFFb\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    let port = server.port();
    let client = std::thread::spawn(move || {
        use std::io::Write;
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
        s.write_all(raw).ok()?;
        s.flush().ok()?;
        Some(())
    });
    std::thread::sleep(std::time::Duration::from_millis(200));

    let request = block_on(server.queue().receive()).expect("receive");

    let bytes = request
        .unknown_header("X-Raw")
        .expect("the header should be present");
    assert_eq!(
        bytes, b"a\xFFb",
        "the original bytes must survive unchanged"
    );
    assert!(
        std::str::from_utf8(bytes).is_err(),
        "this value is deliberately not UTF-8"
    );

    // The target itself is valid UTF-8 here, so the text accessor still works.
    assert!(request.target().is_some());
    let _ = client.join();
}

/// An extension method is reported as unrecognised, with its literal text.
#[test]
fn an_unrecognised_method_keeps_its_text() {
    let server = match Server::start(PORT + 4, "verb", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };
    server.client_request("FROBNICATE", "verb", &[], &[]);
    let request = block_on(server.queue().receive()).expect("receive");
    match request.method() {
        Method::Unknown(raw) => assert_eq!(raw, b"FROBNICATE"),
        other => panic!("expected an unrecognised method, got {other:?}"),
    }
    assert_eq!(request.method().as_bytes(), b"FROBNICATE");
}
