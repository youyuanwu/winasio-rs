// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! End-to-end gRPC over TLS: a tonic client on winasio-tonic's
//! [`WinHttpChannel`](winasio_tonic::WinHttpChannel) (WinHTTP HTTP/2) talking to
//! a tonic server served over HTTP.sys by
//! [`serve_grpc`](winasio_tonic::serve_grpc), all four call types.
//!
//! # Why every gRPC call type gets its own test
//!
//! The four shapes exercise progressively more of the transport:
//!
//! * **Unary** and **server-streaming** need only *send-then-receive*: the
//!   request body is complete before the response is read. They work wherever
//!   HTTP/2 does.
//! * **Client-streaming** and **bidirectional** need *duplex*: the request body
//!   keeps being written after the response has begun. On WinHTTP that is the
//!   automatic-chunking + receive-before-body-end ordering enabled in Phase 1
//!   (M6). Duplex is an additional OS capability beyond HTTP/2 (M9): Windows 11+
//!   supports it fully; Windows Server 2019/2022 supports only unary +
//!   server-streaming; Server 2025 (the CI runner as of 2026) is undocumented
//!   (M11).
//!
//! # Capability model (M8/M9/M11) — precise, never a blanket skip
//!
//! Two independent gates, reported with distinct greppable tokens so a CI log
//! says exactly what ran and why:
//!
//! 1. **TLS binding present?** Same require/skip idiom as `httpsys_tls.rs`:
//!    absent binding prints `GRPC_TLS_TEST: SKIPPED`, and — when
//!    `WINASIO_REQUIRE_TLS_TESTS=1` (CI) — is promoted to a hard panic, so a
//!    broken provisioning step cannot drain all gRPC coverage while CI stays
//!    green. This gate governs ONLY the TLS binding.
//! 2. **Automatic chunking supported?** (M8 probe.) Without it the client cannot
//!    preserve HTTP/2 and *every* gRPC call (not just duplex) would downgrade
//!    and fail (M7). So the whole suite is gated on it: unsupported prints
//!    `GRPC_TLS_TEST: SKIPPED-NO-CHUNKING` and skips — a capability skip that is
//!    always allowed, even under the require-flag (it is not a provisioning
//!    failure).
//!
//! When both gates pass, **unary + server-streaming are required**: they must
//! run and assert successfully. **Client-streaming + bidirectional** attempt the
//! call; success is a genuine run, and a failure is converted into a documented,
//! greppable `GRPC_DUPLEX: SKIPPED-UNSUPPORTED` token that carries the OS version
//! and the exact error — never a silent skip, and never masked as a pass. On a
//! duplex-capable host (Windows 11+, and this developer machine) all four run.
//!
//! # No privileged operation; identical elevated/unelevated
//!
//! Nothing here binds a certificate or mutates machine state — provisioning is
//! done out of process by `scripts/setup-https-test.ps1` (see `httpsys_tls.rs`).
//! Elevation only ever decides *whether to skip* (via the binding's presence),
//! never *what is asserted*.

#![cfg(windows)]

mod common;

use std::pin::Pin;

use common::tls_config;

use windows::core::HSTRING;

use winasio::iocp::ThreadPool;
use winasio::winhttp::Session;
use winasio_axum::CurrentThread;
use winasio_util::{CertificateRelaxations, Server, ServerSession};

use futures::Stream;
use tonic::{Request, Response, Status};

/// The generated Echo client + server stubs (both, from winasio-tonic's proto —
/// see `build.rs`). Client and server therefore share one set of message types.
pub mod echo {
    #![allow(clippy::doc_overindented_list_items)]
    tonic::include_proto!("winasio.echo.v1");
}

use echo::echo_client::EchoClient;
use echo::echo_server::{Echo, EchoServer};
use echo::{EchoReply, EchoRequest};

/// User-agent / client name for the WinHTTP session.
const AGENT: &str = "winasio-grpc-tests";

