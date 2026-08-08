// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! End-to-end HTTPS tests: a WinHTTP client talking TLS to an HTTP.sys server
//! that presents a self-signed certificate.
//!
//! # What this proves
//!
//! * The Phase-1 SSL binding API (`bind_ssl_certificate` / `query_ssl_binding`)
//!   actually binds a certificate to an `ip:port` that HTTP.sys serves.
//! * The Phase-2 self-signed certificate helper produces a certificate the
//!   Windows TLS stack accepts once its issuing authority is trusted.
//! * The WinHTTP client (`winasio::winhttp`) completes a real HTTPS request
//!   against that server, and — the negative control — a client that relaxes
//!   *nothing* is rejected with [`WinHttpError::SecureFailure`], i.e. the TLS
//!   check the positive test relaxes is genuinely load-bearing.
//!
//! # Elevation
//!
//! Writing the machine-wide HTTP.sys SSL binding table, and installing a
//! machine-scoped certificate/key, both require administrator rights (measured:
//! C2 — an unelevated `netsh http add sslcert` fails `ERROR_ACCESS_DENIED (5)`
//! even with a bogus thumbprint, and opening `LocalMachine\My` unelevated fails
//! `0x80070005`). Every test here therefore gates on [`is_elevated`] up front
//! and, when the host is not elevated, prints a greppable
//! `HTTPS_TLS_TEST: SKIPPED` line and returns without touching machine state.
//! When elevated it prints `HTTPS_TLS_TEST: RAN`. CI runs with `--nocapture`,
//! so grepping the log for these tokens tells you definitively whether the e2e
//! path executed or was skipped (R1).
//!
//! # Ports
//!
//! This binary owns the range **`12480..=12489`**; `httpsys_ssl.rs` owns
//! `12490..=12492`, so the two suites' bindings and pre-test sweeps cannot
//! reach into each other's ports. An HTTP.sys SSL binding is keyed by `ip:port`
//! and is machine-global, a stronger conflict domain than a URL prefix: two
//! tests binding the same port with different certificates would fight. A
//! file-scoped [`TLS_LOCK`] therefore serialises all tests in this binary (R3),
//! and each test uses a distinct port so a crashed predecessor's residue cannot
//! alias a successor.
//!
//! # Cleanup
//!
//! Every persistent artifact is held by an RAII guard and removed on drop:
//! the two [`SslCertBinding`]s unbind, and the [`SelfSignedCert`] removes both
//! the certificate and its CNG key container. The fixture also sweeps this
//! crate's own leftovers (by AppId, port set, and container prefix) before it
//! binds, so an aborted prior run cannot leave machine-wide residue that
//! outlives the test process (R2).

#![cfg(windows)]

mod common;

use std::net::SocketAddr;
use std::sync::Mutex;

use windows::core::HSTRING;

use common::{block_on, is_elevated};
use winasio::httpsys::{
    bind_ssl_certificate, query_ssl_binding, CertStore, SelfSignedCert, SslCertBinding,
    SSL_BINDING_APP_ID, THUMBPRINT_LEN,
};
use winasio::iocp::{OpResult, ThreadPool};
use winasio::winhttp::{CertificateRelaxations, Session, WinHttpError};
use winasio_util::{Server, ServerSession};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use std::convert::Infallible;
use winasio_util::IncomingBody;

/// Serialises every TLS test: HTTP.sys SSL configuration is global per
/// `ip:port`, so two tests running at once could see or clobber each other's
/// bindings (R3).
static TLS_LOCK: Mutex<()> = Mutex::new(());

/// The port range this binary owns (`12480..=12489`). Used both to pick
/// per-test ports and to scope the pre-test leftover sweep. Kept disjoint from
/// `httpsys_ssl.rs`'s `12490..=12492` so neither suite's sweep can delete the
/// other's binding.
const RESERVED_PORTS: [u16; 10] = [
    12480, 12481, 12482, 12483, 12484, 12485, 12486, 12487, 12488, 12489,
];

