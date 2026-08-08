// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A server over [`winasio::httpsys`], shaped around [`tower_service::Service`].
//!
//! Accept an [`http::Request`] whose body implements [`http_body::Body`], hand
//! it to a [`Service`](tower_service::Service), and send back the
//! [`http::Response`] it returns. Those are the types axum and the `tower-http`
//! middleware ecosystem are built on, so an axum `Router` — or anything wrapped
//! in a `tower` layer — is servable here directly.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use std::convert::Infallible;
//! use std::future::{ready, Ready};
//! use std::task::{Context, Poll};
//!
//! use bytes::Bytes;
//! use http_body_util::Full;
//! use winasio::iocp::ThreadPool;
//! use winasio_util::{IncomingBody, Server, ServerSession};
//!
//! /// An ordinary tower service. An axum `Router` would serve just as well.
//! #[derive(Clone)]
//! struct Hello;
//!
//! impl tower_service::Service<http::Request<IncomingBody>> for Hello {
//!     type Response = http::Response<Full<Bytes>>;
//!     type Error = Infallible;
//!     type Future = Ready<Result<Self::Response, Infallible>>;
//!
//!     fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
//!         Poll::Ready(Ok(()))
//!     }
//!
//!     fn call(&mut self, _request: http::Request<IncomingBody>) -> Self::Future {
//!         ready(Ok(http::Response::new(Full::new(Bytes::from("hi")))))
//!     }
//! }
//!
//! let session = ServerSession::new()?;
//! let server = Server::builder(&session)
//!     .url("http://localhost:8080/demo/")
//!     .build(&ThreadPool)?;
//!
//! // No runtime anywhere: `block_on` is a bare single-threaded executor.
//! futures::executor::block_on(server.serve_one(&mut Hello))?;
//! # Ok(())
//! # }
//! ```
//!
//! # D1. Why this is generic over the completion backend
//!
//! `RequestQueue<S: Submitter>` is generic one crate down, and the two backends
//! are not interchangeable: `ThreadPoolIo` is `Send + Sync`, `Rc<Proactor>` is
//! neither, and the `Proactor` exists precisely so that a single-threaded loop
//! can drive completions without any thread pool at all. A crate whose selling
//! point is that it needs no runtime should not be the thing that forces one.
//!
//! So [`Server`] is generic over [`Backend`], a sealed trait with exactly two
//! implementations. It exists to give the boxed body-read future a name per
//! backend — see [`backend`] for the full argument — and `S` defaults to
//! [`ThreadPoolIo`], so ordinary code never writes
//! it.
//!
//! Rejected alternatives:
//!
//! * **Fix `ThreadPoolIo`.** Simpler by one trait, and it silently deletes the
//!   single-threaded backend that the layer below went to trouble to support.
//! * **Make the body's future `Pin<Box<dyn Future>>` with no `Send`.** Works for
//!   both backends and makes *every* body `!Send`, so nothing can be spawned —
//!   which is the whole concurrency story this crate offers callers.
//! * **A public, unsealed `Backend`.** The trait names an implementation
//!   detail. A third backend would arrive from `winasio`, not from downstream,
//!   and sealing keeps the associated type changeable.
//!
//! # D2. Why `Server` has a lifetime
//!
//! `UrlGroup<'a>` borrows its [`ServerSession`], so a type owning both would be
//! self-referential. The caller therefore owns the session and lends it:
//! `Server::builder(&session)`. The queue is owned internally behind an `Arc`,
//! because bodies and shutdown handles need to outlive borrows of the server.
//!
//! The subsystem initialisation (`HttpInitialize`) lives in [`ServerSession`]
//! rather than in [`Server`], because a session cannot be created before it has
//! run — measured, the constructor fails with a bare `ERROR_INVALID_HANDLE` and
//! no hint as to why. Since the caller has to create the session first, the
//! initialisation has to come with it.
//!
//! Rejected: `unsafe` lifetime erasure, which this crate has so far managed
//! entirely without; `Box::leak`, which leaks an HTTP.sys session per server and
//! would accumulate across a test suite; a self-referential-struct dependency,
//! which is a lot of machinery to avoid passing one reference.
//!
//! # D3. Who spawns
//!
//! Nothing here does. No task, no thread, no reactor, no runtime. Concurrency
//! is entirely the caller's, and the API is shaped so that both shapes work:
//!
//! * **One at a time.** [`Server::serve_one`] and [`Server::serve`] take
//!   `&mut S` and drive requests sequentially. That is all a single-threaded
//!   `block_on` loop needs.
//! * **Many at once.** [`Server::accept`] hands back an [`Accepted`], which owns
//!   everything it needs and is `Send + 'static` on the thread-pool backend. The
//!   caller clones its service and moves the pair onto whatever it likes.
//!
//! # D4. `poll_ready`, and what a clone means
//!
//! [`tower_service::Service`] has readiness and `&mut self`; hyper's service has
//! neither. Both are honoured rather than skipped:
//!
//! * The sequential driver awaits `poll_ready` **before it accepts**. A service
//!   that is not ready therefore stops this crate pulling requests out of the
//!   kernel queue, which is backpressure that does something. Checking
//!   readiness after accepting would leave the request in hand with nowhere to
//!   put it, which is the failure mode `poll_ready` exists to prevent.
//! * The concurrent driver requires the caller to hand [`Accepted::serve`] a
//!   service **it owns** — in practice a clone, which is how tower services are
//!   used everywhere. `serve` then awaits readiness on that clone and calls it.
//!   The order matters: a reservation belongs to the clone that made it, so
//!   cloning after awaiting readiness would use a reservation the clone does not
//!   have.
//!
//! # Invariants and obligations
//!
//! * **The crate owns response framing.** `Transfer-Encoding` is this crate's
//!   to set; supplying one is [`BodyError::FramingHeaderNotAllowed`]. A
//!   `Content-Length` *may* be supplied — an `axum::Router` sets one, so
//!   refusing it would make a real router unservable — and is then checked
//!   against the body's [`size_hint`](http_body::Body::size_hint) rather than
//!   trusted, with [`BodyError::LengthMismatch`] if the two disagree and
//!   [`ResponseError::BadContentLength`] if it is unusable. Measured, HTTP.sys computes
//!   a length only for a fully buffered reply and does **no** framing at all for
//!   a streamed one — not even on a keep-alive connection, where the result is
//!   an undelimited body running into the next response.
//! * **A reply that under-delivers is an error, not a short message.** Measured,
//!   HTTP.sys accepts five bytes against a declared twenty and emits a silently
//!   truncated message. This crate counts and reports
//!   [`BodyError::LengthMismatch`] instead.
//! * **A body that may not exist is not sent.** Measured, HTTP.sys sends the
//!   body of a `HEAD` reply and of a `204` that was given one. This crate
//!   suppresses both, and never polls a body it will not send.
//! * **Repeated reply headers survive; repeated request headers cannot.**
//!   Measured, HTTP.sys folds repeated inbound headers into one comma-joined
//!   value before this crate sees them. There is nothing here to preserve.
//! * **Nothing is spawned and nothing is swallowed.** A service that returns
//!   `Err` gets a bodiless `500` on the wire and the error is still returned as
//!   [`ServeError::Service`].
//! * **A dropped [`Responder`] answers nothing.** The peer waits for HTTP.sys's
//!   own request timeout. Other requests on the queue are unaffected —
//!   measured, and the invariant that matters when a handler panics. The one
//!   place this crate abandons a request itself is when the head cannot be
//!   expressed as an [`http::Request`] at all, and even there the peer gets a
//!   bodiless `400` before [`AcceptError::MalformedRequest`] is returned.
//! * **Shutdown is abrupt for work in flight.** [`Server::shutdown`] closes the
//!   queue. Measured, that surfaces two ways: a `receive` that was already
//!   waiting fails with `ERROR_OPERATION_ABORTED`, and anything started
//!   afterwards fails with `ERROR_INVALID_HANDLE`. Both are reported as
//!   [`AcceptError::is_queue_closed`], promptly and without hanging. Requests already
//!   received keep their heads readable but can no longer be answered.
//!
//! # What this module is not
//!
//! * **No TLS configuration.** HTTP.sys binds certificates out of band, through
//!   `netsh http add sslcert`. Nothing here could help.
//! * **No routing, no middleware.** That is what a `tower::Service` is.
//! * **No connection management.** Measured, a `Connection: close` set by the
//!   application is a lie in both directions: through the known slot HTTP.sys
//!   drops it, through the unknown list it reaches the wire and HTTP.sys keeps
//!   the connection alive anyway. Persistence is the platform's decision.
//! * **No HTTP/2 or WebSocket specifics, no server push, no authentication.**
//!
//! # Trivially free, so not built
//!
//! * **`Expect: 100-continue`.** Measured, HTTP.sys sends the interim response
//!   itself before the application reads a byte. The header is still visible to
//!   a handler that cares.
//! * **De-chunking request bodies.** Measured, HTTP.sys does it; a chunked
//!   request arrives with its `Transfer-Encoding` header intact and its body
//!   already decoded.
//! * **Detecting a truncated request body.** Measured, HTTP.sys reports it as
//!   `ERROR_OPERATION_ABORTED` for both a graceful close and a reset — unlike
//!   WinHTTP, which is why the client half needed [`ResponseBodyError::Truncated`](crate::ResponseBodyError::Truncated) and
//!   this one does not.

