// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The server story: serve a tonic service over HTTP.sys, with gRPC framing.
//!
//! # Why there is almost nothing here
//!
//! A tonic-generated server (`EchoServer<T>`) *is* a `tower` service — and, once
//! added to an [`axum::Router`], an axum router. `winasio-axum` already serves
//! an axum router over HTTP.sys concurrently. So the server side of gRPC is not
//! a new transport; it is the existing one with **one** thing changed: the
//! response must be framed as raw HTTP/2 DATA frames terminated by a trailers
//! frame (carrying `grpc-status`), not by the size-hint framing an ordinary
//! reply uses.
//!
//! That one change is [`winasio_axum::serve_grpc`], added alongside
//! [`winasio_axum::serve`]. It is identical to `serve` — same concurrent accept
//! loop, backpressure, panic containment — except each response is dispatched
//! through [`winasio_util::Accepted::serve_streaming`] instead of
//! [`serve`](winasio_util::Accepted::serve). The framing choice is made
//! **explicitly** by calling `serve_grpc`, because the caller knows it is
//! serving gRPC; it never rides on server-side request-version sniffing, which
//! the M2 defect (HTTP.sys reporting an HTTP/2 request as `HTTP/1.1` in
//! `Version`) would poison.
//!
//! # Rejected alternative: a bespoke gRPC accept loop
//!
//! An earlier shape had `winasio-tonic` write its own
//! `Server::accept` → `Accepted::into_parts` → `Responder::send_streaming` loop.
//! That would have duplicated `winasio-axum`'s hard-won concurrency, cancel
//! safety and panic containment. Adding a one-flag seam to the existing loop
//! keeps a single implementation of the difficult part and confines the gRPC
//! difference to the framing selection.
//!
//! # Usage
//!
//! ```no_run
//! # fn example(server: &winasio_util::Server<'_>) -> Result<(), winasio_util::ServeError> {
//! # struct EchoImpl;
//! // let service = echo_server::EchoServer::new(EchoImpl);
//! // let router = axum::Router::new().merge(axum::Router::new()); // + the service
//! // winasio_tonic::serve_grpc(server, router, winasio_tonic::CurrentThread::new())?;
//! # let _ = server;
//! # Ok(())
//! # }
//! ```

// Re-export the gRPC serve entry point and everything a caller needs to drive
// it, so a gRPC server can be stood up naming only `winasio-tonic`.
pub use winasio_axum::{serve_grpc, CurrentThread, Executor, Serve, ThreadPerRequest};

// The Router type a tonic server is added to, re-exported for convenience.
pub use winasio_axum::axum;
