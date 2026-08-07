// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The two WinHTTP constants the `windows` 0.62.2 bindings genuinely do not
//! generate.
//!
//! Everything else this module used to define — the nine `ERROR_WINHTTP_*`
//! codes, `ERROR_BUSY`, the four `SECURITY_FLAG_IGNORE_*` bits and the
//! notification mask — **is** exported by the bindings, under the same names
//! and with identical values, and is now imported from there rather than
//! restated here. The `Win32_Networking_WinHttp` feature that supplies them was
//! already enabled, so the duplication bought nothing while carrying a real
//! risk: by this module's own reckoning, a wrong constant here is a silent hang
//! or a leak rather than a compile error.
//!
//! The notification mask is the cautionary case. An earlier draft computed it
//! as a union of the completion and handle flags and got it wrong twice over —
//! it omitted `WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE` (`0x0040_0000`),
//! which would have hung every send, and mis-stated
//! `WINHTTP_CALLBACK_FLAG_HANDLES` as `0x3` instead of `0x600`, which would
//! have suppressed `HANDLE_CLOSING` and leaked every context. The correct value
//! is every bit set, and the bindings have carried it under the name
//! `WINHTTP_CALLBACK_FLAG_ALL_NOTIFICATIONS` all along. **Do not derive it.**

/// The `WINHTTP_STATUS_CALLBACK` value `WinHttpSetStatusCallback` returns to
/// report failure.
///
/// It is `(WINHTTP_STATUS_CALLBACK)(-1L)`, i.e. all bits set, which the
/// bindings cannot express as a constant of that function-pointer type.
pub(crate) const WINHTTP_INVALID_STATUS_CALLBACK: usize = usize::MAX;

/// `WINHTTP_NO_HEADER_INDEX` — passed to `WinHttpQueryHeaders` when the caller
/// wants the first (or only) instance of a header rather than iterating.
///
/// A null-pointer macro in `winhttp.h`, so there is no generated equivalent.
pub(crate) const WINHTTP_NO_HEADER_INDEX: *mut u32 = std::ptr::null_mut();