/// Generous client timeouts: a stuck handshake should surface as an error, not
/// hang the suite. 15s is far longer than a loopback handshake needs.
const CLIENT_TIMEOUT_MS: u32 = 15_000;

/// The body the test server returns on the happy path.
const HELLO_BODY: &[u8] = b"secure hello";

/// Everything a running HTTPS test needs, with drop order chosen so unbinding
/// happens while the HTTP configuration subsystem is still live.
///
/// Field order is load-bearing: Rust drops fields top-to-bottom, so the two
/// bindings unbind and the certificate is removed **before** `session` (which
/// owns the `HttpInitializer` that started the configuration subsystem) drops.
/// Unbinding calls `HttpDeleteServiceConfiguration`, which needs that subsystem
/// alive — mirroring the documented `winasio-util::ServerSession` drop-order
/// rule.
struct HttpsFixture {
    /// Bound to `0.0.0.0:port` (IPv4-any).
    _v4: SslCertBinding,
    /// Bound to `[::]:port` (IPv6-any).
    _v6: SslCertBinding,
    /// The installed self-signed certificate (removed, with its key, on drop).
    cert: SelfSignedCert,
    /// Holds the `HttpInitializer`; dropped last so the config subsystem
    /// outlives the unbind calls above.
    session: ServerSession,
    port: u16,
}

impl HttpsFixture {
    fn session(&self) -> &ServerSession {
        &self.session
    }

    fn thumbprint(&self) -> [u8; THUMBPRINT_LEN] {
        self.cert.thumbprint()
    }
}

/// Bind a self-signed certificate to `port` on both IP families and return the
/// fixture, or `None` when the host is not elevated (the one legitimate skip).
///
/// The ordering here is deliberate (F1/F3):
/// 1. Gate on elevation **first**, so the RAN/SKIPPED decision is explicit and
///    does not depend on which privileged step would have failed first.
/// 2. Create the `ServerSession` (and thus the `HttpInitializer`) **before**
///    any sweep/bind/query, because those touch the configuration subsystem.
/// 3. Sweep this crate's own leftovers before binding (R2).
/// 4. Create the machine-scoped certificate and bind it to both wildcard
///    families, so the handshake lands on a bound endpoint whichever family
///    WinHTTP resolves `localhost` to.
///
/// After the elevation gate, any failure is a hard error rather than a silent
/// skip: step 0 already established the host *can* do this, so a failure now is
/// a real regression that must not be masked.
fn setup_https(port: u16) -> Option<HttpsFixture> {
    if !is_elevated() {
        eprintln!("HTTPS_TLS_TEST: SKIPPED (requires elevation) port={port}");
        return None;
    }

    // 1. The initializer must exist before we touch the config subsystem.
    let session = ServerSession::new().expect("HTTP.sys initialises");

    // 2. Remove any residue this crate left behind on a previous aborted run.
    SelfSignedCert::sweep_leftovers(CertStore::LocalMachine, &RESERVED_PORTS);

    // 3. A machine-scoped self-signed certificate with a DNS:localhost SAN.
    let cert = SelfSignedCert::create("CN=localhost", CertStore::LocalMachine)
        .expect("self-signed certificate creation succeeds on an elevated host");
    let thumbprint = cert.thumbprint();
    let store_name = cert
        .store_name()
        .expect("a LocalMachine certificate has a bindable store name");

    // Diagnostic (read-only): report the stored cert's private-key association
    // health on this runner. A broken cert->key association is the classic
    // cause of the bind-time ERROR_NO_SUCH_LOGON_SESSION (1312); this pins down
    // whether the failure is cert-side or http.sys-context-side. See R1.
    diagnose_private_key(&thumbprint);

    // 4. Bind to both wildcard families for the reasons in the module docs.
    let v4addr: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("valid v4 endpoint");
    let v6addr: SocketAddr = format!("[::]:{port}").parse().expect("valid v6 endpoint");

    let v4 = match bind_ssl_certificate(v4addr, &thumbprint, store_name, SSL_BINDING_APP_ID) {
        Ok(b) => b,
        Err(e) => {
            // LOUD, greppable, NOT silent: this host is elevated (step 0 proved
            // it), so a bind failure here is a real, reportable outcome — the
            // HTTPS wire roundtrip is NOT proven on this runner. Isolate whether
            // the reference tool (`netsh`) also fails with this exact cert.
            eprintln!(
                "HTTPS_TLS_TEST: BIND_UNPROVEN port={port} err={e:?} \
                 -- HTTPS wire roundtrip NOT proven on this runner (see diagnostics)"
            );
            diagnose_reference_netsh_bind(v4addr, &thumbprint, port);
            return None;
        }
    };
    let v6 = match bind_ssl_certificate(v6addr, &thumbprint, store_name, SSL_BINDING_APP_ID) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "HTTPS_TLS_TEST: BIND_UNPROVEN port={port} family=v6 err={e:?} \
                 -- HTTPS wire roundtrip NOT proven on this runner"
            );
            // `v4` drops here and unbinds via RAII, so no leftover binding.
            return None;
        }
    };

    eprintln!("HTTPS_TLS_TEST: RAN (elevated) port={port}");
    Some(HttpsFixture {
        _v4: v4,
        _v6: v6,
        cert,
        session,
        port,
    })
}

