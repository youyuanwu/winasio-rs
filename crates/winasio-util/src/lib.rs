// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A higher-level HTTP client over [`winasio::winhttp`] and HTTP server over
//! [`winasio::httpsys`], shaped around the [`http`] crate.
//!
//! The client sends an [`http::Request`] and gives back an [`http::Response`]
//! whose body implements [`http_body::Body`] over [`bytes::Bytes`]. The server
//! does the reverse, driving a [`tower_service::Service`]. Those are the types
//! hyper and axum use, so a hyper or axum user should find nothing surprising in
//! the shape of the API — only in what it does not have.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use bytes::Bytes;
//! use http_body_util::{BodyExt, Empty};
//! use winasio_util::Client;
//!
//! let client = Client::new("winasio-util/0.1")?;
//! let request = http::Request::get("http://example.com/").body(Empty::<Bytes>::new())?;
//!
//! // No runtime anywhere: `block_on` is a bare single-threaded executor.
//! let response = futures::executor::block_on(client.request(request))?;
//! assert!(response.status().is_success());
//!
//! let body = futures::executor::block_on(response.into_body().collect())?.to_bytes();
//! println!("{} bytes", body.len());
//! # Ok(())
//! # }
//! ```
//!
//! The server half lives in [`server`], which documents its own design
//! decisions; [`Server::builder`] is the entry point.
//!
//! # What this crate is not
//!
//! Deliberately absent, each because doing it badly is worse than not doing it:
//!
//! * **No connection pooling of its own.** This crate never reuses a
//!   connection deliberately — but WinHTTP does, in a process-wide keep-alive
//!   pool that no option here turns off. The [`Client`] documentation records
//!   what was measured about it and the one consequence callers must handle. On
//!   the server side, connection persistence belongs to HTTP.sys: measured, a
//!   `Connection: close` set by the application does not change it.
//! * **No redirect following.** A `3xx` is returned as the response it is. The
//!   [`Client`] documentation explains why the platform's own redirect handling
//!   is switched off rather than left alone.
//! * **No cookie jar, no retry policy, no authentication helpers.** The absence
//!   of a retry is load-bearing rather than merely unimplemented: see
//!   [`Client`] on why a stale pooled connection is reported instead.
//! * **No routing, no middleware, and no TLS configuration on the server.**
//!   Routing is what a [`tower_service::Service`] is for — an axum `Router`
//!   serves directly — and HTTP.sys binds certificates out of band through
//!   `netsh`, where nothing here could help.
//! * **No hyper trait implementations.** This crate does not let hyper drive
//!   WinHTTP or HTTP.sys; it offers hyper's *types* over both. A hyper service
//!   bridges through `hyper-util`'s adapter.
//! * **The two halves do not share a connection, a cache or anything else.**
//!   They share one set of `http` types, and, for the one problem that really
//!   is the same in both directions -- framing an [`http_body::Body`] -- one
//!   [`BodyError`]. Their *failure vocabularies* are otherwise deliberately
//!   disjoint: a WinHTTP stage name is meaningless to HTTP.sys and vice versa,
//!   and each fallible API returns a type whose variants it can all actually
//!   produce. See the [`error`] module docs.
//!
//! # Runtime agnosticism
//!
//! Nothing here spawns a task, blocks a thread or needs a reactor. The
//! dependencies are `winasio`, `windows`, `http`, `http-body`, `bytes` and
//! `tower-service`, none of which pull in an async runtime — `tower-service` is
//! a trait and nothing else. The doc examples and the integration tests drive
//! whole requests, and whole *served* requests, on `futures::executor::block_on`,
//! which has no worker threads and no reactor — if any part of this crate
//! quietly needed a runtime, they could not pass.
//!
//! Concurrency on the server is the caller's: see [`server`] on why the crate
//! spawns nothing and how an accepted request is handed to a task instead.
//!
//! # Invariants and obligations
//!
//! * **The crate owns message framing, in both directions.**
//!   `Content-Length` and `Transfer-Encoding` are derived from the body's
//!   [`size_hint`](http_body::Body::size_hint). On the client, setting either
//!   by hand is an error. On the server, a `Transfer-Encoding` is likewise
//!   refused, but a `Content-Length` is *checked* rather than refused, because
//!   an `axum::Router` sets one and refusing it would make a real router
//!   unservable. A known length is declared; an unknown one is sent chunked.
//!   On the server this is load-bearing rather than tidy: measured, HTTP.sys
//!   applies *no* framing at all to a streamed reply, so an undeclared one runs
//!   into the next response on a keep-alive connection.
//! * **A body that promised a length and produced another is an error.** Both
//!   halves count what they wrote and report [`BodyError::LengthMismatch`];
//!   measured, neither platform checks, and HTTP.sys will happily emit a
//!   silently truncated message.
//! * **A request header that cannot be represented is refused, not
//!   converted.** [`http::HeaderValue`] holds arbitrary bytes and WinHTTP wants
//!   a UTF-16 string; a value that is not printable ASCII produces
//!   [`RequestError::InvalidRequestHeader`] naming the header, rather than silently
//!   sending something the caller did not write. The server side has no such
//!   rule and needs none: measured, HTTP.sys passes reply header bytes through
//!   unchanged.
//! * **Repeated headers survive where the platform lets them.** A response
//!   header block is parsed whole rather than queried by name, because a
//!   by-name query collapses duplicates and `Set-Cookie` is duplicated by
//!   design; reply headers are emitted through the list that keeps repeats.
//!   Measured, inbound *request* headers are folded into one comma-joined value
//!   by HTTP.sys before this crate sees them, so on that one edge there is
//!   nothing left to preserve.
//! * **A body that was cut off is never a body that ended.** If a response
//!   declares a `Content-Length` and the connection ends early, the client
//!   body's final poll yields [`ResponseBodyError::Truncated`]. The platform reports
//!   that case as a clean end of body; this crate does not. Measured, HTTP.sys
//!   is better behaved and reports a truncated *request* body as an error
//!   itself, so the server side propagates rather than re-detects.
//! * **A response body of unknown length cannot be checked.** Chunked and
//!   close-delimited responses have no declared length, so truncation is
//!   undetectable. That is a property of HTTP and is stated rather than guessed
//!   around.
//! * **A reply that may not have a body does not get one.** Measured, HTTP.sys
//!   sends the body of a `HEAD` reply and of a `204` given one; this crate
//!   suppresses both and never polls a body it will not send.
//! * **Trailers: the client reads them; the HTTP/1.1 server does not send
//!   them.** On the HTTP/2 client path (see [`h2`]) a response's trailers are
//!   read after the body ends (`WINHTTP_QUERY_FLAG_TRAILERS`, M12) and yielded
//!   as a trailers frame — this is what carries gRPC's `grpc-status`. A request
//!   body's own trailers frame is still skipped, since WinHTTP has no way to
//!   send request trailers. The server side's response-trailer support is a
//!   separate concern documented in [`server`]; the older claim that *neither*
//!   platform can send or read trailers was measured false and is corrected
//!   here.
//! * **Nothing is spawned and nothing is swallowed.** The server never creates
//!   a task or a thread; a service that returns `Err` gets a bodiless `500` on
//!   the wire and the error is still returned as [`ServeError::Service`].
//!
//! # Known warts
//!
//! * `Transfer-Encoding: chunked` remains visible in a response's
//!   [`HeaderMap`](http::HeaderMap) even though WinHTTP has already de-chunked
//!   the body, and likewise on an inbound request that HTTP.sys de-chunked.
//!   Deleting a header the peer sent is itself a lossy transformation, so it is
//!   reported verbatim and documented here instead.
//! * Response header order is not preserved between different header names.
//!   WinHTTP hoists the headers it has an index for ahead of the ones it does
//!   not. Order *within* one name — the only ordering HTTP gives meaning to —
//!   is preserved.
//! * A server reply whose body has no known length is sent chunked, which an
//!   HTTP/1.0 client cannot parse. Such a client is vanishingly rare against
//!   HTTP.sys and the alternative — buffering an arbitrarily large body — is
//!   worse, so this is recorded rather than handled.

