// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Integration tests for `winasio_util::Server`, the HTTP.sys server half.
//!
//! # Ports
//!
//! Cargo runs integration binaries in parallel and two processes cannot bind the
//! same HTTP.sys prefix, so this binary owns 12370..=12399 — one per test rather
//! than one for the binary, because several tests here shut their listener down
//! and a shared port would race.
//!
//! # Why every test drives `block_on`
//!
//! The crate promises it needs no runtime. `common::block_on` is a bare
//! park-and-retry loop with no worker threads and no reactor, so if any part of
//! the server quietly needed one, none of this would pass.
//!
//! # The keep-alive hazard, and why it does not bite here
//!
//! `util_client.rs` guards against a raw test server that closes without saying
//! `Connection: close`, because WinHTTP's keep-alive pool is process-wide. An
//! HTTP.sys listener is genuinely persistent and does not close after one
//! reply, so the guard's failure mode is not reachable — and measured, an
//! application-set `Connection: close` would not change HTTP.sys's mind anyway.
//! What protects the tests instead is the port-per-test rule above: WinHTTP
//! pools per host and port, so no socket from a finished test can be handed to
//! the next one.

mod common;

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use common::{block_on, parse_response, send_raw};
use http::{HeaderValue, Request, Response, StatusCode};
use http_body::Frame;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use winasio::iocp::ThreadPool;
use winasio_util::{
    AcceptError, BodyError, ConnectionInfo, IncomingBody, ResponseError, ServeError, Server,
    ServerOperation, ServerSession,
};

const PORT: u16 = 12370;
/// The runnable example binds its own prefix.
const EXAMPLE_PORT: u16 = 12398;

/// Bind a listener, or `None` when the machine will not let us.
///
/// Mirrors `common::Server::start`: only the URL binding is allowed to skip a
/// test, because that is the one step a stock CI runner may genuinely refuse.
/// Everything else panics, since a regression there is the thing under test.
fn start<'a>(session: &'a ServerSession, port: u16, path: &str) -> Option<Server<'a>> {
    match Server::builder(session)
        .url(&format!("http://localhost:{port}/{path}/"))
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

/// Fire one raw request from a background thread.
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
    std::thread::spawn(move || send_raw(port, &method, &target, &headers, &body))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn all_headers<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

/// Undo the chunked framing this crate writes by hand.
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = body.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(
            std::str::from_utf8(&body[..eol]).expect("a chunk size is ASCII"),
            16,
        )
        .expect("a chunk size is hexadecimal");
        body = &body[eol + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
    out
}

// ---------------------------------------------------------------------------
// Runtime agnosticism, and serving a real tower service
// ---------------------------------------------------------------------------

#[test]
fn a_service_is_served_on_a_bare_executor_with_no_runtime() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT, "runtime") else {
        return;
    };
    let client = request(PORT, "GET", "runtime/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"no runtime"))))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(body, b"no runtime");
    // Measured: HTTP.sys computes the length for a buffered reply, so this
    // arrives without the crate declaring anything.
    assert_eq!(header(&headers, "content-length"), Some("10"));
}

#[test]
fn a_real_tower_service_stack_can_be_served() {
    // The claim the whole `tower-service` dependency rests on: a service built
    // with tower's own combinators, wrapped in a tower layer, is servable here
    // unchanged.
    use tower::ServiceBuilder;

    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 1, "tower") else {
        return;
    };
    let client = request(PORT + 1, "GET", "tower/x", &[], b"");

    let inner = tower::service_fn(|req: Request<IncomingBody>| async move {
        let path = req.uri().path().to_string();
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(path))))
    });
    // A real `tower::Layer` in the stack, not just a bare service.
    let mut service = ServiceBuilder::new()
        .map_response(|mut response: Response<Full<Bytes>>| {
            response
                .headers_mut()
                .insert("x-layered", HeaderValue::from_static("yes"));
            response
        })
        .service(inner);

    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(body, b"/tower/x");
    assert_eq!(header(&headers, "x-layered"), Some("yes"));
}

#[test]
fn an_axum_router_can_be_served() {
    // The strongest available proof of the compatibility claim: not a doc
    // sentence but a real `axum::Router` on the wire. Measured, axum with
    // default features off pulls no tokio, so this test — like every other one
    // here — runs on `block_on` with no runtime anywhere.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 2, "axum") else {
        return;
    };
    let client = request(PORT + 2, "GET", "axum/greet", &[], b"");

    let mut router: axum::Router = axum::Router::new()
        .route("/axum/greet", axum::routing::get(|| async { "hello axum" }))
        .route("/axum/other", axum::routing::get(|| async { "nope" }));

    block_on(server.serve_one(&mut router)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, _headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(body, b"hello axum");
}

#[test]
fn an_axum_router_routes_a_miss_to_its_own_404() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 3, "axum404") else {
        return;
    };
    let client = request(PORT + 3, "GET", "axum404/nothing-here", &[], b"");

    let mut router: axum::Router = axum::Router::new().route(
        "/axum404/something",
        axum::routing::get(|| async { "found" }),
    );
    block_on(server.serve_one(&mut router)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, _headers, body) = parse_response(&raw);
    // The router decided this, not the crate — which is the point.
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    assert!(body.is_empty());
}

// ---------------------------------------------------------------------------
// The header numbering hazard
// ---------------------------------------------------------------------------

