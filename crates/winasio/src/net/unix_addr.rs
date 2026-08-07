// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The `AF_UNIX` address type.
//!
//! [`std::net::SocketAddr`] cannot hold a filesystem path, so unlike the
//! IP families — where [`crate::net::addr`] deliberately reuses `std`'s
//! vocabulary rather than inventing one — `AF_UNIX` forces a crate-local type.
//! Its shape follows [`std::os::unix::net::SocketAddr`] where the platform
//! allows, so that code moved between Unix and Windows reads the same:
//! [`UnixSocketAddr::as_pathname`] and [`UnixSocketAddr::is_unnamed`] mean what
//! they mean there.
//!
//! # What has to be representable
//!
//! Two things, both measured rather than assumed.
//!
//! * **A pathname.** `sun_path` holds **UTF-8 bytes** on this platform, not
//!   UTF-16 and not a code page: a path containing `日本語` bound successfully,
//!   created exactly that file on disk, and `getsockname` returned
//!   byte-identical bytes. Round-tripping is therefore exact, and this type
//!   stores the bytes rather than a `String` so that it stays exact even for a
//!   peer address this crate did not encode.
//!
//! * **An unnamed address.** Binding a socket to an **empty** `sun_path`
//!   succeeds and creates no file. That is the `AF_UNIX` analogue of binding
//!   the wildcard, and since `ConnectEx` refuses an unbound socket exactly as
//!   it does for TCP, it is what every client this crate connects binds first.
//!   The consequence is that an accepted peer address is *routinely* unnamed —
//!   `GetAcceptExSockaddrs` reports family 1 with an empty path — so "unnamed"
//!   is a normal value here, not an error case.
//!
//! # Why the decoder scans for a NUL instead of trusting the length
//!
//! The length Winsock reports alongside a `sockaddr_un` is not a path length
//! and cannot be treated as one. Measured: `getsockname` on a named socket
//! returned a *trimmed* length (`2 + strlen + 1`, e.g. 58), while
//! `getpeername` and `GetAcceptExSockaddrs` returned the **full 110** in every
//! case — including for an unnamed peer whose path is empty, where 110 would
//! decode as 108 bytes of NUL if believed. So the path is derived from a NUL
//! scan, bounded above by the reported length.
//!
//! And the scan must tolerate *no* NUL at all: a path filling all 108 bytes
//! binds successfully, `getsockname` returns those 108 bytes byte-for-byte,
//! and there is nowhere for a terminator to go. A decoder that assumed one was
//! present would read past the field. The fallback is the full slot.

use std::path::Path;

/// The maximum number of bytes `sockaddr_un::sun_path` can hold.
///
/// Not a guess: `sizeof(SOCKADDR_UN)` is 110 on this platform, of which two
/// bytes are the family, and a 108-byte path binds successfully with no room
/// left for a terminator.
pub const UNIX_PATH_MAX: usize = 108;

/// Why a [`UnixSocketAddr`] could not be built from a path.
///
/// A separate type from [`SocketError`](super::SocketError), deliberately.
/// Constructing an address is pure validation that never reaches the operating
/// system, so every `SocketError` variant would be wrong for it — including
/// the `Win32` catch-all, which would have to carry a fabricated error code.
/// This follows the same principle the workspace applied when it replaced
/// `winasio-util`'s single sixteen-variant error with precise per-API types:
/// an error type should advertise only what its API can actually produce.
///
/// Correspondingly this enum is **not** `#[non_exhaustive]`. The set of ways a
/// path can fail to fit in a fixed 108-byte UTF-8 field is closed by the
/// platform, not open-ended like the set of things the kernel can refuse, so a
/// caller is entitled to match it exhaustively and be told by the compiler if
/// that ever stops being true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnixSocketAddrError {
    /// The path did not fit in `sun_path`.
    ///
    /// Carries the encoded length that was attempted, so a caller can report
    /// how much too long it was. Truncating instead would silently bind a
    /// *different* path, which is the failure mode this rejects.
    PathTooLong {
        /// The length in bytes the path would have needed.
        len: usize,
        /// The most that fits, [`UNIX_PATH_MAX`].
        max: usize,
    },
    /// The path was not valid UTF-8.
    ///
    /// `sun_path` is a UTF-8 byte field on this platform (measured), and a
    /// Windows `Path` can hold ill-formed UTF-16 that has no UTF-8 form. Such
    /// a path has no representation here and is refused rather than mangled
    /// through a lossy conversion, which would bind a path the caller did not
    /// ask for.
    NotUtf8,
    /// The path contained an interior NUL byte.
    ///
    /// Rejected because the decoder finds the end of a path by scanning for a
    /// NUL: an address built from such a path would not survive a round trip
    /// through the kernel, and would compare unequal to itself once read back.
    InteriorNul {
        /// The byte offset of the first NUL.
        position: usize,
    },
}

