// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! WinHTTP constants that the `windows` 0.62.2 bindings do not generate.
//!
//! Everything here is a value the crate needs and the bindings do not export.
//! Each one records where it came from, because a wrong constant in this file
//! is a silent hang or a leak rather than a compile error — a lesson learned
//! the expensive way, see the note on the notification mask below.

/// Every notification, without exception.
///
/// Not a computed union of the completion and handle flags. Computing that
/// union is exactly where an earlier draft of this module went wrong: the value
/// it derived omitted `WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE`
/// (`0x0040_0000`), which would have hung every single send, and mis-stated
/// `WINHTTP_CALLBACK_FLAG_HANDLES` as `0x3` instead of `0x600`, which would
/// have suppressed `HANDLE_CLOSING` and leaked every context.
///
/// `0xFFFF_FFFF` is the mask under which every behavioural fact this module
/// relies on was actually measured, so it is the only mask the design has
/// evidence for. Notifications the module does not care about cost one ignored
/// `match` arm, which is a great deal cheaper than being wrong.
pub(crate) const WINHTTP_CALLBACK_FLAG_ALL_NOTIFICATIONS: u32 = 0xFFFF_FFFF;

/// The `WINHTTP_STATUS_CALLBACK` value `WinHttpSetStatusCallback` returns to
/// report failure. It is `(WINHTTP_STATUS_CALLBACK)(-1L)`, i.e. all bits set,
/// which is not representable as a null check.
pub(crate) const WINHTTP_INVALID_STATUS_CALLBACK: usize = usize::MAX;

/// `WINHTTP_NO_HEADER_INDEX` — passed to `WinHttpQueryHeaders` when the caller
/// wants the first (or only) instance of a header rather than iterating.
pub(crate) const WINHTTP_NO_HEADER_INDEX: *mut u32 = std::ptr::null_mut();

// ---------------------------------------------------------------- error codes
//
// These live in `winhttp.h`, not in `winerror.h`, and the bindings do not
// generate them. They are the numbers the callback actually delivers in
// `WINHTTP_ASYNC_RESULT::dwError`; every one below was observed by a probe or
// is adjacent to one that was.

/// `ERROR_WINHTTP_TIMEOUT`
pub const ERROR_WINHTTP_TIMEOUT: u32 = 12002;
/// `ERROR_WINHTTP_NAME_NOT_RESOLVED`
pub const ERROR_WINHTTP_NAME_NOT_RESOLVED: u32 = 12007;
/// `ERROR_WINHTTP_OPERATION_CANCELLED`
pub const ERROR_WINHTTP_OPERATION_CANCELLED: u32 = 12017;
/// `ERROR_WINHTTP_INCORRECT_HANDLE_STATE`
pub const ERROR_WINHTTP_INCORRECT_HANDLE_STATE: u32 = 12019;
/// `ERROR_WINHTTP_CANNOT_CONNECT`
pub const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;
/// `ERROR_WINHTTP_CONNECTION_ERROR`
pub const ERROR_WINHTTP_CONNECTION_ERROR: u32 = 12030;
/// `ERROR_WINHTTP_HEADER_NOT_FOUND`
pub const ERROR_WINHTTP_HEADER_NOT_FOUND: u32 = 12150;
/// `ERROR_WINHTTP_INVALID_SERVER_RESPONSE`
pub const ERROR_WINHTTP_INVALID_SERVER_RESPONSE: u32 = 12152;
/// `ERROR_WINHTTP_SECURE_FAILURE`
pub const ERROR_WINHTTP_SECURE_FAILURE: u32 = 12175;

/// `ERROR_BUSY` — "the requested resource is in use".
///
/// Not a WinHTTP code. This module raises it for its own refusal to submit a
/// second transfer while one is outstanding, and it is deliberately *not*
/// `ERROR_WINHTTP_INCORRECT_HANDLE_STATE`: a caller must be able to tell the
/// crate's own pre-flight refusal — which is transient and recoverable by
/// dropping the request — from the platform's, which is not.
pub const ERROR_BUSY: u32 = 170;

// --------------------------------------------------- security relaxation bits
//
// The `SECURITY_FLAG_IGNORE_*` values for `WINHTTP_OPTION_SECURITY_FLAGS`.

/// Accept a certificate signed by an authority the machine does not trust.
pub(crate) const SECURITY_FLAG_IGNORE_UNKNOWN_CA: u32 = 0x0100;
/// Accept a certificate whose extended key usage does not permit server auth.
pub(crate) const SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE: u32 = 0x0200;
/// Accept a certificate whose subject does not match the host requested.
pub(crate) const SECURITY_FLAG_IGNORE_CERT_CN_INVALID: u32 = 0x1000;
/// Accept a certificate that has expired or is not yet valid.
pub(crate) const SECURITY_FLAG_IGNORE_CERT_DATE_INVALID: u32 = 0x2000;
