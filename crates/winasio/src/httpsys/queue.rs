// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The request queue -- the listener requests arrive on and replies go out on.

use windows::core::{Error, Result};
use windows::Win32::Foundation::{ERROR_INVALID_HANDLE, HANDLE};
use windows::Win32::Networking::HttpServer::{
    HttpCloseRequestQueue, HttpCreateRequestQueue, HttpServerBindingProperty, HTTP_BINDING_INFO,
    HTTP_PROPERTY_FLAGS,
};

use crate::iocp::{IntoInner, OpCode, Submit, ThreadPoolIo};

use super::error::{check, win32_code};
use super::init::VERSION;
use super::ops::cancel::CancelRequest;
use super::ops::receive::ReceiveRequest;
use super::request::{Request, RequestId, MIN_CAPACITY};
use super::session::UrlGroup;

/// How a queue sizes its receive buffers and how hard it retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveConfig {
    /// Initial buffer size, in bytes. Raised to [`MIN_CAPACITY`] if smaller.
    pub initial_capacity: usize,
    /// How many times an over-large request may be retried at a larger size.
    ///
    /// Zero disables retrying, so an over-large request fails immediately with
    /// [`ReceiveError::TooLarge`].
    pub max_retries: u32,
}

impl Default for ReceiveConfig {
    fn default() -> Self {
        ReceiveConfig {
            initial_capacity: 4096,
            max_retries: 1,
        }
    }
}

/// How much a retry enlarges the buffer.
///
/// Phase 0 established that the size HTTP.sys needs is *not* recoverable on the
/// asynchronous path -- the value exists in the completion's `InternalHigh`, but
/// reading it would mean retaining the `OVERLAPPED` pointer past `operate`,
/// which the [`OpCode`] contract forbids. So the retry grows blindly instead.
///
/// A factor of 16 takes the 4096-byte default to 65536 in a single step, which
/// covers roughly 60 KB of request text -- comfortably beyond the operating
/// system's own default request ceiling of about 16 KB. Measured: a request
/// padded with 4096 bytes needed 5696 bytes of buffer in total.
const GROWTH_FACTOR: usize = 16;

/// Why a receive did not produce a request.
#[derive(Debug)]
pub enum ReceiveError {
    /// The request did not fit, and the retry bound is exhausted.
    ///
    /// The request is still queued. Reject it with
    /// [`RequestQueue::reject`] -- otherwise the next receive returns the very
    /// same request and the accept loop livelocks.
    TooLarge {
        /// The offending request, so it can be rejected.
        id: RequestId,
        /// The largest buffer that was tried.
        attempted_capacity: usize,
        /// How many retries were performed.
        retries: u32,
    },
    /// Anything else -- a closed queue, a cancelled operation, a real failure.
    Failed(Error),
}

impl std::fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiveError::TooLarge {
                id,
                attempted_capacity,
                retries,
            } => write!(
                f,
                "request {id:?} did not fit in {attempted_capacity} bytes after {retries} retries; reject it to clear the queue"
            ),
            ReceiveError::Failed(e) => write!(f, "receive failed: {e}"),
        }
    }
}

impl std::error::Error for ReceiveError {}

impl From<Error> for ReceiveError {
    fn from(e: Error) -> Self {
        ReceiveError::Failed(e)
    }
}

/// A listener.
///
/// Completions are delivered by the Win32 thread pool rather than a
/// caller-driven proactor: a queue is normally shared across tasks on a
/// multi-threaded runtime, and [`Proactor`](crate::iocp::Proactor) is `!Send`.
///
/// Closing is idempotent and reports failure as a value; dropping closes and
/// discards any error, because a panic in `Drop` aborts during unwinding.
pub struct RequestQueue {
    handle: HANDLE,
    /// `None` once closed. Dropping the registration cancels and drains
    /// outstanding operations, which is why it is released *before* the handle.
    io: Option<ThreadPoolIo>,
    config: ReceiveConfig,
}