/// Generous timeout budget: a stuck handshake or duplex hang should surface as a
/// test timeout, not wedge the suite.
const CLIENT_TIMEOUT_MS: u32 = 15_000;

/// How many messages the streaming call types exchange.
const STREAM_N: usize = 3;

// ---------------------------------------------------------------------------
// The server implementation
// ---------------------------------------------------------------------------

/// A trivial Echo service covering all four call shapes. The streaming methods
/// echo **per message as it arrives** (rather than buffering the whole request
/// first) so the bidirectional test genuinely interleaves send and receive —
/// the property that requires duplex.
#[derive(Default, Clone)]
struct EchoService;

type ReplyStream = Pin<Box<dyn Stream<Item = Result<EchoReply, Status>> + Send>>;

#[tonic::async_trait]
impl Echo for EchoService {
    async fn unary(&self, request: Request<EchoRequest>) -> Result<Response<EchoReply>, Status> {
        let message = request.into_inner().message;
        Ok(Response::new(EchoReply {
            message: format!("echo: {message}"),
        }))
    }

    type ServerStreamingStream = ReplyStream;

    async fn server_streaming(
        &self,
        request: Request<EchoRequest>,
    ) -> Result<Response<Self::ServerStreamingStream>, Status> {
        let message = request.into_inner().message;
        // One request in, STREAM_N responses out.
        let replies: Vec<Result<EchoReply, Status>> = (0..STREAM_N)
            .map(|i| {
                Ok(EchoReply {
                    message: format!("{message}-{i}"),
                })
            })
            .collect();
        Ok(Response::new(Box::pin(futures::stream::iter(replies))))
    }

    async fn client_streaming(
        &self,
        request: Request<tonic::Streaming<EchoRequest>>,
    ) -> Result<Response<EchoReply>, Status> {
        let mut stream = request.into_inner();
        let mut count = 0usize;
        let mut last = String::new();
        while let Some(message) = stream.message().await? {
            count += 1;
            last = message.message;
        }
        Ok(Response::new(EchoReply {
            message: format!("received {count} messages, last={last}"),
        }))
    }

    type BidiStreamingStream = ReplyStream;

    async fn bidi_streaming(
        &self,
        request: Request<tonic::Streaming<EchoRequest>>,
    ) -> Result<Response<Self::BidiStreamingStream>, Status> {
        let incoming = request.into_inner();
        // Echo each inbound message as it arrives, so a response is produced
        // before the client has finished sending — real duplex. `unfold`
        // carries the stream as state; `None` state (after end or error) ends
        // the outbound stream.
        let outbound = futures::stream::unfold(Some(incoming), |state| async move {
            let mut incoming = state?;
            match incoming.message().await {
                Ok(Some(message)) => Some((
                    Ok(EchoReply {
                        message: format!("echo: {}", message.message),
                    }),
                    Some(incoming),
                )),
                Ok(None) => None,
                // Surface the error once, then stop (next poll sees `None` state).
                Err(status) => Some((Err(status), None)),
            }
        });
        Ok(Response::new(Box::pin(outbound)))
    }
}

// ---------------------------------------------------------------------------
// Capability detection and the require/skip harness
// ---------------------------------------------------------------------------

/// Whether a missing TLS binding must be a hard failure (CI) rather than a skip.
/// Mirrors `httpsys_tls.rs::tls_tests_required`. Governs ONLY the TLS-binding
/// gate; the auto-chunking / duplex capability skips below are separate and are
/// always allowed.
fn tls_tests_required() -> bool {
    match std::env::var("WINASIO_REQUIRE_TLS_TESTS") {
        Ok(v) => !matches!(v.trim(), "" | "0" | "false" | "False" | "FALSE"),
        Err(_) => false,
    }
}

