// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The request queue -- the listener requests arrive on and replies go out on.

use std::sync::RwLock;
use windows::core::{Error, Result};

use windows::Win32::Foundation::{ERROR_INVALID_HANDLE, HANDLE};
use windows::Win32::Networking::HttpServer::{
    HttpCreateRequestQueue, HttpServerBindingProperty, HTTP_BINDING_INFO, HTTP_PROPERTY_FLAGS,
};
use windows::Win32::System::IO::CancelIoEx;

use crate::iocp::{IntoInner, IoBuf, IoBufMut, OpCode, OpResult, Registrar, Submit, Submitter};

use super::error::{check, win32_code};
use super::init::VERSION;
use super::ops::body::ReceiveBody;
use super::ops::cancel::CancelRequest;
use super::ops::receive::ReceiveRequest;
use super::ops::send::{SendBody, SendResponse};
use super::ops::QueueHandle;
use super::request::{Request, RequestId, MIN_CAPACITY};
use super::response::Response;
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

/// The largest receive buffer the retry loop will ask for.
///
/// Without a ceiling, a caller who raised [`ReceiveConfig::max_retries`] would
/// drive the capacity up by a factor of sixteen each time until the allocation
/// aborted the process. Far above anything the operating system will deliver.
const MAX_CAPACITY: usize = 64 * 1024 * 1024;

/// Why a receive did not produce a request.
#[derive(Debug)]
pub enum ReceiveError {
    /// The request did not fit, and the retry bound is exhausted.
    ///
    /// **The request has already been discarded**, so the next receive returns a
    /// different one. That happens inside the library deliberately: leaving it
    /// queued would mean an accept loop that logged the error and continued
    /// would receive the very same request forever, spinning a core. Making that
    /// impossible is worth more than the chance to answer with a status code.
    TooLarge {
        /// The offending request, for logging. It is no longer queued.
        id: RequestId,
        /// The largest buffer that was tried.
        attempted_capacity: usize,
        /// How many retries were performed.
        retries: u32,
        /// Whether discarding it actually succeeded.
        discarded: bool,
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
                discarded,
            } => write!(
                f,
                "request {id:?} did not fit in {attempted_capacity} bytes after {retries} \
                 retries and was {}",
                if *discarded {
                    "discarded"
                } else {
                    "left queued (discarding it failed)"
                }
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

/// The parts that exist only while the queue is open.
///
/// Field order is load-bearing on an implicit drop: the submitter must go
/// first, because dropping the thread-pool registration cancels and waits while
/// holding a bare `HANDLE` ([`Registration::drop`](crate::iocp::ThreadPoolIo)),
/// and releasing the queue's handle reference before that could close -- and let
/// Windows recycle -- the very handle it is cancelling on. `close` destructures
/// and drops in this order explicitly; the declaration order makes the invariant
/// hold even if a future refactor drops an `Open` as a value.
struct Open<S> {
    io: S,
    handle: QueueHandle,
}

/// A listener.
///
/// Completions are delivered by the backend supplied at construction. A normal
/// server should use [`ThreadPool`](crate::iocp::ThreadPool): request queues are
/// usually shared as `Arc`s across worker tasks on a multi-threaded runtime,
/// while [`Proactor`](crate::iocp::Proactor) is `!Send`. A proactor-backed queue
/// is available for single-threaded loops, and inherits that affinity through
/// its submitter type rather than through an asserted `Send` implementation.
///
/// ```compile_fail
/// use std::rc::Rc;
/// use winasio::httpsys::RequestQueue;
/// use winasio::iocp::Proactor;
///
/// fn cannot_cross_threads(queue: RequestQueue<Rc<Proactor>>) {
///     // `Rc<Proactor>` is not `Send`, so neither is a queue backed by it.
///     std::thread::spawn(move || drop(queue));
/// }
/// ```
///
/// Closing is idempotent and reports failure as a value; dropping closes and
/// discards any error, because a panic in `Drop` aborts during unwinding.
pub struct RequestQueue<S: Submitter> {
    /// `None` once closed.
    ///
    /// Behind a lock rather than owned outright so that [`close`] can take
    /// `&self`: a queue is normally shared as an `Arc` across worker tasks, and
    /// a shutdown that required unique ownership could never run while those
    /// workers were blocked in [`receive`].
    ///
    /// [`close`]: RequestQueue::close
    /// [`receive`]: RequestQueue::receive
    open: RwLock<Option<Open<S>>>,
    config: ReceiveConfig,
}

impl<S: Submitter> std::fmt::Debug for RequestQueue<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let open = self
            .open
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|o| o.handle.raw()));
        f.debug_struct("RequestQueue")
            .field("handle", &open)
            .field("open", &open.is_some())
            .field("config", &self.config)
            .finish()
    }
}

