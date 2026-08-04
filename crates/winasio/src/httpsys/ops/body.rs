// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Reading a request's entity body.

use std::task::Poll;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::HttpServer::HttpReceiveRequestEntityBody;
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::httpsys::request::RequestId;
use crate::iocp::ops::sys::checked_u32_len;
use crate::iocp::{IntoInner, IoBufMut, OpCode};

use super::receive::poll_from_code;
use super::QueueHandle;

/// Read part of a request's body into a caller-owned buffer.
pub struct ReceiveBody<B: IoBufMut> {
    queue: QueueHandle,
    request_id: RequestId,
    buffer: B,
}

impl<B: IoBufMut> ReceiveBody<B> {
    pub(crate) fn new(queue: QueueHandle, request_id: RequestId, buffer: B) -> Self {
        ReceiveBody {
            queue,
            request_id,
            buffer,
        }
    }
}

unsafe impl<B: IoBufMut + Send> OpCode for ReceiveBody<B> {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.queue.raw())
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let queue = self.queue.raw();
        let id = self.request_id.get();
        // Derived from `&mut self`, at the operation's final address. Pointer
        // and length come from one slice, and the slice is `MaybeUninit`, so
        // the buffer's uninitialised capacity never becomes a `&mut [u8]`.
        let buffer = self.buffer.as_uninit();
        let len = match checked_u32_len(buffer.len()) {
            Ok(len) => len,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let ptr = buffer.as_mut_ptr() as *mut core::ffi::c_void;

        let code =
            unsafe { HttpReceiveRequestEntityBody(queue, id, 0, ptr, len, None, Some(optr)) };
        poll_from_code(code, optr)
    }

    unsafe fn on_complete(&mut self, result: &Result<usize>) {
        // Record how much Windows actually wrote, so the buffer comes back with
        // a correct length rather than its original capacity.
        if let Ok(transferred) = result {
            let capacity = self.buffer.bytes_total();
            // Deliberately not clamped. `set_init`'s contract is that the first
            // `len` bytes were *genuinely written*, and for a `Vec<u8>` it is
            // `set_len` over uninitialised capacity. Clamping an over-report to
            // the capacity would satisfy the bounds check while publishing
            // uninitialised heap bytes through a safe `&[u8]`. An over-report
            // means something is badly wrong, so report nothing instead.
            debug_assert!(
                *transferred <= capacity,
                "completion reported {transferred} bytes for a {capacity}-byte buffer"
            );
            if *transferred <= capacity {
                // SAFETY: Windows initialised exactly this many bytes.
                unsafe { self.buffer.set_init(*transferred) };
            }
        }
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.queue.raw(), Some(optr)) }
    }
}

impl<B: IoBufMut> IntoInner for ReceiveBody<B> {
    type Inner = B;

    fn into_inner(self) -> B {
        self.buffer
    }
}
