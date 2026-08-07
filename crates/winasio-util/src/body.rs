// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The response body: an [`http_body::Body`] over a live WinHTTP request.
//!
//! # The problem this module exists to solve
//!
//! Every asynchronous operation in `winasio::winhttp` borrows the request
//! mutably: `read_data(&mut self, ..) -> ReadData<'_, B>`. That is exactly
//! right for `async fn` code, and exactly wrong for a hand-written
//! [`http_body::Body::poll_frame`], which must hold *both* the request and its
//! in-flight operation across calls. Written naively that is a self-referential
//! struct, which safe Rust cannot express.
//!
//! The escape that suggests itself — build a fresh `ReadData` inside every
//! `poll_frame` and drop it when it returns `Pending` — is not merely wasteful
//! here, it is fatal. The module docs one crate down say abandoning a transfer
//! retires the buffer into the request context until the platform delivers the
//! completion, *and parks the request*. Measured: a body that recreated its
//! operation on each poll leaked a buffer per poll and then failed every
//! subsequent operation with `ERROR_BUSY`, surfaced as
//! [`WinHttpError::OperationInProgress`](winasio::winhttp::WinHttpError).
//!
//! # The design
//!
//! Invert the ownership. Instead of a future that borrows the request, build a
//! future that *owns* it and hands it back when it is done:
//!
//! ```text
//! Idle(Request)                      no operation in flight
//! Busy(Pin<Box<dyn Future<Output = (Request, ...)> + Send>>)
//! Done / Failed                      fused
//! ```
//!
//! `poll_frame` moves the [`Request`] out of `Idle` into an `async move` block,
//! boxes it, and stores it in `Busy`. Subsequent polls poll **the same**
//! future; nothing is ever abandoned. When it resolves it yields the request
//! back, and the state returns to `Idle`.
//!
//! The block performs *both* halves of the platform's two-call read protocol —
//! `query_data_available()` and then `read_data()` — so the sequencing that
//! would otherwise have to be spelled out as extra states is just two `await`s
//! inside an ordinary `async` block. Measured: this design was polled 9,800
//! times while `Pending`, delivered every byte, ended cleanly, and left
//! `live_context_count()` at zero.
//!
//! It costs one heap allocation per frame. That is the whole price.
//!
//! # Alternatives, and why each lost
//!
//! * **A self-referential struct behind `unsafe`.** Saves the allocation and
//!   costs the crate its only `unsafe` block. One allocation per *frame* —
//!   not per poll — alongside a read buffer of up to 64 KiB is not worth a
//!   soundness argument that has to be re-checked by every future reader.
//! * **Adding an owned-future variant to `winasio::winhttp`.** Methods taking
//!   `self` and returning a future that carries the request would make this
//!   module trivial. It would also churn a module that shipped two commits
//!   ago, and duplicate its entire operation surface, for a benefit this
//!   design already delivers.
//! * **Driving the platform state machine below the public futures.** Needs
//!   `pub(crate)` internals reachable across crates, and would duplicate the
//!   careful inline-completion handling that makes the existing futures
//!   correct.
//! * **Re-creating the operation on every poll.** Measured to park the request.
//!   Not viable, as above.
//!
//! # Why truncation is detected here
//!
//! Measured, and the single most important finding behind this crate: with
//! `Content-Length: 10`, a server that sends three bytes and then closes the
//! connection *gracefully* produces the trace `query_data_available -> 3`,
//! `read_data -> 3`, `query_data_available -> 0`, and the transfer succeeds.
//! WinHTTP reports the amputation as a body that ended. Only an RST produces an
//! error.
//!
//! The layer below cannot do better: zero available is the only end-of-body
//! signal the platform gives it. So this body counts what it has delivered and
//! compares it against the declared length, and reports
//! [`ResponseBodyError::Truncated`] when they disagree. `crates/winasio/src/net`
//! already paid for this lesson once, in the shape of an outcome classifier
//! that folded connection resets into a graceful close and returned truncated
//! reads as `Ok(())`.
//!
//! Where the response is chunked or close-delimited there is no declared length
//! and truncation is genuinely undetectable. That is a property of HTTP, not of
//! this crate, and it is documented rather than papered over with a guess.
//!
//! # Why reads are capped at 64 KiB
//!
//! `query_data_available` is a *lower bound*, not an exact count — measured
//! reporting 3,986 while the read that followed returned 65,536 — and
//! `read_data` waits until the buffer it was given is full or the body ends.
//! Measured: asking for 65,536 bytes when five were available blocked for 1.2
//! seconds until the server's second write arrived, and then returned
//! everything at once. Sizing the read at `min(available, 64 KiB)` keeps a
//! frame flowing as soon as data exists, while still allowing a single read to
//! absorb a large burst.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use winasio::iocp::OpResult;
use winasio::winhttp::Request;

