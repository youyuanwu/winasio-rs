// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Shared ownership of a Winsock socket.
//!
//! This is [`crate::iocp::Handle`]'s counterpart for sockets, and exists for
//! the same reason: an operation whose future is dropped still needs a live
//! handle to cancel through, so the socket is owned jointly by the safe type
//! and by every operation it has in flight.
//!
//! # Why `Handle` could not be reused
//!
//! `Handle` closes with `CloseHandle`. A socket must be closed with
//! `closesocket`, and the difference is not cosmetic: measured on this
//! platform, `CloseHandle` on a socket *returns success* and a subsequent
//! `closesocket` on the same value **also** returns success. The two do not
//! agree that the object is gone, which means `CloseHandle` is not running
//! Winsock's teardown — the graceful close, the linger behaviour, and any
//! layered-provider bookkeeping. Under a layered service provider that would
//! leak provider state silently.
//!
//! # No `unsafe impl` anywhere
//!
//! `Handle` needs a `SendHandle` newtype because `HANDLE` wraps a raw pointer.
//! `SOCKET` is `struct SOCKET(pub usize)`, so it is already `Send + Sync` and
//! this type derives both. If that ever stops being true, the fix is the
//! field's type, never an `unsafe impl` here.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use windows::core::{Error, Result};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::WinSock::{
    bind, closesocket, getpeername, getsockname, listen, setsockopt, shutdown, WSASocketW,
    ADDRESS_FAMILY, INVALID_SOCKET, IPPROTO_IPV6, IPPROTO_TCP, IPV6_V6ONLY, SD_BOTH, SD_RECEIVE,
    SD_SEND, SOCKET, SOCK_STREAM, SOL_SOCKET, SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT,
    WSAEAFNOSUPPORT, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED,
};

use super::addr::SockAddrBytes;
use super::init::ensure_winsock;

#[cfg(any(test, feature = "test-util"))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Count of sockets this module currently keeps open.
///
/// Test support. `crate::iocp::live_operations` counts *operations*, which is
/// the wrong instrument for "did that socket get closed": an operation can be
/// reclaimed while the socket it created lives on. This counts the thing the
/// question is about.
#[cfg(any(test, feature = "test-util"))]
static LIVE_SOCKETS: AtomicUsize = AtomicUsize::new(0);

/// How many sockets owned by this module are currently open.
///
/// Test support, gated exactly like
/// [`crate::iocp::Proactor::unclaimed_completions`]. Process-global, so a test
/// asserting on it must hold [`socket_guard`] — and so must every other test
/// that creates a socket, or it will perturb the count from another thread.
#[cfg(any(test, feature = "test-util"))]
pub fn live_sockets() -> usize {
    LIVE_SOCKETS.load(Ordering::SeqCst)
}

/// Serialises tests that create sockets, so they do not perturb assertions on
/// the process-global count.
///
/// The same shape as the crate's operation-counter guard: the counter is
/// process-wide, so every test that moves it takes the same lock.
#[cfg(any(test, feature = "test-util"))]
pub fn socket_guard() -> std::sync::MutexGuard<'static, ()> {
    static SOCKET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SOCKET_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared owner of a Winsock socket.
///
/// Cloning is a reference-count bump and performs no allocation, so an
/// operation can hold one without affecting the crate's per-operation
/// allocation budget.
#[derive(Clone)]
pub struct Socket(Arc<Owned>);

struct Owned(SOCKET);

