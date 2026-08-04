// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Raw Win32 bindings for the read and write paths.
//!
//! The `windows` crate's `ReadFile` takes `Option<&mut [u8]>`, which forces a
//! caller filling spare capacity to construct a Rust slice over uninitialised
//! memory. These signatures take a pointer and a length instead, so a buffer's
//! uninitialised tail is handed to Windows without ever becoming a `&mut [u8]`.
//! This is what compio does on its IOCP driver as well.

use windows::core::{Error, Result};
use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, HANDLE};
use windows::Win32::System::IO::OVERLAPPED;

#[link(name = "kernel32")]
extern "system" {
    pub(crate) fn ReadFile(
        hfile: HANDLE,
        buffer: *mut u8,
        bytes_to_read: u32,
        bytes_read: *mut u32,
        overlapped: *mut OVERLAPPED,
    ) -> i32;

    pub(crate) fn WriteFile(
        hfile: HANDLE,
        buffer: *const u8,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *mut OVERLAPPED,
    ) -> i32;
}

/// Reject a transfer length Windows cannot express.
///
/// Silently truncating to `u32` would read or write less than the caller asked
/// for and report that short count as success.
pub(crate) fn checked_u32_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| Error::from_hresult(ERROR_INVALID_PARAMETER.to_hresult()))
}

/// Set the absolute offset an overlapped transfer starts at.
pub(crate) fn set_offset(optr: *mut OVERLAPPED, offset: u64) {
    // SAFETY: `optr` is the `OVERLAPPED` embedded in this operation's stable
    // allocation, supplied by the driver before the operation starts.
    unsafe {
        (*optr).Anonymous.Anonymous.Offset = offset as u32;
        (*optr).Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_length_rejects_values_windows_cannot_express() {
        let err = checked_u32_len(u32::MAX as usize + 1).unwrap_err();
        assert_eq!(err.code(), ERROR_INVALID_PARAMETER.to_hresult());
    }

    #[test]
    fn checked_length_accepts_the_platform_maximum() {
        assert_eq!(checked_u32_len(u32::MAX as usize).unwrap(), u32::MAX);
    }
}