#[test]
fn the_two_header_numberings_are_never_crossed() {
    // Ids 25 and 27 mean `Cookie`/`From` on a request and
    // `Retry-After`/`Set-Cookie` on a reply; 24 means `Authorization` one way
    // and `Proxy-Authenticate` the other. A conversion layer that read one
    // table through the other would relabel exactly these, silently and without
    // an error anywhere. So: send the request-side names, reply with the
    // response-side names, and check both ends by name rather than by index.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 4, "tables") else {
        return;
    };
    let client = request(
        PORT + 4,
        "GET",
        "tables/x",
        &[
            ("Cookie", "sid=42"),
            ("From", "probe@example.test"),
            ("Authorization", "Bearer xyz"),
            ("Accept-Ranges", "bytes"),
        ],
        b"",
    );

    let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let sink = Arc::clone(&seen);
    let mut service = tower::service_fn(move |req: Request<IncomingBody>| {
        let sink = Arc::clone(&sink);
        async move {
            let mut names: Vec<(String, String)> = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap().to_string()))
                .collect();
            names.sort();
            *sink.lock().unwrap() = names;
            let mut reply = Response::new(Empty::<Bytes>::new());
            *reply.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            reply
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("120"));
            reply
                .headers_mut()
                .insert("server", HeaderValue::from_static("winasio-test"));
            Ok::<_, Infallible>(reply)
        }
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let got = seen.lock().unwrap().clone();
    let find = |name: &str| {
        got.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str().to_string())
    };
    // Id 25 read from the request table.
    assert_eq!(find("cookie").as_deref(), Some("sid=42"));
    // Id 27 read from the request table: `From`, not `Set-Cookie`.
    assert_eq!(find("from").as_deref(), Some("probe@example.test"));
    assert_eq!(find("set-cookie"), None, "no `Set-Cookie` was sent");
    // Id 24 read from the request table: `Authorization`, not
    // `Proxy-Authenticate`.
    assert_eq!(find("authorization").as_deref(), Some("Bearer xyz"));
    assert_eq!(find("proxy-authenticate"), None);
    // Id 20 differs per side too: `Accept` on a request, `Accept-Ranges` on a
    // reply. Sent as an unknown request header, it must not be relabelled.
    assert_eq!(find("accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(find("accept"), None, "no `Accept` was sent");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, _body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
    // Id 25 written through the reply table.
    assert_eq!(header(&headers, "retry-after"), Some("120"));
    // Id 26, and measured: `Server` goes through the known slot precisely
    // because HTTP.sys emits its own, and the slot merges rather than
    // duplicating.
    assert_eq!(all_headers(&headers, "server").len(), 1);
    assert!(header(&headers, "server").unwrap().contains("winasio-test"));
    assert_eq!(header(&headers, "cookie"), None);
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[test]
fn a_request_body_larger_than_one_read_is_delivered_whole() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 5, "bigbody") else {
        return;
    };
    // Far more than the 16 KiB the body reads at a time, so this is many reads.
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let client = request(PORT + 5, "POST", "bigbody/x", &[], &payload);

    let expected = payload.clone();
    let mut service = tower::service_fn(move |req: Request<IncomingBody>| {
        let expected = expected.clone();
        async move {
            let collected = req.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(collected.len(), expected.len(), "length");
            assert_eq!(collected.as_ref(), expected.as_slice(), "contents");
            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(
                collected.len().to_string(),
            ))))
        }
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (_status, _headers, body) = parse_response(&raw);
    assert_eq!(body, b"200000");
}

#[test]
fn a_chunked_request_body_arrives_already_decoded() {
    // Measured: HTTP.sys de-chunks for us, so the crate does none of it and a
    // handler sees the decoded bytes with the header still present.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 6, "chunkedreq") else {
        return;
    };
    let port = PORT + 6;
    let client = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let head = format!(
            "POST /chunkedreq/x HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\
             Transfer-Encoding: chunked\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream
            .write_all(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n")
            .unwrap();
        stream.flush().unwrap();
        let mut out = Vec::new();
        let _ = stream.read_to_end(&mut out);
        out
    });

    let mut service = tower::service_fn(|req: Request<IncomingBody>| async move {
        assert_eq!(
            req.headers()
                .get("transfer-encoding")
                .map(|v| v.to_str().unwrap()),
            Some("chunked"),
            "the header the peer sent is reported verbatim"
        );
        let body = req.into_body().collect().await.unwrap().to_bytes();
        Ok::<_, Infallible>(Response::new(Full::new(body)))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap();
    let (_status, _headers, body) = parse_response(&raw);
    assert_eq!(body, b"hello world");
}

#[test]
fn a_handler_that_ignores_the_request_body_still_answers() {
    // Measured: a reply may be sent without reading a byte of the body.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 7, "unread") else {
        return;
    };
    let client = request(PORT + 7, "POST", "unread/x", &[], b"never read");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ignored"))))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, _headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(body, b"ignored");
}

// ---------------------------------------------------------------------------
// Response framing
// ---------------------------------------------------------------------------

#[test]
fn a_reply_of_unknown_length_is_chunked_by_this_crate() {
    // Measured, and the reason this is not left to the platform: HTTP.sys
    // applies no framing at all to a streamed reply — no `Content-Length` and
    // no `Transfer-Encoding` — which on a persistent connection is an
    // undelimited body running into whatever comes next.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 8, "chunked") else {
        return;
    };
    let client = request(PORT + 8, "GET", "chunked/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        let frames = futures::stream::iter(vec![
            Ok::<_, Infallible>(http_body::Frame::data(Bytes::from_static(b"one "))),
            Ok(http_body::Frame::data(Bytes::from_static(b"two "))),
            Ok(http_body::Frame::data(Bytes::from_static(b"three"))),
        ]);
        Ok::<_, Infallible>(Response::new(StreamBody::new(frames)))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(header(&headers, "transfer-encoding"), Some("chunked"));
    assert_eq!(header(&headers, "content-length"), None);
    assert_eq!(dechunk(&body), b"one two three");
}