pub mod backend;
mod head;
mod incoming;

use std::future::Future;
use std::sync::Arc;

use bytes::Buf;
use http::{HeaderMap, Request as HttpRequest, Response as HttpResponse, StatusCode};
use http_body::Body;
use winasio::httpsys::{
    HttpInitializer, ReceiveConfig, ReceiveError, Request, RequestId, RequestQueue, Response,
    ResponseHeader, ServerSession as PlatformSession, UrlGroup,
};
use winasio::iocp::{OpResult, Registrar, ThreadPoolIo};
use windows::core::HSTRING;

pub use backend::Backend;
pub use head::ConnectionInfo;
pub use incoming::IncomingBody;

use crate::error::{
    platform, AcceptError, BodyError, PlatformError, RequestReason, ResponseError, SendStage,
    ServeError, ServerOperation,
};

/// The largest piece of a response body written in one call.
///
/// A frame larger than this is split. HTTP.sys copies what it is given, so this
/// only bounds how much is in flight at once.
const MAX_SEND: usize = 64 * 1024;

/// The HTTP.sys session a [`Server`] listens under.
///
/// Also owns the subsystem initialisation, because `HttpInitialize` has to have
/// run before a session can be created at all — measured, getting that order
/// wrong yields a bare `ERROR_INVALID_HANDLE` from the session constructor with
/// nothing to say why. The subsystem is refcounted per process, so bundling the
/// two costs nothing and removes a way to hold the API wrong.
///
/// Owned by the caller rather than by the [`Server`] because the URL group
/// borrows it; see [D2](self#d2-why-server-has-a-lifetime).
pub struct ServerSession {
    // Drop order is field order, and it is load-bearing: the session must be
    // closed before the subsystem it lives in is released.
    inner: PlatformSession,
    _initializer: HttpInitializer,
}

impl ServerSession {
    /// Start the HTTP Server API and create a session in it.
    pub fn new() -> Result<ServerSession, PlatformError> {
        let initializer = HttpInitializer::new().map_err(platform(ServerOperation::Initialize))?;
        let inner = PlatformSession::new().map_err(platform(ServerOperation::CreateSession))?;
        Ok(ServerSession {
            inner,
            _initializer: initializer,
        })
    }

