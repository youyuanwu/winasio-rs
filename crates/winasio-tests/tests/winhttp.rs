// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Integration tests for the `winhttp` module.
//!
//! The server here is a dumb `std::net::TcpListener` on a `std::thread`. That
//! is deliberate: the point of most of these tests is that the client needs no
//! runtime, and a test whose *server* dragged in an async runtime would make it
//! much harder to see that the client did not.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::HSTRING;

use winasio::iocp::{IoBuf, OpResult};
use winasio::winhttp::{
    encode_headers, live_context_count, CertificateRelaxations, Session, WinHttpError,
};

// ------------------------------------------------------------------ server

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerMode {
    /// Read the whole request, then reply `200` with a fixed body.
    Respond,
    /// Read the whole request, then reply `201` with several extra headers.
    RespondWithHeaders,
    /// Accept the connection and then do nothing at all — never even reading
    /// the request. The *send* is what stalls here, not the receive.
    Silent,
    /// Read the whole request, then never answer. This is what stalls a
    /// `receive_response`: measured, the send completes normally and only the
    /// receive is left waiting.
    ReadThenSilent,
    /// Answer the headers immediately, then pause before sending the body.
    ///
    /// This is the only reliable way to hold an operation genuinely pending:
    /// the headers arrive, so `receive_response` completes, and then
    /// `query_data_available` has nothing to report until the pause ends.
    SlowBody,
    /// Read the request, then close without writing any response.
    AcceptThenClose,
}

struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Server {
    fn host(&self) -> HSTRING {
        HSTRING::from("127.0.0.1")
    }

    /// The bytes of the first request the server received.
    fn first_request(&self) -> String {
        let requests = self.requests.lock().unwrap();
        String::from_utf8_lossy(requests.first().map(Vec::as_slice).unwrap_or(&[])).into_owned()
    }
}

fn spawn_server(mode: ServerMode, body: &'static str) -> Server {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = requests.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let sink = sink.clone();
            std::thread::spawn(move || serve(stream, mode, body, sink));
        }
    });

    Server { port, requests }
}

fn serve(
    mut stream: TcpStream,
    mode: ServerMode,
    body: &'static str,
    sink: Arc<Mutex<Vec<Vec<u8>>>>,
) {
    if mode == ServerMode::Silent {
        // Hold the connection open and never answer, so the client's receive
        // deadline is the only thing that can end the test.
        std::thread::sleep(Duration::from_secs(30));
        return;
    }

    let Some(request) = read_request(&mut stream) else {
        return;
    };
    sink.lock().unwrap().push(request);

    match mode {
        ServerMode::ReadThenSilent => {
            // The request has been read, so the client's send completes. Keep
            // the connection open and never answer.
            std::thread::sleep(Duration::from_secs(30));
        }
        ServerMode::AcceptThenClose => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        ServerMode::Respond => {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
        ServerMode::RespondWithHeaders => {
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nX-Winasio-One: alpha\r\nX-Winasio-Two: beta\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
        ServerMode::SlowBody => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(1_500));
            let _ = stream.write_all(body.as_bytes());
        }
        // Handled before the request was read.
        ServerMode::Silent => {}
    }
    let _ = stream.flush();
}

/// Read headers, then any body a `Content-Length` declares.
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

fn agent() -> HSTRING {
    HSTRING::from("winasio-tests")
}

/// Drive a whole GET on a bare executor and hand back status, headers and body.
///
/// Everything after `Session::new` runs inside `futures::executor::block_on`.
fn get(server: &Server, path: &str) -> (u32, String, Vec<u8>) {
    let session = Session::new(&agent()).unwrap();
    session.set_timeouts(5_000, 5_000, 5_000, 5_000).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from(path), &[], false)
        .unwrap();

    futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await.unwrap();
        request.receive_response().await.unwrap();

        let status = request.status_code().unwrap();
        let headers = request.raw_headers().unwrap();

        let mut body = Vec::new();
        loop {
            let available = request.query_data_available().await.unwrap();
            if available == 0 {
                break;
            }
            let OpResult(read, chunk) = request
                .read_data(Vec::with_capacity(available as usize))
                .await;
            body.extend_from_slice(&chunk[..read.unwrap()]);
        }
        (status, headers, body)
    })
}