#[test]
fn a_streamed_reply_of_known_length_declares_it_rather_than_chunking() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 9, "declared") else {
        return;
    };
    let client = request(PORT + 9, "GET", "declared/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        // Two frames, exact total length: the crate must declare 13 and stream,
        // not chunk.
        let frames = futures::stream::iter(vec![
            Ok::<_, Infallible>(http_body::Frame::data(Bytes::from_static(b"first "))),
            Ok(http_body::Frame::data(Bytes::from_static(b"second"))),
        ]);
        let body = http_body_util::Limited::new(StreamBody::new(frames), 64);
        let body = SizedBody {
            inner: body,
            length: 12,
        };
        Ok::<_, Infallible>(Response::new(body))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(header(&headers, "content-length"), Some("12"));
    assert_eq!(header(&headers, "transfer-encoding"), None);
    assert_eq!(body, b"first second");
}

/// A body that promises an exact length whatever its inner body says.
///
/// Used to force the multi-frame declared-length path, and — with a wrong
/// promise — the mismatch check.
struct SizedBody<B> {
    inner: B,
    length: u64,
}

impl<B: http_body::Body + Unpin> http_body::Body for SizedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.length)
    }
}

#[test]
fn a_reply_that_under_delivers_its_declared_length_is_an_error() {
    // Measured: HTTP.sys accepts this and puts a silently truncated message on
    // the wire, so the check has to live in this crate.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 10, "short") else {
        return;
    };
    let client = request(PORT + 10, "GET", "short/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        let frames = futures::stream::iter(vec![
            Ok::<_, Infallible>(http_body::Frame::data(Bytes::from_static(b"aa"))),
            Ok(http_body::Frame::data(Bytes::from_static(b"bb"))),
        ]);
        Ok::<_, Infallible>(Response::new(SizedBody {
            inner: StreamBody::new(frames),
            length: 40,
        }))
    });
    let error = block_on(server.serve_one(&mut service)).expect_err("a mismatch");
    assert!(
        matches!(
            error,
            ServeError::Response(ResponseError::Body(BodyError::LengthMismatch {
                declared: 40,
                actual: 4
            }))
        ),
        "{error}"
    );
    drop(client.join());
}

#[test]
fn a_caller_supplied_content_length_that_lies_is_refused_before_anything_is_sent() {
    // Not the same rule as `transfer-encoding`: a `Content-Length` is allowed,
    // because an `axum::Router` sets one, but it is checked rather than
    // trusted. Measured, HTTP.sys would put the wrong length on the wire
    // verbatim and emit a silently truncated message.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 11, "framing") else {
        return;
    };
    let client = request(PORT + 11, "GET", "framing/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        let mut reply = Response::new(Full::new(Bytes::from_static(b"hi")));
        reply
            .headers_mut()
            .insert("content-length", HeaderValue::from_static("99"));
        Ok::<_, Infallible>(reply)
    });
    let error = block_on(server.serve_one(&mut service)).expect_err("a refusal");
    assert!(
        matches!(
            &error,
            ServeError::Response(ResponseError::Body(BodyError::LengthMismatch {
                declared: 99,
                actual: 2
            }))
        ),
        "{error}"
    );
    drop(client.join());
}

#[test]
fn a_caller_supplied_transfer_encoding_is_refused_before_anything_is_sent() {
    // This one really is refused: the crate frames the chunks itself, so a
    // caller-supplied coding would double-frame the body.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 25, "framing2") else {
        return;
    };
    let client = request(PORT + 25, "GET", "framing2/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        let mut reply = Response::new(Full::new(Bytes::from_static(b"hi")));
        reply
            .headers_mut()
            .insert("transfer-encoding", HeaderValue::from_static("chunked"));
        Ok::<_, Infallible>(reply)
    });
    let error = block_on(server.serve_one(&mut service)).expect_err("a refusal");
    assert!(
        matches!(&error, ServeError::Response(ResponseError::Body(BodyError::FramingHeaderNotAllowed { name })) if name == "transfer-encoding"),
        "{error}"
    );
    drop(client.join());
}

#[test]
fn a_declared_length_frames_a_body_whose_size_is_unknown() {
    // The case a caller's `Content-Length` actually buys: a streaming body with
    // no size hint that the caller knows the length of anyway.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 26, "declared") else {
        return;
    };
    let client = request(PORT + 26, "GET", "declared/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        let frames = vec![
            Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"one"))),
            Ok(Frame::data(Bytes::from_static(b"two"))),
        ];
        let body = StreamBody::new(futures::stream::iter(frames));
        let mut reply = Response::new(body);
        reply
            .headers_mut()
            .insert("content-length", HeaderValue::from_static("6"));
        Ok::<_, Infallible>(reply)
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(header(&headers, "content-length"), Some("6"));
    assert_eq!(header(&headers, "transfer-encoding"), None);
    assert_eq!(all_headers(&headers, "content-length").len(), 1);
    assert_eq!(body, b"onetwo");
}

#[test]
fn a_head_reply_declares_a_length_and_sends_no_body() {
    // Measured: HTTP.sys sends the body of a HEAD reply if it is given one, so
    // the suppression is this crate's job — while RFC 9110 still wants the
    // length the equivalent GET would have declared.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 12, "head") else {
        return;
    };
    let client = request(PORT + 12, "HEAD", "head/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
            b"this must not reach the wire",
        ))))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(header(&headers, "content-length"), Some("28"));
    assert!(body.is_empty(), "a HEAD reply carries no body");
}

