// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Setup and operation failures reported by the socket types.
//!
//! One error type covers `bind`, `connect` and `accept`. They share nearly
//! every failure mode — registration refusal, skip-mode refusal, and the
//! platform's habit of reporting the same condition under two different
//! numbering schemes — so three near-identical enums would triple the
//! classification table below without giving a caller anything to match on that
//! it does not already have.
//!
//! # Two numbering schemes for one condition
//!
//! A socket failure reaches this crate by one of two routes, and they disagree:
//!
//! * **Inline** — the API returned failure immediately and the code came from
//!   `WSAGetLastError`. These are the Winsock numbers, 10004–11031.
//! * **Completion packet** — the operation was pending and the port delivered
//!   an NTSTATUS, which the driver has already run through
//!   `RtlNtStatusToDosError`. These are ordinary Win32 numbers.
//!
//! A refused connection is `WSAECONNREFUSED` (10061) one way and
//! `ERROR_CONNECTION_REFUSED` (1225) the other. Classifying only one of them
//! would make the outcome depend on timing, so both are mapped.

use windows::core::Error;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CONNECTION_ABORTED, ERROR_CONNECTION_REFUSED,
    ERROR_CONNECTION_UNAVAIL, ERROR_HOST_UNREACHABLE, ERROR_NETNAME_DELETED,
    ERROR_NETWORK_UNREACHABLE, ERROR_OPERATION_ABORTED, ERROR_SEM_TIMEOUT, ERROR_UNEXP_NET_ERR,
};
use windows::Win32::Networking::WinSock::{
    WSAEACCES, WSAEADDRINUSE, WSAEADDRNOTAVAIL, WSAECONNABORTED, WSAECONNREFUSED, WSAECONNRESET,
    WSAEDISCON, WSAEHOSTUNREACH, WSAENETRESET, WSAENETUNREACH, WSAESHUTDOWN, WSAETIMEDOUT,
};

use crate::iocp::RegistrationError;

/// A failure reported while setting up or completing a socket operation.
///
/// `#[non_exhaustive]`: the platform has more conditions than are worth naming
/// today, and promoting one out of [`SocketError::Win32`] later should not be a
/// breaking change.
#[derive(Debug)]
#[non_exhaustive]
pub enum SocketError {
    /// Nothing was listening on the target address.
    ConnectionRefused,
    /// The host or network could not be reached.
    Unreachable,
    /// The connection attempt timed out.
    TimedOut,
    /// The connection was reset or aborted.
    ConnectionAborted,
    /// The requested local address is already in use.
    AddressInUse,
    /// The requested local address is not available on this machine.
    AddressNotAvailable,
    /// Access was denied.
    AccessDenied,
    /// The operation was cancelled — normally because the awaiting future was
    /// dropped, or the owning socket was closed.
    Cancelled,
    /// The socket is already registered with a completion mechanism.
    AlreadyRegistered,
    /// The socket refused the inline-success skip mode, so registration was
    /// refused rather than silently degraded.
    ///
    /// This is its own variant on purpose. The crate's completion driver treats
    /// an inline success as final and does not wait for a packet; on a socket
    /// that cannot suppress that packet, the assumption is wrong and the right
    /// answer is to refuse the socket, loudly. Folding it into
    /// [`SocketError::Win32`] would hide it behind `ERROR_INVALID_PARAMETER` —
    /// the same code [`SocketError::AlreadyRegistered`] carries — and make the
    /// two indistinguishable.
    ///
    /// In practice this means a non-IFS layered service provider is installed.
    SkipModeUnsupported(Error),
    /// Any other platform failure.
    Win32(Error),
}