impl std::fmt::Display for UnixSocketAddrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnixSocketAddrError::PathTooLong { len, max } => write!(
                f,
                "socket path is {len} bytes, which does not fit in the {max} available"
            ),
            UnixSocketAddrError::NotUtf8 => {
                write!(f, "socket path is not valid UTF-8")
            }
            UnixSocketAddrError::InteriorNul { position } => write!(
                f,
                "socket path contains a NUL byte at offset {position}, which would not survive a round trip"
            ),
        }
    }
}

impl std::error::Error for UnixSocketAddrError {}

/// An `AF_UNIX` socket address: a filesystem path, or unnamed.
///
/// Shaped after `std::os::unix::net::SocketAddr` — which does not exist on
/// this target, so it cannot be linked, but is what a reader coming from Unix
/// will expect. The Linux abstract
/// namespace has no Windows counterpart, so there is no `as_abstract_name`;
/// everything else that type offers for a pathname socket is here.
///
/// Equality is over the address bytes, so two addresses naming the same path
/// with different spellings (a relative and an absolute form of the same file,
/// say) are *not* equal. That matches the kernel's own view: `sun_path` is
/// compared as bytes.
#[derive(Clone, Copy)]
pub struct UnixSocketAddr {
    /// The path bytes, unterminated. Only `len` of these are meaningful.
    path: [u8; UNIX_PATH_MAX],
    /// How many leading bytes of `path` are the address. Zero means unnamed.
    len: usize,
}

impl UnixSocketAddr {
    /// The unnamed address — an empty `sun_path`.
    ///
    /// Binding this succeeds and creates no file on disk. It is what a client
    /// binds before `ConnectEx`, which refuses an unbound socket, and it is
    /// what such a client's peer sees it as.
    pub const fn unnamed() -> Self {
        UnixSocketAddr {
            path: [0; UNIX_PATH_MAX],
            len: 0,
        }
    }

    /// Build an address naming a filesystem path.
    ///
    /// A path too long for `sun_path` is **rejected**, not truncated: a
    /// truncated path is a valid path to a different file, so binding it would
    /// succeed and quietly serve the wrong address.
    ///
    /// A relative path is accepted and is resolved by the platform against the
    /// process working directory, which was measured rather than assumed. That
    /// makes a relative path a poor choice for anything long-lived, since the
    /// address a caller intended depends on a mutable piece of process state,
    /// but it is not this type's place to refuse it.
    pub fn from_pathname(path: impl AsRef<Path>) -> Result<Self, UnixSocketAddrError> {
        let path = path.as_ref();
        let bytes = path
            .to_str()
            .ok_or(UnixSocketAddrError::NotUtf8)?
            .as_bytes();
        Self::from_bytes(bytes)
    }

