// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Read outcome classification for sockets.
//!
//! Sockets reuse [`ReadOutcome`] rather than defining their own enum, so the
//! whole-payload helpers in [`crate::io`] work unchanged and files, pipes and
//! sockets share one vocabulary. What is socket-specific is the
//! *classification*, and one case of it is load-bearing.

use windows::core::Result;
use windows::Win32::Foundation::{
    ERROR_CONNECTION_ABORTED, ERROR_NETNAME_DELETED, ERROR_UNEXP_NET_ERR,
};
use windows::Win32::Networking::WinSock::{
    WSAECONNABORTED, WSAECONNRESET, WSAEDISCON, WSAENETRESET, WSAESHUTDOWN,
};

use crate::fs::ReadOutcome;

use super::error::win32_code;

/// Turn a completed `WSARecv` into a read outcome.
///
/// `requested` is the length the operation asked for. It is not decoration —
/// see below.
///
/// Returns `None` for a failure this function does not recognise, leaving the
/// caller to surface the raw error, exactly as [`crate::fs`]'s classifier does.
pub(crate) fn classify_socket_read(
    result: &Result<usize>,
    requested: usize,
) -> Option<ReadOutcome> {
    match result {
        // A graceful close. TCP has no zero-length message, so a successful
        // recv of nothing means the peer sent FIN.
        //
        // This must not be reported as `Bytes(0)`. `crate::io::read_to_end`
        // treats `Bytes(0)` as "this iteration made no progress, go round
        // again", which is right for a message-mode pipe that can deliver a
        // genuinely empty message — and would spin forever on a closed socket.
        Ok(0) if requested > 0 => Some(ReadOutcome::ClosedPeer),

        // ...except when the caller asked for nothing. A zero-length `WSARecv`
        // completes immediately with zero bytes on a perfectly healthy
        // connection, so the rule above would report a closed peer for a caller
        // who simply passed an empty buffer.
        Ok(n) => Some(ReadOutcome::Bytes(*n)),

        Err(e) => match win32_code(e) {
            // The five Winsock spellings, seen when the failure is inline.
            Some(code)
                if code as i32 == WSAECONNRESET.0
                    || code as i32 == WSAECONNABORTED.0
                    || code as i32 == WSAENETRESET.0
                    || code as i32 == WSAESHUTDOWN.0
                    || code as i32 == WSAEDISCON.0 =>
            {
                Some(ReadOutcome::ClosedPeer)
            }
            // The three Win32 spellings, seen when the same condition arrives
            // on a completion packet after `RtlNtStatusToDosError`.
            Some(code)
                if code == ERROR_NETNAME_DELETED.0
                    || code == ERROR_CONNECTION_ABORTED.0
                    || code == ERROR_UNEXP_NET_ERR.0 =>
            {
                Some(ReadOutcome::ClosedPeer)
            }
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::{Error, HRESULT};

    fn err(code: i32) -> Result<usize> {
        Err(Error::from_hresult(HRESULT::from_win32(code as u32)))
    }

    #[test]
    fn a_zero_byte_read_of_a_non_empty_buffer_is_a_closed_peer() {
        // The case `crate::io::read_to_end` would otherwise spin on.
        assert_eq!(
            classify_socket_read(&Ok(0), 4096),
            Some(ReadOutcome::ClosedPeer)
        );
    }

    #[test]
    fn a_zero_byte_read_of_an_empty_buffer_is_not_a_closed_peer() {
        // A zero-length recv completes immediately with zero bytes on a healthy
        // connection. Reporting `ClosedPeer` here would invent a disconnection.
        assert_eq!(classify_socket_read(&Ok(0), 0), Some(ReadOutcome::Bytes(0)));
    }

    #[test]
    fn a_non_empty_read_reports_its_count() {
        assert_eq!(
            classify_socket_read(&Ok(11), 4096),
            Some(ReadOutcome::Bytes(11))
        );
    }

    #[test]
    fn every_closed_peer_code_from_either_scheme_agrees() {
        // The two delivery paths spell the same condition differently, and
        // which one a caller sees depends on whether the failure was inline or
        // arrived on a packet.
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
            assert_eq!(
                classify_socket_read(&err(code), 4096),
                Some(ReadOutcome::ClosedPeer),
                "code {code} should be a closed peer"
            );
        }
    }

    #[test]
    fn an_unrecognised_failure_is_left_to_the_caller() {
        // A control: the classifier must not swallow errors it does not know,
        // or a real fault would look like an orderly shutdown.
        assert_eq!(classify_socket_read(&err(1_000_000), 4096), None);
    }

    #[test]
    fn a_cancelled_read_is_not_a_closed_peer() {
        use windows::Win32::Foundation::ERROR_OPERATION_ABORTED;
        // Cancellation is the caller's own doing; reporting it as a peer
        // closure would tell `read_to_end` the stream ended cleanly.
        assert_eq!(
            classify_socket_read(&err(ERROR_OPERATION_ABORTED.0 as i32), 4096),
            None
        );
    }
}