/// Best-effort OS version string for the M11 reporting obligation. Reads it from
/// `cmd /c ver` — a non-privileged, non-mutating query. Never fails the test.
fn os_version() -> String {
    std::process::Command::new("cmd")
        .args(["/c", "ver"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The M8 automatic-chunking probe: open a throwaway request with
/// `WINHTTP_FLAG_AUTOMATIC_CHUNKING` and see whether WinHTTP accepts it. On a
/// platform lacking support this fails `ERROR_INVALID_PARAMETER` (M8); the
/// helper reports it as `false`. This is exactly the probe the client uses
/// internally to decide between the h2 duplex path and the manual fallback.
fn supports_automatic_chunking(port: u16) -> bool {
    let Ok(session) = Session::new(&HSTRING::from(AGENT)) else {
        return false;
    };
    let Ok(connection) = session.connect(&HSTRING::from("localhost"), port) else {
        return false;
    };
    connection
        .supports_automatic_chunking(&HSTRING::from("/"), true)
        .unwrap_or(false)
}

/// A ready gRPC-over-TLS environment: the config subsystem is live and the
/// certificate is bound to [`port`](Self::port).
struct GrpcEnv {
    session: ServerSession,
    port: u16,
}

/// Resolve the environment, applying both capability gates. Returns `None`
/// (already having printed the right greppable token) when the suite should
/// skip; `Some` — after printing `GRPC_TLS_TEST: RAN` — when it should run.
fn require_grpc_env() -> Option<GrpcEnv> {
    let port = tls_config::https_test_port();
    let session = ServerSession::new().expect("HTTP.sys initialises");

    // Gate 1: the TLS binding (governed by the require-flag).
    let v4: std::net::SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("valid v4 endpoint");
    match winasio::httpsys::query_ssl_binding(v4) {
        Ok(Some(_)) => {}
        Ok(None) => {
            if tls_tests_required() {
                panic!(
                    "GRPC_TLS_TEST: REQUIRED-BUT-ABSENT — WINASIO_REQUIRE_TLS_TESTS is set but no \
                     SSL binding exists on 0.0.0.0:{port}. Provisioning \
                     (scripts/setup-https-test.ps1) must run first. Failing hard so a broken \
                     provisioning step cannot drop all gRPC coverage while CI stays green."
                );
            }
            eprintln!(
                "GRPC_TLS_TEST: SKIPPED (no SSL binding on 0.0.0.0:{port}) -- provision it with an \
                 elevated `pwsh -File scripts/setup-https-test.ps1`, then re-run unelevated"
            );
            return None;
        }
        Err(e) => {
            if tls_tests_required() {
                panic!(
                    "GRPC_TLS_TEST: REQUIRED-BUT-UNREADABLE — WINASIO_REQUIRE_TLS_TESTS is set but \
                     reading the SSL table on 0.0.0.0:{port} failed: {e:?}."
                );
            }
            eprintln!(
                "GRPC_TLS_TEST: SKIPPED (could not read the SSL binding table on 0.0.0.0:{port}: \
                 {e:?})"
            );
            return None;
        }
    }

    // Gate 2: automatic chunking (M7/M8). Without it every gRPC call downgrades
    // off HTTP/2 and fails, so the whole suite skips — a capability skip that is
    // allowed even under the require-flag (it is not a provisioning failure).
    if !supports_automatic_chunking(port) {
        eprintln!(
            "GRPC_TLS_TEST: SKIPPED-NO-CHUNKING (WINHTTP_FLAG_AUTOMATIC_CHUNKING unsupported on \
             this OS: {}) -- gRPC requires HTTP/2, which manual chunking would downgrade (M7)",
            os_version()
        );
        return None;
    }

    eprintln!(
        "GRPC_TLS_TEST: RAN (TLS bound + automatic chunking supported) port={port} os={}",
        os_version()
    );
    Some(GrpcEnv { session, port })
}

impl GrpcEnv {
    fn session(&self) -> &ServerSession {
        &self.session
    }
}

/// Serialises the gRPC tests within this binary: they share the one provisioned
/// port and its root URL prefix.
static GRPC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Stand up the Echo server over HTTP.sys+TLS, run `body` (which gets a ready
/// [`EchoClient`]), then shut the server down and join it.
///
/// The server runs on its own scoped thread inside a current-thread tokio
/// runtime. The runtime is a *test-crate* convenience: it provides a reactor
/// context for the tonic-generated server's async machinery. It does **not**
/// enter winasio-tonic's own dependency graph (guarded by
/// `dependencies.rs::winasio_tonic_pulls_in_no_async_runtime_beyond_tokio`);
/// winasio-axum still owns the accept loop via `serve_grpc`.
fn with_echo_server<F, R>(env: &GrpcEnv, body: F) -> R
where
    F: FnOnce(EchoClient<winasio_tonic::WinHttpChannel>) -> R,
{
    // R6: root `localhost` prefix so tonic's `/{service}/{method}` paths reach
    // us. `localhost` needs no elevation; `+`/`*`/a machine name would.
    let server = Server::builder(env.session())
        .url(&format!("https://localhost:{}/", env.port))
        .build(&ThreadPool)
        .expect("registering the https root prefix succeeds unelevated (R6)");
    let shutdown = server.shutdown_handle();

    // tonic's server side IS an axum router: mount the generated EchoServer as
    // the router's fallback so every path routes to it (D-A / crate design).
    let router =
        winasio_tonic::server::axum::Router::new().fallback_service(EchoServer::new(EchoService));

    std::thread::scope(|scope| {
        let serve_thread = scope.spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread runtime");
            rt.block_on(async {
                // Runs until `shutdown` closes the queue, then resolves Ok.
                // The error hook surfaces per-connection server faults (e.g. an
                // h2 framing fault) on stderr; it is silent on a clean run.
                let _ = winasio_tonic::serve_grpc(&server, router, CurrentThread::new())
                    .on_error(|error| eprintln!("GRPC_SERVER_ERROR: {error}"))
                    .await;
            });
        });

        let client = build_client(env.port);
        // Run the body catching panics so a client-side assertion failure still
        // shuts the server down (otherwise the scope would block forever joining
        // a server thread that never gets its shutdown signal).
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(client)));

        shutdown.shutdown().expect("shutdown the server queue");
        serve_thread.join().expect("server thread");
        match out {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// Build a tonic Echo client over a WinHTTP HTTP/2 channel that trusts the
/// self-signed test certificate (relaxing ONLY CA trust; the `DNS:localhost` SAN
/// satisfies name validation, R5).
fn build_client(port: u16) -> EchoClient<winasio_tonic::WinHttpChannel> {
    let channel = winasio_tonic::WinHttpChannel::with_relaxations(
        format!("https://localhost:{port}")
            .parse()
            .expect("valid origin URI"),
        AGENT,
        CertificateRelaxations {
            unknown_certificate_authority: true,
            ..Default::default()
        },
    )
    .expect("build the WinHttp channel");
    let _ = CLIENT_TIMEOUT_MS; // timeouts are the client's default; kept documented.
    EchoClient::new(channel)
}

/// Drive a client future to completion on a current-thread tokio runtime.
///
/// A tokio runtime is used (not the bare `common::block_on`) because the tonic
/// client stack expects a reactor context for its `tokio::sync`/`tokio_stream`
/// machinery. winasio-util's WinHTTP client needs no runtime of its own — its
/// completions arrive on WinHTTP's callback threads and wake the future — so any
/// executor drives it; tokio is simply the one tonic is comfortable in. This is
/// test-crate-only and does not affect winasio-tonic's graph.
fn run_client<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime")
        .block_on(future)
}

/// Classify a duplex (client-streaming / bidirectional) failure. On a
/// duplex-capable host the call succeeds and this is never reached; where it is,
/// the failure becomes a documented, greppable skip carrying the OS version and
/// the exact error, per the M11 obligation — visible, never silent, never masked
/// as a pass.
fn duplex_skip(call: &str, status: &Status) {
    eprintln!(
        "GRPC_DUPLEX: SKIPPED-UNSUPPORTED call={call} os={} code={:?} message={:?} -- the duplex \
         ordering (M6) appears unsupported on this OS (M9/M11); unary + server-streaming still ran",
        os_version(),
        status.code(),
        status.message()
    );
}

// ---------------------------------------------------------------------------
// Tests — one per call type
// ---------------------------------------------------------------------------
/// Unary: one request, one response, terminating `grpc-status`. Required
/// whenever TLS + chunking are present (does not need duplex).
#[test]
fn grpc_unary() {
    let _lock = GRPC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(env) = require_grpc_env() else {
        return;
    };

    with_echo_server(&env, |mut client| {
        let reply = run_client(async {
            client
                .unary(Request::new(EchoRequest {
                    message: "hello".into(),
                }))
                .await
        })
        .expect("unary call succeeds")
        .into_inner();
        assert_eq!(reply.message, "echo: hello");
    });
}

/// Server-streaming: one request, a stream of responses ended by trailers.
/// Required whenever TLS + chunking are present (does not need duplex).
#[test]
fn grpc_server_streaming() {
    let _lock = GRPC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(env) = require_grpc_env() else {
        return;
    };

    with_echo_server(&env, |mut client| {
        let collected = run_client(async {
            let mut stream = client
                .server_streaming(Request::new(EchoRequest {
                    message: "srv".into(),
                }))
                .await
                .expect("server-streaming call succeeds")
                .into_inner();
            let mut out = Vec::new();
            while let Some(reply) = stream
                .message()
                .await
                .expect("read a server-streamed message")
            {
                out.push(reply.message);
            }
            out
        });
        let expected: Vec<String> = (0..STREAM_N).map(|i| format!("srv-{i}")).collect();
        assert_eq!(collected, expected);
    });
}

/// Client-streaming: a stream of requests, one response. Needs duplex — the
/// client keeps writing the request body after the server has been reached.
/// Attempted; a failure becomes a documented `GRPC_DUPLEX` skip (M9/M11).
#[test]
fn grpc_client_streaming() {
    let _lock = GRPC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(env) = require_grpc_env() else {
        return;
    };

    with_echo_server(&env, |mut client| {
        let requests: Vec<EchoRequest> = (0..STREAM_N)
            .map(|i| EchoRequest {
                message: format!("c{i}"),
            })
            .collect();
        let result = run_client(async {
            client
                .client_streaming(Request::new(futures::stream::iter(requests)))
                .await
        });
        match result {
            Ok(reply) => {
                let reply = reply.into_inner();
                assert_eq!(
                    reply.message,
                    format!("received {STREAM_N} messages, last=c{}", STREAM_N - 1)
                );
                eprintln!("GRPC_DUPLEX: RAN call=client_streaming os={}", os_version());
            }
            Err(status) => duplex_skip("client_streaming", &status),
        }
    });
}

/// Bidirectional: a stream each way, echoed per message. The strongest duplex
/// test — responses are produced before the client finishes sending. Attempted;
/// a failure becomes a documented `GRPC_DUPLEX` skip (M9/M11).
#[test]
fn grpc_bidi_streaming() {
    let _lock = GRPC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(env) = require_grpc_env() else {
        return;
    };

    with_echo_server(&env, |mut client| {
        let requests: Vec<EchoRequest> = (0..STREAM_N)
            .map(|i| EchoRequest {
                message: format!("b{i}"),
            })
            .collect();
        let result = run_client(async {
            let mut inbound = client
                .bidi_streaming(Request::new(futures::stream::iter(requests)))
                .await?
                .into_inner();
            let mut out = Vec::new();
            while let Some(reply) = inbound.message().await? {
                out.push(reply.message);
            }
            Ok::<_, Status>(out)
        });
        match result {
            Ok(collected) => {
                let expected: Vec<String> = (0..STREAM_N).map(|i| format!("echo: b{i}")).collect();
                assert_eq!(collected, expected);
                eprintln!("GRPC_DUPLEX: RAN call=bidi_streaming os={}", os_version());
            }
            Err(status) => duplex_skip("bidi_streaming", &status),
        }
    });
}
