// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Phase 5: composing and sending replies.
//!
//! Covers SC-007 (the client observes what was set), SC-008 (constants and owned
//! values are equivalent), SC-009 (a reply survives being moved), SC-010 (a
//! failed send returns the reply) and the invalid-identifier arm of SC-019.

mod common;

use common::{block_on, parse_response, Server};
use winasio::httpsys::{ReceiveConfig, RequestId, Response, ResponseHeader};

const PORT: u16 = 12362;

/// SC-007: status, reason, both header kinds and body all arrive as set.
///
/// `Server` is excluded deliberately: HTTP.sys appends its own product token to
/// whatever the application sets, so that header is never observed verbatim.
#[test]
fn the_client_observes_exactly_what_was_set() {
    let server = match Server::start(PORT, "send", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    let client = server.request("GET", "send", &[], &[]);
    let request = block_on(server.queue().receive()).expect("receive");

    let mut reply = Response::new(418);
    reply
        .set_reason(&b"I am a teapot"[..])
        .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
        .add_header(&b"X-Trace"[..], &b"abc123"[..])
        .add_body(&b"short and stout"[..]);

    let outcome = block_on(server.queue().send(request.id(), reply));
    outcome.0.expect("send");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);

    assert!(status.contains("418"), "status line was {status:?}");
    assert!(
        status.contains("I am a teapot"),
        "reason phrase missing from {status:?}"
    );
    assert_eq!(body, b"short and stout");

    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find("Content-Type"), Some("text/plain"));
    assert_eq!(find("X-Trace"), Some("abc123"));
}

/// SC-008: a reply of compile-time constants and one of owned values produce
/// identical output.
#[test]
fn constant_and_owned_replies_are_equivalent() {
    let server = match Server::start(PORT + 1, "equiv", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    // Built entirely from `&'static` data -- the allocation-free path.
    let client = server.request("GET", "equiv", &[], &[]);
    let request = block_on(server.queue().receive()).expect("receive");
    let mut borrowed = Response::new(200);
    borrowed
        .set_reason(&b"OK"[..])
        .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
        .add_header(&b"X-Kind"[..], &b"const"[..])
        .add_body(&b"payload"[..]);
    block_on(server.queue().send(request.id(), borrowed))
        .0
        .expect("send");
    let (status_a, headers_a, body_a) = parse_response(&client.join().unwrap().unwrap());

    // The same reply built from owned values.
    let client = server.request("GET", "equiv", &[], &[]);
    let request = block_on(server.queue().receive()).expect("receive");
    let mut owned = Response::new(200);
    owned
        .set_reason(b"OK".to_vec())
        .set_header(ResponseHeader::CONTENT_TYPE, b"text/plain".to_vec())
        .add_header(b"X-Kind".to_vec(), b"const".to_vec())
        .add_body(b"payload".to_vec());
    block_on(server.queue().send(request.id(), owned))
        .0
        .expect("send");
    let (status_b, headers_b, body_b) = parse_response(&client.join().unwrap().unwrap());

    assert_eq!(status_a, status_b, "status lines must match");
    assert_eq!(body_a, body_b, "bodies must match");

    // Byte-identical apart from headers the operating system generates itself.
    let normalise = |hs: &[(String, String)]| {
        let mut v: Vec<(String, String)> = hs
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("Date"))
            .map(|(k, val)| (k.to_ascii_lowercase(), val.clone()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        normalise(&headers_a),
        normalise(&headers_b),
        "every header must match; only Date may differ, because HTTP.sys sets it"
    );
}

/// SC-009: a fully-built reply can be moved before being sent.
///
/// This is what the build-pointers-in-`operate` design buys: nothing inside the
/// reply points at the reply until it has reached its final address.
#[test]
fn a_reply_can_be_moved_before_sending() {
    let server = match Server::start(PORT + 2, "moved", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };
    let client = server.request("GET", "moved", &[], &[]);
    let request = block_on(server.queue().receive()).expect("receive");

    let mut reply = Response::new(201);
    reply
        .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
        .add_header(&b"X-Moved"[..], &b"yes"[..])
        .add_body(&b"relocated"[..]);

    // Move it through a collection and a function before sending.
    let mut held = vec![reply];
    let reply = moved_through(held.pop().unwrap());
    let boxed = Box::new(reply);

    block_on(server.queue().send(request.id(), *boxed))
        .0
        .expect("send");

    let (status, headers, body) = parse_response(&client.join().unwrap().unwrap());
    assert!(status.contains("201"), "got {status:?}");
    assert_eq!(body, b"relocated");
    assert!(headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("X-Moved") && v == "yes"));
}

fn moved_through(r: Response) -> Response {
    r
}

/// SC-010 and SC-019: a send for an identifier that is not valid fails as a
/// value, and hands the reply back rather than consuming it.
#[test]
fn a_failed_send_returns_the_reply() {
    let server = match Server::start(PORT + 3, "failsend", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    let mut reply = Response::new(200);
    reply.add_body(&b"never sent"[..]);

    // No such request.
    let bogus = RequestId::from_raw(0xDEAD_BEEF_DEAD_BEEF);
    let outcome = block_on(server.queue().send(bogus, reply));
    let (result, returned) = outcome.into_parts();

    assert!(result.is_err(), "sending to an invalid id must fail");
    // The reply came back intact and is still usable.
    assert_eq!(returned.status(), 200);
    assert_eq!(returned.body_len(), 10);
}

/// A reply with more unrecognised headers than the inline capacity still works;
/// it just costs an allocation, which FR-027 documents.
#[test]
fn spilling_past_the_inline_capacity_still_sends() {
    let server = match Server::start(PORT + 4, "spill", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };
    let client = server.request("GET", "spill", &[], &[]);
    let request = block_on(server.queue().receive()).expect("receive");

    let mut reply = Response::new(200);
    // Deliberately past `INLINE_UNKNOWN_HEADERS`.
    for i in 0..12u8 {
        reply.add_header(format!("X-N{i}").into_bytes(), b"v".to_vec());
    }
    reply.add_body(&b"spilled"[..]);

    block_on(server.queue().send(request.id(), reply))
        .0
        .expect("send");

    let (_, headers, body) = parse_response(&client.join().unwrap().unwrap());
    assert_eq!(body, b"spilled");
    let count = headers
        .iter()
        .filter(|(k, _)| k.to_ascii_lowercase().starts_with("x-n"))
        .count();
    assert_eq!(count, 12, "every spilled header must still be sent");
}
