// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Setup error classification for file and pipe builders.
//!
//! The categories are intentionally small and matchable; callers should not
//! need to decode HRESULT values for the expected setup failures.

use windows::core::{Error, HRESULT};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILENAME_EXCED_RANGE, ERROR_FILE_NOT_FOUND, ERROR_INVALID_NAME,
    ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY,
};

use crate::iocp::RegistrationError;

/// A setup failure reported by an open, create, or connect operation.
#[derive(Debug)]
pub enum SetupError {
    /// The named object was not found.
    NotFound,
    /// Every instance of the target pipe is busy.
    Busy,
    /// Access was denied by the object or the requested access mode.
    AccessDenied,
    /// The supplied name is not valid for the operation.
    InvalidName,
    /// The handle is already registered with a completion mechanism.
    AlreadyRegistered,
    /// Any other Win32 failure.
    Win32(Error),
}

impl SetupError {
    pub(crate) fn from_windows(err: Error) -> Self {
        match win32_code(&err) {
            Some(code) if code == ERROR_FILE_NOT_FOUND.0 || code == ERROR_PATH_NOT_FOUND.0 => {
                SetupError::NotFound
            }
            Some(code) if code == ERROR_PIPE_BUSY.0 => SetupError::Busy,
            Some(code) if code == ERROR_ACCESS_DENIED.0 => SetupError::AccessDenied,
            Some(code) if code == ERROR_INVALID_NAME.0 || code == ERROR_FILENAME_EXCED_RANGE.0 => {
                SetupError::InvalidName
            }
            _ => SetupError::Win32(err),
        }
    }
}

impl From<RegistrationError> for SetupError {
    fn from(value: RegistrationError) -> Self {
        match value {
            RegistrationError::AlreadyRegistered(_) => SetupError::AlreadyRegistered,
            RegistrationError::Os(e) => SetupError::from_windows(e),
        }
    }
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::NotFound => write!(f, "object was not found"),
            SetupError::Busy => write!(f, "all instances are busy"),
            SetupError::AccessDenied => write!(f, "access denied"),
            SetupError::InvalidName => write!(f, "invalid name"),
            SetupError::AlreadyRegistered => {
                write!(
                    f,
                    "handle is already registered with a completion mechanism"
                )
            }
            SetupError::Win32(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SetupError {}

pub(crate) fn win32_code(err: &Error) -> Option<u32> {
    let HRESULT(raw) = err.code();
    if (raw as u32) >> 16 == 0x8007 {
        Some((raw as u32) & 0xFFFF)
    } else {
        None
    }
}
