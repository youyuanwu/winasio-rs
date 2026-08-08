// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Cross-**process** serialisation for the single provisioned HTTPS test port.
//!
//! # Why an in-process `Mutex` is not enough
//!
//! Every `tests/*.rs` file is compiled into its own test binary, and cargo runs
//! those binaries **concurrently as separate processes**. A `static Mutex` can
//! therefore only serialise tests inside one file; it is invisible to the other
//! binary. Two suites now share the one certificate-bound port
//! ([`tls_config::https_test_port`]): `httpsys_tls.rs` (prefix `/tls/`) and
//! `grpc_tls.rs` (the **root** prefix `/`, which tonic requires because it sends
//! absolute `/{service}/{method}` paths).
//!
//! # What the platform actually enforces (measured)
//!
//! HTTP.sys URL registration is machine-global, and the conflict rules are not
//! symmetric with what "different paths" would suggest. Measured with two
//! concurrent processes on the provisioned port:
//!
//! | Process A holds | Process B registers | Result                       |
//! |-----------------|---------------------|------------------------------|
//! | `/x/`           | `/x/` (same)        | `ERROR_ALREADY_EXISTS` (183) |
//! | `/` (root)      | `/tls/`             | `ERROR_ACCESS_DENIED` (5)    |
//! | `/tls/`         | `/` (root)          | `ERROR_ACCESS_DENIED` (5)    |
//! | `/tls/`         | `/grpc/`            | both succeed                 |
//!
//! So the root prefix is mutually exclusive with *any* sibling prefix on the
//! same port, in **both** orders. Because `grpc_tls.rs` must own the root, the
//! two suites cannot be registered at the same instant.
//!
//! # Why this needed fixing even though the suites passed
//!
//! Each registration window is only a fraction of a second, so the two binaries
//! had simply never overlapped — timing luck, not design, and luck that gets
//! worse as either suite grows or on a loaded CI runner. Worse, the collision
//! surfaces as `ERROR_ACCESS_DENIED`, which everywhere else in this repo means
//! "you needed elevation / you used a wildcard host" (the R6 invariant). A real
//! race would therefore have been misdiagnosed as a privilege problem rather
//! than a test-isolation one.
//!
//! # The lock
//!
//! A Windows **named** mutex in the `Local\` namespace — visible to every
//! process in the session, needs no privileges (the suites must keep running
//! unelevated), and is released by the kernel if a holder dies. Its name embeds
//! the port so it stays tied to the single source of truth: lock and resource
//! cannot drift apart.

use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use super::tls_config;

/// How long to wait for the other suite before declaring a deadlock.
///
/// Generous — the whole of either suite finishes in well under a second — but
/// finite on purpose. An infinite wait would turn a hang in one binary into a
/// hung CI job that only ends at the six-hour GitHub timeout, with no
/// indication of the cause; this fails with one instead.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(180);

/// Exclusive ownership of the provisioned HTTPS port, across all processes.
///
/// Released on drop, including while panicking — so a failing test cannot wedge
/// the other binary. Held for the whole lifetime of a test's HTTP.sys
/// registration, not merely its request.
pub struct HttpsPortGuard {
    handle: HANDLE,
}

impl Drop for HttpsPortGuard {
    fn drop(&mut self) {
        // SAFETY: `handle` is a live mutex handle owned by this thread, which
        // acquired it in `lock_https_port`.
        unsafe {
            // Both are best-effort: a failure here has nowhere useful to go, and
            // process exit would release the mutex anyway.
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Acquire the cross-process lock guarding the certificate-bound test port.
///
/// Blocks until the other test binary releases it. Panics rather than returning
/// an error: every caller is a test that cannot meaningfully continue without
/// exclusive use of the port, and a panic names the cause at the failure site.
pub fn lock_https_port() -> HttpsPortGuard {
    let port = tls_config::https_test_port();
    // `Local\` (per-session) rather than `Global\`: creating a `Global\` object
    // can require privileges the unelevated test process does not have, and the
    // binaries we are serialising against are always in this same session.
    let name = HSTRING::from(format!("Local\\winasio-rs-https-test-port-{port}"));

    // SAFETY: `name` outlives the call; a null security descriptor gives the
    // default, which is what an unprivileged same-session lock wants.
    let handle = unsafe { CreateMutexW(None, false, &name) }
        .unwrap_or_else(|e| panic!("creating the named mutex `{name}` failed: {e:?}"));

    // SAFETY: `handle` is the freshly created mutex.
    let wait = unsafe { WaitForSingleObject(handle, ACQUIRE_TIMEOUT.as_millis() as u32) };

    match wait {
        WAIT_OBJECT_0 => {}
        // The previous owner died holding the lock (a test process was killed
        // mid-registration). The mutex is now ours and the protected resource —
        // an HTTP.sys registration owned by a dead process — has already been
        // torn down by the kernel, so continuing is correct. This mirrors the
        // `unwrap_or_else(|e| e.into_inner())` poison handling the in-process
        // locks in this crate use.
        WAIT_ABANDONED => {
            eprintln!(
                "HTTPS_PORT_LOCK: acquired an ABANDONED mutex on port {port} \
                 (a previous test process died holding it); continuing"
            );
        }
        WAIT_TIMEOUT => {
            // Release nothing: we never acquired it.
            unsafe {
                let _ = CloseHandle(handle);
            }
            panic!(
                "HTTPS_PORT_LOCK: timed out after {:?} waiting for exclusive use of the \
                 HTTPS test port {port}. Another test binary (httpsys_tls or grpc_tls) is \
                 holding it, which normally means one of them deadlocked or hung rather \
                 than that the wait was too short.",
                ACQUIRE_TIMEOUT
            );
        }
        WAIT_FAILED => {
            let error = windows::core::Error::from_thread();
            unsafe {
                let _ = CloseHandle(handle);
            }
            panic!("HTTPS_PORT_LOCK: waiting on the named mutex failed: {error:?}");
        }
        other => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            panic!("HTTPS_PORT_LOCK: unexpected wait result {other:?}");
        }
    }

    HttpsPortGuard { handle }
}