// ------------------------------------------------------------------- tests

#[test]
fn a_get_completes_on_a_bare_executor_with_no_runtime() {
    // The whole point of the rewrite: `futures::executor::block_on` is a
    // single-threaded executor with no reactor and no worker threads. If any
    // part of this module quietly needed a runtime, this test could not pass.
    let server = spawn_server(ServerMode::Respond, "hello from a bare executor");
    let (status, _, body) = get(&server, "/");
    assert_eq!(status, 200);
    assert_eq!(body, b"hello from a bare executor");
}

#[tokio::test]
async fn the_same_client_also_works_inside_a_tokio_runtime() {
    // The client is runtime-agnostic, not runtime-hostile. Running the same
    // code under tokio proves the bare-executor test above is not passing
    // because of some property peculiar to `block_on`.
    let server = spawn_server(ServerMode::Respond, "hello from tokio");
    let (status, _, body) = tokio::task::spawn_blocking(move || get(&server, "/"))
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, b"hello from tokio");
}

#[test]
fn the_status_code_is_the_servers_and_not_a_guess() {
    let server = spawn_server(ServerMode::RespondWithHeaders, "{}");
    let (status, _, _) = get(&server, "/created");
    assert_eq!(status, 201);
}

#[test]
fn a_header_the_server_sent_is_returned_by_name() {
    let server = spawn_server(ServerMode::RespondWithHeaders, "{}");
    let session = Session::new(&agent()).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
        .unwrap();

    futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await.unwrap();
        request.receive_response().await.unwrap();
    });

    assert_eq!(
        request.header(&HSTRING::from("X-Winasio-One")).unwrap(),
        Some("alpha".to_string())
    );
    assert_eq!(
        request.header(&HSTRING::from("X-Winasio-Two")).unwrap(),
        Some("beta".to_string())
    );
}

#[test]
fn an_absent_header_is_none_rather_than_an_error() {
    // "The server did not send that header" and "the query failed" are
    // different answers, and a caller must be able to tell them apart.
    let server = spawn_server(ServerMode::Respond, "body");
    let session = Session::new(&agent()).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
        .unwrap();

    futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await.unwrap();
        request.receive_response().await.unwrap();
    });

    assert_eq!(
        request
            .header(&HSTRING::from("X-Not-Sent-By-Anyone"))
            .unwrap(),
        None
    );
}

#[test]
fn the_raw_headers_carry_the_status_line_and_every_header() {
    let server = spawn_server(ServerMode::RespondWithHeaders, "{}");
    let (_, headers, _) = get(&server, "/");
    assert!(headers.starts_with("HTTP/1.1 201"), "got {headers:?}");
    assert!(headers.contains("X-Winasio-One: alpha"), "got {headers:?}");
    assert!(headers.contains("\r\n"), "headers should be CRLF separated");
}

#[test]
fn query_data_available_reports_zero_exactly_once_at_the_end_of_the_body() {
    // Zero from `query_data_available` is the end-of-body signal. A test that
    // only checked the body contents would not notice if the loop terminated
    // for the wrong reason.
    let server = spawn_server(ServerMode::Respond, "0123456789");
    let session = Session::new(&agent()).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
        .unwrap();

    let (body, zeros) = futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await.unwrap();
        request.receive_response().await.unwrap();

        let mut body = Vec::new();
        let mut zeros = 0;
        loop {
            let available = request.query_data_available().await.unwrap();
            if available == 0 {
                zeros += 1;
                break;
            }
            let OpResult(read, chunk) = request
                .read_data(Vec::with_capacity(available as usize))
                .await;
            body.extend_from_slice(&chunk[..read.unwrap()]);
        }
        (body, zeros)
    });

    assert_eq!(body, b"0123456789");
    assert_eq!(zeros, 1);
}

