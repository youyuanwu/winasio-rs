// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Process-wide Winsock initialisation.
//!
//! Winsock requires `WSAStartup` before any socket call in the process, and
//! callers of this crate should not have to know that. Every entry point in
//! [`crate::net`] that can create a socket calls [`ensure_winsock`] first, so
//! the requirement is met without an initialiser type appearing in the public
//! API.
//!
//! # Why there is no `WsaInitializer` guard
//!
//! [`crate::httpsys`] uses a reference-counted initialiser, and that shape was
//! deliberately *not* copied here. A refcount only works when the last release
//! genuinely means "nothing in this process is using the library any more".
//! Winsock cannot offer that: one module dropping the last initialiser while
//! another module still holds open sockets would call `WSACleanup` out from
//! under them, and Winsock does not tolerate it.
//!
//! So `WSACleanup` is never called. The library stays loaded for the life of
//! the process, which is exactly what `std::net` does, and costs one loaded DLL
//! the process was going to keep anyway.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Once;

use windows::core::{Error, Result, HRESULT};
use windows::Win32::Networking::WinSock::{WSAStartup, WSADATA};

static START: Once = Once::new();

/// The `WSAStartup` return code, cached so later callers see the same answer
/// without a second call. `0` is success.
static STATUS: AtomicI32 = AtomicI32::new(0);

/// Initialise Winsock 2.2 for this process, at most once.
///
/// Idempotent and thread-safe. A failure is remembered and returned to every
/// subsequent caller rather than retried: `WSAStartup` failing means the
/// platform's Winsock is unusable, not that the call was unlucky.
pub(crate) fn ensure_winsock() -> Result<()> {
    START.call_once(|| {
        let mut data = WSADATA::default();
        // 2.2, encoded low-byte-major, as `MAKEWORD(2, 2)` would produce.
        const VERSION: u16 = 0x0202;
        // SAFETY: `data` is a live, writable `WSADATA` owned by this frame, and
        // `WSAStartup` fills it in before returning.
        let code = unsafe { WSAStartup(VERSION, &mut data) };
        STATUS.store(code, Ordering::SeqCst);
    });

    match STATUS.load(Ordering::SeqCst) {
        0 => Ok(()),
        // `WSAStartup` returns its error code directly rather than through
        // `WSAGetLastError`, so it is converted here instead of being read off
        // the thread.
        code => Err(Error::from_hresult(HRESULT::from_win32(code as u32))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winsock_initialises_and_is_idempotent() {
        ensure_winsock().expect("winsock 2.2 is available");
        ensure_winsock().expect("a second call is a no-op");
    }

    #[test]
    fn concurrent_initialisation_agrees() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| ensure_winsock().is_ok()))
            .collect();
        for h in handles {
            assert!(h.join().expect("thread did not panic"));
        }
    }
}