    /// Whether HTTP.sys can send HTTP/2 response trailers on this host (M3).
    ///
    /// Delegates to [`HttpInitializer::supports_response_trailers`]; holding a
    /// `ServerSession` proves the subsystem is initialised, which is the
    /// precondition that call has (a pre-init query is a false negative). Used
    /// to gate the trailer-capable response path — see
    /// [`Responder::send_streaming`].
    pub fn supports_response_trailers(&self) -> bool {
        self._initializer.supports_response_trailers()
    }
}

impl std::fmt::Debug for ServerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSession").finish_non_exhaustive()
    }
}

/// Configuration for a [`Server`], before it exists.
///
/// Created by [`Server::builder`].
pub struct ServerBuilder<'a> {
    session: &'a ServerSession,
    urls: Vec<HSTRING>,
    receive: ReceiveConfig,
}

impl<'a> ServerBuilder<'a> {
    /// Listen on one more URL prefix.
    ///
    /// The prefix is HTTP.sys's own, not a route: `http://localhost:8080/demo/`
    /// claims everything under `/demo/`. Binding one usually requires either an
    /// elevated process or a `netsh http add urlacl` reservation; the failure
    /// arrives from [`build`](ServerBuilder::build) as
    /// [`PlatformError`] with [`ServerOperation::AddUrl`].
    pub fn url(mut self, url: &str) -> Self {
        self.urls.push(HSTRING::from(url));
        self
    }

    /// How the receive loop sizes and re-sizes its request buffer.
    ///
    /// Rarely needed. The default grows the buffer and retries once, then
    /// discards a request that still will not fit — see
    /// [`AcceptError::RequestTooLarge`] for why the discard happens down there.
    pub fn receive_config(mut self, receive: ReceiveConfig) -> Self {
        self.receive = receive;
        self
    }

    /// Create the URL group and request queue, and bind them together.
    ///
    /// `registrar` is the completion backend: `&ThreadPool` for the ordinary
    /// case, or `&Rc<Proactor>` for a single-threaded loop.
    pub fn build<R, S>(self, registrar: &R) -> Result<Server<'a, S>, PlatformError>
    where
        R: Registrar<Io = S>,
        S: Backend,
    {
        let group = UrlGroup::new(&self.session.inner)
            .map_err(platform(ServerOperation::CreateUrlGroup))?;
        for url in &self.urls {
            group
                .add_url(url)
                .map_err(platform(ServerOperation::AddUrl))?;
        }
        let queue = RequestQueue::with_config(registrar, self.receive)
            .map_err(platform(ServerOperation::CreateQueue))?;
        queue
            .bind_url_group(&group)
            .map_err(platform(ServerOperation::BindUrlGroup))?;

        Ok(Server {
            _group: group,
            queue: Arc::new(queue),
            trailers_supported: self.session.supports_response_trailers(),
        })
    }
}

/// A listener bound to one or more URL prefixes.
///
/// See the [module documentation](self) for the design decisions behind its
/// shape, in particular why it carries a lifetime and a backend parameter.
pub struct Server<'a, S: Backend = ThreadPoolIo> {
    /// Held for its `Drop`: closing the group is what stops HTTP.sys routing
    /// new connections here.
    _group: UrlGroup<'a>,
    queue: Arc<RequestQueue<S>>,
    /// Whether the host can send HTTP/2 response trailers (M3), queried once at
    /// build time and copied into each [`Responder`] so the trailer-capable
    /// path can be gated without re-probing per request.
    trailers_supported: bool,
}

impl<S: Backend> std::fmt::Debug for Server<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("open", &self.queue.is_open())
            .finish()
    }
}

impl<'a> Server<'a, ThreadPoolIo> {
    /// Start describing a server that will use `session`.
    ///
    /// The session is borrowed rather than owned; see the
    /// [module documentation](self) for why.
    pub fn builder(session: &'a ServerSession) -> ServerBuilder<'a> {
        ServerBuilder {
            session,
            urls: Vec::new(),
            receive: ReceiveConfig::default(),
        }
    }
}

impl<S: Backend> Server<'_, S> {
    /// The request queue underneath, for anything this wrapper does not cover.
    pub fn queue(&self) -> &Arc<RequestQueue<S>> {
        &self.queue
    }

    /// Whether the queue is still open.
    pub fn is_open(&self) -> bool {
        self.queue.is_open()
    }

    /// Close the request queue.
    ///
    /// Measured, and documented rather than smoothed over: a `receive` that was
    /// already waiting fails promptly with `ERROR_OPERATION_ABORTED`, and every
    /// operation started afterwards — including `read_body` and `send` on a
    /// request that had already been accepted — fails with
    /// `ERROR_INVALID_HANDLE`. Both read as [`AcceptError::is_queue_closed`]. A
    /// request already in a handler keeps its head — the accessors read a buffer
    /// this process owns — but can no longer be answered.
    ///
    /// There is deliberately no drain: waiting for in-flight work would mean
    /// this crate tracking it, and tracking it would mean owning the concurrency
    /// it has promised not to own. A caller that wants a drain stops accepting,
    /// finishes what it holds, and then calls this.
    pub fn shutdown(&self) -> Result<(), PlatformError> {
        self.queue
            .close()
            .map_err(platform(ServerOperation::Shutdown))
    }

    /// A handle that can shut this server down from elsewhere.
    ///
    /// Cloneable, and `Send + Sync` on the thread-pool backend, so it can be
    /// parked in a signal handler or another task.
    pub fn shutdown_handle(&self) -> ShutdownHandle<S> {
        ShutdownHandle {
            queue: Arc::clone(&self.queue),
        }
    }

