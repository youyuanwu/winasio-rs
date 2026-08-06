// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Integration tests for the `winasio-util` HTTP client.
//!
//! The server is a dumb `std::net::TcpListener` on a plain `std::thread` that
//! writes whatever bytes the test tells it to. That is not laziness: most of
//! what needs proving here is how the client behaves when a server misbehaves —
//! a body cut off halfway, a header block with duplicates in it, a response
//! with no `Content-Length` — and no HTTP framework will produce those on
//! request. A raw socket will produce anything.
//!
//! It also keeps the *server* free of an async runtime, so that a test claiming
//! the client needs no runtime is not quietly contradicted by its own fixture.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::executor::block_on;
use http::{HeaderValue, Method, Request, StatusCode, Version};
use http_body::Body as _;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use winasio_util::{Client, Error, ResponseBody, WinHttpError};

// ------------------------------------------------------------------ server

/// What the server does once it has read a request.
///
/// Every variant here answers once and then closes, so every response it writes
/// must carry `Connection: close`. That is not politeness: HTTP/1.1 defaults to
/// a persistent connection, WinHTTP keeps a **process-wide** keep-alive pool,
/// and a socket returned to that pool by a server that then closes it will fail
/// the *next* request with `ERROR_WINHTTP_CONNECTION_ERROR`. `check_announces_close`
/// enforces it; [`Reply::PretendsPersistent`] is the one deliberate exception.
#[derive(Clone)]
enum Reply {
    /// Write these bytes, then close.
    Raw(&'static str),
    /// Write these bytes, wait, write these, then close.
    ///
    /// Holds the response genuinely pending: the head arrives so the response
    /// resolves, and the body then has nothing to read until the pause ends.
    Split(&'static str, Duration, &'static str),
    /// Write these bytes and then reset the connection rather than closing it.
    RawThenReset(&'static str),
    /// Answer `301` to `/`, and `200` to anything else.
    Redirect,
    /// Read the request and never answer.
    Silent,
    /// Answer *without* announcing the close, wait, and then close anyway —
    /// the incorrect-but-common server behaviour that leaves a dead socket in
    /// WinHTTP's pool. The pause is what makes it deterministic: it guarantees
    /// the socket is still open, and so still poolable, when the client
    /// finishes reading the response.
    PretendsPersistent(&'static str, Duration),
}

struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Server {
    fn uri(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    fn request(&self, index: usize) -> String {
        let requests = self.requests.lock().unwrap();
        String::from_utf8_lossy(requests.get(index).map(Vec::as_slice).unwrap_or(&[])).into_owned()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

fn spawn(reply: Reply) -> Server {
    check_announces_close(&reply);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = requests.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let sink = sink.clone();
            let reply = reply.clone();
            std::thread::spawn(move || serve(stream, reply, sink));
        }
    });

    Server { port, requests }
}

/// Refuse to start a server whose response would advertise a connection it is
/// about to close.
///
/// This is checked on the calling thread, so a mistake names the test that made
/// it rather than quietly killing a server thread. It exists because the
/// omission is invisible until it is not: `a_client_is_shared_across_threads`
/// passed locally and in isolation for an entire PR, then failed CI roughly one
/// run in fifteen, because WinHTTP had pooled a socket this server had already
/// closed. The pool is process-wide — measured, not assumed — so every server
/// here was exposed, not merely the one test that shares a `Client`.
fn check_announces_close(reply: &Reply) {
    let head = match reply {
        Reply::Raw(bytes) | Reply::RawThenReset(bytes) | Reply::Split(bytes, _, _) => *bytes,
        // Both of `Redirect`'s responses announce it; `Silent` never answers.
        Reply::Redirect | Reply::Silent => return,
        // The deliberate exception: this variant exists to reproduce the fault.
        Reply::PretendsPersistent(..) => return,
    };
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "this server answers once and then closes, so its response must say \
         `Connection: close`. Without it WinHTTP returns the socket to its \
         process-wide keep-alive pool and the next request to use it dies with \
         ERROR_WINHTTP_CONNECTION_ERROR. Offending response: {head:?}"
    );
}

fn serve(mut stream: TcpStream, reply: Reply, sink: Arc<Mutex<Vec<Vec<u8>>>>) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let head = String::from_utf8_lossy(&request).into_owned();
    sink.lock().unwrap().push(request);

    match reply {
        Reply::Raw(bytes) => {
            let _ = stream.write_all(bytes.as_bytes());
            let _ = stream.flush();
            // A graceful close, which is the case that matters: WinHTTP
            // reports it as a body that ended.
            let _ = stream.shutdown(Shutdown::Both);
        }
        Reply::Split(first, pause, second) => {
            let _ = stream.write_all(first.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(pause);
            let _ = stream.write_all(second.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        }
        Reply::RawThenReset(bytes) => {
            let _ = stream.write_all(bytes.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(50));
            // `SO_LINGER` with a zero timeout makes `close` send an RST rather
            // than a FIN. `TcpStream::set_linger` is still unstable, so the
            // socket option is set directly.
            set_linger_zero(&stream);
        }
        Reply::Redirect => {
            let response = if head.starts_with("GET /moved") {
                "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\narrived"
            } else {
                "HTTP/1.1 301 Moved Permanently\r\nLocation: /moved\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        }
        Reply::Silent => std::thread::sleep(Duration::from_secs(30)),
        Reply::PretendsPersistent(bytes, pause) => {
            let _ = stream.write_all(bytes.as_bytes());
            let _ = stream.flush();
            // Staying open across the client's read is the whole point: the
            // socket looks reusable at exactly the moment WinHTTP decides
            // whether to keep it.
            std::thread::sleep(pause);
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn set_linger_zero(stream: &TcpStream) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{setsockopt, LINGER, SOCKET, SOL_SOCKET, SO_LINGER};

    let linger = LINGER {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: the socket is live for the call, and the option buffer is a
    // correctly sized `LINGER` that outlives it.
    unsafe {
        let bytes = std::slice::from_raw_parts(
            std::ptr::addr_of!(linger).cast::<u8>(),
            std::mem::size_of::<LINGER>(),
        );
        let _ = setsockopt(
            SOCKET(stream.as_raw_socket() as usize),
            SOL_SOCKET,
            SO_LINGER,
            Some(bytes),
        );
    }
}

/// Read headers, then any body a `Content-Length` declares, or a chunked one.
fn read_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => buffer.push(byte[0]),
        }
        if buffer.len() >= 4 && &buffer[buffer.len() - 4..] == b"\r\n\r\n" {
            break;
        }
    }

    let head = String::from_utf8_lossy(&buffer).to_ascii_lowercase();
    if head.contains("transfer-encoding: chunked") {
        // Read until the terminating zero-length chunk.
        loop {
            let mut line = Vec::new();
            loop {
                if stream.read(&mut byte).ok()? == 0 {
                    return Some(buffer);
                }
                buffer.push(byte[0]);
                line.push(byte[0]);
                if line.ends_with(b"\r\n") {
                    break;
                }
            }
            let size =
                usize::from_str_radix(String::from_utf8_lossy(&line[..line.len() - 2]).trim(), 16)
                    .ok()?;
            let mut chunk = vec![0u8; size + 2];
            stream.read_exact(&mut chunk).ok()?;
            buffer.extend_from_slice(&chunk);
            if size == 0 {
                break;
            }
        }
        return Some(buffer);
    }

    let length = head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length > 0 {
        let mut body = vec![0u8; length];
        if stream.read_exact(&mut body).is_err() {
            return Some(buffer);
        }
        buffer.extend_from_slice(&body);
    }
    Some(buffer)
}

// ----------------------------------------------------------------- helpers

fn client() -> Client {
    Client::builder("winasio-util-tests")
        .timeouts(5_000, 5_000, 5_000, 5_000)
        .build()
        .unwrap()
}

fn empty() -> Empty<Bytes> {
    Empty::new()
}

/// Read a whole body, or return the error that stopped it.
fn collect(body: ResponseBody) -> Result<Bytes, Error> {
    block_on(async {
        let mut out = Vec::new();
        let mut body = std::pin::pin!(body);
        while let Some(frame) = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await {
            if let Ok(data) = frame?.into_data() {
                out.extend_from_slice(&data);
            }
        }
        Ok(Bytes::from(out))
    })
}

// ------------------------------------------------------------------- tests

#[test]
fn a_get_completes_on_a_bare_executor_with_no_runtime() {
    // `futures::executor::block_on` has no reactor and no worker threads. If
    // any part of this crate quietly needed a runtime, this could not pass.
    // The server is a plain thread, so nothing in the fixture supplies one
    // either.
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 26\r\nConnection: close\r\n\r\nhello from a bare executor",
    ));
    let client = client();
    let request = Request::get(server.uri("/")).body(empty()).unwrap();

    let response = block_on(client.request(request)).unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = collect(response.into_body()).unwrap();
    assert_eq!(body, Bytes::from_static(b"hello from a bare executor"));
}

#[test]
fn a_response_carries_its_status_version_headers_and_body() {
    let server = spawn(Reply::Raw(
        "HTTP/1.0 201 Created\r\nContent-Type: application/json\r\nX-Note: kept\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    ));
    let response = block_on(
        client().request(
            Request::get(server.uri("/thing?q=1"))
                .body(empty())
                .unwrap(),
        ),
    )
    .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.version(), Version::HTTP_10);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(response.headers().get("x-note").unwrap(), "kept");
    assert_eq!(collect(response.into_body()).unwrap(), &b"{}"[..]);
    // The query is part of the request target, not dropped on the way through.
    assert!(server.request(0).starts_with("GET /thing?q=1 HTTP/1.1"));
}

#[test]
fn duplicate_response_headers_survive_as_separate_entries() {
    // A by-name query on the platform returns only the first `Set-Cookie`.
    // Parsing the raw block is what keeps both, and this is the test that says
    // so.
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nSet-Cookie: c=3\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();

    let cookies: Vec<&str> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(cookies, ["a=1", "b=2", "c=3"]);
}

#[test]
fn a_truncated_body_is_an_error_not_a_body_that_ended() {
    // The measurement this crate exists for. The server promises ten bytes,
    // sends three, and closes *gracefully*. WinHTTP reports that as a body
    // that ended: `query_data_available` returns zero and nothing fails.
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let error = collect(response.into_body()).unwrap_err();
    assert!(
        matches!(
            error,
            Error::TruncatedBody {
                expected: 10,
                received: 3
            }
        ),
        "expected a truncation, got {error}"
    );
}

#[test]
fn a_body_cut_off_by_a_reset_is_also_an_error() {
    // The other half of the same story: an RST does reach the caller as a
    // platform error, and must not be mistaken for a clean end either.
    let server = spawn(Reply::RawThenReset(
        "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();

    let error = collect(response.into_body()).unwrap_err();
    // Either shape is an error, which is the point; which one depends on
    // whether the reset overtakes the data.
    assert!(
        matches!(error, Error::TruncatedBody { .. })
            || error.win_http() == Some(WinHttpError::ConnectionError),
        "expected a failure, got {error}"
    );
}

#[test]
fn a_close_delimited_body_ends_cleanly() {
    // No `Content-Length`, so nothing was promised and nothing can be owed.
    // The body must end rather than invent a truncation.
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nno length declared",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();
    assert_eq!(
        collect(response.into_body()).unwrap(),
        &b"no length declared"[..]
    );
}

#[test]
fn a_chunked_response_is_dechunked_and_ends_cleanly() {
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();
    // The documented wart: the platform de-chunks the body but leaves the
    // header in place, and this crate reports headers verbatim.
    assert_eq!(
        response.headers().get("transfer-encoding").unwrap(),
        "chunked"
    );
    assert_eq!(collect(response.into_body()).unwrap(), &b"hello world"[..]);
}

#[test]
fn a_body_polled_while_pending_is_never_abandoned() {
    // The central design claim. A body that recreated its operation on each
    // poll would retire a buffer per poll and then park the request, failing
    // with `ERROR_BUSY`. This polls hard across a real pause and must still
    // deliver both halves.
    let server = spawn(Reply::Split(
        "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabcde",
        Duration::from_millis(600),
        "fghij",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();

    let (body, polls) = block_on(async {
        let mut body = std::pin::pin!(response.into_body());
        let mut out = Vec::new();
        let mut polls = 0usize;
        loop {
            let frame = std::future::poll_fn(|cx| {
                polls += 1;
                let poll = body.as_mut().poll_frame(cx);
                if poll.is_pending() {
                    // Wake immediately, so the executor comes straight back
                    // and the body is polled while genuinely pending.
                    cx.waker().wake_by_ref();
                }
                poll
            })
            .await;
            let Some(frame) = frame else { break };
            if let Ok(data) = frame.unwrap().into_data() {
                out.extend_from_slice(&data);
            }
        }
        (Bytes::from(out), polls)
    });

    assert_eq!(body, &b"abcdefghij"[..]);
    assert!(
        polls > 100,
        "the body was only polled {polls} times, so it was never really pending"
    );
}

#[test]
fn a_finished_body_keeps_reporting_end_of_stream() {
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();

    block_on(async {
        let mut body = std::pin::pin!(response.into_body());
        let first = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
        assert_eq!(first.unwrap().unwrap().into_data().unwrap(), &b"hi"[..]);
        // The declared length is satisfied, so the body knows it is over
        // without another round trip.
        assert!(body.is_end_stream());
        for _ in 0..3 {
            assert!(std::future::poll_fn(|cx| body.as_mut().poll_frame(cx))
                .await
                .is_none());
        }
    });
}

#[test]
fn a_head_response_has_an_exactly_zero_size_hint() {
    // Measured: a `HEAD` response carrying `Content-Length: 10` reports zero
    // bytes available. Believing the header would invent a truncation.
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n",
    ));
    let response = block_on(
        client().request(
            Request::builder()
                .method(Method::HEAD)
                .uri(server.uri("/"))
                .body(empty())
                .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(response.headers().get("content-length").unwrap(), "10");

    let body = response.into_body();
    assert!(body.is_end_stream());
    assert_eq!(body.size_hint().exact(), Some(0));
    assert!(collect(body).unwrap().is_empty());
}

#[test]
fn a_no_content_response_has_no_body_whatever_it_declares() {
    let server = spawn(Reply::Raw(
        "HTTP/1.1 204 No Content\r\nContent-Length: 10\r\nConnection: close\r\n\r\n",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(collect(response.into_body()).unwrap().is_empty());
}

#[test]
fn a_post_with_a_known_length_body_arrives_complete() {
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    ));
    let response = block_on(
        client().request(
            Request::post(server.uri("/submit"))
                .header("content-type", "text/plain")
                .body(Full::new(Bytes::from_static(b"a body of known length")))
                .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let sent = server.request(0);
    assert!(sent.starts_with("POST /submit HTTP/1.1"), "{sent}");
    assert!(sent.contains("Content-Length: 22"), "{sent}");
    assert!(!sent.to_ascii_lowercase().contains("chunked"), "{sent}");
    assert!(sent.ends_with("a body of known length"), "{sent}");
}

#[test]
fn a_large_known_length_body_is_written_in_full() {
    // Larger than the crate's own write ceiling, so the write loop runs more
    // than once.
    let payload = vec![b'x'; 200 * 1024];
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let response = block_on(
        client().request(
            Request::post(server.uri("/big"))
                .body(Full::new(Bytes::from(payload.clone())))
                .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let sent = server.request(0);
    assert!(
        sent.contains(&format!("Content-Length: {}", payload.len())),
        "{}",
        &sent[..200.min(sent.len())]
    );
    assert!(sent.ends_with(&"x".repeat(payload.len())));
}

#[test]
fn a_body_of_unknown_length_is_sent_chunked() {
    // `StreamBody` cannot know its own length, so the client must choose
    // chunked framing and do the framing itself.
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let frames = futures::stream::iter(vec![
        Ok::<_, std::convert::Infallible>(http_body::Frame::data(Bytes::from_static(b"one"))),
        Ok(http_body::Frame::data(Bytes::from_static(b"two"))),
    ]);
    let response = block_on(
        client().request(
            Request::post(server.uri("/stream"))
                .body(StreamBody::new(frames))
                .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let sent = server.request(0);
    assert!(sent.contains("Transfer-Encoding: chunked"), "{sent}");
    // The platform must not have added a length of its own.
    assert!(!sent.contains("Content-Length"), "{sent}");
    assert!(
        sent.ends_with("3\r\none\r\n3\r\ntwo\r\n0\r\n\r\n"),
        "{sent}"
    );
}

#[test]
fn an_empty_body_sends_no_body_at_all() {
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let response = block_on(
        client().request(
            Request::post(server.uri("/nothing"))
                .body(Full::new(Bytes::new()))
                .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let sent = server.request(0);
    assert!(sent.contains("Content-Length: 0"), "{sent}");
    assert!(sent.ends_with("\r\n\r\n"), "{sent}");
}

#[test]
fn a_non_ascii_request_header_is_rejected_before_anything_is_sent() {
    // Rejecting rather than lossily converting, and rejecting *early* so that
    // the error can name the header rather than being an opaque platform code.
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let request = Request::get(server.uri("/"))
        .header("x-note", HeaderValue::from_bytes(b"caf\xc3\xa9").unwrap())
        .body(empty())
        .unwrap();

    let error = block_on(client().request(request)).unwrap_err();
    assert!(
        matches!(&error, Error::InvalidRequestHeader { name, .. } if name == "x-note"),
        "got {error}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "the request should not have reached the server"
    );
}

#[test]
fn a_caller_supplied_framing_header_is_refused() {
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    for name in ["content-length", "transfer-encoding"] {
        let request = Request::post(server.uri("/"))
            .header(name, "7")
            .body(Full::new(Bytes::from_static(b"1234567")))
            .unwrap();
        let error = block_on(client().request(request)).unwrap_err();
        assert!(
            matches!(&error, Error::FramingHeaderNotAllowed { name: n } if n == name),
            "{name} produced {error}"
        );
    }
    assert_eq!(server.request_count(), 0);
}

#[test]
fn a_redirect_is_returned_rather_than_followed() {
    let server = spawn(Reply::Redirect);
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();

    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(response.headers().get("location").unwrap(), "/moved");
    assert_eq!(
        server.request_count(),
        1,
        "nothing should have been retried"
    );
}

#[test]
fn platform_redirects_can_be_switched_back_on() {
    // The escape hatch, and the proof that the default is a choice rather than
    // an accident.
    let server = spawn(Reply::Redirect);
    let client = Client::builder("winasio-util-tests")
        .timeouts(5_000, 5_000, 5_000, 5_000)
        .platform_redirects(true)
        .build()
        .unwrap();

    let response =
        block_on(client.request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(collect(response.into_body()).unwrap(), &b"arrived"[..]);
    assert_eq!(server.request_count(), 2);
}

#[test]
fn a_body_larger_than_the_platform_can_declare_is_refused() {
    // `WinHttpSendRequest` takes the total length as a `u32`. Without the
    // guard the cast wraps and the platform is told to expect a different
    // body from the one it is about to be sent. Nothing is written, so this
    // needs no real payload — only a size hint that claims one.
    struct Enormous;
    impl http_body::Body for Enormous {
        type Data = Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            std::task::Poll::Ready(None)
        }
        fn size_hint(&self) -> http_body::SizeHint {
            http_body::SizeHint::with_exact(u64::from(u32::MAX) + 1)
        }
    }

    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let error = block_on(client().request(Request::post(server.uri("/")).body(Enormous).unwrap()))
        .unwrap_err();
    assert!(
        matches!(error, Error::BodyTooLarge { .. }),
        "expected a refusal, got {error}"
    );
    assert_eq!(server.request_count(), 0);
}

#[test]
fn a_body_that_breaks_its_size_hint_promise_is_an_error() {
    // A body whose `size_hint` overstates it leaves the platform waiting for
    // bytes that never come — measured, it reports that as a send timeout
    // half a minute later. Caught here instead, naming both numbers.
    struct Liar;
    impl http_body::Body for Liar {
        type Data = Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            std::task::Poll::Ready(None)
        }
        fn size_hint(&self) -> http_body::SizeHint {
            http_body::SizeHint::with_exact(20)
        }
    }

    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let error =
        block_on(client().request(Request::post(server.uri("/")).body(Liar).unwrap())).unwrap_err();
    assert!(
        matches!(
            error,
            Error::BodyLengthMismatch {
                declared: 20,
                actual: 0
            }
        ),
        "expected a mismatch, got {error}"
    );
}

#[test]
fn a_request_body_that_fails_is_reported_as_a_body_failure() {
    struct Broken;
    impl http_body::Body for Broken {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            std::task::Poll::Ready(Some(Err(std::io::Error::other("the body broke"))))
        }
    }

    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let error = block_on(client().request(Request::post(server.uri("/")).body(Broken).unwrap()))
        .unwrap_err();
    // Not folded into a transport error: the caller's own body failed, and
    // the cause is reachable through `source`.
    assert!(matches!(error, Error::BodyError(_)), "got {error}");
    assert!(std::error::Error::source(&error)
        .unwrap()
        .to_string()
        .contains("the body broke"));
}

#[test]
fn a_scheme_that_is_not_http_is_refused() {
    let error =
        block_on(client().request(Request::get("ftp://example.com/x").body(empty()).unwrap()))
            .unwrap_err();
    assert!(matches!(error, Error::UnsupportedScheme { .. }), "{error}");
}

#[test]
fn a_custom_method_token_reaches_the_wire() {
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ));
    let response = block_on(
        client().request(
            Request::builder()
                .method(Method::from_bytes(b"REPORT").unwrap())
                .uri(server.uri("/r"))
                .body(empty())
                .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(server.request(0).starts_with("REPORT /r HTTP/1.1"));
}

#[test]
fn a_receive_deadline_that_elapses_is_a_timeout_not_an_empty_response() {
    let server = spawn(Reply::Silent);
    let client = Client::builder("winasio-util-tests")
        .timeouts(2_000, 2_000, 2_000, 500)
        .build()
        .unwrap();

    let error =
        block_on(client.request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap_err();
    assert_eq!(error.win_http(), Some(WinHttpError::Timeout), "{error}");
}

#[test]
fn a_stale_pooled_connection_is_a_visible_error_not_a_silent_wrong_answer() {
    // WinHTTP keeps a keep-alive connection pool and does not retry a socket
    // that turns out to be dead. All of that was measured rather than read:
    // the pool is process-wide (a brand new `Client` per request reuses
    // sockets just as much as a shared one), and the absence of a retry is the
    // same in synchronous WinHTTP, so it is neither this crate's async
    // plumbing nor its redirect policy that suppresses one.
    //
    // This crate deliberately does not paper over that. A retry is out of
    // scope, and it could not be done honestly anyway: `send` consumes the
    // request body and never gives it back, so there is nothing left to
    // replay, and replaying a POST the server may already have acted on is
    // not safe regardless. What the caller is owed instead is the ability to
    // *see* it, so this pins down that the failure is a recognisable transport
    // error — never a hang, never a truncated body dressed up as a whole one.
    //
    // The server here is the badly behaved one on purpose: it answers without
    // `Connection: close`, so the socket goes into the pool, and only then
    // closes.
    let server = spawn(Reply::PretendsPersistent(
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfresh",
        Duration::from_millis(250),
    ));
    let client = client();

    let mut failures = 0usize;
    for attempt in 0..8 {
        let result = block_on(client.request(Request::get(server.uri("/")).body(empty()).unwrap()))
            .and_then(|response| collect(response.into_body()));
        match result {
            // A request that got a fresh socket must be answered in full.
            Ok(body) => assert_eq!(body, &b"fresh"[..], "attempt {attempt}"),
            // A request that got the stale one must say so recognisably.
            Err(error) => {
                failures += 1;
                assert!(
                    matches!(
                        error.win_http(),
                        Some(WinHttpError::ConnectionError)
                            | Some(WinHttpError::InvalidServerResponse)
                    ),
                    "attempt {attempt} failed unrecognisably: {error:?}"
                );
            }
        }
    }

    // If this ever stops holding, WinHTTP has started retrying stale pooled
    // connections and the documented wart in `winasio_util::client` is stale
    // too. That is worth finding out about, so it is asserted rather than
    // tolerated.
    assert!(
        failures > 0,
        "no request hit the stale pooled socket, so this test proved nothing"
    );
}

#[test]
fn a_client_is_shared_across_threads() {
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nshared",
    ));
    let client = Arc::new(client());
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let client = client.clone();
            let uri = server.uri("/");
            std::thread::spawn(move || {
                let response =
                    block_on(client.request(Request::get(uri).body(empty()).unwrap())).unwrap();
                collect(response.into_body()).unwrap()
            })
        })
        .collect();

    for handle in handles {
        assert_eq!(handle.join().unwrap(), &b"shared"[..]);
    }
}

#[test]
fn dropping_an_unfinished_body_does_not_hang() {
    // Dropping mid-transfer must return promptly rather than waiting for the
    // pause the server is sitting in.
    let server = spawn(Reply::Split(
        "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabcde",
        Duration::from_secs(5),
        "fghij",
    ));
    let response =
        block_on(client().request(Request::get(server.uri("/")).body(empty()).unwrap())).unwrap();

    let started = std::time::Instant::now();
    let body = response.into_body();
    drop(body);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "dropping took {:?}",
        started.elapsed()
    );
}

#[test]
fn the_documented_example_works_against_a_real_server() {
    // The crate-level doc example is `no_run`, because it talks to
    // example.com. This is the same code against a server that exists.
    let server = spawn(Reply::Raw(
        "HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nhello, winasio",
    ));
    let client = Client::new("winasio-util/0.1").unwrap();
    let request = Request::get(server.uri("/"))
        .body(Empty::<Bytes>::new())
        .unwrap();

    let response = block_on(client.request(request)).unwrap();
    assert!(response.status().is_success());

    let body = block_on(response.into_body().collect()).unwrap().to_bytes();
    assert_eq!(body, &b"hello, winasio"[..]);
}
