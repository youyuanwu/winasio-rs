// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Conversion between socket addresses and the Winsock address structures.
//!
//! The public address type for the IP families is `std`'s, not a crate-local
//! one: callers already have `SocketAddr` values, the conversion is
//! mechanical, and a second address vocabulary would buy nothing. `AF_UNIX`
//! gets [`UnixSocketAddr`] only because `std::net::SocketAddr` cannot hold a
//! path at all — see [`super::unix_addr`].
//!
//! Everything is carried in a `SOCKADDR_STORAGE` plus a length. One layout for
//! all three families keeps the operations non-generic over the address
//! family, which matters most for `AcceptEx`, whose address buffer would
//! otherwise have to be sized — and mismatched — per family. `SOCKADDR_UN` is
//! 110 bytes against `SOCKADDR_STORAGE`'s 128, so `AF_UNIX` needed no change
//! to the storage and none to the accept buffer.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use windows::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_INET, AF_INET6, AF_UNIX, IN6_ADDR, IN6_ADDR_0, IN_ADDR, IN_ADDR_0, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0, SOCKADDR_STORAGE, SOCKADDR_UN,
};

use super::unix_addr::{UnixSocketAddr, UNIX_PATH_MAX};

/// `AF_UNIX` as an [`ADDRESS_FAMILY`].
///
/// The `windows` crate exports `AF_INET` and `AF_INET6` as `ADDRESS_FAMILY`
/// but `AF_UNIX` as a bare `u16`, so the wrapper has to be applied somewhere.
/// It is applied once, here, rather than at each use.
pub(crate) const AF_UNIX_FAMILY: ADDRESS_FAMILY = ADDRESS_FAMILY(AF_UNIX);

/// The error for a `sockaddr` whose family the caller cannot interpret.
///
/// One definition rather than one per call site, because it is the answer to
/// exactly one question — "this storage is not the family I asked about" — and
/// three places now ask it: `getsockname`/`getpeername` decoding, and each
/// listener's accept path. `WSAEAFNOSUPPORT` is the honest code: nothing
/// failed at the OS level, the crate simply cannot express what it was handed.
pub(crate) fn unsupported_family() -> windows::core::Error {
    windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(
        windows::Win32::Networking::WinSock::WSAEAFNOSUPPORT.0 as u32,
    ))
}

/// A socket address in the form Winsock takes, with its valid length.
#[derive(Clone, Copy)]
pub(crate) struct SockAddrBytes {
    storage: SOCKADDR_STORAGE,
    len: i32,
}

impl SockAddrBytes {
    /// An all-zero storage of the maximum size, for calls that fill one in.
    pub(crate) fn zeroed() -> Self {
        SockAddrBytes {
            storage: SOCKADDR_STORAGE::default(),
            len: std::mem::size_of::<SOCKADDR_STORAGE>() as i32,
        }
    }

