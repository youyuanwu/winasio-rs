// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The five asynchronous operations.
//!
//! Every future here follows one protocol, described once in
//! [`Request::poll_operation`]. The differences between them are which WinHTTP
//! function they call and what they do with the completion's length.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use windows::core::Error;
use windows::Win32::Networking::WinHttp::{
    WinHttpQueryDataAvailable, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpWriteData,
};

use crate::iocp::{IoBuf, IoBufMut, OpResult};

use super::context::{Completion, OpKind};
use super::error::WinHttpError;
use super::Request;

/// Outcome of the submission half of a poll.
enum Submitted {
    /// The call was accepted; the completion will arrive through the callback
    /// (or may already have arrived inline).
    Accepted,
    /// The call failed synchronously.
    Failed(Error),
}

impl Request {
    /// Drive one operation to completion.
    ///
    /// This is the whole protocol, in one place, because getting it wrong is
    /// silent.
    ///
    /// ```text
    /// first poll:
    ///   lock; reject if the slot is busy; take a generation; store the waker;
    ///   UNLOCK; call WinHTTP; if it failed synchronously roll the slot back
    ///   (only if it is still ours -- the call may have completed inline);
    ///   then fall through to the completion check.
    /// every poll:
    ///   lock; if complete for our generation, take the outcome; otherwise
    ///   replace the waker and return Pending.
    /// ```
    ///
    /// Two details are load-bearing and neither is obvious.
    ///
    /// **The waker is stored before the WinHTTP call, not after.** WinHTTP
    /// completes operations *inline* — on the submitting thread, re-entering
    /// the callback before the submitting call has returned. That is not an
    /// edge case; it is what `WinHttpReceiveResponse`, `WinHttpQueryDataAvailable`
    /// and `WinHttpReadData` were measured doing on every run. Storing the
    /// waker afterwards would lose the wakeup for exactly those three.
    ///
    /// **The lock is released before the WinHTTP call.** For the same reason:
    /// the inline callback takes the same lock, from the same thread, and would
    /// deadlock on the first request.
    fn poll_operation<F>(
        &self,
        kind: OpKind,
        generation: &mut Option<u64>,
        cx: &mut Context<'_>,
        submit: F,
    ) -> Poll<Result<u32, Error>>
    where
        F: FnOnce() -> Submitted,
    {
        let current = match *generation {
            Some(current) => current,
            None => {
                // First poll. Claim the slot.
                let claimed = {
                    let mut inner = self.context.lock();
                    if !inner.is_idle() {
                        None
                    } else {
                        let claimed = inner.next_generation;
                        inner.next_generation = inner.next_generation.wrapping_add(1);
                        inner.begin(kind, claimed);
                        inner.waker = Some(cx.waker().clone());
                        Some(claimed)
                    }
                };
                let Some(claimed) = claimed else {
                    return Poll::Ready(Err(WinHttpError::OperationInProgress.into()));
                };
                *generation = Some(claimed);

                // The lock is released here, before the call. See above.
                match submit() {
                    Submitted::Accepted => {}
                    Submitted::Failed(error) => {
                        // Roll back only if the slot is still ours and still
                        // pending. A synchronous failure can race an inline
                        // completion, and clobbering a completion that already
                        // landed would hang the future forever.
                        self.context.lock().rollback(claimed);
                        return Poll::Ready(Err(error));
                    }
                }
                claimed
            }
        };

        // Completion check. On the first poll this is the recheck that catches
        // an inline completion; on later polls it is the ordinary path.
        let mut inner = self.context.lock();
        match inner.take_completion(current) {
            Some(Completion::Done(length)) => Poll::Ready(Ok(length)),
            Some(Completion::Failed(error)) => Poll::Ready(Err(error)),
            None => {
                // Replace rather than keep: an executor may poll a future from
                // a different task, with a different waker, at any time.
                inner.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    /// Give up on an operation whose future is being dropped.
    fn abandon(&self, generation: Option<u64>, buffer: Option<Box<dyn Any + Send>>) {
        let Some(generation) = generation else {
            // Never submitted: constructing a future and dropping it without
            // polling it is a no-op, which is why submission happens in `poll`
            // and not in the constructor.
            return;
        };
        self.context.lock().abandon(generation, buffer);
    }
}

// ------------------------------------------------------------------ send

/// Future returned by [`Request::send`].
///
/// Owns the request body and the header block for the whole of the send.
/// WinHTTP reads both asynchronously, so a borrow would let the caller free
/// memory the platform is still reading — the same defect as an abandoned read
/// buffer, and the one that is easiest to overlook because a request body does
/// not look like an I/O buffer.
#[must_use = "the buffer is returned here and would otherwise be dropped"]
pub struct SendRequest<'a, B: Send + 'static> {
    request: &'a Request,
    body: Option<B>,
    headers: Option<Vec<u16>>,
    total_length: u32,
    generation: Option<u64>,
}

impl<'a, B: IoBuf + Send> SendRequest<'a, B> {
    pub(crate) fn new(
        request: &'a Request,
        headers: Option<Vec<u16>>,
        body: B,
        total_length: u32,
    ) -> Self {
        SendRequest {
            request,
            body: Some(body),
            headers,
            total_length,
            generation: None,
        }
    }
}

impl<B: IoBuf + Send + Unpin> Future for SendRequest<'_, B> {
    type Output = OpResult<(), B>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this.request.handle.as_raw();
        let context_value = this.request.context_value();
        let (body_ptr, body_len) = match &this.body {
            Some(body) => (body.stable_ptr(), body.bytes_init()),
            None => (std::ptr::null(), 0),
        };
        let headers = this.headers.as_deref();
        let total_length = this.total_length;

        let outcome = this
            .request
            .poll_operation(OpKind::Send, &mut this.generation, cx, || {
                // The context is passed as `dwContext` as well as having been
                // installed with `WINHTTP_OPTION_CONTEXT_VALUE`, and it is
                // deliberately the *same* pointer. A non-zero `dwContext`
                // overrides the option permanently; passing zero leaves the
                // option in place. Passing the identical value makes the
                // question moot.
                let optional = if body_len == 0 {
                    None
                } else {
                    Some(body_ptr.cast::<core::ffi::c_void>())
                };
                // SAFETY: the handle is live for the whole of this call, the
                // header slice and the body are owned by this future and are
                // not freed until the completion arrives or the buffer is
                // retired into the context.
                let call = unsafe {
                    WinHttpSendRequest(
                        handle,
                        headers,
                        optional,
                        body_len as u32,
                        total_length,
                        context_value,
                    )
                };
                match call {
                    Ok(()) => Submitted::Accepted,
                    Err(error) => Submitted::Failed(error),
                }
            });

        match outcome {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                let body = this.body.take().unwrap_or_else(|| {
                    // Unreachable: the buffer is only taken here and in `Drop`,
                    // and `Drop` cannot run while this method is executing.
                    unreachable!("send body taken twice")
                });
                Poll::Ready(OpResult(result.map(|_| ()), body))
            }
        }
    }
}

