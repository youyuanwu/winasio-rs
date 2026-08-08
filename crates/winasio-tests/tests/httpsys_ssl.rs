// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Tests for the HTTP.sys SSL certificate binding API (`winasio::httpsys::ssl`).
//!
//! The binding table is machine-wide and writing it needs administrator rights,
//! so this file is split by what a given host can prove:
//!
//! * **Always-on** — error classification and the `#[non_exhaustive]`
//!   exhaustiveness proof. These are pure and run on any host.
//! * **Elevation-gated** — the one behavioural test that actually calls the bind
//!   API. On an unelevated host it asserts the call reports
//!   [`SslBindError::RequiresElevation`]; on an elevated host it would succeed at
//!   binding, which the end-to-end suite (`httpsys_tls.rs`) covers, so here it
//!   just notes that and returns. Either way it never fails for being on the
//!   "wrong" kind of host.
//!
//! This binary installs no certificate and, on the unelevated path, writes
//! nothing that persists: a refused bind changes no machine state.

#![cfg(windows)]

mod common;

use std::net::SocketAddr;
use std::sync::Mutex;

use common::is_elevated;
use winasio::httpsys::{
    bind_ssl_certificate, cert_present, key_container_present, CertStore, HttpInitializer,
    SelfSignedCert, SslBindError, SSL_BINDING_APP_ID, THUMBPRINT_LEN,
};
use windows::core::Error;
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND};

/// Serialises the certificate-helper tests in this binary. They each create,
/// install, and remove a `CurrentUser\My` certificate and run the prefix sweep,
/// which touches shared per-user store and key-container state; running them
/// concurrently would let one test's sweep see another's in-flight certificate.
static CERT_TEST_LOCK: Mutex<()> = Mutex::new(());

/// This binary owns 12490..=12492 for its throwaway bind attempt, inside the
/// TLS range reserved in the plan and disjoint from the e2e ports in
/// `httpsys_tls.rs`.
const PORT_ELEVATION_PROBE: u16 = 12490;

fn win32_error(code: windows::Win32::Foundation::WIN32_ERROR) -> Error {
    Error::from_hresult(code.to_hresult())
}

// ---------------------------------------------------------------------------
// Always-on: error classification
// ---------------------------------------------------------------------------

#[test]
fn access_denied_maps_to_requires_elevation() {
    // The load-bearing claim of D2: a non-elevated caller's `ERROR_ACCESS_DENIED`
    // is surfaced as a distinct, matchable case, not folded into `Platform`.
    let err = SslBindError::from_win32(win32_error(ERROR_ACCESS_DENIED));
    assert!(
        matches!(err, SslBindError::RequiresElevation),
        "got {err:?}"
    );
}

#[test]
fn already_exists_maps_to_already_bound() {
    let err = SslBindError::from_win32(win32_error(ERROR_ALREADY_EXISTS));
    assert!(matches!(err, SslBindError::AlreadyBound), "got {err:?}");
}

#[test]
fn an_unclassified_code_stays_opaque() {
    // A control: the table must not swallow codes it does not name.
    let err = SslBindError::from_win32(win32_error(ERROR_FILE_NOT_FOUND));
    assert!(matches!(err, SslBindError::Platform(_)), "got {err:?}");
}

/// Compile-level proof that `SslBindError` is exhaustively matchable from
/// another crate — `#[non_exhaustive]` is inert within `winasio`, so this proof
/// must live here. There is deliberately no `_` arm: adding a variant must break
/// this with `E0004`, which is the semver-major signal.
fn describe(error: &SslBindError) -> &'static str {
    match error {
        SslBindError::RequiresElevation => "requires elevation",
        SslBindError::AlreadyBound => "already bound",
        SslBindError::Platform(_) => "platform",
    }
}

#[test]
fn ssl_bind_error_is_exhaustively_matchable_from_another_crate() {
    assert_eq!(
        describe(&SslBindError::from_win32(win32_error(ERROR_ACCESS_DENIED))),
        "requires elevation"
    );
    assert_eq!(
        describe(&SslBindError::from_win32(win32_error(ERROR_ALREADY_EXISTS))),
        "already bound"
    );
    assert_eq!(
        describe(&SslBindError::from_win32(win32_error(ERROR_FILE_NOT_FOUND))),
        "platform"
    );
    // The thumbprint length is part of the public contract the helper relies on.
    assert_eq!(THUMBPRINT_LEN, 20);
    // The AppId is a fixed, non-nil constant.
    assert_ne!(SSL_BINDING_APP_ID, windows::core::GUID::zeroed());
}

// ---------------------------------------------------------------------------
// Elevation-gated: the real bind path classifies elevation correctly
// ---------------------------------------------------------------------------