    /// Encode a `std` address.
    pub(crate) fn from_socket_addr(addr: SocketAddr) -> Self {
        let mut storage = SOCKADDR_STORAGE::default();
        let len = match addr {
            SocketAddr::V4(v4) => {
                let sin = SOCKADDR_IN {
                    sin_family: AF_INET,
                    // Winsock wants the port in network order.
                    sin_port: v4.port().to_be(),
                    sin_addr: IN_ADDR {
                        S_un: IN_ADDR_0 {
                            S_un_b: octets_to_in_addr(v4.ip().octets()),
                        },
                    },
                    sin_zero: [0; 8],
                };
                // SAFETY: `SOCKADDR_STORAGE` is at least as large as, and at
                // least as aligned as, `SOCKADDR_IN`; both are plain data.
                unsafe {
                    std::ptr::write_unaligned(
                        std::ptr::addr_of_mut!(storage).cast::<SOCKADDR_IN>(),
                        sin,
                    );
                }
                std::mem::size_of::<SOCKADDR_IN>() as i32
            }
            SocketAddr::V6(v6) => {
                let sin6 = SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo().to_be(),
                    sin6_addr: IN6_ADDR {
                        u: IN6_ADDR_0 {
                            Byte: v6.ip().octets(),
                        },
                    },
                    Anonymous: SOCKADDR_IN6_0 {
                        sin6_scope_id: v6.scope_id(),
                    },
                };
                // SAFETY: as above, for the v6 layout.
                unsafe {
                    std::ptr::write_unaligned(
                        std::ptr::addr_of_mut!(storage).cast::<SOCKADDR_IN6>(),
                        sin6,
                    );
                }
                std::mem::size_of::<SOCKADDR_IN6>() as i32
            }
        };
        SockAddrBytes { storage, len }
    }

    /// Decode back to a `std` address.
    ///
    /// Returns `None` for a family this crate does not produce, rather than
    /// guessing. A v4-mapped IPv6 address decodes as
    /// [`SocketAddr::V6`] unchanged — see [`crate::net`] on dual-stack
    /// listeners.
    pub(crate) fn to_socket_addr(self) -> Option<SocketAddr> {
        // SAFETY: reading the family field, which is at the same offset in
        // every `sockaddr` variant and is always initialised in a storage this
        // crate fills or hands to Winsock.
        let family =
            unsafe { std::ptr::addr_of!(self.storage).cast::<SOCKADDR>().read() }.sa_family;
        decode(&self.storage, family)
    }

    /// Encode an `AF_UNIX` address.
    ///
    /// The declared length is always the full `sizeof(SOCKADDR_UN)` — 110 —
    /// rather than a trimmed `2 + strlen + 1`. Both are accepted by `bind` and
    /// `ConnectEx`, and the full size is the one that does not have to reason
    /// about whether a terminator is present, which for a 108-byte path it is
    /// not.
    pub(crate) fn from_unix_addr(addr: &UnixSocketAddr) -> Self {
        let mut storage = SOCKADDR_STORAGE::default();
        let mut sun = SOCKADDR_UN {
            sun_family: AF_UNIX_FAMILY,
            sun_path: [0; UNIX_PATH_MAX],
        };
        // `sun_path` is `[i8; 108]` in the `windows` bindings, and the address
        // holds `u8`. The cast is a reinterpretation of the same byte, not a
        // conversion: a path byte above 0x7F — which a UTF-8 path certainly
        // has — must reach the kernel unchanged, and it does.
        for (slot, byte) in sun.sun_path.iter_mut().zip(addr.as_bytes()) {
            *slot = *byte as i8;
        }
        // SAFETY: `SOCKADDR_STORAGE` (128 bytes) is larger than, and at least
        // as aligned as, `SOCKADDR_UN` (110 bytes); both are plain data.
        unsafe {
            std::ptr::write_unaligned(std::ptr::addr_of_mut!(storage).cast::<SOCKADDR_UN>(), sun);
        }
        SockAddrBytes {
            storage,
            len: std::mem::size_of::<SOCKADDR_UN>() as i32,
        }
    }

    /// Decode back to an `AF_UNIX` address.
    ///
    /// Returns `None` for any other family, rather than guessing.
    pub(crate) fn to_unix_addr(self) -> Option<UnixSocketAddr> {
        if self.family() != AF_UNIX_FAMILY {
            return None;
        }
        Some(decode_unix(&self.storage, self.len))
    }

    /// The address family the storage reports.
    pub(crate) fn family(&self) -> ADDRESS_FAMILY {
        // SAFETY: reading the family field, which is at the same offset in
        // every `sockaddr` variant and is always initialised in a storage this
        // crate fills or hands to Winsock.
        unsafe { std::ptr::addr_of!(self.storage).cast::<SOCKADDR>().read() }.sa_family
    }

    /// Copy a `sockaddr` this crate did not allocate into an owned storage.
    ///
    /// This is what lets the accept operation stay family-agnostic: it can
    /// take custody of the bytes `GetAcceptExSockaddrs` points at — which live
    /// in the operation's own buffer and die with it — without knowing what
    /// family they are, leaving the interpretation to whichever listener type
    /// asked for the accept.
    ///
    /// # Safety
    ///
    /// `ptr` must point at `len` readable bytes holding a `sockaddr`.
    pub(crate) unsafe fn copy_from_raw(ptr: *const SOCKADDR, len: i32) -> Option<Self> {
        if ptr.is_null() || len < std::mem::size_of::<SOCKADDR>() as i32 {
            return None;
        }
        let mut storage = SOCKADDR_STORAGE::default();
        let copy = (len as usize).min(std::mem::size_of::<SOCKADDR_STORAGE>());
        // Copy rather than dereference, and read everything afterwards from
        // the *copy*.
        //
        // `ptr` typically points into the accept operation's `Box<[u8; N]>`,
        // whose Rust-level alignment is 1. `SOCKADDR` wants 2. Windows in
        // practice hands back a well-aligned address, but "in practice" is not
        // what the abstract machine asks for, and a dereference through an
        // underaligned pointer is undefined behaviour whatever the hardware
        // does. `storage` is a properly aligned local, which is exactly why it
        // is copied into.
        //
        // SAFETY: the caller guarantees `len` readable bytes at `ptr`; the
        // destination is a live storage of at least `copy` bytes, and the two
        // cannot overlap because `storage` is a fresh local.
        unsafe {
            std::ptr::copy_nonoverlapping(
                ptr.cast::<u8>(),
                std::ptr::addr_of_mut!(storage).cast::<u8>(),
                copy,
            );
        }
        Some(SockAddrBytes {
            storage,
            len: copy as i32,
        })
    }

    /// A pointer to the encoded address, for handing to Winsock.
    pub(crate) fn as_ptr(&self) -> *const SOCKADDR {
        std::ptr::addr_of!(self.storage).cast()
    }

    /// A mutable pointer, for calls that fill the storage in.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut SOCKADDR {
        std::ptr::addr_of_mut!(self.storage).cast()
    }

    /// The encoded length.
    pub(crate) fn len(&self) -> i32 {
        self.len
    }

    /// A pointer to the length, for calls that report how much they wrote.
    pub(crate) fn len_mut(&mut self) -> *mut i32 {
        std::ptr::addr_of_mut!(self.len)
    }
}

