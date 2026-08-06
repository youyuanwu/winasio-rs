// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A higher-level HTTP client over [`winasio::winhttp`], shaped around the
//! [`http`] crate.
//!
//! Send an [`http::Request`], get back an [`http::Response`] whose body
//! implements [`http_body::Body`] over [`bytes::Bytes`]. Those are the types
//! hyper uses, so a hyper user should find nothing surprising in the shape of
//! the API — only in what it does not have.
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
//! # What this crate is not
//!
//! Deliberately absent, each because doing it badly is worse than not doing it:
//!
//! * **No connection pooling of its own.** This crate never reuses a
//!   connection deliberately — but WinHTTP does, in a process-wide keep-alive
//!   pool that no option here turns off. The [`Client`] documentation records
//!   what was measured about it and the one consequence callers must handle.
//! * **No redirect following.** A `3xx` is returned as the response it is. The
//!   [`Client`] documentation explains why the platform's own redirect handling
//!   is switched off rather than left alone.
//! * **No cookie jar, no retry policy, no authentication helpers.** The absence
//!   of a retry is load-bearing rather than merely unimplemented: see
//!   [`Client`] on why a stale pooled connection is reported instead.
//! * **No server side, and no hyper trait implementations.** This crate does
//!   not let hyper drive WinHTTP; it offers hyper's *types* over WinHTTP.
//!
//! # Runtime agnosticism
//!
//! Nothing here spawns a task, blocks a thread or needs a reactor. The
//! dependencies are `winasio`, `windows`, `http`, `http-body` and `bytes`, none
//! of which pull in an async runtime. The doc example above and the integration
//! tests both drive whole requests on `futures::executor::block_on`, which has
//! no worker threads and no reactor — if any part of this crate quietly needed
//! a runtime, they could not pass.
//!
//! # Invariants and obligations
//!
//! * **The crate owns message framing.** `Content-Length` and
//!   `Transfer-Encoding` are derived from the request body's
//!   [`size_hint`](http_body::Body::size_hint); setting either by hand is an
//!   error. A known length is declared; an unknown one is sent chunked.
//! * **A request header that cannot be represented is refused, not
//!   converted.** [`http::HeaderValue`] holds arbitrary bytes and WinHTTP wants
//!   a UTF-16 string; a value that is not printable ASCII produces
//!   [`Error::InvalidRequestHeader`] naming the header, rather than silently
//!   sending something the caller did not write.
//! * **Repeated response headers survive.** The response header block is parsed
//!   whole rather than queried by name, because a by-name query collapses
//!   duplicates and `Set-Cookie` is duplicated by design.
//! * **A body that was cut off is never a body that ended.** If a response
//!   declares a `Content-Length` and the connection ends early, the body's
//!   final poll yields [`Error::TruncatedBody`]. The platform reports that case
//!   as a clean end of body; this crate does not.
//! * **A response body of unknown length cannot be checked.** Chunked and
//!   close-delimited responses have no declared length, so truncation is
//!   undetectable. That is a property of HTTP and is stated rather than guessed
//!   around.
//! * **Trailers are not sent and not received.** A request body's trailers
//!   frame is skipped; WinHTTP exposes no way to send or read trailers through
//!   this API.
//!
//! # Known warts
//!
//! * `Transfer-Encoding: chunked` remains visible in a response's
//!   [`HeaderMap`](http::HeaderMap) even though WinHTTP has already de-chunked
//!   the body. Deleting a header the server sent is itself a lossy
//!   transformation, so it is reported verbatim and documented here instead.
//! * Response header order is not preserved between different header names.
//!   WinHTTP hoists the headers it has an index for ahead of the ones it does
//!   not. Order *within* one name — the only ordering HTTP gives meaning to —
//!   is preserved.

mod body;
mod client;
mod error;
mod headers;
mod uri;

pub use body::ResponseBody;
pub use client::{Client, ClientBuilder};
pub use error::{Error, HeaderReason, Stage};

/// Re-exported from [`winasio::winhttp`] so that configuring a client does not
/// require naming the lower crate.
pub use winasio::winhttp::{CertificateRelaxations, WinHttpError};