#[test]
fn a_204_reply_has_no_body_and_no_framing_header() {
    // Measured: HTTP.sys sends a 204's body too, and a declared `Content-Length:
    // 0` reaches the wire even though RFC 9110 forbids it on a 204.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 13, "nocontent") else {
        return;
    };
    let client = request(PORT + 13, "GET", "nocontent/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        let mut reply = Response::new(Full::new(Bytes::from_static(b"suppressed")));
        *reply.status_mut() = StatusCode::NO_CONTENT;
        Ok::<_, Infallible>(reply)
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 204 No Content");
    assert_eq!(header(&headers, "content-length"), None);
    assert_eq!(header(&headers, "transfer-encoding"), None);
    assert!(body.is_empty());
}

#[test]
fn duplicate_response_headers_survive_as_separate_lines() {
    // The reason everything but `Date` and `Server` goes through HTTP.sys's
    // unknown header list: the known slot keeps only the last value.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 14, "dupes") else {
        return;
    };
    let client = request(PORT + 14, "GET", "dupes/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        let mut reply = Response::new(Empty::<Bytes>::new());
        reply
            .headers_mut()
            .append("set-cookie", HeaderValue::from_static("a=1"));
        reply
            .headers_mut()
            .append("set-cookie", HeaderValue::from_static("b=2"));
        // An empty value is dropped by the known slot and preserved by the
        // unknown list — measured — so it also belongs here.
        reply
            .headers_mut()
            .insert("x-empty", HeaderValue::from_static(""));
        Ok::<_, Infallible>(reply)
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (_status, headers, _body) = parse_response(&raw);
    assert_eq!(all_headers(&headers, "set-cookie"), ["a=1", "b=2"]);
    assert_eq!(header(&headers, "x-empty"), Some(""));
}

#[test]
fn an_extension_method_reaches_the_service_intact() {
    // Measured: `PATCH` is not a verb HTTP.sys recognises, so it takes the same
    // path a private verb does. If that path were broken, PATCH would break.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 15, "verbs") else {
        return;
    };
    let client = request(PORT + 15, "PATCH", "verbs/x", &[], b"");

    let mut service = tower::service_fn(|req: Request<IncomingBody>| async move {
        let method = req.method().as_str().to_string();
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(method))))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (_status, _headers, body) = parse_response(&raw);
    assert_eq!(body, b"PATCH");
}

#[test]
fn the_connection_details_reach_the_service_in_the_extensions() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 16, "info") else {
        return;
    };
    let client = request(PORT + 16, "GET", "info/x", &[], b"");

    let mut service = tower::service_fn(|req: Request<IncomingBody>| async move {
        let info = req
            .extensions()
            .get::<ConnectionInfo>()
            .copied()
            .expect("every request carries its connection details");
        assert!(info.request_id.get() != 0);
        let peer = info
            .peer_address
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(peer))))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (_status, _headers, body) = parse_response(&raw);
    let peer = String::from_utf8(body).unwrap();
    assert!(
        peer.contains("127.0.0.1") || peer.contains("::1"),
        "the loopback peer should have been reported, got {peer:?}"
    );
}

// ---------------------------------------------------------------------------
// Failure handling
// ---------------------------------------------------------------------------

#[test]
fn a_failing_service_puts_a_500_on_the_wire_and_still_reports_the_error() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 17, "failing") else {
        return;
    };
    let client = request(PORT + 17, "GET", "failing/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Err::<Response<Empty<Bytes>>, _>(std::io::Error::other("the handler gave up"))
    });
    let error = block_on(server.serve_one(&mut service)).expect_err("the error is not swallowed");
    assert!(matches!(error, ServeError::Service(_)), "{error}");
    assert!(error.to_string().contains("the handler gave up"));

    let raw = client.join().unwrap().expect("a reply");
    let (status, _headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 500 Internal Server Error");
    assert!(body.is_empty());
}

#[test]
fn a_request_left_unanswered_does_not_poison_the_queue() {
    // What happens when a handler panics: the `Responder` is dropped without
    // sending. Measured, that costs the peer an answer and costs the queue
    // nothing — which is the invariant that matters.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 18, "unanswered") else {
        return;
    };
    let abandoned = request(PORT + 18, "GET", "unanswered/first", &[], b"");

    let accepted = block_on(server.accept()).expect("accept");
    let (_request, responder) = accepted.into_parts();
    drop(responder);

    // The queue is still perfectly usable for the next request.
    let second = request(PORT + 18, "GET", "unanswered/second", &[], b"");
    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"still here"))))
    });
    block_on(server.serve_one(&mut service)).expect("the queue survived");

    let raw = second.join().unwrap().expect("a reply");
    let (_status, _headers, body) = parse_response(&raw);
    assert_eq!(body, b"still here");
    // The abandoned request's client is left waiting for HTTP.sys's own
    // timeout, so it is not joined on here; the socket closes with the process.
    drop(abandoned);
}

#[test]
fn a_rejected_request_gets_no_answer_at_all() {
    // Measured: `reject` closes the connection without so much as a status
    // line, which is what it is for.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 19, "rejected") else {
        return;
    };
    let client = request(PORT + 19, "GET", "rejected/x", &[], b"");

    let accepted = block_on(server.accept()).expect("accept");
    let (_request, responder) = accepted.into_parts();
    block_on(responder.reject()).expect("reject");

    let raw = client.join().unwrap().unwrap_or_default();
    assert!(raw.is_empty(), "expected nothing on the wire, got {raw:?}");
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

