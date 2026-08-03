// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Sending a reply, and streaming further body data.

use std::task::Poll;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::HttpServer::{
    HttpDataChunkFromMemory, HttpSendHttpResponse, HttpSendResponseEntityBody, HTTP_DATA_CHUNK,
    HTTP_SEND_RESPONSE_FLAG_MORE_DATA,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::httpsys::request::RequestId;
use crate::httpsys::response::Response;
use crate::iocp::{IntoInner, IoBuf, OpCode};

use super::receive::poll_from_code;
use super::QueueHandle;

/// Send a complete reply.
///
/// Owns the [`Response`] for the operation's duration and hands it back
/// afterwards -- on failure as well as success, so a failed send does not
/// consume the reply it was trying to send.
pub struct SendResponse {
    queue: QueueHandle,
    request_id: RequestId,
    flags: u32,
    response: Response,
}

// SAFETY: `Response` is already `Send`; the handle is thread-agnostic.
unsafe impl Send for SendResponse {}

impl SendResponse {
    pub(crate) fn new(
        queue: HANDLE,
        request_id: RequestId,
        response: Response,
        more_data: bool,
    ) -> Self {
        SendResponse {
            queue: QueueHandle(queue),
            request_id,
            flags: if more_data {
                HTTP_SEND_RESPONSE_FLAG_MORE_DATA
            } else {
                0
            },
            response,
        }
    }
}

unsafe impl OpCode for SendResponse {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.queue.0)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // Every pointer inside the reply is derived here, from `&mut self`,
        // which already sits at the operation's final address.
        let raw = match unsafe { self.response.build() } {
            Ok(raw) => raw,
            // A value too long for the API's length fields. Reported rather
            // than truncated, so output cannot be silently corrupted.
            Err(e) => return Poll::Ready(Err(e)),
        };
        let code = unsafe {
            HttpSendHttpResponse(
                self.queue.0,
                self.request_id.get(),
                self.flags,
                raw,
                None,
                None,
                None,
                None,
                Some(optr),
                None,
            )
        };
        poll_from_code(code, optr)
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.queue.0, Some(optr)) }
    }
}

impl IntoInner for SendResponse {
    type Inner = Response;

    fn into_inner(mut self) -> Response {
        // The operation's storage is moved out on completion, so the pointers
        // recorded during `build` no longer describe anything. Clear them before
        // the reply reaches the caller.
        self.response.invalidate();
        self.response
    }
}

/// Send a further piece of a reply's body.
///
/// Owns the buffer for the operation's duration and returns it with the outcome.
pub struct SendBody<B: IoBuf> {
    queue: QueueHandle,
    request_id: RequestId,
    flags: u32,
    buffer: B,
    /// The descriptor HTTP.sys reads. It must be owned by the operation for the
    /// call's duration -- a temporary would dangle the moment `operate` returned.
    chunk: HTTP_DATA_CHUNK,
}

// SAFETY: the operation owns both the buffer and the descriptor pointing into
// it. `HTTP_DATA_CHUNK` is only `!Send` because it contains a raw pointer.
unsafe impl<B: IoBuf + Send> Send for SendBody<B> {}

impl<B: IoBuf> SendBody<B> {
    pub(crate) fn new(queue: HANDLE, request_id: RequestId, buffer: B, last: bool) -> Self {
        SendBody {
            queue: QueueHandle(queue),
            request_id,
            flags: if last {
                0
            } else {
                HTTP_SEND_RESPONSE_FLAG_MORE_DATA
            },
            buffer,
            chunk: HTTP_DATA_CHUNK::default(),
        }
    }
}

unsafe impl<B: IoBuf + Send> OpCode for SendBody<B> {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.queue.0)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let len = match u32::try_from(self.buffer.bytes_init()) {
            Ok(len) => len,
            Err(_) => {
                return Poll::Ready(Err(windows::core::Error::new(
                    windows::Win32::Foundation::ERROR_INVALID_PARAMETER.to_hresult(),
                    "body chunk exceeds the u32 length the HTTP Server API accepts",
                )))
            }
        };

        // Derived from `&mut self`; the descriptor lives in this operation.
        self.chunk = HTTP_DATA_CHUNK {
            DataChunkType: HttpDataChunkFromMemory,
            ..Default::default()
        };
        self.chunk.Anonymous.FromMemory.BufferLength = len;
        self.chunk.Anonymous.FromMemory.pBuffer =
            self.buffer.stable_ptr() as *mut core::ffi::c_void;

        let code = unsafe {
            HttpSendResponseEntityBody(
                self.queue.0,
                self.request_id.get(),
                self.flags,
                // The slice is borrowed only for the call, but the kernel keeps
                // the pointer -- so it must refer to the descriptor this
                // operation owns, never a temporary.
                Some(std::slice::from_ref(&self.chunk)),
                None,
                None,
                None,
                Some(optr),
                None,
            )
        };
        poll_from_code(code, optr)
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.queue.0, Some(optr)) }
    }
}

impl<B: IoBuf> IntoInner for SendBody<B> {
    type Inner = B;

    fn into_inner(mut self) -> B {
        // The descriptor pointed into the buffer; it must not outlive the op.
        self.chunk = HTTP_DATA_CHUNK::default();
        self.buffer
    }
}