impl<B: Send + 'static> Drop for SendRequest<'_, B> {
    fn drop(&mut self) {
        // Both the body and the header block are read by WinHTTP for the whole
        // of the send, so both are retired together. Retiring the body but not
        // the headers would leave the platform reading freed memory — the same
        // defect, one indirection further away.
        let body = self.body.take();
        let headers = self.headers.take();
        let retired: Option<Box<dyn Any + Send>> = if self.generation.is_some() {
            Some(Box::new((body, headers)))
        } else {
            None
        };
        self.request.abandon(self.generation, retired);
    }
}

// ----------------------------------------------------------------- write

/// Future returned by [`Request::write_data`].
#[must_use = "the buffer is returned here and would otherwise be dropped"]
pub struct WriteData<'a, B: Send + 'static> {
    request: &'a Request,
    buffer: Option<B>,
    generation: Option<u64>,
}

impl<'a, B: IoBuf + Send> WriteData<'a, B> {
    pub(crate) fn new(request: &'a Request, buffer: B) -> Self {
        WriteData {
            request,
            buffer: Some(buffer),
            generation: None,
        }
    }
}

impl<B: IoBuf + Send + Unpin> Future for WriteData<'_, B> {
    type Output = OpResult<usize, B>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this.request.handle.as_raw();
        let (pointer, length) = match &this.buffer {
            Some(buffer) => (buffer.stable_ptr(), buffer.bytes_init()),
            None => (std::ptr::null(), 0),
        };

        let outcome = this
            .request
            .poll_operation(OpKind::Write, &mut this.generation, cx, || {
                let mut written = 0u32;
                // SAFETY: the buffer is owned by this future for the whole
                // of the operation; if the future is dropped the buffer is
                // retired into the context rather than freed.
                let call = unsafe {
                    WinHttpWriteData(
                        handle,
                        Some(pointer.cast::<core::ffi::c_void>()),
                        length as u32,
                        &mut written,
                    )
                };
                match call {
                    Ok(()) => Submitted::Accepted,
                    Err(error) => Submitted::Failed(error),
                }
            });

        match outcome {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                let buffer = match this.buffer.take() {
                    Some(buffer) => buffer,
                    None => return Poll::Pending,
                };
                // A write cannot report more than it was given. Clamping rather
                // than trusting keeps a platform over-report from becoming a
                // caller-visible lie about how much was sent.
                let written = result.map(|reported| (reported as usize).min(length));
                Poll::Ready(OpResult(written, buffer))
            }
        }
    }
}

impl<B: Send + 'static> Drop for WriteData<'_, B> {
    fn drop(&mut self) {
        let retired = self
            .buffer
            .take()
            .map(|buffer| Box::new(buffer) as Box<dyn Any + Send>);
        self.request.abandon(self.generation, retired);
    }
}

// ------------------------------------------------------- receive response

/// Future returned by [`Request::receive_response`].
#[must_use = "a response is only received when this future is awaited"]
pub struct ReceiveResponse<'a> {
    request: &'a Request,
    generation: Option<u64>,
}

impl<'a> ReceiveResponse<'a> {
    pub(crate) fn new(request: &'a Request) -> Self {
        ReceiveResponse {
            request,
            generation: None,
        }
    }
}