    /// Wait for the next request.
    ///
    /// The returned [`Accepted`] owns everything it needs, so it can be moved
    /// onto a task the caller spawned.
    pub async fn accept(&self) -> Result<Accepted<S>, AcceptError> {
        let request = match self.queue.receive().await {
            Ok(request) => request,
            Err(ReceiveError::TooLarge {
                attempted_capacity, ..
            }) => {
                // The request is already gone; the layer below discards it
                // rather than leave an accept loop re-receiving it forever.
                return Err(AcceptError::RequestTooLarge {
                    capacity: attempted_capacity,
                });
            }
            Err(ReceiveError::Failed(error)) => {
                return Err(AcceptError::Receive(error));
            }
        };
        let id = request.id();
        match Accepted::from_platform(Arc::clone(&self.queue), request, self.trailers_supported) {
            Ok(accepted) => Ok(accepted),
            Err(error) => {
                // The request has already left the kernel queue, so nothing else
                // can answer it. Leaving it would make the peer wait out
                // HTTP.sys's request timeout for a request this crate has
                // already judged, so it gets the `400` its head earned. The
                // error is still returned: a failure to convert is the caller's
                // to see, and a failure to say so is discarded because the
                // interesting failure is the first one.
                let responder = Responder {
                    queue: Arc::clone(&self.queue),
                    id,
                    method: http::Method::GET,
                    trailers_supported: self.trailers_supported,
                };
                let _ = responder.send_status(StatusCode::BAD_REQUEST).await;
                Err(error)
            }
        }
    }

    /// Accept one request and answer it with `service`.
    ///
    /// Readiness is awaited **before** accepting, so a service that is not ready
    /// stops this crate pulling requests out of the kernel queue rather than
    /// accepting work it cannot place. See the [module documentation](self).
    pub async fn serve_one<Svc, B>(&self, service: &mut Svc) -> Result<(), ServeError>
    where
        Svc: tower_service::Service<HttpRequest<IncomingBody<S>>, Response = HttpResponse<B>>,
        Svc::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
        B: Body,
        B::Data: Buf,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        ready(service).await?;
        let accepted = self.accept().await?;
        let (request, responder) = accepted.into_parts();
        responder.dispatch(service.call(request)).await
    }

    /// Answer requests one at a time until the queue closes.
    ///
    /// Returns `Ok(())` when [`shutdown`](Server::shutdown) has been called, and
    /// the first error otherwise — including a malformed request or a failing
    /// service, neither of which is swallowed. A caller who wants to log and
    /// carry on writes the loop themselves over [`serve_one`](Server::serve_one)
    /// and uses [`AcceptError::is_queue_closed`] to spot the exit:
    ///
    /// ```no_run
    /// # async fn example<S: winasio_util::Backend, Svc>(
    /// #     server: &winasio_util::Server<'_, S>,
    /// #     service: &mut Svc,
    /// # ) where
    /// #     Svc: tower_service::Service<
    /// #         http::Request<winasio_util::IncomingBody<S>>,
    /// #         Response = http::Response<http_body_util::Empty<bytes::Bytes>>,
    /// #     >,
    /// #     Svc::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    /// # {
    /// loop {
    ///     match server.serve_one(service).await {
    ///         Ok(()) => {}
    ///         Err(error) if error.is_queue_closed() => break,
    ///         Err(error) => eprintln!("request failed: {error}"),
    ///     }
    /// }
    /// # }
    /// ```
    pub async fn serve<Svc, B>(&self, service: &mut Svc) -> Result<(), ServeError>
    where
        Svc: tower_service::Service<HttpRequest<IncomingBody<S>>, Response = HttpResponse<B>>,
        Svc::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
        B: Body,
        B::Data: Buf,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        loop {
            match self.serve_one(service).await {
                Ok(()) => {}
                Err(error) if error.is_queue_closed() => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

/// Await a service's readiness.
async fn ready<Svc, R>(service: &mut Svc) -> Result<(), ServeError>
where
    Svc: tower_service::Service<R>,
    Svc::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // Hand-written rather than pulled from `tower`'s `ServiceExt`, so that the
    // dependency stays the trait-only `tower-service`.
    std::future::poll_fn(|context| service.poll_ready(context))
        .await
        .map_err(|error| ServeError::Service(error.into()))
}

/// Shuts a [`Server`] down from somewhere else.
///
/// Obtained from [`Server::shutdown_handle`].
pub struct ShutdownHandle<S: Backend = ThreadPoolIo> {
    queue: Arc<RequestQueue<S>>,
}

impl<S: Backend> Clone for ShutdownHandle<S> {
    fn clone(&self) -> Self {
        ShutdownHandle {
            queue: Arc::clone(&self.queue),
        }
    }
}

impl<S: Backend> std::fmt::Debug for ShutdownHandle<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownHandle")
            .field("open", &self.queue.is_open())
            .finish()
    }
}

impl<S: Backend> ShutdownHandle<S> {
    /// Close the request queue. See [`Server::shutdown`].
    pub fn shutdown(&self) -> Result<(), PlatformError> {
        self.queue
            .close()
            .map_err(platform(ServerOperation::Shutdown))
    }
}

/// A request that has been accepted and not yet answered.
///
/// Owns everything it needs — no borrow of the [`Server`] survives here — so it
/// can be moved onto a task the caller spawned. On the thread-pool backend it is
/// `Send + 'static`.
pub struct Accepted<S: Backend = ThreadPoolIo> {
    request: HttpRequest<IncomingBody<S>>,
    responder: Responder<S>,
}

impl<S: Backend> std::fmt::Debug for Accepted<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accepted")
            .field("method", self.request.method())
            .field("uri", self.request.uri())
            .field("responder", &self.responder)
            .finish()
    }
}