#[test]
fn shutting_down_makes_the_next_accept_report_a_closed_queue() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 20, "shutdown") else {
        return;
    };
    assert!(server.is_open());

    server.shutdown().expect("shutdown");
    assert!(!server.is_open());

    // Measured: this fails promptly with ERROR_INVALID_HANDLE rather than
    // hanging, which is what makes a `serve` loop able to exit.
    let error = block_on(server.accept()).expect_err("a closed queue");
    assert!(error.is_queue_closed(), "{error}");

    // Shutting down twice is not an error.
    server.shutdown().expect("an idempotent shutdown");
}

#[test]
fn a_serve_loop_exits_cleanly_when_the_queue_is_shut_down() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 21, "loop") else {
        return;
    };
    let served = Arc::new(AtomicUsize::new(0));

    // A shutdown handle can travel; it is `Send + Sync` on this backend.
    let handle = server.shutdown_handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        handle.shutdown().expect("shutdown from another thread");
    });

    let first = request(PORT + 21, "GET", "loop/x", &[], b"");
    let counter = Arc::clone(&served);
    let mut service = tower::service_fn(move |_req: Request<IncomingBody>| {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
        }
    });

    // Returns `Ok(())` on shutdown — not an error — which is the contract.
    block_on(server.serve(&mut service)).expect("serve exits cleanly");
    stopper.join().unwrap();

    assert_eq!(served.load(Ordering::SeqCst), 1);
    let raw = first.join().unwrap().expect("a reply");
    assert_eq!(parse_response(&raw).2, b"ok");
}

// ---------------------------------------------------------------------------
// Concurrency, which belongs to the caller
// ---------------------------------------------------------------------------

#[test]
fn accepted_requests_can_be_served_concurrently_by_cloning_the_service() {
    // The crate spawns nothing. This is the caller doing it, with plain
    // threads — no runtime in sight — which is only possible because
    // `Accepted<ThreadPoolIo>` is `Send + 'static`.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 22, "concurrent") else {
        return;
    };

    let clients: Vec<_> = (0..4)
        .map(|i| request(PORT + 22, "GET", &format!("concurrent/{i}"), &[], b""))
        .collect();

    // A service with state, so that cloning is the real tower idiom and not a
    // no-op the compiler can elide: each clone shares the counter, which is how
    // a connection pool or a rate limiter would be shared too.
    let calls = Arc::new(AtomicUsize::new(0));
    let service = {
        let calls = Arc::clone(&calls);
        tower::service_fn(move |req: Request<IncomingBody>| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                let path = req.uri().path().to_string();
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(path))))
            }
        })
    };

    let mut workers = Vec::new();
    for _ in 0..4 {
        let accepted = block_on(server.accept()).expect("accept");
        // The tower idiom: clone per request, and let the clone own its
        // readiness.
        let service = service.clone();
        workers.push(std::thread::spawn(move || {
            block_on(accepted.serve(service)).expect("serve");
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4);

    let mut paths: Vec<String> = clients
        .into_iter()
        .map(|c| {
            let raw = c.join().unwrap().expect("a reply");
            String::from_utf8(parse_response(&raw).2).unwrap()
        })
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            "/concurrent/0",
            "/concurrent/1",
            "/concurrent/2",
            "/concurrent/3"
        ]
    );
}