#[test]
fn a_request_body_reaches_the_server_through_send_and_write_data() {
    let server = spawn_server(ServerMode::Respond, "ok");
    let session = Session::new(&agent()).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(
            &HSTRING::from("POST"),
            &HSTRING::from("/submit"),
            &[],
            false,
        )
        .unwrap();

    let headers = encode_headers([("Content-Type", "text/plain")]);
    let first = b"first-half;".to_vec();
    let second = b"second-half".to_vec();
    let total = (first.len() + second.len()) as u32;

    futures::executor::block_on(async {
        // Send with no inline body, declaring the total length, then stream it.
        request
            .send(Some(headers), Vec::new(), total)
            .await
            .unwrap();

        let OpResult(written, buffer) = request.write_data(first).await;
        assert_eq!(written.unwrap(), buffer.len());

        let OpResult(written, buffer) = request.write_data(second).await;
        assert_eq!(written.unwrap(), buffer.len());

        request.receive_response().await.unwrap();
    });

    assert_eq!(request.status_code().unwrap(), 200);
    let received = server.first_request();
    assert!(received.starts_with("POST /submit"), "got {received:?}");
    assert!(
        received.contains("Content-Type: text/plain"),
        "got {received:?}"
    );
    assert!(
        received.ends_with("first-half;second-half"),
        "the body should have arrived intact, got {received:?}"
    );
}

#[test]
fn a_send_body_is_held_until_the_response_is_received_not_until_the_send_completes() {
    // WinHTTP may re-read `lpOptional` after the send completes — to follow a
    // redirect, or to replay the request against an authentication challenge —
    // so the body must outlive `send`. This test watches the body's destructor
    // to prove it does: the flag must still be clear once the send has
    // completed, and set only after the response has been received.
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TrackedBody {
        bytes: Vec<u8>,
        dropped: Arc<AtomicBool>,
    }

    // SAFETY: `stable_ptr` returns the `Vec`'s heap allocation, which does not
    // move when the `TrackedBody` value is moved, and `bytes_init` is that
    // allocation's initialised length.
    unsafe impl IoBuf for TrackedBody {
        fn stable_ptr(&self) -> *const u8 {
            self.bytes.as_ptr()
        }
        fn bytes_init(&self) -> usize {
            self.bytes.len()
        }
    }

    impl Drop for TrackedBody {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    let server = spawn_server(ServerMode::Respond, "ok");
    let session = Session::new(&agent()).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("POST"), &HSTRING::from("/"), &[], false)
        .unwrap();

    let dropped = Arc::new(AtomicBool::new(false));
    let body = TrackedBody {
        bytes: b"payload".to_vec(),
        dropped: Arc::clone(&dropped),
    };
    let length = body.bytes.len() as u32;

    futures::executor::block_on(async {
        request.send(None, body, length).await.unwrap();
        assert!(
            !dropped.load(Ordering::SeqCst),
            "the body must still be alive after the send completes, because \
             WinHTTP may re-send it before the response is received"
        );

        request.receive_response().await.unwrap();
    });

    assert!(
        dropped.load(Ordering::SeqCst),
        "the body should be released once the response has been received"
    );
    assert!(server.first_request().ends_with("payload"));
}

#[test]
fn a_zero_capacity_read_is_refused_rather_than_reported_as_end_of_body() {
    // A zero-length read looks exactly like the end of the body to a caller
    // looping until a read returns nothing. Refusing it turns a silent
    // truncation into a visible error, and hands the buffer straight back.
    let server = spawn_server(ServerMode::Respond, "some body");
    let session = Session::new(&agent()).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
        .unwrap();

    let result = futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await.unwrap();
        request.receive_response().await.unwrap();
        let OpResult(result, buffer) = request.read_data(Vec::<u8>::new()).await;
        // The buffer is returned even though nothing was submitted.
        assert!(buffer.is_empty());
        result
    });

    assert!(result.is_err(), "a zero-capacity read must not succeed");
}

#[test]
fn a_receive_deadline_that_elapses_is_an_error_not_an_empty_body() {
    // The lesson `net` learned: classifying a failure as a successful empty
    // result silently truncates data. A server that never answers must produce
    // an error, not a zero-byte body.
    //
    // The server reads the request first. That matters: against a server that
    // never even reads, it is the *send* that stalls, and this test would be
    // measuring the send deadline while claiming to measure the receive one.
    let server = spawn_server(ServerMode::ReadThenSilent, "");
    let session = Session::new(&agent()).unwrap();
    session.set_timeouts(2_000, 2_000, 2_000, 800).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
        .unwrap();

    let outcome = futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await?;
        request.receive_response().await
    });

    let error = outcome.expect_err("a silent server must not look like success");
    assert_eq!(WinHttpError::from_error(&error), WinHttpError::Timeout);
}