impl SocketError {
    /// Classify a platform error from either numbering scheme.
    pub(crate) fn from_win32(err: Error) -> Self {
        let Some(code) = win32_code(&err) else {
            return SocketError::Win32(err);
        };
        match code as i32 {
            // Refused.
            c if c == WSAECONNREFUSED.0 => SocketError::ConnectionRefused,
            c if c == ERROR_CONNECTION_REFUSED.0 as i32 => SocketError::ConnectionRefused,

            // Unreachable.
            c if c == WSAENETUNREACH.0 || c == WSAEHOSTUNREACH.0 => SocketError::Unreachable,
            c if c == ERROR_NETWORK_UNREACHABLE.0 as i32
                || c == ERROR_HOST_UNREACHABLE.0 as i32 =>
            {
                SocketError::Unreachable
            }

            // Timed out.
            c if c == WSAETIMEDOUT.0 => SocketError::TimedOut,
            c if c == ERROR_SEM_TIMEOUT.0 as i32 => SocketError::TimedOut,

            // Reset or aborted. The same five Winsock codes and three Win32
            // codes that `ReadOutcome::ClosedPeer` covers on the data path.
            c if c == WSAECONNRESET.0
                || c == WSAECONNABORTED.0
                || c == WSAENETRESET.0
                || c == WSAESHUTDOWN.0
                || c == WSAEDISCON.0 =>
            {
                SocketError::ConnectionAborted
            }
            c if c == ERROR_NETNAME_DELETED.0 as i32
                || c == ERROR_CONNECTION_ABORTED.0 as i32
                || c == ERROR_UNEXP_NET_ERR.0 as i32 =>
            {
                SocketError::ConnectionAborted
            }

            // Local address problems. These have no completion-path spelling:
            // `bind` is synchronous.
            c if c == WSAEADDRINUSE.0 => SocketError::AddressInUse,
            c if c == WSAEADDRNOTAVAIL.0 => SocketError::AddressNotAvailable,

            // Access.
            c if c == WSAEACCES.0 => SocketError::AccessDenied,
            c if c == ERROR_ACCESS_DENIED.0 as i32 => SocketError::AccessDenied,

            // Cancellation. Only ever arrives on the completion path — the
            // packet a cancelled operation still delivers.
            c if c == ERROR_OPERATION_ABORTED.0 as i32 => SocketError::Cancelled,

            // `WSAECONNUNAVAIL` has no Winsock spelling in scope; the Win32
            // side is a plausible packet code for a refused connection on some
            // providers, so it is classified rather than left opaque.
            c if c == ERROR_CONNECTION_UNAVAIL.0 as i32 => SocketError::Unreachable,

            _ => SocketError::Win32(err),
        }
    }
}

impl From<RegistrationError> for SocketError {
    fn from(value: RegistrationError) -> Self {
        match value {
            RegistrationError::AlreadyRegistered(_) => SocketError::AlreadyRegistered,
            // Deliberately *not* flattened into `Win32`. See the variant docs:
            // `fs::SetupError` does flatten it, which is why `net` does not
            // reuse that type.
            RegistrationError::SkipModeUnsupported(e) => SocketError::SkipModeUnsupported(e),
            RegistrationError::Os(e) => SocketError::from_win32(e),
        }
    }
}

impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketError::ConnectionRefused => write!(f, "connection refused"),
            SocketError::Unreachable => write!(f, "network or host is unreachable"),
            SocketError::TimedOut => write!(f, "connection timed out"),
            SocketError::ConnectionAborted => write!(f, "connection reset or aborted"),
            SocketError::AddressInUse => write!(f, "address is already in use"),
            SocketError::AddressNotAvailable => write!(f, "address is not available"),
            SocketError::AccessDenied => write!(f, "access denied"),
            SocketError::Cancelled => write!(f, "operation was cancelled"),
            SocketError::AlreadyRegistered => write!(
                f,
                "socket is already registered with a completion mechanism"
            ),
            SocketError::SkipModeUnsupported(e) => write!(
                f,
                "socket does not support suppressing completions for inline success: {e}"
            ),
            SocketError::Win32(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SocketError {}

/// Recover the numeric code from a Win32-facility `HRESULT`.
///
/// Winsock codes (10004–11031) fit in 16 bits, so they survive the same
/// facility mask [`crate::fs`]'s equivalent uses. This is a separate copy only
/// because that one is `pub(crate)` inside `fs`.
pub(crate) fn win32_code(err: &Error) -> Option<u32> {
    let raw = err.code().0 as u32;
    if raw >> 16 == 0x8007 {
        Some(raw & 0xFFFF)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HRESULT;

    fn classify(code: i32) -> SocketError {
        SocketError::from_win32(Error::from_hresult(HRESULT::from_win32(code as u32)))
    }

    #[test]
    fn both_encodings_of_a_refused_connection_agree() {
        // The whole reason this table exists: an inline failure and a
        // completion packet spell the same condition differently, and which one
        // a caller sees depends on timing.
        assert!(matches!(
            classify(WSAECONNREFUSED.0),
            SocketError::ConnectionRefused
        ));
        assert!(matches!(
            classify(ERROR_CONNECTION_REFUSED.0 as i32),
            SocketError::ConnectionRefused
        ));
    }

    #[test]
    fn every_abort_code_from_either_scheme_is_one_variant() {
        for code in [
            WSAECONNRESET.0,
            WSAECONNABORTED.0,
            WSAENETRESET.0,
            WSAESHUTDOWN.0,
            WSAEDISCON.0,
            ERROR_NETNAME_DELETED.0 as i32,
            ERROR_CONNECTION_ABORTED.0 as i32,
            ERROR_UNEXP_NET_ERR.0 as i32,
        ] {
            assert!(
                matches!(classify(code), SocketError::ConnectionAborted),
                "code {code} should classify as ConnectionAborted"
            );
        }
    }

    #[test]
    fn unreachable_and_timeout_map_from_both_schemes() {
        assert!(matches!(
            classify(WSAENETUNREACH.0),
            SocketError::Unreachable
        ));
        assert!(matches!(
            classify(WSAEHOSTUNREACH.0),
            SocketError::Unreachable
        ));
        assert!(matches!(
            classify(ERROR_NETWORK_UNREACHABLE.0 as i32),
            SocketError::Unreachable
        ));
        assert!(matches!(
            classify(ERROR_HOST_UNREACHABLE.0 as i32),
            SocketError::Unreachable
        ));
        assert!(matches!(classify(WSAETIMEDOUT.0), SocketError::TimedOut));
        assert!(matches!(
            classify(ERROR_SEM_TIMEOUT.0 as i32),
            SocketError::TimedOut
        ));
    }

    #[test]
    fn local_address_failures_are_distinguishable() {
        assert!(matches!(
            classify(WSAEADDRINUSE.0),
            SocketError::AddressInUse
        ));
        assert!(matches!(
            classify(WSAEADDRNOTAVAIL.0),
            SocketError::AddressNotAvailable
        ));
    }

    #[test]
    fn a_cancelled_operation_is_not_an_opaque_win32_error() {
        assert!(matches!(
            classify(ERROR_OPERATION_ABORTED.0 as i32),
            SocketError::Cancelled
        ));
    }

    #[test]
    fn an_unclassified_code_stays_opaque() {
        // A control: the table must not swallow codes it does not know.
        assert!(matches!(classify(1_000_000), SocketError::Win32(_)));
    }

    #[test]
    fn a_refused_skip_mode_does_not_collapse_into_win32() {
        // The whole point of FR-026. `fs::SetupError` flattens this case, and
        // the flattened form is indistinguishable from `AlreadyRegistered`
        // because both carry `ERROR_INVALID_PARAMETER`.
        use windows::Win32::Foundation::ERROR_INVALID_PARAMETER;
        let inner = Error::from_hresult(ERROR_INVALID_PARAMETER.to_hresult());
        let err = SocketError::from(RegistrationError::SkipModeUnsupported(inner));
        assert!(
            matches!(err, SocketError::SkipModeUnsupported(_)),
            "got {err:?}"
        );

        let already = SocketError::from(RegistrationError::AlreadyRegistered(Error::from_hresult(
            ERROR_INVALID_PARAMETER.to_hresult(),
        )));
        assert!(matches!(already, SocketError::AlreadyRegistered));
    }
}
