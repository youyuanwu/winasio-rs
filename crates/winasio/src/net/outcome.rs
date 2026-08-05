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
//! *classification*, and two cases of it are load-bearing: the zero-length
//! request, and the difference between a peer that finished and a peer that
//! was cut off.

use windows::core::Result;

use crate::fs::ReadOutcome;

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

        // Everything else is a failure, and stays one.
        //
        // An earlier version folded the reset spellings — `WSAECONNRESET`,
        // `WSAECONNABORTED`, `WSAENETRESET`, `ERROR_NETNAME_DELETED` and the
        // rest — into `ClosedPeer` as well, on the grounds that they all mean
        // "the peer is gone". They do, but they do not all mean the same thing
        // about the *data*, and `ClosedPeer` is a success:
        //
        // * FIN says the peer finished sending. What you have is everything it
        //   meant to send.
        // * RST says the connection was destroyed. What you have is whatever
        //   happened to arrive first.
        //
        // `crate::io::read_to_end` stops on `ClosedPeer` and reports success,
        // so classifying a reset that way returned a silently truncated buffer
        // with `Ok(())` beside it, and no way for the caller to tell it from a
        // complete transfer. That is the truncation hazard every read-until-
        // close protocol steps on. `std::net::TcpStream::read_to_end` returns
        // `Err(ConnectionReset)` here, and so does this crate now: an abrupt
        // loss surfaces as an error, and `Ok(0)` is the only thing that
        // produces `ClosedPeer`.
        //
        // This crate's own `io.rs` already made the argument, about a different
        // code: "reporting it as a peer closure would tell `read_to_end` the
        // stream ended cleanly and hand back a truncated buffer with no error".
        // It just was not applied here.
        Err(_) => None,
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
    fn an_abrupt_loss_is_an_error_not_a_closed_peer() {
        use windows::Win32::Foundation::{
            ERROR_CONNECTION_ABORTED, ERROR_NETNAME_DELETED, ERROR_UNEXP_NET_ERR,
        };
        use windows::Win32::Networking::WinSock::{
            WSAECONNABORTED, WSAECONNRESET, WSAEDISCON, WSAENETRESET, WSAESHUTDOWN,
        };

        // These all mean "the peer is gone", and it is tempting to fold them
        // into `ClosedPeer` for that reason. The temptation is wrong, and this
        // test is here to keep anyone from giving in to it again.
        //
        // `ClosedPeer` is a *success*: `read_to_end` stops on it and reports
        // `Ok`. A FIN earns that, because the peer sent everything it meant to.
        // A reset does not — what arrived is whatever beat the RST — so folding
        // these in returns a truncated buffer with no error attached, which the
        // caller cannot distinguish from a complete one.
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
                None,
                "code {code} must stay an error; only `Ok(0)` is a graceful close"
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
