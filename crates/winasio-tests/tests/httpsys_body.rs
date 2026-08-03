// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Phase 6: entity bodies.
//!
//! Covers SC-011 (a large body round-trips), SC-012 (end of body is normal),
//! SC-013 (a failed read returns the buffer), SC-015 (a streamed reply body
//! arrives as the ordered concatenation) and SC-019's body-read arm.

mod common;

use common::{block_on, parse_response, Server};
use winasio::httpsys::{ReceiveConfig, RequestId, Response, ResponseHeader};

const PORT: u16 = 12363;

/// SC-011: a body of at least a megabyte round-trips byte-for-byte, both by
/// repeated chunked reads and by the read-to-end helper.
#[test]
fn a_large_body_round_trips_both_ways() {
    let server = match Server::start(PORT, "body", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    // Distinctive, non-repeating content so a mis-ordered read is visible.
    let payload: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();

    // Chunked reads.
    let client = server.request("POST", "body", &[], &payload);
    let request = block_on(server.queue().receive()).expect("receive");
    assert!(request.has_more_body());

    let id = request.id();
    let mut collected = Vec::with_capacity(payload.len());
    loop {
        let buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
        let outcome = block_on(server.queue().read_body(id, buffer));
        let (result, buffer) = outcome.into_parts();
        let n = result.expect("read");
        if n == 0 {
            break;
        }
        collected.extend_from_slice(&buffer[..n]);
    }
    assert_eq!(collected.len(), payload.len(), "chunked read length");
    assert_eq!(collected, payload, "chunked read content");

    block_on(server.queue().send(id, Response::new(200)))
        .0
        .expect("send");
    let _ = client.join();

    // The read-to-end helper.
    let client = server.request("POST", "body", &[], &payload);
    let request = block_on(server.queue().receive()).expect("receive");
    let whole =
        block_on(server.queue().read_body_to_end(request.id(), 64 * 1024)).expect("read to end");
    assert_eq!(whole, payload, "read_body_to_end content");

    block_on(server.queue().send(request.id(), Response::new(200)))
        .0
        .expect("send");
    let _ = client.join();
}

/// SC-012: end of body is a normal outcome, both past the end of a real body and
/// on a request that never had one.
#[test]
fn end_of_body_is_a_normal_outcome() {
    let server = match Server::start(PORT + 1, "eof", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    // A request with a body: read it, then read again past the end.
    let client = server.request("POST", "eof", &[], b"hello");
    let request = block_on(server.queue().receive()).expect("receive");
    let id = request.id();

    let first = block_on(server.queue().read_body(id, Vec::with_capacity(64)));
    let (n, buffer) = first.into_parts();
    assert_eq!(n.expect("first read"), 5);
    assert_eq!(&buffer[..5], b"hello");

    let past_end = block_on(server.queue().read_body(id, Vec::with_capacity(64)));
    assert_eq!(
        past_end
            .0
            .expect("reading past the end must not be an error"),
        0,
        "end of body is reported as zero bytes, not a failure"
    );

    block_on(server.queue().send(id, Response::new(200)))
        .0
        .expect("send");
    let _ = client.join();

    // A request with no body at all.
    let client = server.request("GET", "eof", &[], &[]);
    let request = block_on(server.queue().receive()).expect("receive");
    assert!(!request.has_more_body());
    let empty = block_on(
        server
            .queue()
            .read_body(request.id(), Vec::with_capacity(64)),
    );
    assert_eq!(
        empty.0.expect("a body-less request must not error"),
        0,
        "no body reports end immediately"
    );
    assert_eq!(
        block_on(server.queue().read_body_to_end(request.id(), 1024)).expect("read to end"),
        Vec::<u8>::new()
    );

    block_on(server.queue().send(request.id(), Response::new(200)))
        .0
        .expect("send");
    let _ = client.join();
}

/// SC-013 and SC-019: a read against an invalid identifier fails as a value and
/// returns the caller's buffer.
#[test]
fn a_failed_read_returns_the_buffer() {
    let server = match Server::start(PORT + 2, "failread", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    let buffer: Vec<u8> = Vec::with_capacity(128);
    let bogus = RequestId::from_raw(0xDEAD_BEEF_DEAD_BEEF);
    let outcome = block_on(server.queue().read_body(bogus, buffer));
    let (result, returned) = outcome.into_parts();

    assert!(result.is_err(), "reading an invalid id must fail");
    assert_eq!(
        returned.capacity(),
        128,
        "the caller's buffer must come back"
    );
}

/// SC-015: a reply body written as several pieces arrives as the exact ordered
/// concatenation.
#[test]
fn a_streamed_reply_body_arrives_in_order() {
    let server = match Server::start(PORT + 3, "stream", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    let client = server.request("GET", "stream", &[], &[]);
    let request = block_on(server.queue().receive()).expect("receive");
    let id = request.id();

    // Headers first, with more data to follow. Content-Length lets the client
    // know when to stop reading.
    let mut head = Response::new(200);
    head.set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
        .set_header(ResponseHeader::CONTENT_LENGTH, &b"18"[..]);
    block_on(server.queue().send_partial(id, head))
        .0
        .expect("send headers");

    let pieces: [&'static [u8]; 3] = [b"first-", b"second-", b"third!"];
    for (i, piece) in pieces.iter().enumerate() {
        let last = i == pieces.len() - 1;
        let outcome = block_on(server.queue().send_body(id, *piece, last));
        outcome
            .0
            .unwrap_or_else(|e| panic!("piece {i} failed: {e}"));
    }

    let (status, _, body) = parse_response(&client.join().unwrap().expect("a reply"));
    assert!(status.contains("200"), "got {status:?}");
    assert_eq!(
        body, b"first-second-third!",
        "streamed pieces must arrive concatenated in order"
    );
}

/// A failed body write returns the buffer too.
#[test]
fn a_failed_body_write_returns_the_buffer() {
    let server = match Server::start(PORT + 4, "failwrite", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    let bogus = RequestId::from_raw(0xDEAD_BEEF_DEAD_BEEF);
    let buffer = b"never sent".to_vec();
    let outcome = block_on(server.queue().send_body(bogus, buffer, true));
    let (result, returned) = outcome.into_parts();

    assert!(result.is_err(), "writing to an invalid id must fail");
    assert_eq!(returned, b"never sent");
}