mod body;
mod client;
/// The error types.
///
/// Public — unlike `winasio`'s own private `error` module, which holds a single
/// enum whose rationale fits in that enum's own doc. This one holds eight types
/// whose *shared* design story (why there is no single `Error`, why the two
/// halves' vocabularies stay disjoint, why some are structs) has no natural home
/// on any one of them. Every type is also re-exported at the crate root, so the
/// module path is documentation, not a required import.
pub mod error;
/// The HTTP/2 (duplex) client transport, used for gRPC. Public only so its
/// module documentation — which records the measured duplex recipe — is
/// rendered; its types are reached through [`Client`].
pub mod h2;
mod headers;
pub mod server;
mod uri;

pub use body::ResponseBody;
pub use client::{Client, ClientBuilder};
pub use error::{
    AcceptError, BodyError, ClientConfigError, ClientConfigStage, HeaderReason, PlatformError,
    RequestError, RequestReason, RequestStage, ResponseBodyError, ResponseError, SendStage,
    ServeError, ServerOperation,
};
pub use server::{
    Accepted, Backend, ConnectionInfo, IncomingBody, Responder, Server, ServerBuilder,
    ServerSession, ShutdownHandle,
};

/// Re-exported from [`winasio::winhttp`] so that configuring a client does not
/// require naming the lower crate.
pub use winasio::winhttp::{CertificateRelaxations, WinHttpError};

/// Re-exported from [`winasio::httpsys`] so that starting a server does not
/// require naming the lower crate.
pub use winasio::httpsys::{ReceiveConfig, RequestId};

/// Re-exported so that implementing a service by hand does not require adding
/// `tower-service` to a caller's manifest — and, more importantly, so that the
/// version this crate's bounds are written against is the one a caller
/// implements. A `tower::Service`, an axum `Router` and a `tower-http` layer are
/// all this same trait.
pub use tower_service;
