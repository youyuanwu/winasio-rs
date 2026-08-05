// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The Winsock extension functions, resolved once per process.
//!
//! `AcceptEx`, `ConnectEx` and `GetAcceptExSockaddrs` are not exported by
//! `ws2_32.dll`. They belong to the transport provider and must be fetched
//! through `WSAIoctl(SIO_GET_EXTENSION_FUNCTION_POINTER)` with the function's
//! GUID, using a socket of the provider you intend to call them on.
//!
//! # Why one cache is enough
//!
//! Winsock documents these as per-provider, which would suggest caching per
//! socket or per family. Measured on this platform, all three pointers came
//! back identical for `AF_INET` and `AF_INET6` sockets and across repeated
//! sockets of the same family, so one process-wide cache — discovered through a
//! single throwaway `AF_INET` socket — serves both families. This is what mio,
//! tokio and compio all do.
//!
//! A failed lookup is not cached. `OnceLock<Result<..>>` would make one
//! transient failure permanent for the process.

use std::sync::OnceLock;

use windows::core::{Result, GUID};
use windows::Win32::Networking::WinSock::{
    WSAIoctl, AF_INET, LPFN_ACCEPTEX, LPFN_CONNECTEX, LPFN_GETACCEPTEXSOCKADDRS,
    SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR, SOCKET, WSAID_ACCEPTEX, WSAID_CONNECTEX,
    WSAID_GETACCEPTEXSOCKADDRS,
};

use super::socket::Socket;

/// `AcceptEx`, with the `Option` discharged.
pub(crate) type AcceptExFn = unsafe extern "system" fn(
    listen_socket: SOCKET,
    accept_socket: SOCKET,
    output_buffer: *mut core::ffi::c_void,
    receive_data_length: u32,
    local_address_length: u32,
    remote_address_length: u32,
    bytes_received: *mut u32,
    overlapped: *mut windows::Win32::System::IO::OVERLAPPED,
) -> windows::core::BOOL;

/// `ConnectEx`, with the `Option` discharged.
pub(crate) type ConnectExFn = unsafe extern "system" fn(
    socket: SOCKET,
    name: *const SOCKADDR,
    namelen: i32,
    send_buffer: *const core::ffi::c_void,
    send_data_length: u32,
    bytes_sent: *mut u32,
    overlapped: *mut windows::Win32::System::IO::OVERLAPPED,
) -> windows::core::BOOL;

/// `GetAcceptExSockaddrs`, with the `Option` discharged.
pub(crate) type GetAcceptExSockaddrsFn = unsafe extern "system" fn(
    output_buffer: *const core::ffi::c_void,
    receive_data_length: u32,
    local_address_length: u32,
    remote_address_length: u32,
    local_sockaddr: *mut *mut SOCKADDR,
    local_sockaddr_length: *mut i32,
    remote_sockaddr: *mut *mut SOCKADDR,
    remote_sockaddr_length: *mut i32,
);

/// The extension functions this crate uses.
///
/// The `Option` in Winsock's `LPFN_*` aliases is discharged once, here, rather
/// than at each call site: a provider that reports success and hands back a
/// null pointer is rejected during resolution, so callers cannot be handed one.
pub(crate) struct Extensions {
    pub(crate) accept_ex: AcceptExFn,
    pub(crate) connect_ex: ConnectExFn,
    pub(crate) get_accept_ex_sockaddrs: GetAcceptExSockaddrsFn,
}

static EXTENSIONS: OnceLock<Extensions> = OnceLock::new();

/// The process-wide extension function table, resolving it on first use.
pub(crate) fn extensions() -> Result<&'static Extensions> {
    if let Some(found) = EXTENSIONS.get() {
        return Ok(found);
    }
    // Resolving creates a socket, so this is one of the entry points that must
    // initialise Winsock. Without it, calling `extensions()` before anything
    // else in the process would fail — and the unit test below would only pass
    // when some other test happened to run first.
    let resolved = resolve()?;
    // A racing thread may have won; either table is equally valid.
    Ok(EXTENSIONS.get_or_init(|| resolved))
}

