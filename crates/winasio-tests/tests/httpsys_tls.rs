// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! End-to-end HTTPS tests: a WinHTTP client talking TLS to an HTTP.sys server
//! that presents a self-signed certificate.
//!
//! # Design: binding is provisioned out-of-process
//!
//! Binding a certificate to an `ip:port` in the HTTP.sys SSL table is a
//! machine-wide, administrator-only operation (measured: an unelevated
//! `netsh http add sslcert` fails `ERROR_ACCESS_DENIED (5)` even with a bogus
//! thumbprint). Rather than make every test attempt that at run time — which
//! forced the whole suite to run elevated and left machine-wide residue to
//! clean up — provisioning is done **once, out of process**, by
//! [`scripts/setup-https-test.ps1`](../../../../scripts/setup-https-test.ps1):
//! it generates a `CN=localhost` self-signed certificate (with a `DNS:localhost`
//! SAN) into `LocalMachine\My` and binds it to the fixed test port.
//!
//! These tests therefore run **unelevated**. Each one queries the SSL table for
//! the expected binding via [`query_ssl_binding`] and, when it is absent, prints
//! a greppable `HTTPS_TLS_TEST: SKIPPED` line explaining how to provision it and
//! returns without touching machine state — the same `start() -> Option<..>`
//! skip idiom the rest of the HTTP.sys suite uses. When the binding is present
//! it prints `HTTPS_TLS_TEST: RAN` and exercises the full path. CI runs with
//! `--nocapture`, so grepping the log for these tokens says definitively whether
//! the e2e path executed or was skipped (R1).
//!
//! # What running proves
//!
//! * The [`query_ssl_binding`] API reads back the externally-provisioned binding
//!   on both wildcard families.
//! * A WinHTTP client completes a real HTTPS request against HTTP.sys over the
//!   self-signed certificate, relaxing **only** CA-trust — the `DNS:localhost`
//!   SAN is expected to satisfy name validation on its own (R5).
//! * The negative control: a client that relaxes *nothing* is rejected with
//!   [`WinHttpError::SecureFailure`], i.e. the certificate check the positive
//!   test relaxes is genuinely load-bearing.
//!
//! # Registering the URL prefix needs no elevation
//!
//! Registering the `https://localhost:PORT/` URL group is separate from binding
//! the certificate and, measured on an unelevated host, succeeds without
//! administrator rights or a URL ACL (R6) — exactly as the plain-`http` HTTP.sys
//! suite already relies on. So the only privileged step is the one moved to the
//! setup script; the request path here is fully unprivileged.
//!
//! **Invariant/obligation (R6):** these tests MUST register `localhost` URLs
//! only. The unelevated success above comes from HTTP.sys's built-in allowance
//! for the `localhost` hostname, not from any URL ACL (measured: there are no
//! `urlacl` entries for `localhost`, yet `https://localhost:PORT/` registers
//! `rc=0` unelevated, while the wildcard `https://+:PORT/` fails `rc=5`
//! ACCESS_DENIED). Switching the prefix to `+`, `*`, or a machine name would
//! silently require administrator rights and turn this whole suite into a silent
//! skip — so the host is pinned to `localhost` at the one construction site
//! below, and must stay that way.
//!
//! # Serialisation
//!
//! An HTTP.sys SSL binding is keyed by `ip:port` and is machine-global, and only
//! one URL group may own a given prefix at a time. All tests here share the one
//! provisioned port, so a file-scoped [`TLS_LOCK`] serialises them (R3).

#![cfg(windows)]

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Mutex;

use windows::core::HSTRING;

use common::{block_on, tls_config};
use winasio::httpsys::{query_ssl_binding, THUMBPRINT_LEN};
use winasio::iocp::{OpResult, ThreadPool};
use winasio::winhttp::{CertificateRelaxations, Session, WinHttpError};
use winasio_util::{IncomingBody, Server, ServerSession};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;

/// Serialises every TLS test: the provisioned binding and URL prefix are global
/// per `ip:port`, so two tests running at once would contend for the same
/// prefix (R3).
static TLS_LOCK: Mutex<()> = Mutex::new(());

/// Generous client timeouts: a stuck handshake should surface as an error, not
/// hang the suite. 15s is far longer than a loopback handshake needs.
const CLIENT_TIMEOUT_MS: u32 = 15_000;

/// The body the test server returns on the happy path.
const HELLO_BODY: &[u8] = b"secure hello";

/// A ready HTTPS test environment: the config subsystem is live (so the query
/// and URL registration below work) and the certificate is already bound to
/// [`port`](Self::port) by the setup script.
struct HttpsEnv {
    /// Holds the `HttpInitializer`; kept alive for the config-subsystem calls
    /// (`query_ssl_binding`) and the URL-group registration.
    session: ServerSession,
    /// The provisioned port, from the single-source config.
    port: u16,
}

impl HttpsEnv {
    fn session(&self) -> &ServerSession {
        &self.session
    }
}