impl std::fmt::Debug for RequestQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestQueue")
            .field("handle", &self.handle)
            .field("open", &self.io.is_some())
            .finish()
    }
}

// SAFETY: a request-queue handle is thread-agnostic, and `ThreadPoolIo` is
// already `Send + Sync`. `HANDLE` is only `!Send` because it is a raw pointer.
unsafe impl Send for RequestQueue {}
unsafe impl Sync for RequestQueue {}

impl RequestQueue {
    /// Create an anonymous request queue and register it for completions.
    pub fn new() -> Result<RequestQueue> {
        RequestQueue::with_config(ReceiveConfig::default())
    }

    /// Create a queue with a specific receive configuration.
    pub fn with_config(config: ReceiveConfig) -> Result<RequestQueue> {
        let mut handle = HANDLE::default();
        let code = unsafe {
            HttpCreateRequestQueue(
                VERSION,
                windows::core::PCWSTR::null(),
                None,
                None,
                &mut handle,
            )
        };
        check(code)?;
        debug_assert!(!handle.is_invalid());

        match ThreadPoolIo::new(handle) {
            Ok(io) => Ok(RequestQueue {
                handle,
                io: Some(io),
                config,
            }),
            Err(e) => {
                // Do not leak the queue if registration fails.
                let _ = unsafe { HttpCloseRequestQueue(handle) };
                Err(Error::from(e))
            }
        }
    }

    /// The receive configuration in force.
    pub fn config(&self) -> ReceiveConfig {
        self.config
    }

    /// Direct traffic for `group`'s URLs to this queue.
    pub fn bind_url_group(&self, group: &UrlGroup) -> Result<()> {
        let info = HTTP_BINDING_INFO {
            // `Present` bit: the binding is being set rather than cleared.
            Flags: HTTP_PROPERTY_FLAGS { _bitfield: 1 },
            RequestQueueHandle: self.handle,
        };
        unsafe {
            group.set_property(
                HttpServerBindingProperty,
                std::ptr::addr_of!(info) as *const core::ffi::c_void,
                std::mem::size_of::<HTTP_BINDING_INFO>() as u32,
            )
        }
    }

    /// The underlying handle, for operations built against this queue.
    // Consumed by the reply and body operations in later phases.
    #[allow(dead_code)]
    pub(crate) fn handle(&self) -> HANDLE {
        self.handle
    }

    /// Submit an operation, failing rather than panicking once closed.
    pub(crate) fn submit<T: OpCode + Send>(&self, op: T) -> Result<Submit<T>> {
        match self.io.as_ref() {
            Some(io) => Ok(io.submit(op)),
            None => Err(Error::from_hresult(ERROR_INVALID_HANDLE.to_hresult())),
        }
    }

    /// Await the next request.
    ///
    /// Several receives may be outstanding on one queue at a time; each resolves
    /// to a distinct request.
    ///
    /// A request whose metadata exceeds the configured capacity is retried
    /// automatically at a larger size, up to the configured bound. Beyond that
    /// it comes back as [`ReceiveError::TooLarge`], carrying the identifier
    /// needed to [`reject`](RequestQueue::reject) it.
    pub async fn receive(&self) -> std::result::Result<Request, ReceiveError> {
        self.receive_id(RequestId::NEXT).await
    }