fn resolve() -> Result<Extensions> {
    // `Socket::new_overlapped` calls `ensure_winsock` for us.
    let probe = Socket::new_overlapped(AF_INET)?;

    // SAFETY: `probe` is a live socket of the provider being queried, and each
    // lookup writes one function pointer into a live local of the right size.
    let accept_ex: LPFN_ACCEPTEX = unsafe { lookup(&probe, &WSAID_ACCEPTEX) }?;
    // SAFETY: as above.
    let connect_ex: LPFN_CONNECTEX = unsafe { lookup(&probe, &WSAID_CONNECTEX) }?;
    // SAFETY: as above.
    let get_accept_ex_sockaddrs: LPFN_GETACCEPTEXSOCKADDRS =
        unsafe { lookup(&probe, &WSAID_GETACCEPTEXSOCKADDRS) }?;

    // A provider that reports success but hands back a null pointer would
    // otherwise blow up at the call site, far from the cause.
    Ok(Extensions {
        accept_ex: accept_ex.ok_or_else(proc_not_found)?,
        connect_ex: connect_ex.ok_or_else(proc_not_found)?,
        get_accept_ex_sockaddrs: get_accept_ex_sockaddrs.ok_or_else(proc_not_found)?,
    })
}

fn proc_not_found() -> windows::core::Error {
    windows::core::Error::from_hresult(
        windows::Win32::Foundation::ERROR_PROC_NOT_FOUND.to_hresult(),
    )
}

/// Fetch one extension pointer.
///
/// # Safety
///
/// `T` must be the function-pointer type the GUID names, and must be the size
/// Winsock writes (a single pointer).
unsafe fn lookup<T: Copy>(socket: &Socket, guid: &GUID) -> Result<T> {
    let mut out = std::mem::MaybeUninit::<T>::zeroed();
    let mut written = 0u32;
    // SAFETY: the input is a live GUID and the output is a live, zeroed slot of
    // `size_of::<T>()` bytes, which the caller guarantees matches what Winsock
    // writes. The call is synchronous — no overlapped structure is involved —
    // so nothing outlives this frame.
    let rc = unsafe {
        WSAIoctl(
            socket.raw(),
            SIO_GET_EXTENSION_FUNCTION_POINTER,
            Some(std::ptr::from_ref(guid).cast()),
            std::mem::size_of::<GUID>() as u32,
            Some(out.as_mut_ptr().cast()),
            std::mem::size_of::<T>() as u32,
            &mut written,
            None,
            None,
        )
    };
    if rc != 0 {
        return Err(windows::core::Error::from_thread());
    }
    // SAFETY: `WSAIoctl` reported success, so it initialised the output slot.
    Ok(unsafe { out.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Networking::WinSock::AF_INET6;

    #[test]
    fn the_extensions_resolve() {
        let _guard = crate::net::socket::socket_guard();
        // Deliberately does not initialise Winsock first: `extensions()` must
        // stand on its own, or this test would only pass depending on order.
        let ext = extensions().expect("extension functions are available");
        // Non-null by construction: `resolve` rejects a null pointer, so
        // reaching here at all is the assertion. Reading them keeps the fields
        // load-bearing rather than dead.
        assert_ne!(ext.accept_ex as usize, 0);
        assert_ne!(ext.connect_ex as usize, 0);
        assert_ne!(ext.get_accept_ex_sockaddrs as usize, 0);
    }

    #[test]
    fn repeated_lookups_return_the_same_table() {
        let _guard = crate::net::socket::socket_guard();
        let a = extensions().expect("resolve");
        let b = extensions().expect("resolve again");
        assert!(std::ptr::eq(a, b), "the table is cached, not re-resolved");
    }

    #[test]
    fn the_pointers_do_not_depend_on_the_address_family() {
        let _guard = crate::net::socket::socket_guard();
        // The premise of the single process-wide cache. If a future platform
        // ever disagreed, this fails here rather than misbehaving at a call
        // site on the family that was not used for discovery.
        let v4 = Socket::new_overlapped(AF_INET).expect("v4 socket");
        let v6 = Socket::new_overlapped(AF_INET6).expect("v6 socket");

        // SAFETY: both are live sockets and `LPFN_ACCEPTEX` is what the GUID
        // names.
        let a: LPFN_ACCEPTEX = unsafe { lookup(&v4, &WSAID_ACCEPTEX) }.expect("v4 lookup");
        // SAFETY: as above.
        let b: LPFN_ACCEPTEX = unsafe { lookup(&v6, &WSAID_ACCEPTEX) }.expect("v6 lookup");

        assert_eq!(
            a.map(|f| f as usize),
            b.map(|f| f as usize),
            "AcceptEx must be the same function for both families"
        );
    }
}