#[test]
fn a_server_that_closes_without_answering_is_an_error_not_an_empty_body() {
    let server = spawn_server(ServerMode::AcceptThenClose, "");
    let session = Session::new(&agent()).unwrap();
    session.set_timeouts(5_000, 5_000, 5_000, 5_000).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
        .unwrap();

    let outcome = futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await?;
        request.receive_response().await
    });

    assert!(
        outcome.is_err(),
        "a peer that vanished must be reported as a failure"
    );
}

#[test]
fn a_second_operation_started_behind_an_abandoned_one_is_refused() {
    // WinHTTP allows one transfer per request. Abandoning a future is the one
    // case `&mut self` cannot catch, so it must be refused here rather than
    // relayed to the platform and turned into an opaque handle-state error.
    //
    // The server sends the headers and then pauses. That shape was arrived at
    // by measurement: a server that never answers at all does not leave a
    // `receive_response` pending, because WinHTTP has already spent the
    // deadline during the send and reports the timeout inline on the first
    // poll. Only a body that has not arrived yet holds an operation open.
    let server = spawn_server(ServerMode::SlowBody, "delayed body");
    let session = Session::new(&agent()).unwrap();
    session.set_timeouts(5_000, 5_000, 5_000, 5_000).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
        .unwrap();

    let error = futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await.unwrap();
        request.receive_response().await.unwrap();

        // Poll a read once, so it is genuinely submitted, then drop it.
        let mut read = Box::pin(request.read_data(Vec::<u8>::with_capacity(64)));
        let polled = futures::poll!(read.as_mut());
        assert!(
            polled.is_pending(),
            "the body cannot have arrived yet, got {polled:?}"
        );
        drop(read);

        // The abandoned operation still occupies the request.
        let OpResult(result, _) = request.read_data(Vec::<u8>::with_capacity(64)).await;
        result.expect_err("must be refused")
    });

    assert_eq!(
        WinHttpError::from_error(&error),
        WinHttpError::OperationInProgress
    );
}

#[test]
fn abandoning_an_operation_and_dropping_the_request_releases_its_context() {
    // The whole reason `HANDLE_CLOSING` is handled: it is the only signal that
    // says WinHTTP will never call back again, and therefore the only correct
    // place to free the context. If it were mishandled, this count would drift
    // upwards and nothing else in the suite would notice.
    let server = spawn_server(ServerMode::Silent, "");
    let before = live_context_count();

    for _ in 0..8 {
        let session = Session::new(&agent()).unwrap();
        session.set_timeouts(2_000, 2_000, 2_000, 2_000).unwrap();
        let connection = session.connect(&server.host(), server.port).unwrap();
        let mut request = connection
            .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
            .unwrap();

        futures::executor::block_on(async {
            let mut send = Box::pin(request.send(None, vec![0u8; 4096], 4096));
            let _ = futures::poll!(send.as_mut());
            // Dropped mid-flight: the buffer is retired, not freed.
            drop(send);
        });
        drop(request);
        drop(connection);
        drop(session);
    }

    // `HANDLE_CLOSING` may be delivered on a WinHTTP pool thread after the
    // close returns, so the count settles shortly afterwards rather than
    // immediately.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while live_context_count() > before && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        live_context_count(),
        before,
        "every abandoned request must release its context"
    );
}

#[test]
fn a_request_keeps_working_after_its_session_and_connection_values_are_dropped() {
    // Measured, and it contradicts the obvious assumption: closing the
    // *connection* handle is harmless, but closing the *session* handle
    // cancels every later operation on a derived request with 12017, delivered
    // inline before the submitting call returns.
    //
    // The crate hides that by holding the session handle open behind an `Arc`,
    // so dropping the values in any order is safe. If that reference were ever
    // dropped, this test would fail with `Cancelled` rather than hang — which
    // is exactly why it exists.
    let server = spawn_server(ServerMode::Respond, "still here");
    let mut request = {
        let session = Session::new(&agent()).unwrap();
        session.set_timeouts(5_000, 5_000, 5_000, 5_000).unwrap();
        let connection = session.connect(&server.host(), server.port).unwrap();
        connection
            .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)
            .unwrap()
        // `session` and `connection` are dropped here.
    };

    let body = futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await.unwrap();
        request.receive_response().await.unwrap();
        let available = request.query_data_available().await.unwrap();
        let OpResult(read, chunk) = request
            .read_data(Vec::with_capacity(available as usize))
            .await;
        chunk[..read.unwrap()].to_vec()
    });

    assert_eq!(body, b"still here");
}