    /// Await one specific request, by identifier.
    pub async fn receive_id(&self, id: RequestId) -> std::result::Result<Request, ReceiveError> {
        let mut capacity = self.config.initial_capacity.max(MIN_CAPACITY);
        let mut target = id;
        let mut retries = 0u32;

        loop {
            let request = Request::with_capacity(capacity);
            let op = ReceiveRequest::new(self.handle, target, request);
            let outcome = self.submit(op)?.await;
            let (result, op) = outcome.into_parts();

            match result {
                Ok(_) => {
                    let mut request = op.into_inner();
                    request.set_retries(retries);
                    return Ok(request);
                }
                Err(e) if is_more_data(&e) => {
                    // HTTP.sys filled in the header even though the tail did not
                    // fit, so the identifier is available to target the retry.
                    let recovered = op.recovered_id();
                    if retries >= self.config.max_retries {
                        return Err(ReceiveError::TooLarge {
                            id: recovered,
                            attempted_capacity: capacity,
                            retries,
                        });
                    }
                    retries += 1;
                    target = recovered;
                    capacity = capacity.saturating_mul(GROWTH_FACTOR);
                }
                Err(e) => return Err(ReceiveError::Failed(e)),
            }
        }
    }

    /// Discard a queued request without replying to it.
    ///
    /// This is how an over-large request is cleared after
    /// [`ReceiveError::TooLarge`]; leaving it queued would make the next receive
    /// return the same request again.
    pub async fn reject(&self, id: RequestId) -> Result<()> {
        let outcome = self.submit(CancelRequest::new(self.handle, id))?.await;
        outcome.0.map(|_| ())
    }

    /// Close the queue, draining outstanding operations first.
    ///
    /// Idempotent: closing an already-closed queue succeeds and does nothing.
    pub fn close(&mut self) -> Result<()> {
        if self.io.is_none() && self.handle.is_invalid() {
            return Ok(());
        }
        // Release the registration first. Dropping it cancels and drains
        // in-flight operations, so the kernel no longer holds pointers into
        // them by the time the handle goes away.
        self.io = None;

        let handle = std::mem::take(&mut self.handle);
        if handle.is_invalid() {
            return Ok(());
        }
        check(unsafe { HttpCloseRequestQueue(handle) })
    }
}

impl Drop for RequestQueue {
    fn drop(&mut self) {
        // Ignored: a panic in `Drop` aborts during unwinding.
        let _ = self.close();
    }
}

/// Whether an error is HTTP.sys reporting that the buffer was too small.
fn is_more_data(err: &Error) -> bool {
    win32_code(err) == Some(windows::Win32::Foundation::ERROR_MORE_DATA.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;
    use windows::Win32::System::IO::OVERLAPPED;

    struct NeverRuns;

    unsafe impl OpCode for NeverRuns {
        fn handle(&self) -> Option<HANDLE> {
            None
        }
        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            unreachable!("a closed queue must reject the submission before operating")
        }
    }

    /// FR-010: submitting to a closed queue returns an error value rather than
    /// panicking. Exercised in-crate because `submit` is not public and the
    /// public operations do not exist until later phases.
    #[test]
    fn submitting_to_a_closed_queue_is_an_error() {
        let mut queue = match RequestQueue::new() {
            Ok(q) => q,
            // No HTTP service available; nothing to assert.
            Err(_) => return,
        };
        queue.close().expect("first close succeeds");
        assert!(queue.submit(NeverRuns).is_err());
    }

    /// FR-011: closing twice succeeds.
    #[test]
    fn closing_twice_succeeds() {
        let mut queue = match RequestQueue::new() {
            Ok(q) => q,
            Err(_) => return,
        };
        queue.close().expect("first close succeeds");
        queue.close().expect("second close is a no-op");
    }

    /// SC-004: a queue holding an unusable handle must drop without panicking.
    #[test]
    fn dropping_a_queue_with_an_invalid_handle_does_not_panic() {
        let bogus = RequestQueue {
            handle: HANDLE(usize::MAX as *mut core::ffi::c_void),
            io: None,
            config: ReceiveConfig::default(),
        };
        drop(bogus);
    }

    #[test]
    fn default_config_matches_the_specification() {
        let c = ReceiveConfig::default();
        assert_eq!(c.initial_capacity, 4096);
        assert_eq!(c.max_retries, 1);
        // One retry at x16 must clear the operating system's own ~16 KB ceiling.
        assert!(c.initial_capacity * GROWTH_FACTOR > 16 * 1024 * 3);
    }
}
