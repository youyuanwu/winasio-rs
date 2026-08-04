// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! File read and write operations.
//!
//! These double as worked examples of the byte-buffer shape: the operation owns
//! its buffer, hands it back on completion, and derives every pointer it gives
//! Windows from `&mut self`. Reads pass a pointer and a length straight to
//! Windows rather than building a `&mut [u8]`, because the writable region
//! includes the buffer's uninitialised spare capacity.

use std::task::Poll;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::iocp::buf::{IoBuf, IoBufMut};
use crate::iocp::op::{win32_result, IntoInner, OpCode};

use super::sys::{checked_u32_len, set_offset, ReadFile, WriteFile};

/// A handle that may be moved between threads.
///
/// `HANDLE` is a raw pointer and therefore not [`Send`], but a kernel handle is
/// perfectly usable from any thread. Operations wrap theirs so they satisfy the
/// thread-pool backend's `T: Send` bound.
#[derive(Debug, Clone, Copy)]
pub struct SendHandle(pub HANDLE);

// SAFETY: kernel handles are process-wide and thread-agnostic.
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

impl From<HANDLE> for SendHandle {
    fn from(h: HANDLE) -> Self {
        SendHandle(h)
    }
}

/// Read from a file at an absolute offset.
pub struct ReadAt<B: IoBufMut> {
    handle: SendHandle,
    offset: u64,
    buffer: B,
}

impl<B: IoBufMut> ReadAt<B> {
    /// Read into `buffer` starting at `offset`.
    pub fn new(handle: impl Into<SendHandle>, offset: u64, buffer: B) -> Self {
        ReadAt {
            handle: handle.into(),
            offset,
            buffer,
        }
    }
}

impl<B: IoBufMut> IntoInner for ReadAt<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.buffer
    }
}

unsafe impl<B: IoBufMut + Send> OpCode for ReadAt<B> {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.handle.0)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // Offsets live in the OVERLAPPED the caller gave us, which is inside
        // this operation's own allocation.
        set_offset(optr, self.offset);

        let buffer = self.buffer.as_uninit();
        let len = match checked_u32_len(buffer.len()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        // The pointer and length come from one slice, and the slice is
        // `MaybeUninit`, so no reference to uninitialised memory is created.
        let ptr = buffer.as_mut_ptr().cast::<u8>();

        let ok = unsafe { ReadFile(self.handle.0, ptr, len, std::ptr::null_mut(), optr) } != 0;
        unsafe { win32_result(ok, optr) }
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.handle.0, Some(optr)) }
    }

    unsafe fn on_complete(&mut self, result: &Result<usize>) {
        if let Ok(transferred) = result {
            // Clamp rather than assert: this runs inside the completion path,
            // where a panic would unwind through a callback.
            let n = (*transferred).min(self.buffer.bytes_total());
            // SAFETY: Windows reported writing this many bytes into the buffer.
            unsafe { self.buffer.set_init(n) };
        }
    }
}

/// Write to a file at an absolute offset.
pub struct WriteAt<B: IoBuf> {
    handle: SendHandle,
    offset: u64,
    buffer: B,
}

impl<B: IoBuf> WriteAt<B> {
    /// Write `buffer` starting at `offset`.
    pub fn new(handle: impl Into<SendHandle>, offset: u64, buffer: B) -> Self {
        WriteAt {
            handle: handle.into(),
            offset,
            buffer,
        }
    }
}

impl<B: IoBuf> IntoInner for WriteAt<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.buffer
    }
}

unsafe impl<B: IoBuf + Send> OpCode for WriteAt<B> {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.handle.0)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        set_offset(optr, self.offset);

        let len = match checked_u32_len(self.buffer.bytes_init()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        // Only initialised bytes are sent, so this side never had the
        // uninitialised-slice problem; it uses the raw binding for symmetry and
        // to get the same length check.
        let ptr = self.buffer.stable_ptr();

        let ok = unsafe { WriteFile(self.handle.0, ptr, len, std::ptr::null_mut(), optr) } != 0;
        unsafe { win32_result(ok, optr) }
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.handle.0, Some(optr)) }
    }
}
