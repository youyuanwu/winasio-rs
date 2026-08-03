// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! End-to-end HTTP.sys coverage, and the example server.
//!
//! Covers SC-001: a complete server -- set up, receive, interpret, reply -- is
//! written with no `unsafe` in its own code, and is executed here rather than
//! merely compiled. `cargo test --all-targets` builds `examples/` but does not
//! run them, so the example is included and driven directly.

mod common;

use common::{block_on, parse_response, send_raw, Server};
use winasio::httpsys::{Method, ReceiveConfig, Response, ResponseHeader};

/// The example server, compiled into this test binary so it can be run.
#[allow(dead_code)]
mod example {
    include!("../examples/httpsys_server.rs");
}

const PORT: u16 = 12356;
const EXAMPLE_PORT: u16 = 12367;

/// SC-001: the example server really serves, and contains no `unsafe` at all.
///
/// The example is checked for `unsafe` textually below, so this is not merely a
/// claim in a comment.
#[test]
fn the_example_server_serves_requests() {
    // Serve exactly three requests, then stop.
    let server = std::thread::spawn(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(example::run_server(EXAMPLE_PORT, "example", 3))
    });

    // Give the listener time to bind. If it cannot (no URL reservation), the
    // client requests simply fail and the test skips.
    std::thread::sleep(std::time::Duration::from_millis(600));

    let get = send_raw(EXAMPLE_PORT, "GET", "example/hello", &[], &[]);
    let Some(get) = get else {
        eprintln!("skipping: the example could not bind {EXAMPLE_PORT}");
        return;
    };
    let (status, headers, body) = parse_response(&get);
    assert!(status.contains("200"), "got {status:?}");
    assert!(
        String::from_utf8_lossy(&body).contains("/example/hello"),
        "body was {:?}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("X-Powered-By") && v == "winasio"),
        "the example's own header should be present"
    );

    let post = send_raw(EXAMPLE_PORT, "POST", "example/data", &[], b"twelve bytes").unwrap();
    let (_, _, body) = parse_response(&post);
    assert!(
        String::from_utf8_lossy(&body).contains("received 12 bytes"),
        "the example should read the body, got {:?}",
        String::from_utf8_lossy(&body)
    );

    let odd = send_raw(EXAMPLE_PORT, "DELETE", "example/x", &[], &[]).unwrap();
    let (status, _, _) = parse_response(&odd);
    assert!(status.contains("405"), "got {status:?}");

    server.join().expect("example thread").expect("example ran");
}

/// SC-001's other half: the example contains no `unsafe` whatsoever.
///
/// Asserted textually rather than trusted, because the whole point of the
/// criterion is that a complete server needs none.
#[test]
fn the_example_server_contains_no_unsafe() {
    let source = include_str!("../examples/httpsys_server.rs");
    for (n, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !code.contains("unsafe"),
            "examples/httpsys_server.rs:{} uses `unsafe`: {line}",
            n + 1
        );
    }
}

/// A full request/response cycle over the public API, with no `unsafe` anywhere
/// in this test -- which is the point. The API this replaces required the caller
/// to dereference a kernel-written pointer by hand just to read the URL.
#[test]
fn a_complete_cycle_uses_only_safe_code() {
    let server = match Server::start(PORT, "e2e", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    let client = server.request(
        "POST",
        "e2e/resource?x=1",
        &[("X-Request-Id", "abc"), ("Content-Type", "text/plain")],
        b"request body",
    );

    let request = block_on(server.queue().receive()).expect("receive");

    assert_eq!(request.method(), Method::Post);
    assert!(request.target().unwrap().contains("/e2e/resource"));
    assert_eq!(request.query_lossy(), "?x=1");
    assert_eq!(request.unknown_header("X-Request-Id"), Some(&b"abc"[..]));
    assert_eq!(request.version(), (1, 1));
    assert!(request.peer_address().is_some());

    let body = block_on(server.queue().read_body_to_end(request.id(), 4096)).expect("body");
    assert_eq!(body, b"request body");

    let mut reply = Response::new(201);
    reply
        .set_reason(&b"Created"[..])
        .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
        .set_header(ResponseHeader::LOCATION, &b"/e2e/resource/1"[..])
        .add_header(&b"X-Request-Id"[..], b"abc".to_vec())
        .add_body(&b"created\n"[..]);

    block_on(server.queue().send(request.id(), reply))
        .0
        .expect("send");

    let (status, headers, body) = parse_response(&client.join().unwrap().expect("a reply"));
    assert!(status.contains("201"), "got {status:?}");
    assert!(status.contains("Created"));
    assert_eq!(body, b"created\n");

    let find = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find("Location"), Some("/e2e/resource/1"));
    assert_eq!(find("X-Request-Id"), Some("abc"));
    assert_eq!(find("Content-Type"), Some("text/plain"));
}

/// A serve loop keeps working across many sequential requests, which is the
/// shape every real caller will write.
#[test]
fn a_serve_loop_handles_many_requests_in_sequence() {
    let server = match Server::start(PORT + 1, "loop", ReceiveConfig::default()) {
        Some(s) => s,
        None => return,
    };

    for i in 0..25u32 {
        let client = server.request("GET", &format!("loop/{i}"), &[], &[]);
        let request = block_on(server.queue().receive()).expect("receive");

        let mut reply = Response::new(200);
        reply.add_body(request.raw_target().to_vec());
        block_on(server.queue().send(request.id(), reply))
            .0
            .expect("send");

        let (_, _, body) = parse_response(&client.join().unwrap().expect("a reply"));
        assert_eq!(String::from_utf8_lossy(&body), format!("/loop/{i}"));
    }
}