impl Owned {
    fn new(raw: SOCKET) -> Self {
        #[cfg(any(test, feature = "test-util"))]
        if raw != INVALID_SOCKET {
            LIVE_SOCKETS.fetch_add(1, Ordering::SeqCst);
        }
        Owned(raw)
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if self.0 == INVALID_SOCKET {
            return;
        }
        // SAFETY: this runs only when the last reference is released, so no
        // operation can still be using the socket, and `from_raw`'s contract
        // transferred the responsibility for closing it here. A socket is
        // closed at most once because `Owned` is never cloned — only the `Arc`
        // around it is.
        unsafe { closesocket(self.0) };
        #[cfg(any(test, feature = "test-util"))]
        LIVE_SOCKETS.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Socket {
    /// Take ownership of a raw socket.
    ///
    /// # Safety
    ///
    /// * `raw` must be a valid socket currently owned by the caller.
    /// * Ownership transfers here: nothing else may close it or alias it as an
    ///   owner.
    /// * This is deliberately not a safe constructor, for the same reason
    ///   [`crate::iocp::Handle::from_raw`] is not.
    pub unsafe fn from_raw(raw: SOCKET) -> Self {
        Socket(Arc::new(Owned::new(raw)))
    }

    /// Create an overlapped TCP socket of the given family.
    pub(crate) fn new_overlapped(family: ADDRESS_FAMILY) -> Result<Socket> {
        ensure_winsock()?;
        // `WSA_FLAG_NO_HANDLE_INHERIT` alongside the overlapped flag: without
        // it a child process spawned while a connection is open inherits the
        // socket, and the connection then stays half-alive until that child
        // exits — the peer never sees FIN and the port is not released.
        // `std::net` sets the equivalent for the same reason.
        //
        // SAFETY: a plain socket creation with no provider info; the returned
        // socket is owned solely by this call.
        let raw = unsafe {
            WSASocketW(
                family.0 as i32,
                SOCK_STREAM.0,
                IPPROTO_TCP.0,
                None,
                0,
                WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
            )
        }?;
        // SAFETY: `WSASocketW` returned a socket owned by this frame and
        // nothing else.
        Ok(unsafe { Socket::from_raw(raw) })
    }

    /// The underlying socket, borrowed.
    ///
    /// Stays valid only while this `Socket` — or another clone of it — is
    /// alive.
    pub fn raw(&self) -> SOCKET {
        self.0 .0
    }

    /// The socket as a kernel handle, for the completion machinery.
    ///
    /// Constructed per call rather than cached, so this type has exactly one
    /// owner of the underlying value and cannot disagree with itself about it.
    pub fn as_handle(&self) -> HANDLE {
        HANDLE(self.raw().0 as *mut std::ffi::c_void)
    }

    /// How many references currently share this socket.
    #[cfg(any(test, feature = "test-util"))]
    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }

    /// Ask the kernel to abandon every operation this thread's process started
    /// on this socket.
    ///
    /// `CancelIoEx` with a null `OVERLAPPED` targets all of them. Completions
    /// still arrive afterwards, normally carrying `ERROR_OPERATION_ABORTED` —
    /// which is why the socket must not be closed until the operations holding
    /// their own `Socket` clones have released them.
    pub(crate) fn cancel_all(&self) -> Result<()> {
        // SAFETY: the socket is alive for the duration of the call because
        // `&self` holds a reference to it.
        unsafe { windows::Win32::System::IO::CancelIoEx(self.as_handle(), None) }
    }

    pub(crate) fn bind_to(&self, addr: SocketAddr) -> Result<()> {
        let encoded = SockAddrBytes::from_socket_addr(addr);
        // SAFETY: `encoded` outlives the call and describes `len()` valid bytes.
        let rc = unsafe { bind(self.raw(), encoded.as_ptr(), encoded.len()) };
        last_error_if(rc)
    }

    pub(crate) fn listen_on(&self, backlog: i32) -> Result<()> {
        // SAFETY: a live socket bound by the caller.
        let rc = unsafe { listen(self.raw(), backlog) };
        last_error_if(rc)
    }

    pub(crate) fn shutdown_dir(&self, how: std::net::Shutdown) -> Result<()> {
        let how = match how {
            std::net::Shutdown::Read => SD_RECEIVE,
            std::net::Shutdown::Write => SD_SEND,
            std::net::Shutdown::Both => SD_BOTH,
        };
        // SAFETY: a live socket owned by this value.
        let rc = unsafe { shutdown(self.raw(), how) };
        last_error_if(rc)
    }

    pub(crate) fn local_addr(&self) -> Result<SocketAddr> {
        let mut bytes = SockAddrBytes::zeroed();
        // SAFETY: `bytes` is a live storage of the length it reports, and
        // Winsock writes at most that much.
        let rc = unsafe { getsockname(self.raw(), bytes.as_mut_ptr(), bytes.len_mut()) };
        last_error_if(rc)?;
        bytes.to_socket_addr().ok_or_else(unsupported_family)
    }

    pub(crate) fn peer_addr(&self) -> Result<SocketAddr> {
        let mut bytes = SockAddrBytes::zeroed();
        // SAFETY: as above.
        let rc = unsafe { getpeername(self.raw(), bytes.as_mut_ptr(), bytes.len_mut()) };
        last_error_if(rc)?;
        bytes.to_socket_addr().ok_or_else(unsupported_family)
    }

    /// Set `IPV6_V6ONLY`, controlling whether an IPv6 listener also accepts
    /// IPv4 clients.
    pub(crate) fn set_only_v6(&self, only_v6: bool) -> Result<()> {
        let value: u32 = u32::from(only_v6);
        // SAFETY: the option takes a 4-byte integer, which is what is passed.
        let rc = unsafe {
            setsockopt(
                self.raw(),
                IPPROTO_IPV6.0,
                IPV6_V6ONLY,
                Some(&value.to_ne_bytes()),
            )
        };
        last_error_if(rc)
    }

    /// Apply `SO_UPDATE_ACCEPT_CONTEXT` to a socket returned by `AcceptEx`.
    ///
    /// Until this runs, the accepted socket has none of the listener's
    /// properties and `getpeername` on it fails with `WSAENOTCONN`.
    ///
    /// The option takes the **listening socket** as its value. Passing `None` —
    /// which is correct for `SO_UPDATE_CONNECT_CONTEXT`, and is what the two
    /// options' superficial symmetry suggests — fails with `WSAEFAULT` and
    /// leaves the socket unusable.
    pub(crate) fn update_accept_context(&self, listener: &Socket) -> Result<()> {
        let value = listener.raw().0;
        // SAFETY: the option takes a `SOCKET`-sized value, which is what the
        // native-endian byte form of the listening socket is.
        let rc = unsafe {
            setsockopt(
                self.raw(),
                SOL_SOCKET,
                SO_UPDATE_ACCEPT_CONTEXT,
                Some(&value.to_ne_bytes()),
            )
        };
        last_error_if(rc)
    }

    /// Apply `SO_UPDATE_CONNECT_CONTEXT` to a socket returned by `ConnectEx`.
    ///
    /// This one genuinely takes no value.
    pub(crate) fn update_connect_context(&self) -> Result<()> {
        // SAFETY: the option takes no value, so `None` is the correct form.
        let rc = unsafe { setsockopt(self.raw(), SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, None) };
        last_error_if(rc)
    }

    /// Whether this socket is still open, by asking Winsock about it.
    ///
    /// Test support: a direct instrument for "was that closed?", as opposed to
    /// inferring it from a counter.
    #[cfg(any(test, feature = "test-util"))]
    pub fn is_open_raw(raw: SOCKET) -> bool {
        use windows::Win32::Networking::WinSock::{getsockopt, SO_TYPE};
        let mut value = 0i32;
        let mut len = std::mem::size_of::<i32>() as i32;
        // SAFETY: `value` and `len` are live locals of the sizes described. A
        // closed or recycled socket value is a defined input here: Winsock
        // reports `WSAENOTSOCK` rather than misbehaving.
        let rc = unsafe {
            getsockopt(
                raw,
                SOL_SOCKET,
                SO_TYPE,
                windows::core::PSTR(std::ptr::addr_of_mut!(value).cast()),
                &mut len,
            )
        };
        rc == 0
    }
}

impl fmt::Debug for Socket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Socket").field(&self.raw().0).finish()
    }
}