/// Return a ready [`HttpsEnv`] when the setup script has bound a certificate to
/// the test port, or `None` (with a greppable reason) when it has not — the one
/// legitimate skip.
///
/// Detection is by the binding itself, not by elevation: the whole point of the
/// redesign is that these tests need no privileges, so "can I see the binding?"
/// is the right question. Reading the SSL table is permitted unelevated
/// (measured: `netsh http show sslcert` works without administrator rights, C7).
fn require_bound_endpoint() -> Option<HttpsEnv> {
    let port = tls_config::https_test_port();

    // The initializer must exist before we touch the config subsystem.
    let session = ServerSession::new().expect("HTTP.sys initialises");

    let v4: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("valid v4 endpoint");
    match query_ssl_binding(v4) {
        Ok(Some(thumbprint)) => {
            eprintln!(
                "HTTPS_TLS_TEST: RAN (certificate bound by scripts/setup-https-test.ps1) \
                 port={port} thumbprint={}",
                thumb_hex(&thumbprint)
            );
            Some(HttpsEnv { session, port })
        }
        Ok(None) => {
            eprintln!(
                "HTTPS_TLS_TEST: SKIPPED (no SSL binding on 0.0.0.0:{port}) -- provision it with an \
                 elevated `pwsh -File scripts/setup-https-test.ps1`, then re-run these tests \
                 unelevated"
            );
            None
        }
        Err(e) => {
            // Reading the table should not fail unelevated; if it does, treat it
            // as an environment we cannot run in and skip loudly rather than
            // failing for being on the wrong host.
            eprintln!(
                "HTTPS_TLS_TEST: SKIPPED (could not read the SSL binding table on \
                 0.0.0.0:{port}: {e:?})"
            );
            None
        }
    }
}

/// Print a SHA-1 thumbprint as lowercase hex.
fn thumb_hex(thumbprint: &[u8; THUMBPRINT_LEN]) -> String {
    thumbprint.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the one-shot HTTP.sys server for the environment, listening on
/// `https://localhost:{port}/tls/`. Registering the prefix needs no elevation
/// (R6) — but ONLY because the host is `localhost`; `+`/`*`/a machine name would
/// silently require admin (see the module-level R6 invariant), so keep it here.
fn build_server(env: &HttpsEnv) -> Server<'_> {
    Server::builder(env.session())
        .url(&format!("https://localhost:{}/tls/", env.port))
        .build(&ThreadPool)
        .expect("registering the https URL prefix succeeds unelevated (R6)")
}

/// Drive one HTTPS GET from the client side and return `(status, body)`.
///
/// `relaxations` selects which certificate checks to waive; the negative
/// control passes the default (waive nothing). Runs entirely on the calling
/// thread inside `block_on`, so the caller is expected to be serving the
/// request from another thread.
fn https_get(
    port: u16,
    path: &str,
    relaxations: CertificateRelaxations,
) -> Result<(u32, Vec<u8>), WinHttpError> {
    let session = Session::new(&HSTRING::from("winasio-tls-tests")).map_err(from_err)?;
    session
        .set_timeouts(
            CLIENT_TIMEOUT_MS as i32,
            CLIENT_TIMEOUT_MS as i32,
            CLIENT_TIMEOUT_MS as i32,
            CLIENT_TIMEOUT_MS as i32,
        )
        .map_err(from_err)?;
    let connection = session
        .connect(&HSTRING::from("localhost"), port)
        .map_err(from_err)?;
    let mut request = connection
        .open_request(&HSTRING::from("GET"), &HSTRING::from(path), &[], true)
        .map_err(from_err)?;
    request
        .relax_certificate_validation(relaxations)
        .map_err(from_err)?;

    block_on(async {
        request.send(None, Vec::new(), 0).await.map_err(from_err)?;
        request.receive_response().await.map_err(from_err)?;
        let status = request.status_code().map_err(from_err)?;
        let mut body = Vec::new();
        loop {
            let available = request.query_data_available().await.map_err(from_err)?;
            if available == 0 {
                break;
            }
            let OpResult(read, chunk) = request
                .read_data(Vec::with_capacity(available as usize))
                .await;
            let read = read.map_err(from_err)?;
            body.extend_from_slice(&chunk[..read]);
        }
        Ok((status, body))
    })
}

