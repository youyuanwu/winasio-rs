// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Stream-style operations over a shared handle.
//!
//! These are the operations used by the safe file and pipe layers. They own a
//! [`Handle`](crate::iocp::Handle) clone so late cancellation cannot target a
//! closed handle, and they call raw Win32 bindings so reads never construct a
//! Rust slice over uninitialised spare capacity.

use std::task::Poll;

use windows::core::{Error, Result};
use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, HANDLE};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::fs::outcome::{classify_read, ReadOutcome};
use crate::iocp::buf::{IoBuf, IoBufMut};
use crate::iocp::handle::Handle;
use crate::iocp::op::{win32_result, IntoInner, OpCode};

#[link(name = "kernel32")]
extern "system" {
    fn ReadFile(
        hfile: HANDLE,
        buffer: *mut u8,
        bytes_to_read: u32,
        bytes_read: *mut u32,
        overlapped: *mut OVERLAPPED,
    ) -> i32;

    fn WriteFile(
        hfile: HANDLE,
        buffer: *const u8,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *mut OVERLAPPED,
    ) -> i32;
}

fn checked_u32_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| Error::from_hresult(ERROR_INVALID_PARAMETER.to_hresult()))
}

fn set_offset(optr: *mut OVERLAPPED, offset: u64) {
    // SAFETY: `optr` is the `OVERLAPPED` embedded in this operation's stable
    // allocation, supplied by the driver before the operation starts.
    unsafe {
        (*optr).Anonymous.Anonymous.Offset = offset as u32;
        (*optr).Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    }
}

fn inline_transferred_count(result: &Result<usize>, optr: *mut OVERLAPPED) -> usize {
    match result {
        Ok(transferred) => *transferred,
        Err(err) if err.code() == ERROR_MORE_DATA.to_hresult() => {
            // SAFETY: `optr` is the same `OVERLAPPED` passed to `ReadFile`.
            // Reading the field is not a Windows call, so it does not disturb
            // the thread-local last-error value already consumed by
            // `win32_result`.
            unsafe { (*optr).InternalHigh }
        }
        Err(_) => 0,
    }
}

/// Read from a shared handle at an absolute offset.
pub struct ReadHandleAt<B: IoBufMut> {
    handle: Handle,
    offset: u64,
    buffer: B,
    outcome: Option<ReadOutcome>,
}

impl<B: IoBufMut> ReadHandleAt<B> {
    /// Read into `buffer` starting at `offset`.
    pub fn new(handle: Handle, offset: u64, buffer: B) -> Self {
        ReadHandleAt {
            handle,
            offset,
            buffer,
            outcome: None,
        }
    }

    fn record_completion(&mut self, result: &Result<usize>, transferred: usize) {
        self.outcome = classify_read(result, transferred);
        if result.is_ok() || matches!(self.outcome, Some(ReadOutcome::MoreData(_))) {
            let n = transferred.min(self.buffer.bytes_total());
            // SAFETY: Windows reported initialising `transferred` bytes. The
            // value is clamped to this buffer's capacity before publication.
            unsafe { self.buffer.set_init(n) };
        }
    }

    /// Convert the low-level byte-count result into the safe read outcome.
    pub(crate) fn finish(self, result: Result<usize>) -> (Result<ReadOutcome>, B) {
        let outcome = match (result, self.outcome) {
            (_, Some(outcome)) => Ok(outcome),
            (Ok(n), None) => Ok(ReadOutcome::Bytes(n)),
            (Err(e), None) => Err(e),
        };
        (outcome, self.buffer)
    }
}

impl<B: IoBufMut> IntoInner for ReadHandleAt<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.buffer
    }
}

unsafe impl<B: IoBufMut + Send> OpCode for ReadHandleAt<B> {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.handle.raw())
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        set_offset(optr, self.offset);

        let len = match checked_u32_len(self.buffer.bytes_total()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let ptr = self.buffer.stable_mut_ptr();

        // SAFETY: `ptr` and `len` describe writable storage owned by this
        // operation. The operation allocation is retained until completion.
        let ok = unsafe { ReadFile(self.handle.raw(), ptr, len, std::ptr::null_mut(), optr) };
        // No Windows call may occur between `ReadFile` and `win32_result`.
        let result = unsafe { win32_result(ok != 0, optr) };
        if let Poll::Ready(ref ready) = result {
            let transferred = inline_transferred_count(ready, optr);
            self.record_completion(ready, transferred);
        }
        result
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        // SAFETY: `optr` is the same overlapped pointer passed to `operate`;
        // the handle is kept alive by this operation's `Handle` clone.
        unsafe { CancelIoEx(self.handle.raw(), Some(optr)) }
    }

    unsafe fn on_complete_with(&mut self, result: &Result<usize>, transferred: usize) {
        self.record_completion(result, transferred);
    }
}

/// Write to a shared handle at an absolute offset.
pub struct WriteHandleAt<B: IoBuf> {
    handle: Handle,
    offset: u64,
    buffer: B,
}

impl<B: IoBuf> WriteHandleAt<B> {
    /// Write `buffer` starting at `offset`.
    pub fn new(handle: Handle, offset: u64, buffer: B) -> Self {
        WriteHandleAt {
            handle,
            offset,
            buffer,
        }
    }
}

impl<B: IoBuf> IntoInner for WriteHandleAt<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.buffer
    }
}

unsafe impl<B: IoBuf + Send> OpCode for WriteHandleAt<B> {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.handle.raw())
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        set_offset(optr, self.offset);

        let len = match checked_u32_len(self.buffer.bytes_init()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let ptr = self.buffer.stable_ptr();

        // SAFETY: `ptr` and `len` describe initialised bytes owned by this
        // operation. The operation allocation is retained until completion.
        let ok = unsafe { WriteFile(self.handle.raw(), ptr, len, std::ptr::null_mut(), optr) };
        // No Windows call may occur between `WriteFile` and `win32_result`.
        unsafe { win32_result(ok != 0, optr) }
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        // SAFETY: `optr` is the same overlapped pointer passed to `operate`;
        // the handle is kept alive by this operation's `Handle` clone.
        unsafe { CancelIoEx(self.handle.raw(), Some(optr)) }
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

    #[test]
    fn inline_more_data_uses_internal_high_for_count_and_init() {
        let mut overlapped = OVERLAPPED {
            InternalHigh: 7,
            ..OVERLAPPED::default()
        };
        let err = Error::from_hresult(ERROR_MORE_DATA.to_hresult());
        let result = Err(err);

        let transferred = inline_transferred_count(&result, &mut overlapped);
        assert_eq!(transferred, 7);

        // SAFETY: an invalid handle is owned by nobody and will not be closed.
        // This test never starts I/O with it; it only exercises completion
        // bookkeeping on the operation value.
        let handle = unsafe { Handle::from_raw(HANDLE::default()) };
        let mut op = ReadHandleAt::new(handle, 0, Vec::with_capacity(16));
        op.record_completion(&result, transferred);
        let (outcome, buffer) = op.finish(result);

        assert_eq!(outcome.unwrap(), ReadOutcome::MoreData(7));
        assert_eq!(buffer.len(), 7);
    }
}
