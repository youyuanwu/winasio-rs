// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A concurrent driver that serves an [`axum::Router`] (or any equivalently
//! shaped [`tower_service::Service`]) over HTTP.sys, generic over a
//! caller-supplied [`Executor`] so the crate itself depends on no async runtime.
//!
//! # What this crate is (and is not)
//!
//! An `axum::Router` *already* works with [`winasio_util`]'s server, because
//! that server drives any `tower_service::Service` and a `Router` is one. What
//! `winasio-util` offers there is a deliberately *sequential* serve. The value
//! this crate adds is the **driver**: a concurrent accept-and-dispatch loop
//! shaped like [`axum::serve`], an [`Executor`] seam so concurrency is the
//! caller's choice of runtime (or none), and axum-shaped ergonomics. Request
//! decoding, response framing, body handling, `poll_ready` backpressure, and
//! connection info all remain `winasio-util`'s; this crate builds its loop on
//! top of them.
//!
//! # Executors
//!
//! Two executors ship built in:
//!
//! - [`CurrentThread`] drives many in-flight requests concurrently on the
//!   caller's own thread via a `FuturesUnordered`, spawning nothing and keeping
//!   the runtime-free story.
//! - [`ThreadPerRequest`] runs each request on a fresh `std::thread`, giving real
//!   parallelism.
//!
//! A runtime user plugs their runtime in with a tiny [`Executor`] impl, without
//! this crate depending on that runtime.
//!
//! The concurrent serve loop itself is added in a later phase; this phase
//! establishes the executor seam.
//!
//! # `axum` is a runtime-free dependency
//!
//! This crate depends on `axum` with `default-features = false`, which
//! (measured) pulls no tokio/mio/hyper/hyper-util into its normal dependency
//! tree. `axum`'s `ConnectInfo<SocketAddr>` extractor is gated behind axum's
//! `tokio` feature and is therefore deliberately unavailable here; the peer
//! address remains reachable through [`winasio_util::ConnectionInfo`]. See the
//! executor and serve module docs for the full decision record.

// Re-exported so downstream code can name the exact `axum` this crate builds on,
// and to anchor the normal (runtime-free) `axum` dependency and its version,
// mirroring `winasio_util`'s `pub use tower_service`.
pub use axum;

mod executor;

pub use executor::{CurrentThread, Executor, RequestTask, ThreadPerRequest};