/// Classify a raw `windows` error as a `WinHttpError` so the negative control
/// can assert the *specific* failure, not just "an error".
fn from_err(error: windows::core::Error) -> WinHttpError {
    WinHttpError::from_error(&error)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The happy path: an HTTPS request that trusts the self-signed authority
/// succeeds end to end (SC-001, FR-008).
#[test]
fn https_roundtrip_positive() {
    let _lock = TLS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(env) = require_bound_endpoint() else {
        return;
    };
    let port = env.port;
    let server = build_server(&env);

    // The client runs on its own thread; the server answers one request here.
    let client = std::thread::spawn(move || {
        // R5: relax ONLY the CA-trust check. The DNS:localhost SAN plus the
        // `localhost` request host should satisfy name validation on their own.
        https_get(
            port,
            "/tls/x",
            CertificateRelaxations {
                unknown_certificate_authority: true,
                ..Default::default()
            },
        )
    });

    let mut service = tower::service_fn(|_req: Request<IncomingBody>| async {
        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(HELLO_BODY))))
    });
    // If the TLS handshake fails — the exact regression this test guards — the
    // client returns a `WinHttpError` within CLIENT_TIMEOUT_MS while `serve_one`
    // never receives a request and eventually trips the harness deadline with a
    // generic "timed out" panic. Catch that so the client's real error (the only
    // useful diagnostic) reaches the log instead of the server-side symptom.
    let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(server.serve_one(&mut service))
    }));
    let client = client.join().expect("client thread");

    let (status, body) = match (client, served) {
        (Ok(resp), Ok(serve_result)) => {
            serve_result.expect("serve_one");
            resp
        }
        (Err(e), _) => panic!(
            "the HTTPS request failed: {e:?} \
             (server never received a request — likely a TLS handshake failure)"
        ),
        (Ok(_), Err(_)) => panic!(
            "serve_one failed or timed out while the client did not error; \
             the server side did not complete the request"
        ),
    };
    assert_eq!(status, 200, "an accepted HTTPS request returns 200");
    assert_eq!(
        body, HELLO_BODY,
        "the server's body arrives intact over TLS"
    );
}

/// The externally-provisioned binding is observable through the query API on
/// both wildcard families, and both families report the same certificate
/// (SC-003) — i.e. the setup script's dual-family binding contract holds.
#[test]
fn https_binding_observable_on_both_families() {
    let _lock = TLS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(env) = require_bound_endpoint() else {
        return;
    };
    let port = env.port;

    let v4: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let v6: SocketAddr = format!("[::]:{port}").parse().unwrap();

    let v4_thumb = query_ssl_binding(v4).expect("query v4 succeeds");
    let v6_thumb = query_ssl_binding(v6).expect("query v6 succeeds");

    assert!(
        v4_thumb.is_some(),
        "the v4 binding is observable (the setup script binds 0.0.0.0)"
    );
    assert!(
        v6_thumb.is_some(),
        "the v6 binding is observable (the setup script binds [::])"
    );
    assert_eq!(
        v4_thumb, v6_thumb,
        "both families report the same provisioned certificate"
    );
}

/// The negative control: a client that waives no certificate checks must be
/// rejected against the self-signed certificate, and specifically with
/// [`WinHttpError::SecureFailure`] — not a timeout or a refused connection, so
/// the failure is the certificate check and nothing else (SC-002, FR-009).
#[test]
fn https_negative_control_unrelaxed_fails() {
    let _lock = TLS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(env) = require_bound_endpoint() else {
        return;
    };
    let port = env.port;
    // The URL prefix must be registered for HTTP.sys to listen and perform the
    // handshake at all; the request itself never reaches `serve_one` because the
    // handshake fails first, so no server loop is run here.
    let _server = build_server(&env);

    let result = https_get(port, "/tls/x", CertificateRelaxations::default());

    match result {
        Err(WinHttpError::SecureFailure) => {
            // The certificate check fired exactly as intended.
        }
        Err(WinHttpError::Timeout) => panic!(
            "an unrelaxed client TIMED OUT rather than being rejected — the most likely cause is \
             that the handshake SUCCEEDED (certificate validation is not being enforced) and then \
             no server loop answered the request; a working control must fail with SecureFailure"
        ),
        Err(other) => panic!(
            "expected SecureFailure from an unrelaxed client against a self-signed cert, \
             got {other:?} (a different error means the control failed for the wrong reason)"
        ),
        Ok((status, _)) => panic!(
            "an unrelaxed client unexpectedly SUCCEEDED (status {status}) against a self-signed \
             certificate — certificate validation is not being enforced"
        ),
    }
}

// ---------------------------------------------------------------------------
// Always-on: the single-source config parser (no elevation, no binding needed)
// ---------------------------------------------------------------------------
//
// These pin down that this test binary and `scripts/setup-https-test.ps1` read
// the same port from the one config file, so the two cannot drift into binding
// one port and connecting to another (E-F).

#[test]
fn shared_config_port_is_a_plausible_user_port() {
    let port = tls_config::https_test_port();
    assert!(
        port >= 1024,
        "the shared test port must be a non-privileged port, got {port}"
    );
}

#[test]
fn config_parser_reads_a_simple_assignment() {
    let src = "# comment\n$HttpsTestPort = 12495\n$Other = 1\n";
    assert_eq!(
        tls_config::parse_u16_assignment(src, "HttpsTestPort"),
        Some(12495)
    );
}

#[test]
fn config_parser_ignores_similar_names_and_comments() {
    let src = "#$HttpsTestPort = 1\n$HttpsTestPortX = 2\n$HttpsTestPort = 12495\n";
    assert_eq!(
        tls_config::parse_u16_assignment(src, "HttpsTestPort"),
        Some(12495)
    );
}

#[test]
fn config_parser_returns_none_when_absent() {
    assert_eq!(
        tls_config::parse_u16_assignment("$Foo = 3\n", "HttpsTestPort"),
        None
    );
}
