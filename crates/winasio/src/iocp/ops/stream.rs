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

use windows::core::Result;
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::System::Pipes::ConnectNamedPipe;
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::fs::outcome::{classify_read, ReadOutcome};
use crate::iocp::buf::{IoBuf, IoBufMut};
use crate::iocp::handle::Handle;
use crate::iocp::op::{win32_result, IntoInner, OpCode};

use super::sys::{checked_u32_len, set_offset, ReadFile, WriteFile};

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

        let buffer = self.buffer.as_uninit();
        let len = match checked_u32_len(buffer.len()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let ptr = buffer.as_mut_ptr().cast::<u8>();

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

/// Read from a shared stream handle.
pub struct ReadHandle<B: IoBufMut> {
    inner: ReadHandleAt<B>,
}

impl<B: IoBufMut> ReadHandle<B> {
    /// Read into `buffer`.
    pub fn new(handle: Handle, buffer: B) -> Self {
        ReadHandle {
            inner: ReadHandleAt::new(handle, 0, buffer),
        }
    }

    /// Convert the low-level byte-count result into the safe read outcome.
    pub(crate) fn finish(self, result: Result<usize>) -> (Result<ReadOutcome>, B) {
        self.inner.finish(result)
    }
}

impl<B: IoBufMut> IntoInner for ReadHandle<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.inner.into_inner()
    }
}

unsafe impl<B: IoBufMut + Send> OpCode for ReadHandle<B> {
    fn handle(&self) -> Option<HANDLE> {
        self.inner.handle()
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // Pipes ignore the offset fields, but they must still be initialised.
        set_offset(optr, 0);

        let buffer = self.inner.buffer.as_uninit();
        let len = match checked_u32_len(buffer.len()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let ptr = buffer.as_mut_ptr().cast::<u8>();

        // SAFETY: `ptr` and `len` describe writable storage owned by this
        // operation. The operation allocation is retained until completion.
        let ok = unsafe {
            ReadFile(
                self.inner.handle.raw(),
                ptr,
                len,
                std::ptr::null_mut(),
                optr,
            )
        };
        // No Windows call may occur between `ReadFile` and `win32_result`.
        let result = unsafe { win32_result(ok != 0, optr) };
        if let Poll::Ready(ref ready) = result {
            let transferred = inline_transferred_count(ready, optr);
            self.inner.record_completion(ready, transferred);
        }
        result
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        // SAFETY: `optr` is the same overlapped pointer passed to `operate`;
        // the handle is kept alive by this operation's `Handle` clone.
        unsafe { CancelIoEx(self.inner.handle.raw(), Some(optr)) }
    }

    unsafe fn on_complete_with(&mut self, result: &Result<usize>, transferred: usize) {
        self.inner.record_completion(result, transferred);
    }
}

/// Write to a shared stream handle.
pub struct WriteHandle<B: IoBuf> {
    inner: WriteHandleAt<B>,
}

impl<B: IoBuf> WriteHandle<B> {
    /// Write `buffer`.
    pub fn new(handle: Handle, buffer: B) -> Self {
        WriteHandle {
            inner: WriteHandleAt::new(handle, 0, buffer),
        }
    }
}

impl<B: IoBuf> IntoInner for WriteHandle<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.inner.into_inner()
    }
}

unsafe impl<B: IoBuf + Send> OpCode for WriteHandle<B> {
    fn handle(&self) -> Option<HANDLE> {
        self.inner.handle()
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // Pipes ignore the offset fields, but they must still be initialised.
        set_offset(optr, 0);

        let len = match checked_u32_len(self.inner.buffer.bytes_init()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let ptr = self.inner.buffer.stable_ptr();

        // SAFETY: `ptr` and `len` describe initialised bytes owned by this
        // operation. The operation allocation is retained until completion.
        let ok = unsafe {
            WriteFile(
                self.inner.handle.raw(),
                ptr,
                len,
                std::ptr::null_mut(),
                optr,
            )
        };
        // No Windows call may occur between `WriteFile` and `win32_result`.
        unsafe { win32_result(ok != 0, optr) }
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        // SAFETY: `optr` is the same overlapped pointer passed to `operate`;
        // the handle is kept alive by this operation's `Handle` clone.
        unsafe { CancelIoEx(self.inner.handle.raw(), Some(optr)) }
    }
}

/// Connect a server-side named pipe instance.
pub struct ConnectPipe {
    handle: Handle,
}

impl ConnectPipe {
    /// Connect `handle` to a client.
    pub fn new(handle: Handle) -> Self {
        ConnectPipe { handle }
    }
}

impl IntoInner for ConnectPipe {
    type Inner = ();

    fn into_inner(self) {}
}

unsafe impl OpCode for ConnectPipe {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.handle.raw())
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // Pipes ignore offsets, but the driver reuses the common OVERLAPPED
        // allocation and the fields must not contain garbage.
        set_offset(optr, 0);

        // SAFETY: `optr` is the operation's stable OVERLAPPED allocation and
        // `handle` is a live server-side named-pipe handle.
        let res = unsafe { ConnectNamedPipe(self.handle.raw(), Some(optr)) };
        match res {
            // No Windows call may occur between `ConnectNamedPipe` and
            // `win32_result`.
            Ok(()) => unsafe { win32_result(true, optr) },
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                // A client that connected before we asked is still success,
                // and no completion packet will be posted for this condition.
                Poll::Ready(Ok(0))
            }
            Err(_) => unsafe { win32_result(false, optr) },
        }
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
    use windows::core::Error;
    use windows::Win32::Foundation::ERROR_MORE_DATA;

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
