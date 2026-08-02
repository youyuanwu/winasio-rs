// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! HTTP Server API operations.
//!
//! These are the struct-filling shape that motivated leaving a buffer trait out
//! of [`OpCode`]: HTTP.sys writes a variable-length blob into a caller-supplied
//! region and stores pointers to it inside the same allocation.

use std::pin::Pin;
use std::task::Poll;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::HttpServer::{
    HttpReceiveHttpRequest, HttpSendHttpResponse, HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::iocp::{win32_result, IntoInner, OpCode};

use super::{Request, Response};

/// A handle usable from any thread; `HANDLE` is a raw pointer and so not `Send`.
#[derive(Clone, Copy)]
pub(crate) struct QueueHandle(pub(crate) HANDLE);

// SAFETY: a request-queue handle is thread-agnostic.
unsafe impl Send for QueueHandle {}
unsafe impl Sync for QueueHandle {}

/// Receive one request from a request queue.
///
/// # Why the request is pinned
///
/// HTTP.sys receives `Request` as a single buffer and writes the URL, headers
/// and entity metadata into its inline tail, storing pointers to that tail in
/// the `HTTP_REQUEST_V2` header — `pRawUrl`, `KnownHeaders[i].pRawValue`,
/// `pUnknownHeaders`, `pRequestInfo` and so on. Moving the value after
/// completion would leave every one of those pointers dangling.
///
/// A plain `Box<Request>` is not enough, because safe code can write
/// `let r = *boxed;` and move the value out. `Pin<Box<Request>>` prevents that,
/// so the guarantee survives being handed back to the caller.
pub struct ReceiveRequest {
    queue: QueueHandle,
    request_id: u64,
    flags: HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
    request: Pin<Box<Request>>,
}

impl ReceiveRequest {
    /// Receive the next request, or a specific one by id.
    pub fn new(queue: HANDLE, request_id: u64, flags: HTTP_RECEIVE_HTTP_REQUEST_FLAGS) -> Self {
        ReceiveRequest {
            queue: QueueHandle(queue),
            request_id,
            flags,
            request: Box::pin(Request::default()),
        }
    }
}

unsafe impl OpCode for ReceiveRequest {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.queue.0)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // SAFETY: the request is pinned in its own heap allocation, which the
        // driver keeps alive until the operation reaches a terminal state.
        let raw = unsafe { self.request.as_mut().get_unchecked_mut().raw_ptr() };

        let ec = unsafe {
            HttpReceiveHttpRequest(
                self.queue.0,
                self.request_id,
                self.flags,
                raw,
                Request::size(),
                None,
                Some(optr),
            )
        };
        win32_error_to_poll(ec, optr)
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.queue.0, Some(optr)) }
    }
}

impl IntoInner for ReceiveRequest {
    type Inner = Pin<Box<Request>>;

    fn into_inner(self) -> Self::Inner {
        self.request
    }
}

/// Send a response for a previously received request.
///
/// `Response` owns the chunk descriptor and body it points at, both on the heap,
/// so it is safe to move once the operation has finished. It must not move or be
/// mutated *while* in flight, though: HTTP.sys is given a pointer into this
/// operation's allocation. Ownership transfer is what enforces that.
pub struct SendResponse {
    queue: QueueHandle,
    request_id: u64,
    flags: u32,
    response: Response,
}

impl SendResponse {
    /// Send `response` for `request_id`.
    pub fn new(queue: HANDLE, request_id: u64, flags: u32, response: Response) -> Self {
        SendResponse {
            queue: QueueHandle(queue),
            request_id,
            flags,
            response,
        }
    }
}

unsafe impl OpCode for SendResponse {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.queue.0)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // SAFETY: derived from `&mut self`, which lives in the operation's own
        // stable allocation for the duration.
        let raw = self.response.raw();

        let ec = unsafe {
            HttpSendHttpResponse(
                self.queue.0,
                self.request_id,
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
        win32_error_to_poll(ec, optr)
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.queue.0, Some(optr)) }
    }
}

impl IntoInner for SendResponse {
    type Inner = Response;

    fn into_inner(self) -> Response {
        self.response
    }
}

/// The HTTP Server API returns a Win32 error code directly rather than setting
/// the thread's last error, so it cannot use [`win32_result`] unchanged.
fn win32_error_to_poll(code: u32, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
    use windows::Win32::Foundation::{ERROR_IO_PENDING, NO_ERROR, WIN32_ERROR};

    let err = WIN32_ERROR(code);
    if err == NO_ERROR {
        // Completed inline; the byte count is in the OVERLAPPED.
        // SAFETY: `optr` is the pointer just handed to the API.
        return unsafe { win32_result(true, optr) };
    }
    if err == ERROR_IO_PENDING {
        return Poll::Pending;
    }
    Poll::Ready(Err(windows::core::Error::from_hresult(err.to_hresult())))
}
