// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Failure classification for the WinHTTP client.
//!
//! # Why operations resolve to `windows::core::Error` and not to this type
//!
//! Reads, writes, sends and receives resolve to
//! [`OpResult`](crate::iocp::OpResult), whose result half is a
//! [`windows::core::Result`] — exactly as files, pipes and sockets do.
//! [`WinHttpError`] is the *classifier* a caller reaches for when it needs to
//! act on the difference between, say, a cancellation and a timeout. This
//! mirrors [`SocketError`](crate::net::SocketError) rather than inventing a
//! second, almost-identical result type for one module.
//!
//! # Why the descriptions are written out here
//!
//! WinHTTP's error codes live in `winhttp.dll`'s message table, which
//! `FormatMessage` does not consult unless it is asked to load that module.
//! [`windows::core::Error::message`] therefore returns an **empty string** for
//! every code in this file — 12002, 12017, 12029, 12150, 12175 and the rest.
//! That was measured, not assumed. A caller that logs `{e}` on a raw error
//! gets `"The operation completed successfully."` or nothing at all, so this
//! type supplies its own text.

use windows::core::{Error, HRESULT};
use windows::Win32::Foundation::ERROR_BUSY;
use windows::Win32::Networking::WinHttp::{
    ERROR_WINHTTP_CANNOT_CONNECT, ERROR_WINHTTP_CONNECTION_ERROR, ERROR_WINHTTP_HEADER_NOT_FOUND,
    ERROR_WINHTTP_INCORRECT_HANDLE_STATE, ERROR_WINHTTP_INVALID_SERVER_RESPONSE,
    ERROR_WINHTTP_NAME_NOT_RESOLVED, ERROR_WINHTTP_OPERATION_CANCELLED,
    ERROR_WINHTTP_SECURE_FAILURE, ERROR_WINHTTP_TIMEOUT,
};

/// `ERROR_BUSY` as a plain `u32`.
///
/// The `WINHTTP_*` codes are generated as bare `u32` constants, but the
/// `Foundation` ones are generated as the `WIN32_ERROR` newtype, and a field
/// access is an expression rather than a constant path — so `ERROR_BUSY.0`
/// cannot appear in a `match` pattern. Naming it once here keeps the arms below
/// uniform.
const ERROR_BUSY_CODE: u32 = ERROR_BUSY.0;

/// A failure reported by a WinHTTP request.
///
/// # Why this enum is exhaustive
///
/// Deliberately **not** `#[non_exhaustive]`, unlike the other error enums in
/// this crate, because here the attribute would cost a real guarantee and buy
/// almost nothing.
///
/// [`WinHttpError::Other`] already absorbs every code this enum does not name,
/// and WinHTTP defines around sixty of them. A caller is therefore never
/// surprised at *runtime* by an unclassified failure: it arrives as `Other`
/// carrying its raw code. The only change `#[non_exhaustive]` would guard
/// against is *promoting* a code out of `Other` into a named variant — and that
/// already changes behaviour for anyone matching `Other(12002)`, whether or not
/// the attribute is present. Sealing the enum would protect against a break the
/// attribute cannot actually prevent.
///
/// What it costs is concrete: with `#[non_exhaustive]` no downstream crate can
/// match this enum exhaustively, so the compiler can never tell a caller that a
/// new variant appeared and they might want to handle it. For an enum whose
/// entire purpose is to be branched on, that is the more valuable guarantee,
/// and it is the one kept here.
///
/// The consequence is accepted rather than worked around: **adding a named
/// variant is a breaking change** and needs a semver-major release. The
/// `every_winhttp_error_variant_can_be_matched_from_another_crate` test in the
/// integration suite exists to make that promise visible from outside, where
/// the attribute would have applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinHttpError {
    /// The operation was cancelled, normally because the request handle was
    /// closed while the operation was still in flight.
    ///
    /// This is a *failure*, and the crate never reports it as an empty
    /// successful body. The distinction matters for the same reason it matters
    /// in [`crate::net`]: a caller collecting a response body has no other way
    /// to tell "the body was empty" from "the body was truncated".
    Cancelled,
    /// A resolve, connect, send or receive deadline elapsed.
    Timeout,
    /// The server could not be reached.
    CannotConnect,
    /// The connection was lost part-way through.
    ConnectionError,
    /// The TLS handshake failed, or the server's certificate was rejected.
    ///
    /// See [`Request::relax_certificate_validation`](super::Request::relax_certificate_validation)
    /// if a specific check needs to be waived.
    SecureFailure,
    /// The host name did not resolve.
    NameNotResolved,
    /// The server sent something that is not a well-formed HTTP response.
    InvalidServerResponse,
    /// The requested header is not present on the response.
    ///
    /// Reached only through a raw query. The ergonomic accessor
    /// [`Request::header`](super::Request::header) reports an absent header as
    /// `Ok(None)`, because an absence is an ordinary answer and not a
    /// transport failure.
    HeaderNotFound,
    /// WinHTTP refused the call because the handle is not in a state that
    /// permits it — most often a second transfer attempted while one is
    /// already outstanding.
    IncorrectHandleState,
    /// This crate refused to submit a transfer because one is already
    /// outstanding on the request.
    ///
    /// Distinct from [`WinHttpError::IncorrectHandleState`], which is
    /// WinHTTP's own refusal. This variant means the crate stopped the call
    /// before it reached WinHTTP, which it does so that the caller gets a
    /// stable, documented error instead of a platform code that depends on
    /// timing.
    ///
    /// It is transient: it is reported while an *abandoned* operation's
    /// completion is still outstanding, and clears when that completion
    /// arrives. The supported recovery is to drop the request.
    OperationInProgress,
    /// Any other WinHTTP or Win32 failure, carrying its raw code.
    Other(u32),
}

