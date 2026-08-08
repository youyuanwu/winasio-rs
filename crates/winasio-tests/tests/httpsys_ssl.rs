// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Tests for the HTTP.sys SSL certificate binding API (`winasio::httpsys::ssl`).
//!
//! **Every test here runs unelevated and mutates no machine state.** Writing the
//! SSL binding table is an administrator-only, machine-wide side effect, so no
//! test performs it — that is the exclusive job of `scripts/setup-https-test.ps1`
//! (run deliberately and elevated). These tests exercise only:
//!
//! * **Error classification** — built purely from raw Win32 codes (no syscall,
//!   no side effect), including the load-bearing proof that `ERROR_ACCESS_DENIED`
//!   maps to a distinct [`SslBindError::RequiresElevation`], and the
//!   `#[non_exhaustive]` exhaustiveness proof.
//! * **The read path** — a real (unelevated) `query_ssl_binding` of the SSL
//!   config table on an unbound port (C7: reads need no admin). Read-only, so it
//!   behaves identically whether or not the process is elevated.
//!
//! No test's outcome depends on the process's privilege level, and none installs
//! a certificate or binds a port. The certificate is provisioned out-of-process
//! by `scripts/setup-https-test.ps1` (see `httpsys_tls.rs`).

#![cfg(windows)]

mod common;

use std::net::SocketAddr;

use winasio::httpsys::{
    query_ssl_binding, HttpInitializer, SslBindError, SSL_BINDING_APP_ID, THUMBPRINT_LEN,
};
use windows::core::Error;
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND};

/// A never-bound port this binary reads back through `query_ssl_binding` to
/// prove the read path is unelevated (C7). Inside the TLS range reserved in the
/// plan and disjoint from the e2e port in `httpsys_tls.rs`. Nothing here ever
/// *binds* a port — see the note where the bind test used to live.
const PORT_QUERY_PROBE: u16 = 12491;

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

/// C7 (re-measured here, unelevated): **reading** the HTTP.sys SSL config table
/// is permitted without administrator rights, unlike **writing** it. `netsh http
/// show sslcert` runs unelevated; this proves our own `query_ssl_binding` does
/// too. A never-bound port must read back as `Ok(None)`.
///
/// This is deliberately *always-on*, not elevation-gated: it needs no admin and
/// no setup, yet it genuinely exercises `ssl.rs`'s real read path — the two-pass
/// `HttpQueryServiceConfiguration` sizing dance and the unsafe walk of the
/// returned `HTTP_SERVICE_CONFIG_SSL_SET`. It is the honest coverage the
/// setup-script-does-the-binding decision would otherwise cost this module, and
/// it dogfoods the exact mechanism the e2e suite uses to decide RUN vs SKIP.
#[test]
fn query_unbound_port_reads_none_unelevated() {
    // Precondition: the config subsystem needs a live initializer (ssl docs).
    let _http = HttpInitializer::new().expect("HTTP.sys initialises");
    let endpoint: SocketAddr = format!("0.0.0.0:{PORT_QUERY_PROBE}")
        .parse()
        .expect("a valid endpoint");
    match query_ssl_binding(endpoint) {
        Ok(None) => eprintln!(
            "HTTPS_TLS_TEST: RAN (unelevated; query_ssl_binding read the SSL table and \
             reported no binding on an unbound port) test=query_unbound_port_reads_none_unelevated"
        ),
        Ok(Some(tp)) => panic!(
            "port {PORT_QUERY_PROBE} unexpectedly already has an SSL binding ({tp:02x?}); \
             choose a truly-unbound probe port"
        ),
        Err(SslBindError::RequiresElevation) => panic!(
            "query_ssl_binding required elevation — C7 FALSIFIED: the SSL read path is NOT \
             unelevated. The e2e suite's binding-detection strategy must change; report this."
        ),
        Err(other) => panic!("unexpected error querying an unbound port: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// No behavioural bind test lives here by design.
//
// Binding (`HttpSetServiceConfiguration`) is a mutating, machine-wide,
// administrator-only operation. A test that *attempts* it is unsafe on two
// counts: its outcome would depend on whether the test process happens to be
// elevated (dev machine: no, GitHub runners: yes), and on an elevated host it
// would leave a real, persistent SSL binding behind as a side effect — exactly
// the machine-hygiene failure R2 exists to prevent. So creating and removing
// that global state is the *exclusive* job of `scripts/setup-https-test.ps1`,
// run deliberately and elevated by a human or CI. The `RequiresElevation`
// classification is proved purely above (`access_denied_maps_to_requires_elevation`)
// by building the error from a raw code — deterministic in every environment,
// no syscall, no side effect.
// ---------------------------------------------------------------------------