impl<S: Backend> Accepted<S> {
    fn from_platform(
        queue: Arc<RequestQueue<S>>,
        request: Request,
        trailers_supported: bool,
    ) -> Result<Accepted<S>, AcceptError> {
        let head = head::to_http(&request)?;
        let declared = crate::headers::content_length(&head.headers);
        let body = incoming::body_for(
            Arc::clone(&queue),
            request.id(),
            request.has_more_body(),
            declared,
        );

        let responder = Responder {
            queue,
            id: request.id(),
            method: head.method.clone(),
            trailers_supported,
        };

        let mut builder = HttpRequest::builder()
            .method(head.method)
            .uri(head.uri)
            .version(head.version);
        // `headers_mut` is `None` only if one of the above failed, and each was
        // validated on the way in.
        if let Some(headers) = builder.headers_mut() {
            *headers = head.headers;
        }
        if let Some(extensions) = builder.extensions_mut() {
            extensions.insert(head.info);
        }
        let request = builder
            .body(body)
            .map_err(|error| AcceptError::MalformedRequest {
                reason: RequestReason::Target,
                value: error.to_string(),
            })?;

        Ok(Accepted { request, responder })
    }

    /// The request, for inspection before it is dispatched.
    pub fn request(&self) -> &HttpRequest<IncomingBody<S>> {
        &self.request
    }

    /// Split into the request and the thing that answers it.
    pub fn into_parts(self) -> (HttpRequest<IncomingBody<S>>, Responder<S>) {
        (self.request, self.responder)
    }

    /// Answer this request with a service the caller owns.
    ///
    /// Takes the service by value because a concurrent caller cannot share
    /// `&mut` across tasks; the idiom is to clone per request, which is what
    /// tower services are for. Readiness is awaited on the service passed in —
    /// see the [module documentation](self) on why that order matters.
    pub async fn serve<Svc, B>(self, mut service: Svc) -> Result<(), ServeError>
    where
        Svc: tower_service::Service<HttpRequest<IncomingBody<S>>, Response = HttpResponse<B>>,
        Svc::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
        B: Body,
        B::Data: Buf,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (request, responder) = self.into_parts();
        ready(&mut service).await?;
        responder.dispatch(service.call(request)).await
    }
}

/// The right to answer one request, exactly once.
///
/// Dropping it without sending answers nothing: the peer waits for HTTP.sys's
/// own request timeout and then sees the connection go. That is what happens
/// when a handler panics, and measured, it leaves the queue and every other
/// request on it entirely usable.
pub struct Responder<S: Backend = ThreadPoolIo> {
    queue: Arc<RequestQueue<S>>,
    id: RequestId,
    /// Kept because the framing rules depend on it: a `HEAD` reply declares a
    /// length and sends no body.
    method: http::Method,
    /// Whether the host can send HTTP/2 response trailers (M3). Copied from the
    /// [`Server`] so [`send_streaming`](Self::send_streaming) can gate the
    /// trailer chunk without re-probing per request.
    trailers_supported: bool,
}

impl<S: Backend> std::fmt::Debug for Responder<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Responder")
            .field("request", &self.id.get())
            .field("method", &self.method)
            .finish()
    }
}

impl<S: Backend> Responder<S> {
    /// The request this will answer.
    pub fn request_id(&self) -> RequestId {
        self.id
    }

    /// Refuse the request without answering it.
    ///
    /// Measured: the peer receives nothing at all and the connection is closed.
    /// Useful for a request that should not get so much as a status code.
    pub async fn reject(self) -> Result<(), PlatformError> {
        self.queue
            .reject(self.id)
            .await
            .map_err(platform(ServerOperation::Reject))
    }