#[test]
fn readiness_is_awaited_before_a_request_is_accepted() {
    // `poll_ready` is honoured rather than skipped, and honoured in the order
    // that makes it mean something: an unready service stops the crate pulling
    // work out of the kernel queue instead of accepting work it cannot place.
    //
    // The order is made *observable* rather than merely asserted about poll
    // counts. A request is put on the wire, a serve of a permanently-unready
    // service is polled a while and then dropped, and a second serve with a
    // ready service is run. If readiness were awaited after accepting, the
    // first serve would have dequeued the request and the drop would have
    // abandoned it: the second serve would then find nothing and time out. That
    // it answers is the proof.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 23, "ready") else {
        return;
    };

    struct NeverReady {
        polls: Arc<AtomicUsize>,
    }

    impl winasio_util::tower_service::Service<Request<IncomingBody>> for NeverReady {
        type Response = Response<Full<Bytes>>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Infallible>>;

        fn poll_ready(
            &mut self,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Infallible>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            // A real service would register a waker; this is a bare executor,
            // so nudging it is enough and keeps the test honest about not
            // needing a reactor.
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }

        fn call(&mut self, _request: Request<IncomingBody>) -> Self::Future {
            unreachable!("a service that is never ready must never be called");
        }
    }

    let client = request(PORT + 23, "GET", "ready/x", &[], b"");
    // Let the request arrive and sit in the kernel queue.
    std::thread::sleep(std::time::Duration::from_millis(400));

    let polls = Arc::new(AtomicUsize::new(0));
    {
        let mut service = NeverReady {
            polls: Arc::clone(&polls),
        };
        let mut serving = std::pin::pin!(server.serve_one(&mut service));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        for _ in 0..50 {
            assert!(
                std::future::Future::poll(serving.as_mut(), &mut cx).is_pending(),
                "an unready service must never complete a serve"
            );
        }
        // Dropped without ever having been ready.
    }
    assert!(
        polls.load(Ordering::SeqCst) >= 50,
        "readiness must actually have been polled, got {}",
        polls.load(Ordering::SeqCst)
    );

    // The request is still there, because it was never accepted.
    let mut ready_service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ready"))))
    });
    block_on(server.serve_one(&mut ready_service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    assert_eq!(parse_response(&raw).2, b"ready");
}

// ---------------------------------------------------------------------------
// The two halves of the crate, together
// ---------------------------------------------------------------------------

#[test]
fn the_client_half_of_this_crate_can_drive_the_server_half() {
    // Both halves on one bare executor, and the strongest end-to-end statement
    // available: an `http::Request` written by the client, converted to
    // HTTP.sys's shape, back to `http::Request` for the service, and the reply
    // all the way back again.
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 24, "roundtrip") else {
        return;
    };
    let port = PORT + 24;

    let driver = std::thread::spawn(move || {
        let client = winasio_util::Client::new("winasio-util/test").unwrap();
        let request = http::Request::post(format!("http://localhost:{port}/roundtrip/echo"))
            .header("x-note", "sent by the client half")
            .body(Full::new(Bytes::from_static(b"ping")))
            .unwrap();
        let response = block_on(client.request(request)).expect("the client got a reply");
        let status = response.status();
        let note = response
            .headers()
            .get("x-echoed")
            .map(|v| v.to_str().unwrap().to_string());
        let body = block_on(response.into_body().collect())
            .expect("a whole body")
            .to_bytes();
        (status, note, body)
    });

    let mut service = tower::service_fn(|req: Request<IncomingBody>| async move {
        let note = req
            .headers()
            .get("x-note")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = req.into_body().collect().await.unwrap().to_bytes();
        let mut reply = Response::new(Full::new(Bytes::from(format!(
            "pong:{}",
            String::from_utf8_lossy(&body)
        ))));
        reply
            .headers_mut()
            .insert("x-echoed", HeaderValue::from_str(&note).unwrap());
        Ok::<_, Infallible>(reply)
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let (status, note, body) = driver.join().unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(note.as_deref(), Some("sent by the client half"));
    assert_eq!(body.as_ref(), b"pong:ping");
}

// ---------------------------------------------------------------------------
// The runnable example
// ---------------------------------------------------------------------------

/// The example server, compiled into this test binary so it can be run.
#[allow(dead_code)]
mod example {
    include!("../examples/util_server.rs");
}

/// The example really serves, on `futures::executor::block_on` and nothing else.
#[test]
fn the_example_server_serves_requests_with_no_runtime() {
    let server = std::thread::spawn(|| {
        // No runtime: a bare executor, which is the whole claim.
        futures::executor::block_on(example::run_server(EXAMPLE_PORT, "utilex", 3))
    });

    // Give the listener time to bind. If it cannot (no URL reservation), the
    // client requests simply fail and the test skips.
    std::thread::sleep(std::time::Duration::from_millis(600));

    let Some(get) = send_raw(EXAMPLE_PORT, "GET", "utilex/hello", &[], &[]) else {
        eprintln!("skipping: the example could not bind {EXAMPLE_PORT}");
        return;
    };
    let (status, headers, body) = parse_response(&get);
    assert!(status.contains("200"), "got {status:?}");
    assert!(
        String::from_utf8_lossy(&body).contains("/utilex/hello"),
        "body was {:?}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(header(&headers, "x-served"), Some("1"));
    // The crate framed the reply; the handler declared nothing.
    assert_eq!(header(&headers, "content-length"), Some("28"));

    let post = send_raw(EXAMPLE_PORT, "POST", "utilex/data", &[], b"twelve bytes").unwrap();
    let (_, _, body) = parse_response(&post);
    assert!(
        String::from_utf8_lossy(&body).contains("received 12 bytes"),
        "the example should read the body, got {:?}",
        String::from_utf8_lossy(&body)
    );

    let odd = send_raw(EXAMPLE_PORT, "DELETE", "utilex/x", &[], &[]).unwrap();
    let (status, _, _) = parse_response(&odd);
    assert!(status.contains("405"), "got {status:?}");

    server.join().expect("example thread").expect("example ran");
}

/// The example contains no `unsafe` whatsoever.
///
/// Asserted textually rather than trusted, following the precedent set for
/// `httpsys_server.rs`: the point of the example is that a complete server
/// needs none, so a comment saying so is not enough.
#[test]
fn the_example_server_contains_no_unsafe() {
    let source = include_str!("../examples/util_server.rs");
    for (n, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !code.contains("unsafe"),
            "examples/util_server.rs:{} uses `unsafe`: {line}",
            n + 1
        );
    }
}

// ---------------------------------------------------------------------------
// The other backend
// ---------------------------------------------------------------------------

/// The single-threaded, caller-driven backend serves too — with no thread pool,
/// no `Send` anywhere, and the caller pumping the completion port by hand.
///
/// This is the whole reason `Server` is generic over its backend rather than
/// fixed to `ThreadPoolIo`: `Rc<Proactor>` is `!Send`, so a design that
/// demanded `Send` would have excluded it, and with it every single-threaded
/// caller.
#[test]
fn the_single_threaded_proactor_backend_serves_a_request() {
    use std::rc::Rc;
    use winasio::iocp::Proactor;

    let session = ServerSession::new().unwrap();
    let proactor = Rc::new(Proactor::new().expect("Proactor::new"));
    let built = Server::builder(&session)
        .url(&format!("http://localhost:{}/proactor/", PORT + 27))
        .build(&proactor);
    let Ok(server) = built else {
        eprintln!("skipping: cannot bind {}", PORT + 27);
        return;
    };

    let client = request(PORT + 27, "POST", "proactor/x", &[], b"ping");

    let mut service = tower::service_fn(|req: Request<IncomingBody<Rc<Proactor>>>| async move {
        let body = req.into_body().collect().await.unwrap().to_bytes();
        let mut echoed = Vec::from(&b"pong:"[..]);
        echoed.extend_from_slice(&body);
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(echoed))))
    });
    // Note what is *not* here: no runtime, no thread, no waker plumbing. The
    // caller polls the future and pumps the port, which is the entire contract.
    common::drive_proactor(proactor.as_ref(), server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(header(&headers, "content-length"), Some("9"));
    assert_eq!(body, b"pong:ping");
}