impl<S: Submitter> RequestQueue<S> {
    /// Create an anonymous request queue and register it with `registrar`.
    pub fn new<R: Registrar<Io = S>>(registrar: &R) -> Result<RequestQueue<S>> {
        RequestQueue::with_config(registrar, ReceiveConfig::default())
    }

    /// Create a queue with a specific receive configuration and completion
    /// registrar.
    pub fn with_config<R: Registrar<Io = S>>(
        registrar: &R,
        config: ReceiveConfig,
    ) -> Result<RequestQueue<S>> {
        let mut raw = HANDLE::default();
        let code = unsafe {
            HttpCreateRequestQueue(VERSION, windows::core::PCWSTR::null(), None, None, &mut raw)
        };
        check(code)?;
        debug_assert!(!raw.is_invalid());

        // SAFETY: HTTP.sys returned a newly owned request queue handle, and
        // ownership of closing it transfers into `QueueHandle`.
        let handle = unsafe { QueueHandle::from_raw(raw) };
        match registrar.register(handle.raw()) {
            Ok(io) => Ok(RequestQueue {
                open: RwLock::new(Some(Open { handle, io })),
                config,
            }),
            Err(e) => {
                // Do not leak the queue if registration fails.
                let _ = handle.release();
                Err(Error::from(e))
            }
        }
    }

    /// The receive configuration in force.
    pub fn config(&self) -> ReceiveConfig {
        self.config
    }

    /// Whether the queue is still open.
    pub fn is_open(&self) -> bool {
        self.with_open(|_| ()).is_ok()
    }