    /// Send a response.
    ///
    /// Framing is chosen from `body.size_hint()`; see the
    /// [module documentation](self) for the rules and the measurements behind
    /// them.
    pub async fn send<B>(self, response: HttpResponse<B>) -> Result<(), ResponseError>
    where
        B: Body,
        B::Data: Buf,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (parts, body) = response.into_parts();
        self.send_parts(parts.status, &parts.headers, body).await
    }

    /// Send a response as raw HTTP/2 DATA frames, ending with trailers (M3).
    ///
    /// This is the gRPC-shaped path, and it is deliberately **not**
    /// [`send`](Self::send). `send` chooses framing from the body's size hint:
    /// a known length takes a `Content-Length` fast path that ends the response
    /// with the body, and an unknown length takes `Transfer-Encoding: chunked`
    /// framed by hand. Both are wrong for gRPC:
    ///
    /// * The `Content-Length` fast path terminates before trailers, so the
    ///   `grpc-status` trailer a unary reply carries would be dropped.
    /// * Manual chunked framing downgrades the connection to HTTP/1.1 —
    ///   measured by Microsoft's own `WinHttpHandler` on the client half (M7),
    ///   and the same rule holds for a hand-framed body on the server half.
    ///   gRPC requires HTTP/2.
    ///
    /// So this method takes neither branch. It sends the head with more data to
    /// follow, writes each body data frame **raw** (HTTP.sys wraps it in an
    /// HTTP/2 DATA frame itself — no `Content-Length`, no `Transfer-Encoding`,
    /// no hand framing), and terminates with the body's trailers as an HTTP/2
    /// trailers frame via [`RequestQueue::send_trailers`](crate).
    ///
    /// The caller selects this path **explicitly** rather than by sniffing the
    /// request version. Sniffing would be circular: the M2 defect meant an
    /// HTTP/2 request was mis-read as HTTP/1.1, and a framing choice hung off
    /// that read would inherit the bug. `winasio-tonic` knows it is speaking
    /// gRPC and asks for this framing outright.
    ///
    /// # Trailer support
    ///
    /// If the host cannot send trailers ([`ServerSession::supports_response_trailers`]
    /// is `false` — old HTTP.sys), the body is still streamed but its trailers
    /// are dropped and the response is ended with an empty terminal frame. A
    /// gRPC peer reads that as a missing `grpc-status`; there is no way to
    /// deliver one on a host that cannot frame trailers, so the honest outcome
    /// is to send what can be sent rather than fail a response that is otherwise
    /// complete. On Windows 11 / Server 2022+ trailers are supported (M3).
    pub async fn send_streaming<B>(self, response: HttpResponse<B>) -> Result<(), ResponseError>
    where
        B: Body,
        B::Data: Buf,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (parts, body) = response.into_parts();
        let reply = head::from_http(parts.status, &parts.headers)?;

        // Head first, with more data to follow. No length and no encoding are
        // declared: HTTP.sys frames an HTTP/2 response body itself once the head
        // says nothing about framing, which is exactly what gRPC needs.
        let OpResult(sent, _) = self.queue.send_partial(self.id, reply).await;
        sent.map_err(ResponseError::send(SendStage::Head))?;

        let mut body = std::pin::pin!(body);
        let mut trailers: Option<HeaderMap> = None;
        loop {
            match next_frame(body.as_mut()).await? {
                Some(Frame::Data(bytes)) => {
                    // Raw, un-framed. `write` never marks this last: the response
                    // is closed by the trailers frame (or the empty terminal
                    // frame below), never by a body chunk.
                    if !bytes.is_empty() {
                        self.write(bytes, false).await?;
                    }
                }
                Some(Frame::Trailers(map)) => {
                    // http_body yields trailers as the final frame; fold in case
                    // a body somehow produces more than one map.
                    trailers = Some(match trailers {
                        Some(mut existing) => {
                            existing.extend(map);
                            existing
                        }
                        None => map,
                    });
                }
                None => break,
            }
        }

        match trailers {
            Some(map) if self.trailers_supported && !map.is_empty() => {
                let pairs = encode_trailers(&map);
                let OpResult(sent, _) = self.queue.send_trailers(self.id, pairs).await;
                sent.map_err(ResponseError::send(SendStage::Trailers))?;
                Ok(())
            }
            _ => {
                // No trailers, or a host that cannot frame them: end the body.
                self.write(Vec::new(), true).await
            }
        }
    }

    /// Run a service's future and put whatever it produced on the wire.
    async fn dispatch<F, B, E>(self, call: F) -> Result<(), ServeError>
    where
        F: Future<Output = Result<HttpResponse<B>, E>>,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
        B: Body,
        B::Data: Buf,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        match call.await {
            Ok(response) => self.send(response).await.map_err(ServeError::Response),
            Err(error) => {
                // A bodiless 500 so the peer is not left waiting, then the
                // error itself: the crate does not decide that a failed handler
                // is nothing worth reporting. A failure to send the 500 is
                // discarded because the interesting failure is the first one.
                let _ = self.send_status(StatusCode::INTERNAL_SERVER_ERROR).await;
                Err(ServeError::Service(error.into()))
            }
        }
    }

    /// Send a status line and nothing else.
    async fn send_status(self, status: StatusCode) -> Result<(), ResponseError> {
        let mut reply = head::from_http(status, &HeaderMap::new())?;
        reply.set_header(ResponseHeader::CONTENT_LENGTH, b"0".to_vec());
        self.finish(reply).await
    }

    async fn send_parts<B>(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: B,
    ) -> Result<(), ResponseError>
    where
        B: Body,
        B::Data: Buf,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let mut reply = head::from_http(status, headers)?;
        let declared = head::declared_length(headers)?;
        let exact = body.size_hint().exact();

        if !head::may_send_body(&self.method, status) {
            // Measured: HTTP.sys would send the body regardless, so it is
            // dropped here without being polled. A `HEAD` still declares the
            // length its `GET` would have had; a 1xx/204/304 declares nothing.
            //
            // A caller's declaration wins over the size hint here, and is not
            // checked against it, because the `HEAD` idiom is exactly to pair a
            // real `Content-Length` with an empty body.
            drop(body);
            if head::may_declare_length(status) {
                if let Some(length) = declared.or(exact) {
                    reply.set_header(
                        ResponseHeader::CONTENT_LENGTH,
                        length.to_string().into_bytes(),
                    );
                }
            }
            return self.finish(reply).await;
        }

        // A declaration and a size hint that disagree describe two different
        // messages; there is no honest way to pick one, so neither is sent.
        if let (Some(declared), Some(exact)) = (declared, exact) {
            if declared != exact {
                return Err(BodyError::LengthMismatch {
                    declared,
                    actual: exact,
                }
                .into());
            }
        }
        let length = exact.or(declared);

        let mut body = std::pin::pin!(body);
        // Two frames, not one. The buffered fast path below is only safe if the
        // body is *finished*, and the only way to know that is to ask for
        // another frame -- a body's `size_hint` is a promise, a caller's
        // `Content-Length` is not even that. Taking the fast path on the first
        // frame alone would silently drop everything after it. For the common
        // case (a `Full<Bytes>`, an axum reply) the second poll resolves
        // immediately and costs nothing.
        let first = next_chunk(body.as_mut()).await?;
        let second = if first.is_some() {
            next_chunk(body.as_mut()).await?
        } else {
            None
        };
        let prefix: Vec<Vec<u8>> = [first, second].into_iter().flatten().collect();
        let produced: u64 = prefix.iter().map(|c| c.len() as u64).sum();

        match length {
            Some(0) => {
                reply.set_header(ResponseHeader::CONTENT_LENGTH, b"0".to_vec());
                if produced != 0 {
                    return Err(BodyError::LengthMismatch {
                        declared: 0,
                        actual: produced,
                    }
                    .into());
                }
                self.finish(reply).await
            }
            Some(length) => {
                if prefix.len() < 2 && produced == length {
                    // The whole body, and the body agrees it is over. One
                    // syscall, and HTTP.sys computes the length itself --
                    // measured -- so nothing needs declaring.
                    if let Some(chunk) = prefix.into_iter().next() {
                        reply.add_body(chunk);
                    }
                    return self.finish(reply).await;
                }
                // Measured: the known slot suppresses HTTP.sys's own
                // computation, whereas the unknown list would produce a second
                // `Content-Length` line.
                reply.set_header(
                    ResponseHeader::CONTENT_LENGTH,
                    length.to_string().into_bytes(),
                );
                self.stream(reply, body, prefix, Framing::Exact(length))
                    .await
            }
            None => {
                // Measured: a streamed reply with nothing declared gets no
                // framing at all from HTTP.sys, not even on a keep-alive
                // connection. Chunked framing written by hand passes through
                // untouched, which is the same rule the client half follows for
                // request bodies.
                reply.set_header(ResponseHeader::TRANSFER_ENCODING, b"chunked".to_vec());
                self.stream(reply, body, prefix, Framing::Chunked).await
            }
        }
    }

    /// Send the head, then the body, in pieces.
    async fn stream<B>(
        &self,
        reply: Response,
        mut body: std::pin::Pin<&mut B>,
        prefix: Vec<Vec<u8>>,
        framing: Framing,
    ) -> Result<(), ResponseError>
    where
        B: Body,
        B::Data: Buf,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let OpResult(sent, _) = self.queue.send_partial(self.id, reply).await;
        sent.map_err(ResponseError::send(SendStage::Head))?;

        let mut written: u64 = 0;
        let mut pending = prefix.into_iter();
        let mut chunk = pending.next();
        while let Some(piece) = chunk {
            if let Framing::Exact(declared) = framing {
                // Checked *before* the write, not after. A body that produces
                // more than it declared would otherwise have its surplus already
                // on the wire, where a peer reads `declared` bytes as the body
                // and parses the rest as the start of the next response --
                // a response desync, and worse than an error.
                if written + piece.len() as u64 > declared {
                    return Err(BodyError::LengthMismatch {
                        declared,
                        actual: written + piece.len() as u64,
                    }
                    .into());
                }
            }
            written += piece.len() as u64;
            match framing {
                Framing::Exact(_) => self.write(piece, false).await?,
                Framing::Chunked => {
                    let mut framed = format!("{:x}\r\n", piece.len()).into_bytes();
                    framed.extend_from_slice(&piece);
                    framed.extend_from_slice(b"\r\n");
                    self.write(framed, false).await?;
                }
            }
            chunk = match pending.next() {
                Some(piece) => Some(piece),
                None => next_chunk(body.as_mut()).await?,
            };
        }

        match framing {
            Framing::Exact(declared) if declared != written => {
                // Measured: HTTP.sys accepts an under-delivered length and puts
                // a silently truncated message on the wire. Nothing below this
                // crate will notice, so this crate has to.
                return Err(BodyError::LengthMismatch {
                    declared,
                    actual: written,
                }
                .into());
            }
            Framing::Exact(_) => self.write(Vec::new(), true).await?,
            Framing::Chunked => self.write(b"0\r\n\r\n".to_vec(), true).await?,
        }
        Ok(())
    }

    async fn write(&self, buffer: Vec<u8>, last: bool) -> Result<(), ResponseError> {
        // A frame larger than one write is split rather than handed over whole,
        // so that the amount in flight stays bounded whatever the body produces.
        if buffer.len() > MAX_SEND {
            let mut rest = &buffer[..];
            while rest.len() > MAX_SEND {
                let (now, later) = rest.split_at(MAX_SEND);
                let OpResult(sent, _) = self.queue.send_body(self.id, now.to_vec(), false).await;
                sent.map_err(ResponseError::send(SendStage::Body))?;
                rest = later;
            }
            let OpResult(sent, _) = self.queue.send_body(self.id, rest.to_vec(), last).await;
            sent.map_err(ResponseError::send(SendStage::Body))?;
            return Ok(());
        }
        let OpResult(sent, _) = self.queue.send_body(self.id, buffer, last).await;
        sent.map_err(ResponseError::send(SendStage::Body))?;
        Ok(())
    }

    async fn finish(&self, reply: Response) -> Result<(), ResponseError> {
        let OpResult(sent, _) = self.queue.send(self.id, reply).await;
        sent.map_err(ResponseError::send(SendStage::Head))?;
        Ok(())
    }
}