fn decode(storage: &SOCKADDR_STORAGE, family: ADDRESS_FAMILY) -> Option<SocketAddr> {
    if family == AF_INET {
        // SAFETY: the family field says this storage holds a `SOCKADDR_IN`,
        // which is smaller than `SOCKADDR_STORAGE`. `read_unaligned` makes no
        // alignment demand.
        let sin = unsafe {
            std::ptr::addr_of!(*storage)
                .cast::<SOCKADDR_IN>()
                .read_unaligned()
        };
        // SAFETY: the union's byte form is always valid to read; every arm is
        // plain data of the same size.
        let octets = unsafe { in_addr_to_octets(sin.sin_addr.S_un.S_un_b) };
        Some(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::from(octets),
            u16::from_be(sin.sin_port),
        )))
    } else if family == AF_INET6 {
        // SAFETY: as above, for the v6 layout.
        let sin6 = unsafe {
            std::ptr::addr_of!(*storage)
                .cast::<SOCKADDR_IN6>()
                .read_unaligned()
        };
        // SAFETY: the union's `Byte` arm is the full 16-byte address.
        let octets = unsafe { sin6.sin6_addr.u.Byte };
        // SAFETY: the anonymous union has a single meaningful arm here.
        let scope_id = unsafe { sin6.Anonymous.sin6_scope_id };
        Some(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(octets),
            u16::from_be(sin6.sin6_port),
            u32::from_be(sin6.sin6_flowinfo),
            scope_id,
        )))
    } else {
        None
    }
}

/// Decode the `AF_UNIX` part of a storage the caller has already confirmed is
/// `AF_UNIX`.
///
/// `reported` is the whole `sockaddr` length Winsock gave, so the bytes it
/// vouches for in `sun_path` are two fewer. See [`super::unix_addr`] for why
/// that is only an upper bound on the scan.
fn decode_unix(storage: &SOCKADDR_STORAGE, reported: i32) -> UnixSocketAddr {
    // SAFETY: the caller checked the family field says `AF_UNIX`, and
    // `SOCKADDR_UN` (110 bytes) is smaller than `SOCKADDR_STORAGE` (128).
    // `read_unaligned` makes no alignment demand.
    let sun = unsafe {
        std::ptr::addr_of!(*storage)
            .cast::<SOCKADDR_UN>()
            .read_unaligned()
    };
    // `[i8; 108]` to `[u8; 108]`: the same bytes seen as unsigned, which is
    // what a UTF-8 path is. Not a numeric conversion.
    let mut slot = [0u8; UNIX_PATH_MAX];
    for (out, byte) in slot.iter_mut().zip(sun.sun_path.iter()) {
        *out = *byte as u8;
    }
    // Two bytes of the reported length are the family. A length shorter than
    // that vouches for nothing, which `saturating_sub` renders as zero rather
    // than wrapping to a huge bound.
    let available =
        (reported.max(0) as usize).saturating_sub(std::mem::size_of::<ADDRESS_FAMILY>());
    UnixSocketAddr::from_sun_path(&slot, available)
}