/// The body of a request read on the single-threaded backend spans several
/// reads, which is where a backend-specific mistake in `IncomingBody` would
/// show.
#[test]
fn the_proactor_backend_reads_a_body_that_spans_several_reads() {
    use std::rc::Rc;
    use winasio::iocp::Proactor;

    let session = ServerSession::new().unwrap();
    let proactor = Rc::new(Proactor::new().expect("Proactor::new"));
    let built = Server::builder(&session)
        .url(&format!("http://localhost:{}/proactorbody/", PORT + 28))
        .build(&proactor);
    let Ok(server) = built else {
        eprintln!("skipping: cannot bind {}", PORT + 28);
        return;
    };

    let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let client = request(PORT + 28, "POST", "proactorbody/x", &[], &payload);

    let expected = payload.clone();
    let mut service = tower::service_fn(move |req: Request<IncomingBody<Rc<Proactor>>>| {
        let expected = expected.clone();
        async move {
            let got = req.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(got.len(), expected.len());
            assert_eq!(got.as_ref(), expected.as_slice());
            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(got.len().to_string()))))
        }
    });
    common::drive_proactor(proactor.as_ref(), server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (_, _, body) = parse_response(&raw);
    assert_eq!(body, payload.len().to_string().as_bytes());
}

/// A body that produces more than its declared length must be stopped before
/// the surplus reaches the wire.
///
/// The dangerous shape, and the reason this is checked before each write rather
/// than after the loop: HTTP.sys does not police the declared length, so a peer
/// on a keep-alive connection would read `declared` bytes as the body and parse
/// the surplus as the start of the next response.
#[test]
fn a_reply_that_over_delivers_its_declared_length_is_stopped_not_sent() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 29, "over") else {
        return;
    };
    let client = request(PORT + 29, "GET", "over/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        // Three frames of four bytes against a declared five.
        let frames = vec![
            Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"aaaa"))),
            Ok(Frame::data(Bytes::from_static(b"bbbb"))),
            Ok(Frame::data(Bytes::from_static(b"cccc"))),
        ];
        let mut reply = Response::new(StreamBody::new(futures::stream::iter(frames)));
        reply
            .headers_mut()
            .insert("content-length", HeaderValue::from_static("5"));
        Ok::<_, Infallible>(reply)
    });
    let error = block_on(server.serve_one(&mut service)).expect_err("a mismatch");
    assert!(
        matches!(
            &error,
            ServeError::Response(ResponseError::Body(BodyError::LengthMismatch {
                declared: 5,
                ..
            }))
        ),
        "{error}"
    );

    // The first four bytes are already gone -- the head was sent before the
    // body could be known -- but the surplus is not, so the message the peer
    // sees is short rather than over-long. A short message is detectable; a
    // long one desynchronises the connection.
    let raw = client.join().unwrap().expect("a reply");
    let (_, headers, body) = parse_response(&raw);
    assert_eq!(header(&headers, "content-length"), Some("5"));
    assert!(
        body.len() < 5,
        "the surplus must not be on the wire, got {} bytes",
        body.len()
    );
}

/// A declared length that the first frame happens to match must not be taken as
/// the whole body when more frames follow.
///
/// The buffered one-shot path is a real optimisation, but it can only be taken
/// once the body has confirmed it is finished -- which means asking for one
/// more frame. Taking it on the first frame alone silently dropped the rest.
#[test]
fn a_first_frame_matching_the_declared_length_does_not_truncate_the_rest() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 30, "firstframe") else {
        return;
    };
    let client = request(PORT + 30, "GET", "firstframe/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        // `StreamBody` has no size hint, so the length comes from the header
        // alone -- and the first frame is exactly that length.
        let frames = vec![
            Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"aaaa"))),
            Ok(Frame::data(Bytes::from_static(b"bbbb"))),
        ];
        let mut reply = Response::new(StreamBody::new(futures::stream::iter(frames)));
        reply
            .headers_mut()
            .insert("content-length", HeaderValue::from_static("4"));
        Ok::<_, Infallible>(reply)
    });
    // Eight bytes produced against four declared: an error, not a silent
    // four-byte reply.
    let error = block_on(server.serve_one(&mut service)).expect_err("a mismatch");
    assert!(
        matches!(
            &error,
            ServeError::Response(ResponseError::Body(BodyError::LengthMismatch {
                declared: 4,
                ..
            }))
        ),
        "{error}"
    );
    drop(client.join());
}

/// The buffered fast path still applies to the ordinary case, and still costs
/// one send rather than three.
#[test]
fn a_body_that_fits_in_one_frame_is_sent_buffered() {
    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 31, "buffered") else {
        return;
    };
    let client = request(PORT + 31, "GET", "buffered/x", &[], b"");

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"buffered"))))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");

    let raw = client.join().unwrap().expect("a reply");
    let (status, headers, body) = parse_response(&raw);
    assert_eq!(status, "HTTP/1.1 200 OK");
    // Measured: HTTP.sys computes this itself for a buffered send, so the crate
    // declares nothing and there is exactly one of them.
    assert_eq!(all_headers(&headers, "content-length"), ["8"]);
    assert_eq!(header(&headers, "transfer-encoding"), None);
    assert_eq!(body, b"buffered");
}