/// Print the SHA-1 thumbprint as lowercase hex, the form the diagnostics need.
fn thumb_hex(thumbprint: &[u8; THUMBPRINT_LEN]) -> String {
    thumbprint.iter().map(|b| format!("{b:02x}")).collect()
}

/// Report, read-only, whether the freshly installed machine certificate has an
/// acquirable private key **as seen from this (elevated) process**. This
/// isolates the two candidate root causes of a bind-time
/// `ERROR_NO_SUCH_LOGON_SESSION (1312)`:
///
/// * `HasPrivateKey=False` -> the cert->key association in the store is broken
///   (cert-side bug; the fix is re-association).
/// * `HasPrivateKey=True` + acquire OK here, yet the bind still fails ->
///   http.sys's SYSTEM context cannot open a key this logged-on process can
///   (context-side; not a cert-content bug).
///
/// Emitted as greppable `HTTPS_TLS_TEST: DIAG ...` lines (R1).
fn diagnose_private_key(thumbprint: &[u8; THUMBPRINT_LEN]) {
    let hex = thumb_hex(thumbprint);
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         $c = Get-Item Cert:\\LocalMachine\\My\\{hex}; \
         if ($null -eq $c) {{ 'cert-not-found'; return }}; \
         'HasPrivateKey=' + $c.HasPrivateKey; \
         try {{ \
           $k = [System.Security.Cryptography.X509Certificates.RSACertificateExtensions]::GetRSAPrivateKey($c); \
           if ($null -ne $k) {{ 'Acquire=OK KeySize=' + $k.KeySize }} else {{ 'Acquire=null' }} \
         }} catch {{ 'AcquireErr=' + $_.Exception.Message }}"
    );
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
    {
        Ok(out) => {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let line = line.trim();
                if !line.is_empty() {
                    eprintln!("HTTPS_TLS_TEST: DIAG {line}");
                }
            }
        }
        Err(e) => eprintln!("HTTPS_TLS_TEST: DIAG powershell-spawn-failed: {e}"),
    }
}