fn octets_to_in_addr(o: [u8; 4]) -> windows::Win32::Networking::WinSock::IN_ADDR_0_0 {
    windows::Win32::Networking::WinSock::IN_ADDR_0_0 {
        s_b1: o[0],
        s_b2: o[1],
        s_b3: o[2],
        s_b4: o[3],
    }
}

fn in_addr_to_octets(b: windows::Win32::Networking::WinSock::IN_ADDR_0_0) -> [u8; 4] {
    [b.s_b1, b.s_b2, b.s_b3, b.s_b4]
}

/// The address family a `SocketAddr` needs a socket of.
pub(crate) fn family_of(addr: &SocketAddr) -> ADDRESS_FAMILY {
    match addr {
        SocketAddr::V4(_) => AF_INET,
        SocketAddr::V6(_) => AF_INET6,
    }
}

/// The wildcard address of the same family, port 0.
///
/// `ConnectEx` requires the socket to be bound already, unlike `connect`, so
/// every connect binds this first.
///
/// The `AF_UNIX` counterpart is [`UnixSocketAddr::unnamed`] — the empty
/// `sun_path`, which binds successfully and creates no file. It is not
/// expressed here because it takes no argument to choose from: there is only
/// one Unix wildcard, whereas the IP wildcard has to match the family of the
/// destination.
pub(crate) fn wildcard_for(addr: &SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(addr: SocketAddr) {
        let encoded = SockAddrBytes::from_socket_addr(addr);
        let decoded = encoded.to_socket_addr().expect("a family we produce");
        assert_eq!(decoded, addr, "round trip must preserve the address");
    }

    #[test]
    fn ipv4_addresses_round_trip() {
        round_trip("127.0.0.1:8080".parse().unwrap());
        round_trip("0.0.0.0:0".parse().unwrap());
        round_trip("192.168.1.254:65535".parse().unwrap());
    }

    #[test]
    fn ipv6_addresses_round_trip() {
        round_trip("[::1]:443".parse().unwrap());
        round_trip("[::]:0".parse().unwrap());
        round_trip("[2001:db8::dead:beef]:1".parse().unwrap());
    }

    #[test]
    fn the_ipv6_scope_id_survives() {
        // A link-local address is useless without its scope, and the scope
        // lives in an anonymous union that is easy to drop on the floor.
        let addr = SocketAddr::V6(SocketAddrV6::new(
            "fe80::1".parse().unwrap(),
            0,
            0,
            3, // interface index
        ));
        let decoded = SockAddrBytes::from_socket_addr(addr)
            .to_socket_addr()
            .unwrap();
        match decoded {
            SocketAddr::V6(v6) => assert_eq!(v6.scope_id(), 3),
            other => panic!("expected v6, got {other}"),
        }
    }

    #[test]
    fn the_ipv6_flowinfo_survives() {
        let addr = SocketAddr::V6(SocketAddrV6::new("::1".parse().unwrap(), 9, 0x12345, 0));
        let decoded = SockAddrBytes::from_socket_addr(addr)
            .to_socket_addr()
            .unwrap();
        match decoded {
            SocketAddr::V6(v6) => assert_eq!(v6.flowinfo(), 0x12345),
            other => panic!("expected v6, got {other}"),
        }
    }

    #[test]
    fn the_port_is_stored_in_network_order() {
        // Getting this wrong is silent: the round trip would still pass if both
        // directions were byte-swapped. Check the wire bytes directly.
        let encoded = SockAddrBytes::from_socket_addr("127.0.0.1:8080".parse().unwrap());
        // SAFETY: `from_socket_addr` wrote a `SOCKADDR_IN`, whose `sin_port`
        // occupies bytes 2..4 of the storage; both reads are in bounds.
        let bytes: [u8; 2] = unsafe {
            let p = encoded.as_ptr().cast::<u8>().add(2);
            [*p, *p.add(1)]
        };
        assert_eq!(bytes, 8080u16.to_be_bytes());
    }

    #[test]
    fn an_unknown_family_is_rejected_rather_than_guessed() {
        let mut bytes = SockAddrBytes::zeroed();
        // AF_APPLETALK; a family this crate neither produces nor decodes.
        // SAFETY: writing the family field of a live, zeroed storage.
        unsafe { (*bytes.as_mut_ptr()).sa_family = ADDRESS_FAMILY(16) };
        assert!(bytes.to_socket_addr().is_none());
        assert!(bytes.to_unix_addr().is_none());
    }

    #[test]
    fn wildcards_match_the_family_of_the_target() {
        assert_eq!(
            wildcard_for(&"1.2.3.4:5".parse().unwrap()),
            "0.0.0.0:0".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            wildcard_for(&"[2001:db8::1]:5".parse().unwrap()),
            "[::]:0".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn encoded_lengths_are_the_platform_sizes() {
        assert_eq!(
            SockAddrBytes::from_socket_addr("1.2.3.4:5".parse().unwrap()).len(),
            std::mem::size_of::<SOCKADDR_IN>() as i32
        );
        assert_eq!(
            SockAddrBytes::from_socket_addr("[::1]:5".parse().unwrap()).len(),
            std::mem::size_of::<SOCKADDR_IN6>() as i32
        );
    }

    #[test]
    fn a_raw_sockaddr_decodes_the_same_way() {
        let addr: SocketAddr = "10.0.0.7:1234".parse().unwrap();
        let encoded = SockAddrBytes::from_socket_addr(addr);
        // SAFETY: `encoded` holds a valid `SOCKADDR_IN` of the stated length.
        let decoded = unsafe { SockAddrBytes::copy_from_raw(encoded.as_ptr(), encoded.len()) }
            .and_then(|bytes| bytes.to_socket_addr());
        assert_eq!(decoded, Some(addr));
    }

    #[test]
    fn a_null_or_short_raw_sockaddr_is_rejected() {
        // SAFETY: the null and short cases are exactly what this checks for.
        assert!(unsafe { SockAddrBytes::copy_from_raw(std::ptr::null(), 16) }.is_none());
        let encoded = SockAddrBytes::from_socket_addr("1.2.3.4:5".parse().unwrap());
        // SAFETY: reading nothing; the length check rejects before any read.
        assert!(unsafe { SockAddrBytes::copy_from_raw(encoded.as_ptr(), 1) }.is_none());
    }

    // -----------------------------------------------------------------
    // AF_UNIX
    // -----------------------------------------------------------------

    #[test]
    fn a_unix_pathname_round_trips_through_the_storage() {
        let addr = UnixSocketAddr::from_pathname(r"C:\temp\winasio.sock").expect("build");
        let encoded = SockAddrBytes::from_unix_addr(&addr);
        assert_eq!(encoded.family(), AF_UNIX_FAMILY);
        assert_eq!(encoded.to_unix_addr(), Some(addr));
    }

    #[test]
    fn the_unnamed_unix_address_round_trips() {
        // The wildcard case, and the one a length-believing decoder gets
        // wrong: the declared length is the full 110 even though the path is
        // empty.
        let addr = UnixSocketAddr::unnamed();
        let encoded = SockAddrBytes::from_unix_addr(&addr);
        assert_eq!(encoded.len(), std::mem::size_of::<SOCKADDR_UN>() as i32);
        let decoded = encoded.to_unix_addr().expect("AF_UNIX");
        assert!(decoded.is_unnamed(), "got {decoded:?}");
    }

    #[test]
    fn a_full_length_unix_path_round_trips_with_no_terminator() {
        // Measured: 108 bytes with nowhere for a NUL binds and reads back
        // whole. Encoding must not need a terminator either.
        let bytes = vec![b'p'; 108];
        let addr = UnixSocketAddr::from_bytes(&bytes).expect("108 fits");
        let decoded = SockAddrBytes::from_unix_addr(&addr)
            .to_unix_addr()
            .expect("AF_UNIX");
        assert_eq!(decoded.as_bytes(), &bytes[..]);
    }

    #[test]
    fn a_non_ascii_unix_path_survives_the_i8_field_unchanged() {
        // `sun_path` is `[i8; 108]` in the bindings and UTF-8 bytes in fact,
        // so every byte above 0x7F crosses a signedness boundary twice. A
        // conversion instead of a reinterpretation would corrupt them, and a
        // pure-ASCII test would never notice.
        let path = "/tmp/日本語ø.sock";
        assert!(path.bytes().any(|b| b >= 0x80), "the test needs high bytes");
        let addr = UnixSocketAddr::from_pathname(path).expect("build");
        let decoded = SockAddrBytes::from_unix_addr(&addr)
            .to_unix_addr()
            .expect("AF_UNIX");
        assert_eq!(decoded.as_bytes(), path.as_bytes());
    }

    #[test]
    fn a_unix_storage_does_not_decode_as_an_ip_address_or_the_reverse() {
        // The families must not be interchangeable in either direction, or a
        // mis-set family field would produce a plausible-looking wrong answer
        // instead of a refusal.
        let unix =
            SockAddrBytes::from_unix_addr(&UnixSocketAddr::from_pathname("/tmp/x").expect("build"));
        assert!(unix.to_socket_addr().is_none());

        let inet = SockAddrBytes::from_socket_addr("127.0.0.1:9".parse().unwrap());
        assert!(inet.to_unix_addr().is_none());
    }

    #[test]
    fn a_trimmed_reported_length_still_decodes_the_whole_path() {
        // `getsockname` reports `2 + strlen + 1`, not the full 110. The
        // decoder subtracts the family and scans within what is left, so the
        // terminator is found at exactly the boundary.
        let path = b"/tmp/abc";
        let addr = UnixSocketAddr::from_bytes(path).expect("build");
        let mut encoded = SockAddrBytes::from_unix_addr(&addr);
        encoded.len = 2 + path.len() as i32 + 1;
        assert_eq!(encoded.to_unix_addr().expect("AF_UNIX").as_bytes(), path);
    }

    #[test]
    fn a_reported_length_too_short_to_hold_a_family_decodes_as_unnamed() {
        // Defence in depth: `available` is computed by subtracting the family
        // size, and a plain subtraction would wrap to a huge bound on a length
        // the platform should never produce.
        let addr = UnixSocketAddr::from_pathname("/tmp/y").expect("build");
        let mut encoded = SockAddrBytes::from_unix_addr(&addr);
        encoded.len = 1;
        assert!(encoded.to_unix_addr().expect("AF_UNIX").is_unnamed());
        encoded.len = -5;
        assert!(encoded.to_unix_addr().expect("AF_UNIX").is_unnamed());
    }

    #[test]
    fn a_unix_address_fits_inside_the_shared_storage() {
        // The premise of the one-layout-for-every-family design, and of the
        // `AcceptEx` buffer needing no resize. If `SOCKADDR_UN` ever outgrew
        // `SOCKADDR_STORAGE`, `from_unix_addr`'s write would run off the end.
        assert!(std::mem::size_of::<SOCKADDR_UN>() <= std::mem::size_of::<SOCKADDR_STORAGE>());
        assert_eq!(std::mem::size_of::<SOCKADDR_UN>(), 110);
    }

    #[test]
    fn copying_a_raw_sockaddr_preserves_it_for_either_family() {
        // The accept path's mechanism: take custody of bytes owned by the
        // operation, decode later.
        let inet: SocketAddr = "10.1.2.3:77".parse().unwrap();
        let encoded = SockAddrBytes::from_socket_addr(inet);
        // SAFETY: `encoded` holds a valid `SOCKADDR_IN` of the stated length.
        let copied =
            unsafe { SockAddrBytes::copy_from_raw(encoded.as_ptr(), encoded.len()) }.expect("copy");
        assert_eq!(copied.to_socket_addr(), Some(inet));

        let unix = UnixSocketAddr::from_pathname("/tmp/copied").expect("build");
        let encoded = SockAddrBytes::from_unix_addr(&unix);
        // SAFETY: as above, for a `SOCKADDR_UN`.
        let copied =
            unsafe { SockAddrBytes::copy_from_raw(encoded.as_ptr(), encoded.len()) }.expect("copy");
        assert_eq!(copied.to_unix_addr(), Some(unix));
    }

    #[test]
    fn copying_a_null_or_short_raw_sockaddr_is_refused() {
        // SAFETY: the null and short cases are what this checks for; neither
        // dereferences.
        assert!(unsafe { SockAddrBytes::copy_from_raw(std::ptr::null(), 110) }.is_none());
        let encoded = SockAddrBytes::from_socket_addr("1.2.3.4:5".parse().unwrap());
        // SAFETY: the length check rejects before any read.
        assert!(unsafe { SockAddrBytes::copy_from_raw(encoded.as_ptr(), 1) }.is_none());
    }
}