    /// Build an address from raw `sun_path` bytes.
    ///
    /// The escape hatch for a path this crate cannot express as a `Path`, and
    /// the constructor the tests use to build the 108-byte unterminated case
    /// deliberately.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, UnixSocketAddrError> {
        if bytes.len() > UNIX_PATH_MAX {
            return Err(UnixSocketAddrError::PathTooLong {
                len: bytes.len(),
                max: UNIX_PATH_MAX,
            });
        }
        if let Some(position) = bytes.iter().position(|&b| b == 0) {
            return Err(UnixSocketAddrError::InteriorNul { position });
        }
        let mut path = [0u8; UNIX_PATH_MAX];
        path[..bytes.len()].copy_from_slice(bytes);
        Ok(UnixSocketAddr {
            path,
            len: bytes.len(),
        })
    }

    /// Decode from the `sun_path` field of a `sockaddr_un` the kernel filled in.
    ///
    /// `available` is the number of `sun_path` bytes the reported length
    /// vouches for. See the module docs for why that is an upper bound on the
    /// scan and not the answer itself.
    ///
    /// Infallible on purpose: every byte pattern the kernel can put here is a
    /// representable address, including all 108 bytes with no terminator, and
    /// including none at all. There is nothing to reject.
    pub(crate) fn from_sun_path(slot: &[u8; UNIX_PATH_MAX], available: usize) -> Self {
        let bound = available.min(UNIX_PATH_MAX);
        // The NUL scan, bounded above by what the length vouches for. Falling
        // back to `bound` rather than panicking is the 108-byte unterminated
        // case, which the platform really does produce.
        let len = slot[..bound].iter().position(|&b| b == 0).unwrap_or(bound);
        UnixSocketAddr { path: *slot, len }
    }

    /// Whether this address names nothing.
    pub fn is_unnamed(&self) -> bool {
        self.len == 0
    }

    /// The path this address names, or `None` if it is unnamed.
    ///
    /// Returns `None` rather than an empty path for the unnamed case, matching
    /// `std::os::unix::net::SocketAddr::as_pathname`: an empty path is not a
    /// path, and a caller who handed one to `std::fs` would get a confusing
    /// error a long way from here.
    pub fn as_pathname(&self) -> Option<&Path> {
        if self.is_unnamed() {
            return None;
        }
        // The bytes came either from `from_pathname`, where they were `&str`,
        // or from the kernel, where the round trip was measured to be exact
        // UTF-8. A non-UTF-8 peer path is conceivable and yields `None` rather
        // than a lossy path that names a different file.
        std::str::from_utf8(self.as_bytes()).ok().map(Path::new)
    }

    /// The address bytes, exactly as they sit in `sun_path`, unterminated.
    ///
    /// Empty for an unnamed address. This is the lossless view:
    /// [`UnixSocketAddr::as_pathname`] can lose a path that is not UTF-8,
    /// this cannot.
    pub fn as_bytes(&self) -> &[u8] {
        &self.path[..self.len]
    }
}

/// Equality is over the address bytes only, **not** over the whole 108-byte
/// slot.
///
/// This cannot be derived, and the difference is reachable rather than
/// theoretical. `from_sun_path` takes custody of the entire slot, including
/// whatever sits past the terminator; on the accept path that slot is a copy
/// of the provider's own buffer, which is under no obligation to have zeroed
/// its tail. An address built by [`UnixSocketAddr::from_pathname`] has a zero
/// tail by construction. A derived `PartialEq` would then report two addresses
/// naming the identical path as different, depending on nothing the caller can
/// see or control.
///
/// The bytes past `len` are not part of the address, so they are not part of
/// its identity.
impl PartialEq for UnixSocketAddr {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for UnixSocketAddr {}

/// Hashed over the same bytes equality compares, as the `Hash`/`Eq` contract
/// requires. Deriving this while hand-writing `PartialEq` would break that
/// contract silently — two equal addresses could hash differently and go
/// missing from a `HashMap`.
impl std::hash::Hash for UnixSocketAddr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl std::fmt::Debug for UnixSocketAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The unnamed case gets a word rather than an empty string, because an
        // empty string in a log is indistinguishable from a formatting bug.
        match self.as_pathname() {
            Some(path) => f.debug_tuple("UnixSocketAddr").field(&path).finish(),
            None if self.is_unnamed() => f.write_str("UnixSocketAddr(unnamed)"),
            // Named, but not UTF-8. Show the bytes rather than nothing.
            None => f
                .debug_tuple("UnixSocketAddr")
                .field(&self.as_bytes())
                .finish(),
        }
    }
}

impl std::fmt::Display for UnixSocketAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.as_pathname() {
            Some(path) => write!(f, "{}", path.display()),
            None if self.is_unnamed() => f.write_str("(unnamed)"),
            None => write!(f, "{:?}", self.as_bytes()),
        }
    }
}

impl TryFrom<&Path> for UnixSocketAddr {
    type Error = UnixSocketAddrError;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        UnixSocketAddr::from_pathname(value)
    }
}

impl TryFrom<&std::path::PathBuf> for UnixSocketAddr {
    type Error = UnixSocketAddrError;