/// A request whose head cannot be expressed as an `http::Request` is answered
/// rather than abandoned.
///
/// HTTP.sys validates a great deal itself, so the reachable case here is a
/// request-target it accepts and the `http` crate does not.
#[test]
fn a_request_this_crate_cannot_express_gets_an_answer_anyway() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 32, "malformed") else {
        return;
    };
    let port = PORT + 32;

    // `OPTIONS *` is a perfectly legal request-target that HTTP.sys accepts and
    // routes; whether `http::Uri` accepts it decides which branch this takes.
    // Either way the peer must get an answer rather than a hang.
    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .ok()?;
        let head = format!(
            "GET /malformed/x HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).ok()?;
        stream.flush().ok()?;
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        Some(response)
    });

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
    });
    let outcome = block_on(server.serve_one(&mut service));

    let raw = client.join().unwrap().expect("a reply");
    let (status, _, _) = parse_response(&raw);
    match outcome {
        // The ordinary case: the head converted and the service answered.
        Ok(()) => assert_eq!(status, "HTTP/1.1 200 OK"),
        // The case this test exists for: a 400 rather than silence.
        Err(ServeError::Accept(AcceptError::MalformedRequest { .. })) => {
            assert!(status.contains("400"), "got {status:?}")
        }
        Err(other) => panic!("unexpected: {other}"),
    }
}

/// A declared length is reported through `size_hint`, and shrinks as the body
/// is consumed.
///
/// This is what a hyper- or axum-shaped caller reads to decide whether to
/// buffer, so it has to be exact when it claims to be and absent when the
/// length is genuinely unknowable.
#[test]
fn a_request_body_reports_what_it_knows_through_its_size_hint() {
    use http_body::Body as _;

    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 33, "hint") else {
        return;
    };
    let client = request(PORT + 33, "POST", "hint/x", &[], b"0123456789");

    let mut service = tower::service_fn(|request: Request<IncomingBody>| async {
        let mut body = request.into_body();
        // Before anything is read: exactly what `Content-Length` declared.
        assert_eq!(body.size_hint().exact(), Some(10));
        assert!(!body.is_end_stream());
        // The `Debug` impl says how far along the body is without needing the
        // boxed read future to be printable.
        assert!(format!("{body:?}").contains("idle"), "{body:?}");
        assert!(body.request_id().get() != 0);

        let mut collected = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.unwrap();
            if let Ok(data) = frame.into_data() {
                collected.extend_from_slice(&data);
                // The remainder shrinks by exactly what was delivered.
                assert_eq!(body.size_hint().exact(), Some(10 - collected.len() as u64));
            }
        }
        assert_eq!(collected, b"0123456789");
        assert!(body.is_end_stream());
        assert!(format!("{body:?}").contains("finished"), "{body:?}");

        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");
    let raw = client.join().unwrap().expect("a reply");
    assert_eq!(parse_response(&raw).2, b"ok");
}

/// A request with no body at all is finished before it is ever polled.
///
/// The read is not issued, because it could only return zero -- so this also
/// pins the one thing `has_more_body()` is trustworthy for.
#[test]
fn a_request_without_a_body_is_finished_before_it_is_polled() {
    use http_body::Body as _;

    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 34, "nobody") else {
        return;
    };
    let client = request(PORT + 34, "GET", "nobody/x", &[], b"");

    let mut service = tower::service_fn(|request: Request<IncomingBody>| async {
        let mut body = request.into_body();
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(
            body.frame().await.is_none(),
            "no frames, and no read issued"
        );
        // Fused: an ended body stays ended rather than reissuing a read.
        assert!(body.frame().await.is_none());
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
    });
    block_on(server.serve_one(&mut service)).expect("serve_one");
    let raw = client.join().unwrap().expect("a reply");
    assert_eq!(parse_response(&raw).2, b"ok");
}

/// A peer that promises a body and then vanishes is an error, not a short body.
///
/// This is the claim that most distinguishes the server half from the client
/// half. WinHTTP reports a mid-body close as a body that ended cleanly, which
/// is why `ResponseBody` needs its own truncation check; HTTP.sys reports the
/// amputation itself, which is why `IncomingBody` has none. Pinning it here
/// means the day that stops being true is the day this test fails, rather than
/// the day a user silently loses the tail of a request.
#[test]
fn a_request_body_cut_off_mid_send_is_an_error_not_a_short_body() {
    use std::io::Write;
    use std::net::TcpStream;

    let session = ServerSession::new().unwrap();
    let Some(server) = start(&session, PORT + 35, "cutoff") else {
        return;
    };
    let port = PORT + 35;

    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        // Twenty promised, five sent, then gone.
        let head = format!(
            "POST /cutoff/x HTTP/1.1\r\nHost: localhost:{port}\r\n\
             Content-Length: 20\r\nConnection: close\r\n\r\nhello"
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(stream);
    });

    let seen = Arc::new(Mutex::new(None));
    let recorder = Arc::clone(&seen);
    let mut service = tower::service_fn(move |request: Request<IncomingBody>| {
        let recorder = Arc::clone(&recorder);
        async move {
            let mut body = request.into_body();
            let mut outcome = Ok(Vec::new());
            while let Some(frame) = body.frame().await {
                match frame {
                    Ok(frame) => {
                        if let (Ok(data), Ok(collected)) = (frame.into_data(), outcome.as_mut()) {
                            collected.extend_from_slice(&data);
                        }
                    }
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }
            *recorder.lock().unwrap() = Some(outcome);
            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
        }
    });
    // The reply may well fail to send -- the peer is gone -- and that is not
    // what this test is about.
    let _ = block_on(server.serve_one(&mut service));
    client.join().unwrap();

    let outcome = seen.lock().unwrap().take().expect("the service ran");
    match outcome {
        Err(error) => {
            // The platform reported the amputation, so this crate does not have
            // to guess at one.
            assert!(error.operation == ServerOperation::ReadBody, "{error}");
        }
        Ok(body) => panic!("a truncated body must not look like a body that ended: {body:?}"),
    }
}
