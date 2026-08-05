// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Conversion between [`std::net::SocketAddr`] and the Winsock address
//! structures.
//!
//! The public address type is `std`'s, not a crate-local one: callers already
//! have `SocketAddr` values, the conversion is mechanical, and a second address
//! vocabulary would buy nothing.
//!
//! Everything is carried in a `SOCKADDR_STORAGE` plus a length. One layout for
//! both families keeps the operations non-generic over the address family,
//! which matters most for `AcceptEx`, whose address buffer would otherwise have
//! to be sized — and mismatched — per family.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use windows::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_INET, AF_INET6, IN6_ADDR, IN6_ADDR_0, IN_ADDR, IN_ADDR_0, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0, SOCKADDR_STORAGE,
};

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

/// Decode a `sockaddr` this crate did not allocate.
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes holding a `sockaddr` of the family
/// its `sa_family` field reports.
pub(crate) unsafe fn decode_raw(ptr: *const SOCKADDR, len: i32) -> Option<SocketAddr> {
    if ptr.is_null() || len < std::mem::size_of::<SOCKADDR>() as i32 {
        return None;
    }
    let mut storage = SOCKADDR_STORAGE::default();
    let copy = (len as usize).min(std::mem::size_of::<SOCKADDR_STORAGE>());
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
    // SAFETY: `ptr` is readable for at least a `SOCKADDR`, checked above.
    let family = unsafe { (*ptr).sa_family };
    decode(&storage, family)
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
        // AF_UNIX; a family this crate never produces.
        // SAFETY: writing the family field of a live, zeroed storage.
        unsafe { (*bytes.as_mut_ptr()).sa_family = ADDRESS_FAMILY(1) };
        assert!(bytes.to_socket_addr().is_none());
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
        let decoded = unsafe { decode_raw(encoded.as_ptr(), encoded.len()) };
        assert_eq!(decoded, Some(addr));
    }

    #[test]
    fn a_null_or_short_raw_sockaddr_is_rejected() {
        // SAFETY: the null and short cases are exactly what this checks for.
        assert!(unsafe { decode_raw(std::ptr::null(), 16) }.is_none());
        let encoded = SockAddrBytes::from_socket_addr("1.2.3.4:5".parse().unwrap());
        // SAFETY: reading nothing; the length check rejects before any read.
        assert!(unsafe { decode_raw(encoded.as_ptr(), 1) }.is_none());
    }
}