#[test]
fn a_secure_request_to_a_server_speaking_plain_http_fails() {
    // Measured, not assumed: this surfaces as a timeout rather than a
    // certificate failure, because the plain-HTTP listener never answers the
    // TLS handshake. The short receive deadline is what keeps the test quick.
    let server = spawn_server(ServerMode::Respond, "not tls");
    let session = Session::new(&agent()).unwrap();
    session.set_timeouts(2_000, 2_000, 2_000, 1_000).unwrap();
    let connection = session.connect(&server.host(), server.port).unwrap();
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], true)
        .unwrap();
    request
        .relax_certificate_validation(CertificateRelaxations {
            unknown_certificate_authority: true,
            wrong_host_name: true,
            ..Default::default()
        })
        .unwrap();

    let outcome = futures::executor::block_on(async { request.send(None, Vec::new(), 0).await });
    assert!(
        outcome.is_err(),
        "TLS to a plain-HTTP listener must not succeed"
    );
}

#[test]
fn every_winhttp_error_variant_round_trips_through_its_code() {
    // The classifier is the only thing standing between a caller and a raw
    // `HRESULT`, so a variant that does not survive the round trip would make
    // its own error unmatchable.
    for variant in [
        WinHttpError::Timeout,
        WinHttpError::CannotConnect,
        WinHttpError::ConnectionError,
        WinHttpError::NameNotResolved,
        WinHttpError::SecureFailure,
        WinHttpError::HeaderNotFound,
        WinHttpError::IncorrectHandleState,
        WinHttpError::Cancelled,
        WinHttpError::OperationInProgress,
        WinHttpError::InvalidServerResponse,
    ] {
        assert_eq!(WinHttpError::from_win32(variant.code()), variant);
    }
}

#[test]
fn the_readme_example_is_a_subset_of_a_test_that_actually_runs() {
    // A README example that has never been compiled is a liability. This keeps
    // the documented snippet literally identical, line for line, to code the
    // suite executes.
    let readme = include_str!("../../../README.md");
    let source = include_str!("winhttp.rs");

    let snippet = readme
        .split("# Winhttp")
        .nth(1)
        .and_then(|section| section.split("```rs").nth(1))
        .and_then(|rest| rest.split("```").next())
        .expect("the README should contain a fenced example under `# Winhttp`");

    let wanted: Vec<&str> = snippet
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert!(wanted.len() > 5, "the example should be substantial");

    let actual: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    assert!(
        actual.windows(wanted.len()).any(|window| window == wanted),
        "the README example is no longer a contiguous part of any test:\n{}",
        wanted.join("\n")
    );
}

#[test]
fn the_documented_example_fetches_a_body() -> Result<(), windows::core::Error> {
    let server = spawn_server(ServerMode::Respond, "hello, winasio");
    let host = server.host();
    let port = server.port;

    // ---- README example begins ----
    let session = Session::new(&HSTRING::from("winasio-example"))?;
    session.set_timeouts(5_000, 5_000, 5_000, 5_000)?;
    let connection = session.connect(&host, port)?;
    let mut request =
        connection.open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)?;

    let body = futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await?;
        request.receive_response().await?;
        let status = request.status_code()?;
        assert_eq!(status, 200);

        let mut body = Vec::new();
        loop {
            let available = request.query_data_available().await?;
            if available == 0 {
                break;
            }
            let OpResult(read, chunk) = request
                .read_data(Vec::with_capacity(available as usize))
                .await;
            body.extend_from_slice(&chunk[..read?]);
        }
        Ok::<_, windows::core::Error>(body)
    })?;
    // ---- README example ends ----

    assert_eq!(body, b"hello, winasio");
    Ok(())
}
