// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! File read and write operations.
//!
//! These double as worked examples of the byte-buffer shape: the operation owns
//! its buffer, hands it back on completion, and derives every pointer it gives
//! Windows from `&mut self`.

use std::task::Poll;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::iocp::buf::{IoBuf, IoBufMut};
use crate::iocp::op::{win32_result, IntoInner, OpCode};

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
    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // Offsets live in the OVERLAPPED the caller gave us, which is inside
        // this operation's own allocation.
        unsafe {
            (*optr).Anonymous.Anonymous.Offset = self.offset as u32;
            (*optr).Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        let total = self.buffer.bytes_total();
        let ptr = self.buffer.stable_mut_ptr();
        // SAFETY: `ptr` addresses `total` writable bytes owned by this
        // operation, which outlives the call because the allocation is pinned
        // by the driver's reference count.
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, total) };

        let ok = unsafe { ReadFile(self.handle.0, Some(slice), None, Some(optr)) }.is_ok();
        win32_result(ok, 0)
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.handle.0, Some(optr)) }
    }

    unsafe fn on_complete(&mut self, result: &Result<usize>) {
        if let Ok(transferred) = result {
            // SAFETY: Windows reported writing this many bytes into the buffer.
            unsafe { self.buffer.set_init(*transferred) };
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
    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        unsafe {
            (*optr).Anonymous.Anonymous.Offset = self.offset as u32;
            (*optr).Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        let len = self.buffer.bytes_init();
        let ptr = self.buffer.stable_ptr();
        // SAFETY: `ptr` addresses `len` initialised bytes owned by this
        // operation, kept alive by the driver's reference count.
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };

        let ok = unsafe { WriteFile(self.handle.0, Some(slice), None, Some(optr)) }.is_ok();
        win32_result(ok, 0)
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.handle.0, Some(optr)) }
    }
}
