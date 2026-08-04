// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Phase 1: lifecycle types report failure as values and never panic.
//!
//! Covers SC-002 (setup and unbindable URL), SC-003 (shutdown is observable),
//! and the FR-010/FR-011 arms of SC-019.

use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::core::HSTRING;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::HttpServer::{
    HttpCreateRequestQueue, HTTPAPI_VERSION, HTTP_INITIALIZE_CONFIG, HTTP_INITIALIZE_SERVER,
};

use winasio::httpsys::{HttpInitializer, RequestQueue, ServerSession, UrlGroup};
use winasio::iocp::ThreadPool;

const PORT: u16 = 12360;

/// Serialises every test that constructs an initializer.
///
/// `HttpInitialize`/`HttpTerminate` are reference-counted per process, so
/// SC-003's "the subsystem is really down" observation is only valid while no
/// other initializer is live -- and cargo runs tests in a binary concurrently.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn url(path: &str) -> HSTRING {
    HSTRING::from(format!("http://localhost:{PORT}/{path}/"))
}

/// SC-002: the full chain stands up, and a URL that cannot be bound reports an
/// error value rather than panicking.
#[test]
fn setup_chain_succeeds_and_reports_failure_as_a_value() {
    let _guard = serial();
    let http = HttpInitializer::new().expect("initialise");

    let session = ServerSession::new().expect("session");
    let group = UrlGroup::new(&session).expect("url group");
    let queue = RequestQueue::new(&ThreadPool).expect("queue");
    queue.bind_url_group(&group).expect("bind");
    group.add_url(&url("lifecycle")).expect("add url");

    // A malformed prefix cannot be bound. This must come back as a value.
    let bad = group.add_url(&HSTRING::from("not-a-url"));
    assert!(bad.is_err(), "an unbindable URL must return Err, not panic");

    drop(queue);
    drop(group);
    drop(session);
    drop(http);
}

/// SC-002: the subsystem is reference-counted, so nesting initializers works
/// and each start is matched by its own shutdown.
#[test]
fn repeated_initialisation_is_reference_counted() {
    let _guard = serial();
    let outer = HttpInitializer::new().expect("first initialise");
    {
        let inner = HttpInitializer::new().expect("second initialise");
        // The inner shutdown must not tear the subsystem down under the outer.
        drop(inner);
        RequestQueue::new(&ThreadPool).expect("subsystem still up after the inner drop");
    }
    drop(outer);
}

/// SC-003: dropping the initializer really does shut the subsystem down.
///
/// Observable because `HttpCreateRequestQueue` reports `ERROR_DLL_INIT_FAILED`
/// when the subsystem has not been initialised. This is the proof that
/// `HttpTerminate` ran -- the old API could never call it at all, because
/// `HttpInitializer::default()` returned `()` and so nothing was ever dropped.
#[test]
fn dropping_the_initializer_shuts_the_subsystem_down() {
    let _guard = serial();
    const VERSION: HTTPAPI_VERSION = HTTPAPI_VERSION {
        HttpApiMajorVersion: 2,
        HttpApiMinorVersion: 0,
    };

    {
        let http = HttpInitializer::new().expect("initialise");
        RequestQueue::new(&ThreadPool).expect("queue while initialised");
        drop(http);
    }

    // With the subsystem down, creating a queue must fail.
    let mut handle = HANDLE::default();
    let code = unsafe {
        HttpCreateRequestQueue(
            VERSION,
            windows::core::PCWSTR::null(),
            None,
            None,
            &mut handle,
        )
    };
    assert_ne!(
        code, 0,
        "the subsystem must be down once the initializer is dropped"
    );

    // Leave the process as we found it for any later test in this binary.
    let _ = unsafe {
        windows::Win32::Networking::HttpServer::HttpInitialize(
            VERSION,
            HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG,
            None,
        )
    };
    let _ = unsafe {
        windows::Win32::Networking::HttpServer::HttpTerminate(
            HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG,
            None,
        )
    };
}

/// FR-011 / SC-019: closing an already-closed queue succeeds.
#[test]
fn closing_a_queue_twice_succeeds() {
    let _guard = serial();
    let _http = HttpInitializer::new().expect("initialise");
    let queue = RequestQueue::new(&ThreadPool).expect("queue");
    queue.close().expect("first close");
    queue.close().expect("second close is a no-op");
}

/// Dropping an already-closed queue must also be a no-op.
#[test]
fn dropping_a_closed_queue_is_fine() {
    let _guard = serial();
    let _http = HttpInitializer::new().expect("initialise");
    let queue = RequestQueue::new(&ThreadPool).expect("queue");
    queue.close().expect("close");
    drop(queue);
}
