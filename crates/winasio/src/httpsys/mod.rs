// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A safe, asynchronous wrapper over the Windows HTTP Server API (HTTP.sys).
//!
//! This is a building block, not a framework. It gives you everything needed to
//! interpret a request, read its body, compose a reply and send it, and leaves
//! the accept loop to you. Operations are built on [`crate::iocp`], so nothing
//! here depends on a particular async runtime.
//!
//! # A minimal server
//!
//! ```no_run
//! use std::sync::Arc;
//! use windows::core::HSTRING;
//! use winasio::httpsys::{
//!     HttpInitializer, RequestQueue, Response, ResponseHeader, ServerSession, UrlGroup,
//! };
//! use winasio::iocp::ThreadPool;
//!
//! # async fn run() -> windows::core::Result<()> {
//! let _http = HttpInitializer::new()?;
//! let session = ServerSession::new()?;
//! let group = UrlGroup::new(&session)?;
//!
//! let queue = Arc::new(RequestQueue::new(&ThreadPool)?);
//! queue.bind_url_group(&group)?;
//! group.add_url(&HSTRING::from("http://localhost:8080/demo/"))?;
//!
//! while let Ok(request) = queue.receive().await {
//!     // Borrowed from the request's own buffer; no allocation.
//!     let target = request.target().unwrap_or_default().to_owned();
//!
//!     let mut reply = Response::new(200);
//!     reply
//!         .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
//!         .add_body(format!("you asked for {target}").into_bytes());
//!
//!     queue.send(request.id(), reply).await.0?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! A complete, runnable version lives in the test crate's
//! `examples/httpsys_server.rs`.
//!
//! # Invariants and obligations
//!
//! * **A received [`Request`] is an ordinary value.** HTTP.sys writes the URL,
//!   headers and addresses into the tail of a buffer and stores pointers to that
//!   tail in the header at its start. Because the buffer is a heap allocation of
//!   its own, moving a `Request` moves a pointer rather than the bytes, so those
//!   pointers stay valid. No `Pin` is involved.
//!
//! * **Accessors borrow, they do not copy.** [`Request::raw_target`],
//!   [`Request::header`] and the rest return slices into the request's buffer,
//!   tied to its lifetime. Reading a request allocates nothing.
//!
//!   The one exception is the operating system's pre-parsed URL components,
//!   which it supplies as UTF-16. Those are available borrowed as `&[u16]` --
//!   [`Request::path_wide`] and friends -- or converted with the `*_lossy`
//!   forms, which do allocate.
//!
//! * **Request and reply header names are different types.** HTTP.sys numbers
//!   the two sets differently, and every identifier from 20 to 29 means a
//!   *different header* on each side. [`RequestHeader`] and [`ResponseHeader`]
//!   are therefore not interchangeable, and the compiler enforces it.
//!
//! * **A reply may be built and moved freely before it is sent.** Every pointer
//!   inside it is derived at send time, once the operation has reached its final
//!   address. Values that are compile-time constants cost no allocation.
//!
//! * **State comes back.** Sends and body operations resolve to an
//!   [`OpResult`](crate::iocp::OpResult), which carries the outcome *and* the
//!   reply or buffer that was handed in -- on failure as well as success.
//!
//! * **The completion backend is chosen per queue.** [`RequestQueue::new`]
//!   registers the HTTP.sys handle with the registrar you pass, so the same
//!   request and reply API works with either
//!   [`ThreadPool`](crate::iocp::ThreadPool) or
//!   [`Proactor`](crate::iocp::Proactor). A normal server should still use the
//!   Win32 thread pool: request queues are usually shared as `Arc`s across
//!   worker tasks on a multi-threaded runtime, while `Proactor` is `!Send`.
//!   Proactor-backed queues are for single-threaded loops, and that affinity is
//!   derived from the submitter type -- `RequestQueue<Rc<Proactor>>` is not
//!   `Send`.
//!
//! * **Over-large requests are retried, then discarded.** A request whose
//!   metadata exceeds the configured capacity is retried at a larger size, up to
//!   [`ReceiveConfig::max_retries`]. Beyond that the library **discards it** and
//!   reports [`ReceiveError::TooLarge`]. Discarding happens here rather than in
//!   your code deliberately: a queued request that cannot be delivered would be
//!   returned by every subsequent receive, so an accept loop that logged the
//!   error and continued would spin forever. [`RequestQueue::reject`] remains
//!   available for discarding a request you have already received.
//!
//! * **Closing is how a server stops.** [`RequestQueue::close`] takes `&self`,
//!   so a queue shared as an `Arc` can be shut down while workers are blocked in
//!   [`RequestQueue::receive`]; their receives then resolve with an error.
//!   Closing cancels outstanding operations first. If an operation still owns a
//!   handle clone, the HTTP.sys close is deferred to that clone's drop; otherwise
//!   `close` reports the real close status.
//!
//! * **Caller obligations the API does not enforce.** The operating system
//!   forbids two sends running concurrently on the *same* request identifier, so
//!   a request is best owned end to end by whoever received it. Serving
//!   *different* requests from many threads at once is fine. The operating
//!   system also appends its own product token to whatever `Server` header the
//!   application sets, so that header is not observed verbatim by a client.
//!
//! # Allocation budget
//!
//! Serving one request end to end costs **three** allocations: the receive
//! operation's record, the request's metadata buffer, and the send operation's
//! record. That figure does not change with the number of headers read or set.
//!
//! Beyond that: a receive retry adds two, each body operation adds one, and a
//! reply exceeding [`INLINE_UNKNOWN_HEADERS`] unrecognised headers or
//! [`INLINE_CHUNKS`] body chunks adds **two** for that kind — the overflow
//! storage, plus the contiguous descriptor array the operating system requires.
//! These figures are measured by the test suite, not merely asserted here.

mod error;
mod header;
mod init;
mod ops;
mod queue;
mod request;
mod response;
mod session;

pub use header::{RequestHeader, ResponseHeader};
pub use init::HttpInitializer;
pub use ops::body::ReceiveBody;
pub use ops::cancel::CancelRequest;
pub use ops::receive::ReceiveRequest;
pub use ops::send::{SendBody, SendResponse};
pub use queue::{ReceiveConfig, ReceiveError, RequestQueue};
pub use request::{Method, Request, RequestId, UnknownHeaders, MIN_CAPACITY};
pub use response::{Response, Value, INLINE_CHUNKS, INLINE_UNKNOWN_HEADERS};
pub use session::{ServerSession, UrlGroup};

/// The element the request buffer is allocated as.
///
/// Its alignment must cover `HTTP_REQUEST_V2`'s; see [`Request`].
pub(crate) type BufferUnit = u64;

// `align_of::<HTTP_REQUEST_V2>()` is 8 on x86_64, measured during development.
// This turns any future divergence -- a new architecture, or a bindings change
// -- into a build failure rather than silent undefined behaviour. Misalignment
// is reported by the operating system as `ERROR_NOACCESS`, but forming an
// under-aligned pointer to the structure is undefined behaviour regardless.
const _: () = assert!(
    std::mem::align_of::<BufferUnit>()
        >= std::mem::align_of::<windows::Win32::Networking::HttpServer::HTTP_REQUEST_V2>(),
    "the request buffer's element type is not aligned enough for HTTP_REQUEST_V2"
);