impl WinHttpError {
    /// Classify a raw Win32 or WinHTTP error code.
    pub fn from_win32(code: u32) -> Self {
        match code {
            ERROR_WINHTTP_TIMEOUT => WinHttpError::Timeout,
            ERROR_WINHTTP_NAME_NOT_RESOLVED => WinHttpError::NameNotResolved,
            ERROR_WINHTTP_OPERATION_CANCELLED => WinHttpError::Cancelled,
            ERROR_WINHTTP_INCORRECT_HANDLE_STATE => WinHttpError::IncorrectHandleState,
            ERROR_WINHTTP_CANNOT_CONNECT => WinHttpError::CannotConnect,
            ERROR_WINHTTP_CONNECTION_ERROR => WinHttpError::ConnectionError,
            ERROR_WINHTTP_HEADER_NOT_FOUND => WinHttpError::HeaderNotFound,
            ERROR_WINHTTP_INVALID_SERVER_RESPONSE => WinHttpError::InvalidServerResponse,
            ERROR_WINHTTP_SECURE_FAILURE => WinHttpError::SecureFailure,
            ERROR_BUSY_CODE => WinHttpError::OperationInProgress,
            other => WinHttpError::Other(other),
        }
    }

    /// Classify a [`windows::core::Error`] produced by this module.
    ///
    /// The module always builds its errors with [`HRESULT::from_win32`], so the
    /// low sixteen bits carry the WinHTTP code. An error whose facility is not
    /// `FACILITY_WIN32` cannot have come from a WinHTTP status code and is
    /// reported as [`WinHttpError::Other`] with the raw `HRESULT` value
    /// truncated, rather than being mis-classified by accident.
    pub fn from_error(error: &Error) -> Self {
        let hr = error.code().0 as u32;
        // 0x8007_xxxx is FACILITY_WIN32 with the failure bit set.
        if hr & 0xFFFF_0000 == 0x8007_0000 {
            WinHttpError::from_win32(hr & 0xFFFF)
        } else {
            WinHttpError::Other(hr)
        }
    }

    /// The raw code this classification was derived from.
    pub fn code(&self) -> u32 {
        match self {
            WinHttpError::Timeout => ERROR_WINHTTP_TIMEOUT,
            WinHttpError::NameNotResolved => ERROR_WINHTTP_NAME_NOT_RESOLVED,
            WinHttpError::Cancelled => ERROR_WINHTTP_OPERATION_CANCELLED,
            WinHttpError::IncorrectHandleState => ERROR_WINHTTP_INCORRECT_HANDLE_STATE,
            WinHttpError::CannotConnect => ERROR_WINHTTP_CANNOT_CONNECT,
            WinHttpError::ConnectionError => ERROR_WINHTTP_CONNECTION_ERROR,
            WinHttpError::HeaderNotFound => ERROR_WINHTTP_HEADER_NOT_FOUND,
            WinHttpError::InvalidServerResponse => ERROR_WINHTTP_INVALID_SERVER_RESPONSE,
            WinHttpError::SecureFailure => ERROR_WINHTTP_SECURE_FAILURE,
            WinHttpError::Other(code) => *code,
            // Raised by this crate, not by the platform, so it borrows a Win32
            // code that means the same thing and is not one WinHTTP produces.
            WinHttpError::OperationInProgress => ERROR_BUSY_CODE,
        }
    }
}

impl std::fmt::Display for WinHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // These strings exist because the system has none. See the module docs.
        let text = match self {
            WinHttpError::Cancelled => "the operation was cancelled",
            WinHttpError::Timeout => "the operation timed out",
            WinHttpError::CannotConnect => "the server could not be reached",
            WinHttpError::ConnectionError => "the connection was lost",
            WinHttpError::SecureFailure => "the TLS handshake or certificate check failed",
            WinHttpError::NameNotResolved => "the host name did not resolve",
            WinHttpError::InvalidServerResponse => "the server sent a malformed response",
            WinHttpError::HeaderNotFound => "the requested header is not present",
            WinHttpError::IncorrectHandleState => {
                "the request is not in a state that permits this call"
            }
            WinHttpError::OperationInProgress => {
                "another transfer is already outstanding on this request"
            }
            WinHttpError::Other(code) => return write!(f, "WinHTTP error {code}"),
        };
        f.write_str(text)
    }
}