/// How the response body is delimited on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// A declared `Content-Length`, which the produced bytes must match.
    Exact(u64),
    /// `Transfer-Encoding: chunked`, framed by this crate.
    Chunked,
}

/// The next data frame of a body, skipping trailers.
///
/// Trailers are dropped here because the size-hint framing path
/// ([`Responder::send`]) has no way to place them: a `Content-Length` response
/// is already closed by its body, and a hand-framed chunked response would have
/// to downgrade to HTTP/1.1 to carry them (M7). The trailer-capable path is
/// [`Responder::send_streaming`], which uses [`next_frame`] instead and sends
/// trailers as a real HTTP/2 trailers frame (M3, M14).
async fn next_chunk<B>(mut body: std::pin::Pin<&mut B>) -> Result<Option<Vec<u8>>, BodyError>
where
    B: Body,
    B::Data: Buf,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    loop {
        let frame = std::future::poll_fn(|context| body.as_mut().poll_frame(context)).await;
        let Some(frame) = frame else {
            return Ok(None);
        };
        let frame = frame.map_err(|error| BodyError::Source(error.into()))?;
        match frame.into_data() {
            Ok(mut data) => {
                let bytes = data.copy_to_bytes(data.remaining());
                if bytes.is_empty() {
                    // An empty data frame is not the end of a body, and sending
                    // it would be an empty chunk -- which in chunked framing is
                    // the terminator.
                    continue;
                }
                return Ok(Some(bytes.to_vec()));
            }
            Err(_trailers) => continue,
        }
    }
}

/// One frame of a body, preserving trailers.
///
/// Unlike [`next_chunk`], this keeps trailer frames so [`Responder::send_streaming`]
/// can end an HTTP/2 response with them (M3). Empty data frames are still
/// skipped — they carry nothing and are not an end-of-body signal.
enum Frame {
    /// A non-empty body data frame.
    Data(Vec<u8>),
    /// A trailers frame (HTTP/2 trailing header block).
    Trailers(HeaderMap),
}

