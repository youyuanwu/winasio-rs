// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The request body: an [`http_body::Body`] over a live HTTP.sys request.
//!
//! # Why this is simpler than the client's `ResponseBody`
//!
//! On the client every operation borrows the request mutably, so the body has
//! to move the request *into* each future and take it back out again. Here
//! `RequestQueue::read_body` takes `&self` and a [`RequestId`], so the body can
//! simply clone an `Arc<RequestQueue<_>>` into the future. The future owns
//! everything it needs, is `'static`, and can be stored across polls without a
//! self-referential struct and without `unsafe`.
//!
//! What remains is the same discipline: the **same** future is polled to
//! completion, never recreated per poll.
//!
//! # Why the same future is held, and why the reason differs from the client's
//!
//! On WinHTTP, recreating the read per poll was measured to park the request
//! with `ERROR_BUSY`. That hazard was hypothesised to apply here too, and
//! **measurement says it does not**: a `read_body` future polled once to
//! `Pending` and then dropped left the request perfectly usable, and the eight
//! bytes it had been waiting for were still delivered to the reads that
//! followed.
//!
//! The future is still held, for a different reason. A read that *completed*
//! between polls and was then dropped would retire its buffer — data and all —
//! and those bytes would never be seen again. Recreating the operation per poll
//! turns "the peer wrote while we were not looking" into silent data loss. So
//! the rule is the same and the rationale is not; recording the difference
//! matters, because a future reader who assumes the `ERROR_BUSY` story applies
//! here will draw the wrong conclusions about what is safe.
//!
//! # The end-of-body signal
//!
//! Measured, and the opposite of the client half: `Request::has_more_body()` is
//! a **snapshot taken when the request was received**, not a live signal. It
//! stays `true` after the body has been read to its end. The true signal is
//! `read_body` returning `Ok(0)` — the layer below maps `ERROR_HANDLE_EOF` onto
//! it — and it is idempotent, so a body that is polled again after ending
//! simply ends again.
//!
//! `has_more_body()` is still worth one thing: it distinguishes a request that
//! never had a body from one that did, which is how [`IncomingBody::empty`]
//! avoids issuing a read that can only return zero.
//!
//! # Why there is no truncation check
//!
//! The client half exists in large part to catch a body that was cut off, because
//! WinHTTP reports a mid-body connection close as a body that ended. HTTP.sys
//! does not: measured, a peer that declares `Content-Length: 20`, writes five
//! bytes and closes — gracefully *or* with a reset — makes the next `read_body`
//! fail with `ERROR_OPERATION_ABORTED`. The platform reports the amputation, so
//! this body only has to propagate it, and inventing a second check on top would
//! be duplicated work that could only disagree.
//!
//! # Reading rhythm
//!
//! Also measured, also unlike WinHTTP: `read_body` returns whatever has arrived
//! rather than waiting for the buffer to fill. A body written in three four-byte
//! pieces a quarter of a second apart produced three four-byte frames as they
//! arrived. So there is no availability query to make, no latency cap to
//! choose, and one buffer size is simply a throughput knob.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use winasio::httpsys::{RequestId, RequestQueue};
use winasio::iocp::ThreadPoolIo;

use super::backend::Backend;
use crate::error::{Error, ServerStage};

/// How much of the body is asked for at a time.
///
/// Not a latency trade-off — measured, HTTP.sys returns what has arrived rather
/// than waiting to fill the buffer — so this is only the largest single copy
/// the body will make.
const READ_SIZE: usize = 16 * 1024;

enum State<S: Backend> {
    /// Nothing in flight; the buffer is parked here between reads.
    Idle(Vec<u8>),
    /// One read in flight, owning the buffer until it resolves.
    Busy(S::Read),
    /// The body ended, cleanly or not. Fused.
    Finished,
}

/// The body of an [`http::Request`] handed to a service by this crate.
///
/// Yields [`Bytes`] frames as HTTP.sys delivers them. Dropping it without
/// reading to the end is fine and is what a handler that ignores the body does;
/// measured, a reply may still be sent afterwards.
///
/// # Trailers
///
/// Never produced. HTTP.sys de-chunks the request body before this crate sees
/// it and exposes no way to read trailers.
pub struct IncomingBody<S: Backend = ThreadPoolIo> {
    state: State<S>,
    queue: Arc<RequestQueue<S>>,
    id: RequestId,
    /// What `Content-Length` declared, when it declared something usable.
    declared: Option<u64>,
    delivered: u64,
}