impl Future for ReceiveResponse<'_> {
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this.request.handle.as_raw();
        this.request
            .poll_operation(OpKind::ReceiveResponse, &mut this.generation, cx, || {
                // SAFETY: the handle is live for the duration of the call.
                match unsafe { WinHttpReceiveResponse(handle, std::ptr::null_mut()) } {
                    Ok(()) => Submitted::Accepted,
                    Err(error) => Submitted::Failed(error),
                }
            })
            .map(|result| result.map(|_| ()))
    }
}

impl Drop for ReceiveResponse<'_> {
    fn drop(&mut self) {
        self.request.abandon(self.generation, None);
    }
}

// -------------------------------------------------- query data available

/// Future returned by [`Request::query_data_available`].
#[must_use = "the byte count is only produced when this future is awaited"]
pub struct QueryDataAvailable<'a> {
    request: &'a Request,
    generation: Option<u64>,
}

impl<'a> QueryDataAvailable<'a> {
    pub(crate) fn new(request: &'a Request) -> Self {
        QueryDataAvailable {
            request,
            generation: None,
        }
    }
}

impl Future for QueryDataAvailable<'_> {
    type Output = Result<u32, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this.request.handle.as_raw();
        this.request
            .poll_operation(OpKind::QueryDataAvailable, &mut this.generation, cx, || {
                // SAFETY: the handle is live for the duration of the call. The
                // out-parameter is null because the answer arrives through the
                // callback, not here.
                match unsafe { WinHttpQueryDataAvailable(handle, std::ptr::null_mut()) } {
                    Ok(()) => Submitted::Accepted,
                    Err(error) => Submitted::Failed(error),
                }
            })
    }
}

impl Drop for QueryDataAvailable<'_> {
    fn drop(&mut self) {
        self.request.abandon(self.generation, None);
    }
}

// ------------------------------------------------------------------ read

/// Future returned by [`Request::read_data`].
#[must_use = "the buffer is returned here and would otherwise be dropped"]
pub struct ReadData<'a, B: Send + 'static> {
    request: &'a Request,
    buffer: Option<B>,
    capacity: usize,
    generation: Option<u64>,
}

impl<'a, B: IoBufMut + Send> ReadData<'a, B> {
    pub(crate) fn new(request: &'a Request, mut buffer: B) -> Self {
        let capacity = buffer.bytes_total();
        ReadData {
            request,
            buffer: Some(buffer),
            capacity,
            generation: None,
        }
    }
}

impl<B: IoBufMut + Send + Unpin> Future for ReadData<'_, B> {
    type Output = OpResult<usize, B>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this.request.handle.as_raw();
        let capacity = this.capacity;

        // A zero-capacity read is rejected here rather than submitted. WinHTTP
        // would complete it immediately with zero bytes, which a caller looping
        // "until the read returns nothing" would read as end-of-body — turning
        // a caller's buffer mistake into silent truncation.
        if capacity == 0 {
            if let Some(buffer) = this.buffer.take() {
                return Poll::Ready(OpResult(
                    Err(Error::from_hresult(windows::core::HRESULT::from_win32(
                        windows::Win32::Foundation::ERROR_INVALID_PARAMETER.0,
                    ))),
                    buffer,
                ));
            }
            return Poll::Pending;
        }

        let pointer = match &mut this.buffer {
            Some(buffer) => buffer.as_uninit().as_mut_ptr(),
            None => return Poll::Pending,
        };

        let outcome = this
            .request
            .poll_operation(OpKind::Read, &mut this.generation, cx, || {
                // The length comes from the buffer's own capacity and from
                // nowhere else. The previous API took a separate length
                // argument and passed it through unclamped, so a caller could
                // direct WinHTTP to write past the end of its own buffer.
                //
                // SAFETY: the buffer is owned by this future for the whole of
                // the operation and is retired into the context, not freed, if
                // the future is dropped first.
                let call = unsafe {
                    WinHttpReadData(
                        handle,
                        pointer.cast::<core::ffi::c_void>(),
                        capacity as u32,
                        std::ptr::null_mut(),
                    )
                };
                match call {
                    Ok(()) => Submitted::Accepted,
                    Err(error) => Submitted::Failed(error),
                }
            });

        match outcome {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                let mut buffer = match this.buffer.take() {
                    Some(buffer) => buffer,
                    None => return Poll::Pending,
                };
                let result = result.map(|reported| {
                    let read = (reported as usize).min(capacity);
                    // SAFETY: `read` bytes were written by WinHTTP into this
                    // buffer's spare capacity, and it has been clamped to that
                    // capacity, so publishing them cannot expose uninitialised
                    // memory even if the platform over-reports.
                    unsafe { buffer.set_init(read) };
                    read
                });
                Poll::Ready(OpResult(result, buffer))
            }
        }
    }
}

impl<B: Send + 'static> Drop for ReadData<'_, B> {
    fn drop(&mut self) {
        let retired = self
            .buffer
            .take()
            .map(|buffer| Box::new(buffer) as Box<dyn Any + Send>);
        self.request.abandon(self.generation, retired);
    }
}
