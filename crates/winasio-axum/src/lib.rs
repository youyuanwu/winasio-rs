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
//! shaped like `axum::serve`, an [`Executor`] seam so concurrency is the
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
//! this crate depending on that runtime. The [`Executor`] trait documentation
//! records why it carries a defaulted `poll_progress` hook (so a current-thread
//! executor is drivable while a spawning one stays a single method) and why
//! [`RequestTask`] is uniformly `Send`.
//!
//! # The serve loop
//!
//! [`serve`] is the concurrent counterpart to
//! [`winasio_util::Server::serve`]'s sequential loop. Its full contract — the
//! cancel-safe accept, the instance-correct `poll_ready` backpressure gate,
//! uniform panic containment, the error policy, and the deliberately **abrupt**
//! shutdown (in-flight work is not drained; `ThreadPerRequest` callbacks may run
//! after `serve` returns) — is documented on the [`serve`] function. Prefer this
//! crate's [`serve`] when you want concurrency; prefer `winasio-util`'s
//! `serve`/`serve_one` when you want one request at a time.
//!
//! # Routes carry the HTTP.sys URL prefix (M8)
//!
//! HTTP.sys delivers request URIs with the *registered* URL prefix included. A
//! server reserved at `http://localhost:PORT/axum/` therefore delivers
//! `/axum/greet`, so routes must be written to match — `.route("/axum/greet",
//! ..)`, not `.route("/greet", ..)`. This crate does **not** strip the prefix:
//! stripping would need a rewriting layer that could disagree with `axum`'s own
//! routing, and the behaviour is a property of HTTP.sys the caller already sees
//! with `winasio-util`. It is documented rather than papered over. A caller who
//! wants prefix-relative routes can mount their router under the prefix with
//! [`axum::Router::nest`] themselves.
//!
//! # `axum` is a runtime-free dependency; `ConnectInfo` is opt-in (D-A / M4)
//!
//! This crate depends on `axum` with `default-features = false`, which
//! (measured) pulls no tokio/mio/hyper/hyper-util into its normal dependency
//! tree. `axum`'s `ConnectInfo<SocketAddr>` extractor
//! (`axum::extract::ConnectInfo`) is gated behind axum's `tokio` feature, and
//! enabling that feature pulls tokio + mio + hyper + hyper-util into the
//! *normal* graph (measured) — the cost this crate refuses. The peer address is
//! instead reachable directly through [`winasio_util::ConnectionInfo`] in the
//! request extensions:
//!
//! ```no_run
//! use winasio_util::ConnectionInfo;
//!
//! async fn peer(info: axum::Extension<ConnectionInfo>) -> String {
//!     match info.0.peer_address {
//!         Some(address) => format!("peer {address}"),
//!         None => "peer unknown".to_string(),
//!     }
//! }
//! ```
//!
//! A caller who specifically wants axum's own `ConnectInfo<SocketAddr>`
//! extractor can restore it in two steps: enable axum's `tokio` feature *in
//! their own crate*, then insert `ConnectInfo(addr)` into the request extensions
//! from the [`ConnectionInfo`](winasio_util::ConnectionInfo) peer address with a
//! tiny `tower` layer (measured to satisfy the extractor; `winasio-tests`
//! carries the worked recipe and its control).
//!
//! **Feature-unification caveat.** Cargo unifies features across a workspace
//! build, so a *sibling* crate enabling `axum/tokio` (as `winasio-tests` does,
//! only to name the `ConnectInfo` type for that recipe test) makes that feature
//! present in a `--workspace --all-features` build. The runtime-free guarantee
//! is therefore defined on **`winasio-axum`'s own normal dependency tree** and
//! is enforced by an isolated check — `cargo tree -e normal -p winasio-axum`
//! shows none of tokio/mio/hyper/hyper-util (asserted by
//! `winasio-tests::dependencies::winasio_axum_pulls_in_no_async_runtime`).
//!
//! # Dependencies (R6)
//!
//! The only non-workspace-internal runtime dependencies are `axum`
//! (default features off), `winasio-util`, and `futures-util` (with
//! `default-features = false, features = ["std"]`), the last used solely for
//! [`FuturesUnordered`](futures_util::stream::FuturesUnordered) (the
//! [`CurrentThread`] executor's concurrency) and
//! [`catch_unwind`](futures_util::future::FutureExt::catch_unwind) (panic
//! containment). `futures-util` was chosen over the full `futures` facade to
//! keep the dependency narrow; it brings no runtime.
//!
//! # Invariants and obligations
//!
//! - **Framing, decoding, and backpressure are `winasio-util`'s.** This crate
//!   adds only the concurrent loop; request decoding, response framing (a
//!   handler-supplied `Transfer-Encoding` is an error), body handling, and the
//!   `poll_ready` semantics all remain `winasio-util`'s.
//! - **[`CurrentThread`] spawns nothing.** Its concurrency is cooperative on the
//!   loop's own thread; a handler that blocks that thread blocks the loop. Use
//!   [`ThreadPerRequest`] (or a real runtime) for blocking handlers.
//! - **Errors and panics are observed, not swallowed.** Every per-request
//!   failure and caught panic is routed to the caller's [`Serve::on_error`]
//!   observer; the crate takes no logging dependency.
//! - **Shutdown is abrupt.** [`serve`] returns without draining in-flight work;
//!   a [`ThreadPerRequest`] task (and its observer callback) may still run after
//!   `serve` returns. A caller needing "all work finished" must coordinate it.
//! - **Shutdown is observed between accepts, and readiness has priority.** The
//!   loop notices a closed queue only at an accept point, and each accept is
//!   preceded by the `poll_ready` gate (backpressure: a request is not pulled
//!   while the service is unready). A service that never becomes ready therefore
//!   parks the loop in that gate, and a concurrent queue-close is not observed
//!   until readiness resolves. A realistic `axum::Router` is always ready, so
//!   this affects only pathological always-unready services; it is the direct,
//!   measured consequence of preserving `poll_ready` backpressure.
//! - **The serve future is not `Send`.** It borrows the `Server` and is meant to
//!   be driven on the caller's own thread (`block_on`) or awaited in the
//!   caller's runtime — not spawned onto a work-stealing runtime. Parallelism
//!   across requests comes from the [`Executor`], not from spawning the loop
//!   itself; a caller needing the loop on another thread should move the
//!   `Server` there and drive it locally.
//! - **There is no built-in admission cap.** With an always-ready service,
//!   [`ThreadPerRequest`] spawns one thread per in-flight request and
//!   [`CurrentThread`]'s in-flight set grows unbounded. Bound concurrency with a
//!   service-level limit (for example a `tower` concurrency layer) whose
//!   `poll_ready` gates admission through the loop's readiness gate.

// Re-exported so downstream code can name the exact `axum` this crate builds on,
// and to anchor the normal (runtime-free) `axum` dependency and its version,
// mirroring `winasio_util`'s `pub use tower_service`.
pub use axum;

mod executor;
mod serve;

pub use executor::{CurrentThread, Executor, RequestTask, ThreadPerRequest};
pub use serve::{serve, HandlerPanic, Serve};

// Re-exported so a caller can name the error type the serve loop reports without
// also depending on `winasio-util` directly.
pub use winasio_util::ServeError;