impl<S: Backend> IncomingBody<S> {
    pub(crate) fn new(
        queue: Arc<RequestQueue<S>>,
        id: RequestId,
        declared: Option<u64>,
    ) -> IncomingBody<S> {
        IncomingBody {
            state: State::Idle(vec![0u8; READ_SIZE]),
            queue,
            id,
            declared,
            delivered: 0,
        }
    }

    /// A body for a request that never had one.
    ///
    /// Reading it would only ever return `Ok(0)`, so the read is not issued at
    /// all and `size_hint` can promise zero.
    pub(crate) fn empty(queue: Arc<RequestQueue<S>>, id: RequestId) -> IncomingBody<S> {
        IncomingBody {
            state: State::Finished,
            queue,
            id,
            declared: Some(0),
            delivered: 0,
        }
    }

    /// The request this body belongs to.
    pub fn request_id(&self) -> RequestId {
        self.id
    }
}

impl<S: Backend> std::fmt::Debug for IncomingBody<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written: the boxed future in `Busy` has no `Debug`, and what a
        // caller wants to know is how far along the body is.
        let state = match self.state {
            State::Idle(_) => "idle",
            State::Busy(_) => "reading",
            State::Finished => "finished",
        };
        f.debug_struct("IncomingBody")
            .field("state", &state)
            .field("request", &self.id.get())
            .field("declared", &self.declared)
            .field("delivered", &self.delivered)
            .finish()
    }
}

impl<S: Backend> Body for IncomingBody<S> {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        loop {
            match std::mem::replace(&mut self.state, State::Finished) {
                State::Finished => {
                    // Fused: an ended body stays ended and a failure is
                    // reported once rather than on every subsequent poll.
                    return Poll::Ready(None);
                }
                State::Idle(buffer) => {
                    let queue = Arc::clone(&self.queue);
                    let id = self.id;
                    self.state = State::Busy(S::read_body(queue, id, buffer));
                }
                State::Busy(mut operation) => {
                    let (buffer, read) = match Pin::new(&mut operation).poll(context) {
                        Poll::Ready(step) => step,
                        Poll::Pending => {
                            // Put back the *same* future. See the module docs
                            // for why recreating it would lose data.
                            self.state = State::Busy(operation);
                            return Poll::Pending;
                        }
                    };
                    let read = match read {
                        Ok(read) => read,
                        Err(error) => {
                            // Includes ERROR_OPERATION_ABORTED, which is how a
                            // peer that abandoned its body arrives here.
                            return Poll::Ready(Some(Err(Error::platform(ServerStage::ReadBody)(
                                error,
                            ))));
                        }
                    };
                    if read == 0 {
                        // The end-of-body signal. See the module docs: it is
                        // this, and not `has_more_body()`.
                        return Poll::Ready(None);
                    }
                    let chunk = Bytes::copy_from_slice(&buffer[..read]);
                    self.delivered += read as u64;
                    self.state = State::Idle(buffer);
                    return Poll::Ready(Some(Ok(Frame::data(chunk))));
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self.state, State::Finished)
    }

    fn size_hint(&self) -> SizeHint {
        match self.declared {
            // A declared length is exact, and HTTP.sys enforces it: a peer that
            // sends fewer bytes produces an error rather than a short body.
            Some(declared) => SizeHint::with_exact(declared.saturating_sub(self.delivered)),
            // A chunked request has no declared length. HTTP.sys has already
            // de-chunked it, so the length is knowable to nobody here.
            None => SizeHint::new(),
        }
    }
}

/// Whether an inbound request is worth issuing a read for at all.
///
/// `has_more_body()` is only trustworthy in this direction: measured, it is a
/// snapshot taken at receive time, so `true` may become stale but `false` never
/// does.
pub(crate) fn body_for<S: Backend>(
    queue: Arc<RequestQueue<S>>,
    id: RequestId,
    has_more: bool,
    declared: Option<u64>,
) -> IncomingBody<S> {
    if !has_more && declared.unwrap_or(0) == 0 {
        IncomingBody::empty(queue, id)
    } else {
        IncomingBody::new(queue, id, declared)
    }
}
