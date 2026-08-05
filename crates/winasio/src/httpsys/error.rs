// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The HTTP Server API's error convention.
//!
//! Every `Http*` function **returns** a Win32 error code directly rather than
//! setting the thread's last error, which is why this module exists instead of
//! the usual [`windows::core::Result`] plumbing.

use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::{NO_ERROR, WIN32_ERROR};

/// Convert a Win32 code returned by an HTTP Server API function into a result.
pub(crate) fn check(code: u32) -> Result<()> {
    let err = WIN32_ERROR(code);
    if err == NO_ERROR {
        Ok(())
    } else {
        Err(Error::from_hresult(err.to_hresult()))
    }
}

/// The Win32 code carried by an error, if it is a Win32-derived one.
///
/// Used to recognise the API's expected outcomes -- `ERROR_MORE_DATA` on an
/// undersized receive, `ERROR_HANDLE_EOF` at the end of a body -- without
/// string matching.
// Consumed by the receive retry path and the body reader.
pub(crate) fn win32_code(err: &Error) -> Option<u32> {
    let HRESULT(raw) = err.code();
    // Win32 codes are mapped into HRESULT as 0x8007xxxx.
    if (raw as u32) >> 16 == 0x8007 {
        Some((raw as u32) & 0xFFFF)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{ERROR_HANDLE_EOF, ERROR_MORE_DATA};

    #[test]
    fn no_error_is_ok() {
        assert!(check(0).is_ok());
    }

    #[test]
    fn nonzero_code_round_trips() {
        let err = check(ERROR_MORE_DATA.0).unwrap_err();
        assert_eq!(win32_code(&err), Some(ERROR_MORE_DATA.0));

        let err = check(ERROR_HANDLE_EOF.0).unwrap_err();
        assert_eq!(win32_code(&err), Some(ERROR_HANDLE_EOF.0));
    }
}