#[test]
fn bind_without_elevation_reports_requires_elevation() {
    // Proves the *real* API call — not just the `From` impl — classifies the
    // unelevated failure as `RequiresElevation`, which is what the e2e suite's
    // skip gate depends on. On an elevated host binding a throwaway thumbprint
    // would instead succeed (covered end-to-end elsewhere), so we skip.
    if is_elevated() {
        eprintln!(
            "HTTPS_TLS_TEST: SKIPPED (elevated host; unelevated-bind classification not applicable) \
             test=bind_without_elevation_reports_requires_elevation"
        );
        return;
    }

    // Precondition: a live initializer must exist before touching the config
    // subsystem (see the ssl module docs).
    let _http = HttpInitializer::new().expect("HTTP.sys initialises");
    let endpoint: SocketAddr = format!("0.0.0.0:{PORT_ELEVATION_PROBE}")
        .parse()
        .expect("a valid endpoint");
    let thumbprint = [0u8; THUMBPRINT_LEN];

    let result = bind_ssl_certificate(endpoint, &thumbprint, "MY", SSL_BINDING_APP_ID);
    match result {
        Err(SslBindError::RequiresElevation) => {
            eprintln!(
                "HTTPS_TLS_TEST: RAN (unelevated; real bind correctly reported RequiresElevation) \
                 test=bind_without_elevation_reports_requires_elevation"
            );
        }
        other => panic!(
            "expected RequiresElevation from an unelevated real bind, got {other:?} \
             (if this host is actually elevated, is_elevated() misreported)"
        ),
    }
}

// ---------------------------------------------------------------------------
// Always-on: self-signed certificate helper round-trip (CurrentUser\My)
// ---------------------------------------------------------------------------
//
// These exercise the `test-util` certificate helper without touching the
// machine-wide SSL binding table, so they need no elevation and run on any
// host. They install into `CurrentUser\My`, which an unelevated process may
// write (measured: C2), and assert complete cleanup — both the certificate and
// its CNG key container — on drop and on unwind.

#[test]
fn self_signed_cert_roundtrip_current_user() {
    let _guard = CERT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Best-effort sweep of any leftovers from a previously aborted run so the
    // assertions below start from a clean slate. No ports: this store holds no
    // SSL bindings.
    SelfSignedCert::sweep_leftovers(CertStore::CurrentUser, &[]);

    let cert = SelfSignedCert::create("CN=localhost", CertStore::CurrentUser)
        .expect("self-signed certificate creation succeeds in CurrentUser\\My");

    // Thumbprint is a real 20-byte SHA-1, not all zero.
    let thumbprint = cert.thumbprint();
    assert_eq!(thumbprint.len(), THUMBPRINT_LEN);
    assert_ne!(thumbprint, [0u8; THUMBPRINT_LEN], "thumbprint must be real");

    let container = cert.container().to_string();

    // While alive: both the certificate and its key container are present.
    assert!(
        cert_present(&thumbprint, CertStore::CurrentUser),
        "certificate must be installed while the guard is alive"
    );
    assert!(
        key_container_present(&container, CertStore::CurrentUser),
        "key container must exist while the guard is alive"
    );

    drop(cert);

    // After drop: both are gone. This is the machine-hygiene contract.
    assert!(
        !cert_present(&thumbprint, CertStore::CurrentUser),
        "certificate must be removed on drop"
    );
    assert!(
        !key_container_present(&container, CertStore::CurrentUser),
        "key container must be deleted on drop"
    );

    eprintln!(
        "HTTPS_TLS_TEST: RAN (cert helper round-trip in CurrentUser\\My; clean after drop) \
         test=self_signed_cert_roundtrip_current_user"
    );
}

#[test]
fn self_signed_cert_cleanup_on_unwind() {
    let _guard = CERT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    SelfSignedCert::sweep_leftovers(CertStore::CurrentUser, &[]);

    // Capture identity across the panic boundary so we can assert cleanup after
    // the guard is dropped by unwinding.
    let captured: std::sync::Mutex<Option<([u8; THUMBPRINT_LEN], String)>> =
        std::sync::Mutex::new(None);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cert = SelfSignedCert::create("CN=localhost", CertStore::CurrentUser)
            .expect("self-signed certificate creation succeeds");
        *captured.lock().unwrap() = Some((cert.thumbprint(), cert.container().to_string()));
        assert!(cert_present(&cert.thumbprint(), CertStore::CurrentUser));
        // Force an unwind while the guard is in scope.
        panic!("deliberate panic to exercise Drop-based cleanup");
    }));

    assert!(result.is_err(), "the closure must have panicked");

    let (thumbprint, container) = captured
        .lock()
        .unwrap()
        .take()
        .expect("the certificate was created before the panic");

    assert!(
        !cert_present(&thumbprint, CertStore::CurrentUser),
        "certificate must be removed even when the guard drops via unwind"
    );
    assert!(
        !key_container_present(&container, CertStore::CurrentUser),
        "key container must be deleted even when the guard drops via unwind"
    );

    eprintln!(
        "HTTPS_TLS_TEST: RAN (cert helper cleaned up on unwind; clean after panic) \
         test=self_signed_cert_cleanup_on_unwind"
    );
}