use crate::error::ResponseBodyError;

/// The largest single read this body will ask the platform for.
///
/// See the module documentation: `read_data` blocks until its buffer is full,
/// so an unbounded buffer would trade latency for throughput on every frame.
const MAX_READ: usize = 64 * 1024;

/// What one turn of the read protocol produced.
type Step = (Request, Result<Option<Vec<u8>>, ResponseBodyError>);

enum State {
    /// The request is idle and this body owns it.
    Idle(Request),
    /// One read is in flight, owning the request until it resolves.
    Busy(Pin<Box<dyn Future<Output = Step> + Send>>),
    /// The body ended, cleanly or not. Fused.
    Finished,
}

/// The body of an [`http::Response`] produced by
/// [`Client::request`](crate::Client::request).
///
/// Yields [`Bytes`] frames as the platform delivers them. Dropping it before it
/// ends is fine and closes the request handle promptly; the bytes that were
/// still in flight are discarded.
///
/// # Truncation
///
/// If the response declared a `Content-Length` and the connection ends before
/// that many bytes arrive, the final poll yields [`ResponseBodyError::Truncated`]
/// rather than end-of-stream. A body without a declared length cannot be
/// checked; see the module documentation.
pub struct ResponseBody {
    state: State,
    /// How many bytes the response still owes, when it declared a length.
    remaining: Option<u64>,
    /// How many have been delivered, for the truncation message.
    delivered: u64,
}

impl ResponseBody {
    /// A body that will read from `request` until the platform says stop.
    pub(crate) fn new(request: Request, declared: Option<u64>) -> Self {
        ResponseBody {
            state: State::Idle(request),
            remaining: declared,
            delivered: 0,
        }
    }

    /// A body that is over before it starts.
    ///
    /// Used for a response that cannot carry one — a `HEAD`, a `204`, a `304` —
    /// where the platform reports zero available regardless of what the headers
    /// declared, so the declared length must not be believed.
    pub(crate) fn empty(request: Request) -> Self {
        // The request is dropped here rather than held: nothing more will be
        // read from it, and holding it would keep a connection alive for no
        // reason.
        drop(request);
        ResponseBody {
            state: State::Finished,
            remaining: Some(0),
            delivered: 0,
        }
    }

    /// Whether the body owes bytes it is never going to receive.
    fn shortfall(&self) -> Option<ResponseBodyError> {
        match self.remaining {
            Some(0) | None => None,
            Some(missing) => Some(ResponseBodyError::Truncated {
                expected: self.delivered + missing,
                received: self.delivered,
            }),
        }
    }
}