    fn try_from(value: &std::path::PathBuf) -> Result<Self, Self::Error> {
        UnixSocketAddr::from_pathname(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unnamed_address_names_nothing() {
        let addr = UnixSocketAddr::unnamed();
        assert!(addr.is_unnamed());
        assert_eq!(addr.as_pathname(), None);
        assert_eq!(addr.as_bytes(), b"");
    }

    #[test]
    fn a_pathname_round_trips() {
        let addr = UnixSocketAddr::from_pathname(r"C:\temp\a.sock").expect("build");
        assert!(!addr.is_unnamed());
        assert_eq!(addr.as_pathname(), Some(Path::new(r"C:\temp\a.sock")));
        assert_eq!(addr.as_bytes(), br"C:\temp\a.sock");
    }

    #[test]
    fn a_path_of_exactly_the_maximum_is_accepted() {
        // The boundary on the accepting side. Measured: 108 bytes binds.
        let bytes = vec![b'x'; UNIX_PATH_MAX];
        let addr = UnixSocketAddr::from_bytes(&bytes).expect("108 bytes fits");
        assert_eq!(addr.as_bytes().len(), UNIX_PATH_MAX);
        assert_eq!(addr.as_bytes(), &bytes[..]);
    }

    #[test]
    fn an_over_long_path_is_rejected_rather_than_truncated() {
        // Truncation is the dangerous failure: the shortened path is itself a
        // valid path, so a bind would succeed on the wrong address and nothing
        // downstream could tell.
        let bytes = vec![b'x'; UNIX_PATH_MAX + 1];
        let err = UnixSocketAddr::from_bytes(&bytes).expect_err("109 bytes must not fit");
        assert_eq!(
            err,
            UnixSocketAddrError::PathTooLong {
                len: UNIX_PATH_MAX + 1,
                max: UNIX_PATH_MAX
            }
        );
    }

    #[test]
    fn an_interior_nul_is_rejected() {
        // Such an address could be constructed but not read back: the decoder
        // stops at the first NUL, so the round trip would silently shorten it.
        let err = UnixSocketAddr::from_bytes(b"a\0b").expect_err("interior NUL");
        assert_eq!(err, UnixSocketAddrError::InteriorNul { position: 1 });
    }

    #[test]
    fn decoding_stops_at_the_first_nul_within_the_reported_length() {
        let mut slot = [0u8; UNIX_PATH_MAX];
        slot[..5].copy_from_slice(b"/a/b/");
        // The full slot is vouched for, but the address ends at the NUL.
        let addr = UnixSocketAddr::from_sun_path(&slot, UNIX_PATH_MAX);
        assert_eq!(addr.as_bytes(), b"/a/b/");
    }

    #[test]
    fn a_full_slot_with_no_terminator_decodes_as_the_whole_slot() {
        // Measured: a 108-byte path binds and reads back with no NUL anywhere.
        // A decoder that assumed a terminator would run off the end of the
        // field; one that returned an error would reject a real address.
        let slot = [b'x'; UNIX_PATH_MAX];
        let addr = UnixSocketAddr::from_sun_path(&slot, UNIX_PATH_MAX);
        assert_eq!(addr.as_bytes().len(), UNIX_PATH_MAX);
        assert_eq!(addr.as_bytes(), &slot[..]);
        assert!(!addr.is_unnamed());
    }

    #[test]
    fn an_all_nul_slot_decodes_as_unnamed_not_as_a_path_of_nuls() {
        // This is what `getpeername` hands back for a wildcard-bound peer,
        // together with a reported length of the full 110. Believing that
        // length as a path length would produce a 108-byte path of NULs.
        let slot = [0u8; UNIX_PATH_MAX];
        let addr = UnixSocketAddr::from_sun_path(&slot, UNIX_PATH_MAX);
        assert!(addr.is_unnamed());
        assert_eq!(addr.as_pathname(), None);
    }

    #[test]
    fn the_reported_length_bounds_the_scan_from_above() {
        // The trimmed-length case: `getsockname` reports only as far as the
        // terminator. Bytes beyond it must not be picked up even if non-NUL,
        // which is what an unbounded scan over a reused storage would do.
        let mut slot = [b'z'; UNIX_PATH_MAX];
        slot[..3].copy_from_slice(b"abc");
        let addr = UnixSocketAddr::from_sun_path(&slot, 3);
        assert_eq!(addr.as_bytes(), b"abc");
    }

    #[test]
    fn a_length_beyond_the_slot_is_clamped_rather_than_panicking() {
        // The kernel reports 110 — the whole `sockaddr_un` — where only 108
        // bytes are path. An unclamped index would panic on a slice bound.
        let slot = [b'q'; UNIX_PATH_MAX];
        let addr = UnixSocketAddr::from_sun_path(&slot, 500);
        assert_eq!(addr.as_bytes().len(), UNIX_PATH_MAX);
    }

    #[test]
    fn a_non_ascii_path_survives_byte_for_byte() {
        // Measured: `sun_path` is UTF-8, and a non-ASCII path round-tripped
        // through the kernel byte-identically.
        let path = "/tmp/日本語ø.sock";
        let addr = UnixSocketAddr::from_pathname(path).expect("build");
        assert_eq!(addr.as_bytes(), path.as_bytes());
        assert_eq!(addr.as_pathname(), Some(Path::new(path)));
        // And through the decoder, as the kernel would return it.
        let mut slot = [0u8; UNIX_PATH_MAX];
        slot[..path.len()].copy_from_slice(path.as_bytes());
        assert_eq!(
            UnixSocketAddr::from_sun_path(&slot, UNIX_PATH_MAX),
            addr,
            "the encode and decode directions must agree"
        );
    }

    #[test]
    fn equality_ignores_bytes_past_the_address() {
        // Not a hypothetical. `from_sun_path` takes the whole slot, and on the
        // accept path that slot is a copy of the provider's buffer, whose tail
        // is not guaranteed to be zero. A derived `PartialEq` compares the
        // padding and would call these two different addresses.
        //
        // The earlier version of this test used a zero-filled slot on both
        // sides, so it passed with the derived impl and proved nothing. This
        // one fails without the hand-written `PartialEq`.
        let a = UnixSocketAddr::from_bytes(b"/x").expect("build");

        let mut dirty = [0u8; UNIX_PATH_MAX];
        dirty[..2].copy_from_slice(b"/x");
        dirty[2] = 0; // the terminator
        for byte in dirty.iter_mut().skip(3) {
            *byte = 0xAB; // leftovers the provider never cleared
        }
        let b = UnixSocketAddr::from_sun_path(&dirty, UNIX_PATH_MAX);

        assert_eq!(b.as_bytes(), b"/x", "the scan must stop at the terminator");
        assert_eq!(a, b, "bytes past the address are not part of it");

        // And `Hash` must agree with `Eq`, or these would go missing from a
        // `HashMap` depending on which one was inserted.
        let hash = |addr: &UnixSocketAddr| {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            addr.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(&a), hash(&b), "equal addresses must hash equally");
    }

    #[test]
    fn debug_and_display_name_the_unnamed_case_rather_than_printing_nothing() {
        let unnamed = UnixSocketAddr::unnamed();
        assert!(format!("{unnamed:?}").contains("unnamed"));
        assert!(format!("{unnamed}").contains("unnamed"));
        let named = UnixSocketAddr::from_pathname("/tmp/s").expect("build");
        assert!(format!("{named:?}").contains("/tmp/s"));
        assert_eq!(format!("{named}"), "/tmp/s");
    }

    #[test]
    fn every_construction_error_says_something_specific() {
        // A control against a `Display` that forgot an arm and printed the
        // same sentence for all three.
        let messages = [
            UnixSocketAddrError::PathTooLong {
                len: 200,
                max: UNIX_PATH_MAX,
            }
            .to_string(),
            UnixSocketAddrError::NotUtf8.to_string(),
            UnixSocketAddrError::InteriorNul { position: 3 }.to_string(),
        ];
        for (i, a) in messages.iter().enumerate() {
            assert!(!a.is_empty());
            for b in messages.iter().skip(i + 1) {
                assert_ne!(a, b, "each variant needs its own description");
            }
        }
    }

    #[test]
    fn a_path_converts_through_try_from() {
        let addr: UnixSocketAddr = Path::new("/tmp/t.sock").try_into().expect("convert");
        assert_eq!(addr.as_pathname(), Some(Path::new("/tmp/t.sock")));
    }
}
