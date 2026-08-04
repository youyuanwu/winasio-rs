// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Receiving a request.

use std::task::Poll;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::HttpServer::{
    HttpReceiveHttpRequest, HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::httpsys::error::check;
use crate::httpsys::request::{Request, RequestId};
use crate::iocp::{IntoInner, OpCode};

use super::QueueHandle;

/// Receive one request into a caller-owned buffer.
///
/// The buffer is owned by this operation for its whole duration, which is what
/// makes it sound to hand HTTP.sys a pointer into it.
pub struct ReceiveRequest {
    queue: QueueHandle,
    request_id: RequestId,
    request: Request,
}

impl ReceiveRequest {
    pub(crate) fn new(queue: QueueHandle, request_id: RequestId, request: Request) -> Self {
        ReceiveRequest {
            queue,
            request_id,
            request,
        }
    }

    /// The identifier recovered from a partially filled buffer.
    ///
    /// After `ERROR_MORE_DATA`, HTTP.sys has written the request header even
    /// though the variable-length tail did not fit -- Phase 0 confirmed this
    /// holds on the asynchronous path. That identifier is what lets a retry
    /// target the same request, and what lets an over-large one be rejected.
    pub(crate) fn recovered_id(&self) -> RequestId {
        self.request.id()
    }
}

unsafe impl OpCode for ReceiveRequest {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.queue.raw())
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let queue = self.queue.raw();
        let id = self.request_id.get();
        let capacity = self.request.capacity() as u32;
        // Derived from `&mut self`, which already lives at its final address.
        let buffer = self.request.write_ptr();

        let code = unsafe {
            HttpReceiveHttpRequest(
                queue,
                id,
                HTTP_RECEIVE_HTTP_REQUEST_FLAGS(0),
                buffer,
                capacity,
                // Must be null for an overlapped call.
                None,
                Some(optr),
            )
        };
        poll_from_code(code, optr)
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.queue.raw(), Some(optr)) }
    }
}

impl IntoInner for ReceiveRequest {
    type Inner = Request;

    fn into_inner(self) -> Request {
        self.request
    }
}

/// Translate the HTTP Server API's direct return code into a poll.
///
/// The API returns a Win32 code rather than setting the thread's last error, so
/// [`win32_result`](crate::iocp::win32_result) does not apply unchanged.
pub(crate) fn poll_from_code(code: u32, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
    use windows::Win32::Foundation::{ERROR_IO_PENDING, NO_ERROR, WIN32_ERROR};

    let err = WIN32_ERROR(code);
    if err == ERROR_IO_PENDING {
        return Poll::Pending;
    }
    if err == NO_ERROR {
        // Completed inline; Windows recorded the byte count in the OVERLAPPED.
        // SAFETY: `optr` is the pointer just handed to the API.
        return Poll::Ready(Ok(unsafe { (*optr).InternalHigh }));
    }
    Poll::Ready(check(code).map(|_| 0))
}