impl std::error::Error for WinHttpError {}

impl From<&Error> for WinHttpError {
    fn from(error: &Error) -> Self {
        WinHttpError::from_error(error)
    }
}

impl From<Error> for WinHttpError {
    fn from(error: Error) -> Self {
        WinHttpError::from_error(&error)
    }
}

impl From<WinHttpError> for Error {
    fn from(error: WinHttpError) -> Self {
        Error::from_hresult(HRESULT::from_win32(error.code()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_code_round_trips_through_classification() {
        // A table rather than one assertion, because the failure mode of a
        // wrong entry is a silently mis-named error in a log, which no other
        // test would catch.
        for (code, expected) in [
            (ERROR_WINHTTP_TIMEOUT, WinHttpError::Timeout),
            (ERROR_WINHTTP_OPERATION_CANCELLED, WinHttpError::Cancelled),
            (ERROR_WINHTTP_CANNOT_CONNECT, WinHttpError::CannotConnect),
            (
                ERROR_WINHTTP_CONNECTION_ERROR,
                WinHttpError::ConnectionError,
            ),
            (ERROR_WINHTTP_SECURE_FAILURE, WinHttpError::SecureFailure),
            (
                ERROR_WINHTTP_NAME_NOT_RESOLVED,
                WinHttpError::NameNotResolved,
            ),
            (ERROR_WINHTTP_HEADER_NOT_FOUND, WinHttpError::HeaderNotFound),
            (
                ERROR_WINHTTP_INVALID_SERVER_RESPONSE,
                WinHttpError::InvalidServerResponse,
            ),
            (
                ERROR_WINHTTP_INCORRECT_HANDLE_STATE,
                WinHttpError::IncorrectHandleState,
            ),
        ] {
            assert_eq!(WinHttpError::from_win32(code), expected);
            assert_eq!(expected.code(), code);
        }
    }

    #[test]
    fn a_winhttp_code_survives_the_trip_through_an_hresult() {
        // This is the regression test for the defect that made the previous
        // implementation abort. The old code built `HRESULT(dwError as i32)`,
        // which for 12002 is 0x00002EE2 — sign bit clear, therefore a
        // *success* HRESULT. Combined with an `assert!(is_err())` on the
        // caller side, every async failure aborted the process from whichever
        // thread ran the callback.
        let error: Error = WinHttpError::Timeout.into();
        assert!(
            error.code().is_err(),
            "a WinHTTP failure must produce a failing HRESULT, got {:#010x}",
            error.code().0
        );
        assert_eq!(WinHttpError::from_error(&error), WinHttpError::Timeout);
    }

    #[test]
    fn an_unknown_code_is_preserved_rather_than_guessed_at() {
        assert_eq!(WinHttpError::from_win32(12345), WinHttpError::Other(12345));
        assert_eq!(WinHttpError::Other(12345).code(), 12345);
    }

    #[test]
    fn the_crates_own_refusal_is_distinguishable_from_the_platforms() {
        // `OperationInProgress` is raised by this crate before the call reaches
        // WinHTTP; `IncorrectHandleState` is WinHTTP's own refusal. Folding
        // them together would leave a caller unable to tell a transient,
        // recoverable state from a permanent one — the same class of mistake
        // as reporting a failure as an empty success.
        let ours: Error = WinHttpError::OperationInProgress.into();
        let theirs: Error = WinHttpError::IncorrectHandleState.into();
        assert_ne!(ours.code(), theirs.code());
        assert_eq!(
            WinHttpError::from_error(&ours),
            WinHttpError::OperationInProgress
        );
        assert_eq!(
            WinHttpError::from_error(&theirs),
            WinHttpError::IncorrectHandleState
        );
    }

    #[test]
    fn every_variant_has_its_own_description() {
        // The system message table has no text for any of these codes, so an
        // empty or duplicated description would silently produce useless logs.
        let all = [
            WinHttpError::Cancelled,
            WinHttpError::Timeout,
            WinHttpError::CannotConnect,
            WinHttpError::ConnectionError,
            WinHttpError::SecureFailure,
            WinHttpError::NameNotResolved,
            WinHttpError::InvalidServerResponse,
            WinHttpError::HeaderNotFound,
            WinHttpError::IncorrectHandleState,
            WinHttpError::OperationInProgress,
        ];
        let mut seen: Vec<String> = Vec::new();
        for variant in all {
            let text = variant.to_string();
            assert!(!text.is_empty(), "{variant:?} has no description");
            assert!(
                !seen.contains(&text),
                "{variant:?} duplicates another description: {text}"
            );
            seen.push(text);
        }
    }
}