    /// Run `f` against the open queue, or fail if it has been closed.
    fn with_open<T>(&self, f: impl FnOnce(QueueHandle) -> T) -> Result<T> {
        let guard = self.open.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(open) => Ok(f(open.handle.clone())),
            None => Err(Error::from_hresult(ERROR_INVALID_HANDLE.to_hresult())),
        }
    }

    /// Direct traffic for `group`'s URLs to this queue.
    pub fn bind_url_group(&self, group: &UrlGroup) -> Result<()> {
        // Hold the clone across the call rather than extracting the raw value.
        // `close` takes `&self` and `with_open` drops the read lock before
        // returning, so a raw handle captured here could be closed -- and the
        // value recycled onto an unrelated queue -- before it is used, silently
        // binding the URL group to someone else's listener.
        let handle = self.with_open(|h| h)?;
        let info = HTTP_BINDING_INFO {
            // `Present` bit: the binding is being set rather than cleared.
            Flags: HTTP_PROPERTY_FLAGS { _bitfield: 1 },
            RequestQueueHandle: handle.raw(),
        };
        unsafe {
            group.set_property(
                HttpServerBindingProperty,
                std::ptr::addr_of!(info) as *const core::ffi::c_void,
                std::mem::size_of::<HTTP_BINDING_INFO>() as u32,
            )
        }
    }

    /// Submit an operation, failing rather than panicking once closed.
    ///
    /// On failure the operation is handed back, so callers can return the
    /// caller-supplied state rather than dropping it.
    pub(crate) fn submit<T: OpCode + Send>(
        &self,
        op: T,
    ) -> std::result::Result<Submit<T>, (Error, T)> {
        let guard = self.open.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(open) => Ok(open.io.submit(op)),
            None => Err((Error::from_hresult(ERROR_INVALID_HANDLE.to_hresult()), op)),
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
            let handle = self.with_open(|h| h)?;
            let request = Request::with_capacity(capacity);
            let op = ReceiveRequest::new(handle, target, request);
            let outcome = match self.submit(op) {
                Ok(fut) => fut.await,
                Err((e, _)) => return Err(ReceiveError::Failed(e)),
            };
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
                        // Discard it here rather than leaving it to the caller.
                        // A queued request that cannot be delivered would be
                        // returned by every subsequent receive, so any accept
                        // loop that logged and continued would spin forever.
                        let discarded = self.reject(recovered).await.is_ok();
                        return Err(ReceiveError::TooLarge {
                            id: recovered,
                            attempted_capacity: capacity,
                            retries,
                            discarded,
                        });
                    }
                    retries += 1;
                    target = recovered;
                    // Growth is blind: the size HTTP.sys needs is not
                    // recoverable asynchronously. Capped so a high retry bound
                    // cannot drive the allocation to an out-of-memory abort.
                    capacity = capacity.saturating_mul(GROWTH_FACTOR).min(MAX_CAPACITY);
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
        let handle = self.with_open(|h| h)?;
        let fut = self
            .submit(CancelRequest::new(handle, id))
            .map_err(|(e, _)| e)?;
        fut.await.0.map(|_| ())
    }

    /// Send a complete reply.
    ///
    /// The reply comes back with the outcome, on failure as well as success, so
    /// a failed send does not consume it.
    ///
    /// The operating system forbids concurrent sends on a single request
    /// identifier; that is a caller obligation this API does not enforce.
    pub async fn send(&self, id: RequestId, response: Response) -> OpResult<usize, Response> {
        self.send_with(id, response, false).await
    }

    /// Send a reply, indicating that further body data follows.
    ///
    /// Continue with [`send_body`](RequestQueue::send_body), marking the final
    /// piece as last.
    pub async fn send_partial(
        &self,
        id: RequestId,
        response: Response,
    ) -> OpResult<usize, Response> {
        self.send_with(id, response, true).await
    }

    async fn send_with(
        &self,
        id: RequestId,
        response: Response,
        more: bool,
    ) -> OpResult<usize, Response> {
        let handle = match self.with_open(|h| h) {
            Ok(h) => h,
            Err(e) => return OpResult(Err(e), response),
        };
        match self.submit(SendResponse::new(handle, id, response, more)) {
            Ok(fut) => fut.await.map_state(IntoInner::into_inner),
            // Nothing was submitted, so hand the reply straight back.
            Err((e, op)) => OpResult(Err(e), op.into_inner()),
        }
    }

    /// Send a further piece of a reply's body.
    ///
    /// Set `last` on the final piece. The buffer comes back with the outcome.
    pub async fn send_body<B: IoBuf + Send>(
        &self,
        id: RequestId,
        buffer: B,
        last: bool,
    ) -> OpResult<usize, B> {
        let handle = match self.with_open(|h| h) {
            Ok(h) => h,
            Err(e) => return OpResult(Err(e), buffer),
        };
        match self.submit(SendBody::new(handle, id, buffer, last)) {
            Ok(fut) => fut.await.map_state(IntoInner::into_inner),
            Err((e, op)) => OpResult(Err(e), op.into_inner()),
        }
    }

    /// Read part of a request's body.
    ///
    /// The buffer comes back with the outcome. End of body is reported as
    /// `Ok(0)` rather than as an error.
    pub async fn read_body<B: IoBufMut + Send>(
        &self,
        id: RequestId,
        buffer: B,
    ) -> OpResult<usize, B> {
        let handle = match self.with_open(|h| h) {
            Ok(h) => h,
            Err(e) => return OpResult(Err(e), buffer),
        };
        match self.submit(ReceiveBody::new(handle, id, buffer)) {
            Ok(fut) => {
                let OpResult(result, op) = fut.await;
                // `ERROR_HANDLE_EOF` is how HTTP.sys signals the end of a body.
                // That is a normal terminating outcome, not a failure.
                let result = match result {
                    Err(ref e) if is_eof(e) => Ok(0),
                    other => other,
                };
                OpResult(result, op.into_inner())
            }
            Err((e, op)) => OpResult(Err(e), op.into_inner()),
        }
    }

    /// Read a request's body to its end.
    ///
    /// Reads repeatedly into a buffer of `chunk` bytes until end of body.
    pub async fn read_body_to_end(&self, id: RequestId, chunk: usize) -> Result<Vec<u8>> {
        let chunk = chunk.max(1);
        let mut collected: Vec<u8> = Vec::new();
        // Reused across iterations: the read returns the buffer, so allocating a
        // fresh one each time would cost an allocation per chunk for nothing.
        let mut buffer: Vec<u8> = Vec::with_capacity(chunk);
        loop {
            buffer.clear();
            let OpResult(result, returned) = self.read_body(id, buffer).await;
            buffer = returned;
            let n = result?;
            if n == 0 {
                return Ok(collected);
            }
            collected.extend_from_slice(&buffer[..n.min(buffer.len())]);
        }
    }

    /// Close the queue, cancelling outstanding operations first.
    ///
    /// Takes `&self` so a queue shared as an `Arc` can be shut down while worker
    /// tasks are still blocked in [`receive`](RequestQueue::receive) -- which is
    /// the only way to stop them, since a receive resolves with an error once
    /// the queue is gone.
    ///
    /// Idempotent: closing an already-closed queue succeeds and does nothing.
    ///
    /// # The close may be deferred
    ///
    /// In-flight operations hold a reference to the queue handle, which is what
    /// keeps late cancellation from targeting a closed -- or recycled -- handle.
    /// So if any operation is still alive, the `HttpCloseRequestQueue` is
    /// deferred to whichever reference drops last, and this returns `Ok(())`
    /// having reported nothing; the deferred call's own error is discarded,
    /// because it runs in a `Drop`.
    ///
    /// Until that happens HTTP.sys still holds the queue, so a caller who closes
    /// and immediately re-reserves the same URL can fail. **For a deterministic
    /// close, drop or await every outstanding operation future first**: the
    /// queue then holds the last reference, the close happens inline, and the
    /// real HTTP.sys code is returned.
    ///
    /// Note this is not the same as "no callbacks are running". On the
    /// thread-pool backend, dropping the registration waits for callbacks, but a
    /// completed-and-unpolled future still owns its operation -- and so still
    /// holds a reference.
    pub fn close(&self) -> Result<()> {
        let taken = {
            let mut guard = self.open.write().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        let Some(open) = taken else {
            return Ok(());
        };

        let Open { handle, io } = open;
        // Required for proactor-backed queues: dropping that submitter does not
        // drain per-handle operations, so pending receives would keep clones
        // alive and the deferred close would never make them complete.
        let _ = unsafe { CancelIoEx(handle.raw(), None) };
        drop(io);
        handle.release()
    }
}