/// The next data or trailers frame of a body, skipping empty data frames.
async fn next_frame<B>(mut body: std::pin::Pin<&mut B>) -> Result<Option<Frame>, BodyError>
where
    B: Body,
    B::Data: Buf,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    loop {
        let frame = std::future::poll_fn(|context| body.as_mut().poll_frame(context)).await;
        let Some(frame) = frame else {
            return Ok(None);
        };
        let frame = frame.map_err(|error| BodyError::Source(error.into()))?;
        match frame.into_data() {
            Ok(mut data) => {
                let bytes = data.copy_to_bytes(data.remaining());
                if bytes.is_empty() {
                    continue;
                }
                return Ok(Some(Frame::Data(bytes.to_vec())));
            }
            Err(frame) => match frame.into_trailers() {
                Ok(map) => return Ok(Some(Frame::Trailers(map))),
                // Neither data nor trailers: an unknown frame kind. Skip it
                // rather than guess at bytes to put on the wire.
                Err(_other) => continue,
            },
        }
    }
}

/// Flatten a trailer [`HeaderMap`] into the name/value byte pairs
/// [`RequestQueue::send_trailers`](crate) wants.
///
/// A multi-valued trailer becomes one pair per value, matching how it would be
/// sent as repeated header lines. Names are lower-cased already by `http`'s
/// [`HeaderName`], which is what an HTTP/2 trailer block requires.
fn encode_trailers(map: &HeaderMap) -> Vec<(Vec<u8>, Vec<u8>)> {
    map.iter()
        .map(|(name, value)| (name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    #[test]
    fn the_thread_pool_flavour_can_be_moved_onto_a_spawned_task() {
        // The reason `Backend` carries the future type as an associated type.
        // If this stops compiling, a caller can no longer hand an accepted
        // request to a runtime, which is the only concurrency story the crate
        // has.
        assert_send::<IncomingBody<ThreadPoolIo>>();
        assert_send::<Accepted<ThreadPoolIo>>();
        assert_send::<Responder<ThreadPoolIo>>();
        assert_send::<ShutdownHandle<ThreadPoolIo>>();
        assert_sync::<ShutdownHandle<ThreadPoolIo>>();
    }

    #[test]
    fn an_empty_data_frame_is_not_the_end_of_a_body() {
        // In chunked framing an empty chunk *is* the terminator, so a body that
        // yields an empty frame in the middle must not be allowed to write one.
        use futures::stream;
        use http_body_util::StreamBody;

        let frames = stream::iter(vec![
            Ok::<_, std::convert::Infallible>(http_body::Frame::data(Bytes::new())),
            Ok(http_body::Frame::data(Bytes::from_static(b"real"))),
            Ok(http_body::Frame::data(Bytes::new())),
        ]);
        let body = StreamBody::new(frames);
        let mut body = std::pin::pin!(body);

        let first = futures::executor::block_on(next_chunk(body.as_mut())).unwrap();
        assert_eq!(first.as_deref(), Some(&b"real"[..]));
        let second = futures::executor::block_on(next_chunk(body.as_mut())).unwrap();
        assert_eq!(second, None, "trailing empty frames are not chunks");
    }

    #[test]
    fn a_chunk_header_is_hexadecimal_as_the_wire_format_requires() {
        // Written by hand because HTTP.sys does no chunked framing at all --
        // measured -- so a decimal length here would produce a message no
        // client could parse.
        assert_eq!(format!("{:x}\r\n", 5usize), "5\r\n");
        assert_eq!(format!("{:x}\r\n", 4096usize), "1000\r\n");
    }

    #[test]
    fn next_frame_preserves_trailers_where_next_chunk_drops_them() {
        // The whole reason `send_streaming` exists (M3, M14): the trailer-capable
        // path must see the trailers frame that `next_chunk` throws away, or
        // there is nothing to turn into an HTTP/2 trailers frame.
        use futures::stream;
        use http_body_util::StreamBody;

        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());

        let make_body = || {
            let frames = stream::iter(vec![
                Ok::<_, std::convert::Infallible>(http_body::Frame::data(Bytes::from_static(
                    b"msg",
                ))),
                Ok(http_body::Frame::trailers(trailers.clone())),
            ]);
            StreamBody::new(frames)
        };

        // `next_frame` keeps them.
        let body = make_body();
        let mut body = std::pin::pin!(body);
        match futures::executor::block_on(next_frame(body.as_mut())).unwrap() {
            Some(Frame::Data(bytes)) => assert_eq!(bytes, b"msg"),
            other => panic!("expected a data frame, got {}", other.is_some()),
        }
        match futures::executor::block_on(next_frame(body.as_mut())).unwrap() {
            Some(Frame::Trailers(map)) => {
                assert_eq!(map.get("grpc-status").unwrap(), "0");
            }
            _ => panic!("expected a trailers frame"),
        }

        // `next_chunk` drops them: one data chunk, then end.
        let body = make_body();
        let mut body = std::pin::pin!(body);
        let first = futures::executor::block_on(next_chunk(body.as_mut())).unwrap();
        assert_eq!(first.as_deref(), Some(&b"msg"[..]));
        let second = futures::executor::block_on(next_chunk(body.as_mut())).unwrap();
        assert_eq!(second, None, "next_chunk skips trailers and ends");
    }

    #[test]
    fn encode_trailers_lowercases_names_and_keeps_values() {
        // HTTP/2 trailer blocks require lower-cased names; `http`'s HeaderName
        // stores them that way, so the flattening just has to not undo it.
        let mut map = HeaderMap::new();
        map.insert("Grpc-Status", "0".parse().unwrap());
        map.insert("grpc-message", "".parse().unwrap());

        let pairs = encode_trailers(&map);
        assert!(pairs.iter().any(|(n, v)| n == b"grpc-status" && v == b"0"));
        assert!(pairs
            .iter()
            .any(|(n, v)| n == b"grpc-message" && v.is_empty()));
    }
}
