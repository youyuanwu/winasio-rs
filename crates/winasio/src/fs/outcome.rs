// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Read outcomes.

use windows::core::Result;
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_MORE_DATA, ERROR_NETNAME_DELETED,
    ERROR_PIPE_NOT_CONNECTED,
};

use super::error::win32_code;

/// The non-error outcomes an asynchronous read can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Bytes were transferred. A zero-byte transfer is represented here, not as
    /// end of file.
    Bytes(usize),
    /// The read reached the end of a file.
    Eof,
    /// A stream peer closed its end.
    ClosedPeer,
    /// A message-mode pipe delivered part of a message and left more to read.
    MoreData(usize),
}

pub(crate) fn classify_read(result: &Result<usize>, transferred: usize) -> Option<ReadOutcome> {
    match result {
        Ok(n) => Some(ReadOutcome::Bytes(*n)),
        Err(e) => match win32_code(e) {
            Some(code) if code == ERROR_HANDLE_EOF.0 => Some(ReadOutcome::Eof),
            Some(code)
                if code == ERROR_BROKEN_PIPE.0
                    || code == ERROR_PIPE_NOT_CONNECTED.0
                    || code == ERROR_NETNAME_DELETED.0 =>
            {
                Some(ReadOutcome::ClosedPeer)
            }
            Some(code) if code == ERROR_MORE_DATA.0 => Some(ReadOutcome::MoreData(transferred)),
            _ => None,
        },
    }
}