/// Turn a Winsock `-1` return into the error the thread recorded.
///
/// Winsock reports failure with `SOCKET_ERROR` and leaves the detail in
/// `WSAGetLastError`, which on this platform is `GetLastError` — the same
/// thread-local slot `windows::core::Error::from_thread` reads.
fn last_error_if(rc: i32) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::from_thread())
    }
}

fn unsupported_family() -> Error {
    Error::from_hresult(windows::core::HRESULT::from_win32(WSAEAFNOSUPPORT.0 as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Networking::WinSock::AF_INET;
    use windows::Win32::Networking::WinSock::AF_INET6;

    fn is_send_sync<T: Send + Sync>() {}

    #[test]
    fn socket_is_send_and_sync_without_any_unsafe_impl() {
        // Derived from `SOCKET`, which is a `usize` newtype. If this ever
        // fails, the fix is the field's type, never an `unsafe impl`.
        is_send_sync::<Socket>();
    }

    #[test]
    fn clone_shares_the_same_raw_socket() {
        let _guard = socket_guard();
        let s = Socket::new_overlapped(AF_INET).expect("create socket");
        let c = s.clone();
        assert_eq!(s.raw(), c.raw());
        assert_eq!(s.ref_count(), 2, "the clone shares one allocation");
        drop(c);
        assert_eq!(s.ref_count(), 1);
    }

    #[test]
    fn a_real_socket_is_closed_exactly_once_when_the_last_clone_drops() {
        let _guard = socket_guard();
        let s = Socket::new_overlapped(AF_INET).expect("create socket");
        let raw = s.raw();
        let clone = s.clone();

        drop(s);
        assert!(
            Socket::is_open_raw(raw),
            "the socket must outlive every reference, not just the first"
        );

        drop(clone);
        assert!(
            !Socket::is_open_raw(raw),
            "the last reference must close it"
        );
    }

    #[test]
    fn an_invalid_socket_is_not_closed() {
        let _guard = socket_guard();
        // SAFETY: `INVALID_SOCKET` is owned by nobody and is never closed.
        let s = unsafe { Socket::from_raw(INVALID_SOCKET) };
        assert_eq!(s.raw(), INVALID_SOCKET);
        drop(s);
    }

    #[test]
    fn a_fresh_socket_can_be_bound_and_reports_its_ephemeral_port() {
        let _guard = socket_guard();
        let s = Socket::new_overlapped(AF_INET).expect("create socket");
        s.bind_to("127.0.0.1:0".parse().unwrap()).expect("bind");
        let local = s.local_addr().expect("getsockname");
        assert_eq!(local.ip(), "127.0.0.1".parse::<std::net::IpAddr>().unwrap());
        assert_ne!(local.port(), 0, "the ephemeral port must be reported back");
    }

    #[test]
    fn an_ipv6_socket_can_leave_v6_only_mode() {
        let _guard = socket_guard();
        let s = Socket::new_overlapped(AF_INET6).expect("create socket");
        s.set_only_v6(false).expect("clear IPV6_V6ONLY");
        s.bind_to("[::]:0".parse().unwrap()).expect("bind");
    }

    #[test]
    fn the_liveness_probe_distinguishes_open_from_closed() {
        let _guard = socket_guard();
        // The instrument the leak tests depend on. If this ever stopped
        // discriminating, those tests would pass vacuously.
        let s = Socket::new_overlapped(AF_INET).expect("create socket");
        let raw = s.raw();
        assert!(Socket::is_open_raw(raw));
        drop(s);
        assert!(!Socket::is_open_raw(raw));
    }

    #[test]
    fn the_live_socket_count_follows_creation_and_drop() {
        let _guard = socket_guard();
        let before = live_sockets();
        let s = Socket::new_overlapped(AF_INET).expect("create socket");
        assert_eq!(live_sockets(), before + 1);
        let c = s.clone();
        assert_eq!(live_sockets(), before + 1, "a clone is not a new socket");
        drop(c);
        drop(s);
        assert_eq!(live_sockets(), before);
    }
}