/// One turn of the platform's two-call read protocol, owning the request.
///
/// `Ok(None)` means the platform reported end of body. `Ok(Some(bytes))` is a
/// non-empty chunk.
async fn read_once(mut request: Request, cap: Option<u64>) -> Step {
    let available = match request.query_data_available().await {
        Ok(available) => available,
        Err(error) => return (request, Err(ResponseBodyError::Read(error))),
    };
    if available == 0 {
        return (request, Ok(None));
    }

    // `available` is a lower bound and `read_data` fills what it is given, so
    // the buffer is the smaller of what is claimed and what latency permits.
    // A declared remainder caps it further: reading past the declared length
    // would block waiting for bytes the response promised not to send.
    let mut wanted = (available as usize).min(MAX_READ);
    if let Some(remaining) = cap {
        wanted = wanted.min(remaining.max(1) as usize);
    }

    let OpResult(read, mut buffer) = request.read_data(Vec::<u8>::with_capacity(wanted)).await;
    let read = match read {
        Ok(read) => read,
        Err(error) => return (request, Err(ResponseBodyError::Read(error))),
    };
    buffer.truncate(read);
    if buffer.is_empty() {
        // Never observed: the platform reports availability before it reports
        // data, and a zero-length read after a non-zero availability did not
        // occur in any measurement. Treated as end of body anyway, because the
        // alternative is a loop that cannot make progress.
        return (request, Ok(None));
    }
    (request, Ok(Some(buffer)))
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because the boxed future in `Busy` has no `Debug`, and
        // because a caller debugging a body wants to know how much is left and
        // whether a read is in flight, not what a handle value is.
        let state = match self.state {
            State::Idle(_) => "idle",
            State::Busy(_) => "reading",
            State::Finished => "finished",
        };
        f.debug_struct("ResponseBody")
            .field("state", &state)
            .field("remaining", &self.remaining)
            .field("delivered", &self.delivered)
            .finish()
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = ResponseBodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ResponseBodyError>>> {
        loop {
            match std::mem::replace(&mut self.state, State::Finished) {
                State::Finished => {
                    // Fused: an ended body stays ended, and a failure is
                    // reported once rather than on every subsequent poll.
                    return Poll::Ready(None);
                }
                State::Idle(request) => {
                    if self.remaining == Some(0) {
                        // The response delivered everything it declared. HTTP
                        // says the message body is exactly that many bytes, so
                        // anything still on the socket is not part of it, and
                        // asking for more would block until the connection
                        // closed.
                        drop(request);
                        return Poll::Ready(None);
                    }
                    // The one place the request changes hands. It moves into
                    // the future, which gives it back when it resolves.
                    let cap = self.remaining;
                    self.state = State::Busy(Box::pin(read_once(request, cap)));
                }
                State::Busy(mut operation) => {
                    let step = match operation.as_mut().poll(context) {
                        Poll::Ready(step) => step,
                        Poll::Pending => {
                            // Put the *same* future back. Dropping it here is
                            // what parks the request; see the module docs.
                            self.state = State::Busy(operation);
                            return Poll::Pending;
                        }
                    };
                    let (request, outcome) = step;
                    match outcome {
                        Err(error) => {
                            drop(request);
                            return Poll::Ready(Some(Err(error)));
                        }
                        Ok(None) => {
                            drop(request);
                            return match self.shortfall() {
                                Some(error) => Poll::Ready(Some(Err(error))),
                                None => Poll::Ready(None),
                            };
                        }
                        Ok(Some(chunk)) => {
                            let read = chunk.len() as u64;
                            self.delivered += read;
                            if let Some(remaining) = self.remaining.as_mut() {
                                *remaining = remaining.saturating_sub(read);
                            }
                            self.state = State::Idle(request);
                            // `Bytes::from(Vec<u8>)` takes the allocation over
                            // rather than copying it.
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk)))));
                        }
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self.state, State::Finished) || self.remaining == Some(0)
    }

    fn size_hint(&self) -> SizeHint {
        match self.remaining {
            // What the response declared, less what has already been handed
            // over, which is what `SizeHint` is asking about.
            Some(remaining) => SizeHint::with_exact(remaining),
            // A chunked or close-delimited response never learns its length,
            // so the hint stays open for the body's whole life.
            None => SizeHint::default(),
        }
    }
}

/// A body that could not cross a thread boundary would be useless in most of
/// the places a response goes, and this is where that stops being true
/// silently.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<ResponseBody>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_that_cannot_carry_bytes_is_over_before_it_starts() {
        // Constructed without a request, because `empty` is only reachable
        // with one and the assertions here are about the reported shape.
        let body = ResponseBody {
            state: State::Finished,
            remaining: Some(0),
            delivered: 0,
        };
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));
        assert!(body.shortfall().is_none());
    }

    #[test]
    fn an_undelivered_remainder_is_a_truncation_and_names_both_numbers() {
        let body = ResponseBody {
            state: State::Finished,
            remaining: Some(7),
            delivered: 3,
        };
        assert!(matches!(
            body.shortfall(),
            Some(ResponseBodyError::Truncated {
                expected: 10,
                received: 3
            })
        ));
    }

    #[test]
    fn a_body_of_unknown_length_reports_an_open_hint_and_no_shortfall() {
        // The chunked and close-delimited case: nothing was promised, so
        // nothing can be owed.
        let body = ResponseBody {
            state: State::Finished,
            remaining: None,
            delivered: 99,
        };
        assert_eq!(body.size_hint().exact(), None);
        assert_eq!(body.size_hint().lower(), 0);
        assert!(body.shortfall().is_none());
    }

    #[test]
    fn the_size_hint_is_what_the_response_still_owes() {
        let body = ResponseBody {
            state: State::Finished,
            remaining: Some(4),
            delivered: 6,
        };
        assert_eq!(body.size_hint().exact(), Some(4));
    }

    #[test]
    fn a_body_that_still_owes_bytes_is_not_at_end_of_stream() {
        // The `Finished` fixtures above cannot show this, because a finished
        // body is at end of stream whatever it owes. This one is mid-read.
        let body = ResponseBody {
            state: State::Busy(Box::pin(std::future::pending())),
            remaining: Some(4),
            delivered: 6,
        };
        assert!(!body.is_end_stream());
    }
}