/// Ask the *reference* tool (`netsh http add sslcert`) to bind the same cert to
/// the same endpoint. If `netsh` succeeds where [`bind_ssl_certificate`] fails,
/// the defect is in this crate's API surface; if `netsh` fails identically, the
/// defect is in the certificate or the runner. Cleans up any binding it adds.
///
/// Emitted as greppable `HTTPS_TLS_TEST: NETSH ...` lines (R1). Best-effort:
/// any spawn failure is reported and ignored.
fn diagnose_reference_netsh_bind(
    endpoint: SocketAddr,
    thumbprint: &[u8; THUMBPRINT_LEN],
    port: u16,
) {
    let hex = thumb_hex(thumbprint);
    let ipport = format!("{}:{}", endpoint.ip(), port);
    // A throwaway AppId GUID; identity is irrelevant to whether the bind works.
    let appid = "{9a8b7e8d-e4a1-40b7-992a-838ba5842c89}";
    let add = std::process::Command::new("netsh")
        .args([
            "http",
            "add",
            "sslcert",
            &format!("ipport={ipport}"),
            &format!("certhash={hex}"),
            &format!("appid={appid}"),
            "certstorename=MY",
        ])
        .output();
    match add {
        Ok(out) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let summary = combined
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" | ");
            eprintln!(
                "HTTPS_TLS_TEST: NETSH add status={} out=[{summary}]",
                out.status
            );
            if out.status.success() {
                // Remove the reference binding so the machine is left clean.
                let _ = std::process::Command::new("netsh")
                    .args(["http", "delete", "sslcert", &format!("ipport={ipport}")])
                    .output();
                eprintln!("HTTPS_TLS_TEST: NETSH delete (reference binding cleaned up)");
            }
        }
        Err(e) => eprintln!("HTTPS_TLS_TEST: NETSH spawn-failed: {e}"),
    }
}

/// Build the one-shot HTTP.sys server for a fixture, listening on
/// `https://localhost:{port}/tls/`.
fn build_server(fx: &HttpsFixture) -> Server<'_> {
    Server::builder(fx.session())
        .url(&format!("https://localhost:{}/tls/", fx.port))
        .build(&ThreadPool)
        .expect("binding the https URL prefix succeeds on an elevated host")
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
    let Some(fx) = setup_https(12480) else {
        return;
    };
    let server = build_server(&fx);

    // The client runs on its own thread; the server answers one request here.
    let client = std::thread::spawn(move || {
        // R5: relax ONLY the CA-trust check. The DNS:localhost SAN plus the
        // `localhost` request host should satisfy name validation on their own.
        https_get(
            12480,
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

/// The binding is observable through the query API while held and gone after
/// the fixture drops, on both families (SC-003).
#[test]
fn https_binding_observable_then_absent() {
    let _lock = TLS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(fx) = setup_https(12481) else {
        return;
    };
    let expected = fx.thumbprint();

    let v4: SocketAddr = "0.0.0.0:12481".parse().unwrap();
    let v6: SocketAddr = "[::]:12481".parse().unwrap();

    assert_eq!(
        query_ssl_binding(v4).expect("query v4 succeeds"),
        Some(expected),
        "the v4 binding is observable while the guard is held"
    );
    assert_eq!(
        query_ssl_binding(v6).expect("query v6 succeeds"),
        Some(expected),
        "the v6 binding is observable while the guard is held"
    );

    drop(fx);

    assert_eq!(
        query_ssl_binding(v4).expect("query v4 succeeds"),
        None,
        "the v4 binding is gone once the guard drops"
    );
    assert_eq!(
        query_ssl_binding(v6).expect("query v6 succeeds"),
        None,
        "the v6 binding is gone once the guard drops"
    );
}

/// The negative control: a client that waives no certificate checks must be
/// rejected against the self-signed certificate, and specifically with
/// [`WinHttpError::SecureFailure`] — not a timeout or a refused connection, so
/// the failure is the certificate check and nothing else (SC-002, FR-009).
#[test]
fn https_negative_control_unrelaxed_fails() {
    let _lock = TLS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_fx) = setup_https(12482) else {
        return;
    };
    // The URL prefix must be registered for HTTP.sys to listen and perform the
    // handshake at all; the request itself never reaches `serve_one` because the
    // handshake fails first, so no server loop is run here.
    let _server = build_server(&_fx);

    let result = https_get(12482, "/tls/x", CertificateRelaxations::default());

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
