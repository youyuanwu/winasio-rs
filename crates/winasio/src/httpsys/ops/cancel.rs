// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Rejecting a queued request.

use std::task::Poll;

use windows::core::Result;
use windows::Win32::Networking::HttpServer::HttpCancelHttpRequest;
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use crate::httpsys::request::RequestId;
use crate::iocp::{IntoInner, OpCode};

use super::receive::poll_from_code;
use super::QueueHandle;

/// Discard a request without replying to it.
///
/// This is how an over-large request is removed from the queue once the retry
/// bound is exhausted. Without it, an accept loop that merely logged the failure
/// would receive the same request forever.
///
/// Phase 0 verified this works against a request left in the `ERROR_MORE_DATA`
/// state: the cancel succeeds and re-receiving that identifier afterwards fails
/// with `ERROR_CONNECTION_INVALID`.
pub struct CancelRequest {
    queue: QueueHandle,
    request_id: RequestId,
}

impl CancelRequest {
    pub(crate) fn new(queue: QueueHandle, request_id: RequestId) -> Self {
        CancelRequest { queue, request_id }
    }
}

unsafe impl OpCode for CancelRequest {
    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let code =
            unsafe { HttpCancelHttpRequest(self.queue.raw(), self.request_id.get(), Some(optr)) };
        poll_from_code(code, optr)
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.queue.raw(), Some(optr)) }
    }
}

impl IntoInner for CancelRequest {
    type Inner = ();

    fn into_inner(self) {}
}