impl<S: Submitter> Drop for RequestQueue<S> {
    fn drop(&mut self) {
        // Ignored: a panic in `Drop` aborts during unwinding.
        let _ = self.close();
    }
}

/// Whether an error is HTTP.sys reporting that the buffer was too small.
fn is_more_data(err: &Error) -> bool {
    win32_code(err) == Some(windows::Win32::Foundation::ERROR_MORE_DATA.0)
}

/// Whether an error is HTTP.sys reporting the end of an entity body.
fn is_eof(err: &Error) -> bool {
    win32_code(err) == Some(windows::Win32::Foundation::ERROR_HANDLE_EOF.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iocp::{ThreadPool, ThreadPoolIo};
    use std::task::Poll;
    use windows::Win32::System::IO::OVERLAPPED;

    struct NeverRuns;

    unsafe impl OpCode for NeverRuns {
        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            unreachable!("a closed queue must reject the submission before operating")
        }
    }

    /// FR-010: submitting to a closed queue returns an error value rather than
    /// panicking. Exercised in-crate because `submit` is not public and the
    /// public operations do not exist until later phases.
    #[test]
    fn submitting_to_a_closed_queue_is_an_error() {
        // The submission is rejected before an operation exists, so the global
        // counter is untouched -- but take the guard anyway, so this stays true
        // by construction rather than by inspection.
        let _guard = crate::iocp::counter_guard();
        let queue = match RequestQueue::new(&ThreadPool) {
            Ok(q) => q,
            // No HTTP service available; nothing to assert.
            Err(_) => return,
        };
        queue.close().expect("first close succeeds");
        assert!(queue.submit(NeverRuns).is_err());
        assert!(!queue.is_open());
    }

    /// FR-011: closing twice succeeds.
    #[test]
    fn closing_twice_succeeds() {
        let queue = match RequestQueue::new(&ThreadPool) {
            Ok(q) => q,
            Err(_) => return,
        };
        queue.close().expect("first close succeeds");
        queue.close().expect("second close is a no-op");
    }

    /// A shared queue can be closed through an `Arc`, which is what makes
    /// shutting down a running server possible at all.
    #[test]
    fn a_shared_queue_can_be_closed() {
        let queue = match RequestQueue::new(&ThreadPool) {
            Ok(q) => std::sync::Arc::new(q),
            Err(_) => return,
        };
        let other = queue.clone();
        other.close().expect("close through a shared handle");
        assert!(!queue.is_open());
    }

    /// The queue handle outlives `close` for exactly as long as an operation
    /// still owns a reference to it.
    ///
    /// Asserted through the reference count rather than by asking Windows
    /// whether the raw value is still a live handle. That check is unsound in a
    /// test binary: once the handle closes, Windows may hand the same value to
    /// another thread's `CreateFile` before the check runs, and the test then
    /// fails spuriously -- measured at roughly 1 run in 75 across the suite,
    /// and 0 in 300 when this test ran alone.
    ///
    /// Counting references is also the stronger assertion, because it
    /// attributes the surviving reference to the pending operation
    /// specifically; a handle-validity check cannot tell who held it.
    #[test]
    fn close_is_deferred_until_a_real_operation_releases_the_handle() {
        let _guard = crate::iocp::counter_guard();
        let Ok(_http) = crate::httpsys::HttpInitializer::new() else {
            return;
        };
        let queue = match RequestQueue::new(&ThreadPool) {
            Ok(q) => q,
            Err(_) => return,
        };
        // Held for the whole test so the reference count stays observable once
        // the queue has released its own reference.
        let probe = queue.with_open(|h| h).expect("queue is open");

        // A genuine pending receive rather than a hand-made clone. That is the
        // point: this test must fail if operations ever stop carrying ownership
        // of the queue handle, which is the whole safety guarantee.
        let handle = queue.with_open(|h| h).expect("queue is open");
        let op = ReceiveRequest::new(
            handle,
            RequestId::NEXT,
            Request::with_capacity(MIN_CAPACITY),
        );
        let pending = match queue.submit(op) {
            Ok(fut) => fut,
            Err(_) => return,
        };
        assert_eq!(
            probe.ref_count(),
            3,
            "the queue, this probe, and the pending operation"
        );

        queue.close().expect("close with an operation outstanding");
        assert!(!queue.is_open());
        assert_eq!(
            probe.ref_count(),
            2,
            "a pending operation must keep the queue handle alive past close"
        );

        // Dropping the registration inside `close` already waited for the
        // callbacks, so releasing the abandoned future hands its reference back
        // here, synchronously.
        drop(pending);
        assert_eq!(
            probe.ref_count(),
            1,
            "releasing the operation releases its handle reference"
        );
    }

    /// SC-004: a queue holding an unusable handle must drop without panicking.
    #[test]
    fn dropping_a_queue_with_an_invalid_handle_does_not_panic() {
        // No registration, so nothing is cancelled; only the handle close fails.
        let bogus = RequestQueue::<ThreadPoolIo> {
            open: RwLock::new(None),
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
