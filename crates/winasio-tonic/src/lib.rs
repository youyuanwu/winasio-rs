// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! gRPC (tonic) over WinHTTP and HTTP.sys, with no async runtime and no hyper.
//!
//! `winasio-tonic` lets a [tonic](https://docs.rs/tonic) client and server speak
//! gRPC using this workspace's transports — WinHTTP on the client, HTTP.sys on
//! the server — instead of tonic's built-in hyper/tokio stack. It is two small
//! pieces:
//!
//! * [`WinHttpChannel`] — a `tower` [`Service`](tower_service::Service) a tonic
//!   client accepts as its transport ([`tonic::client::GrpcService`]), driving
//!   [`winasio_util::Client`] in HTTP/2 mode.
//! * [`serve_grpc`] — the server entry point, re-exported from `winasio-axum`,
//!   which serves a tonic service (an [`axum::Router`]) over HTTP.sys with gRPC
//!   response framing.
//!
//! The generated stubs for the example [`echo`] service are produced from a
//! checked-in `.proto` at build time (D3).
//!
//! # D1. Server side is an axum router, so it is mostly already done
//!
//! A tonic server is generated as a `tower` service that becomes an
//! [`axum::Router`]. `winasio-axum` already serves an axum router over HTTP.sys.
//! The only gRPC-specific need is response framing (below), so the server story
//! is a thin re-export of [`serve_grpc`] rather than a new transport. See
//! [`server`].
//!
//! # D2. tokio is allowed here, but only its non-runtime pieces
//!
//! This is the one crate in the workspace whose graph may contain `tokio` — but
//! **only** the parts that are not a runtime. `tonic` (built with
//! `default-features = false`, only `codegen`) reaches for `tokio` features
//! `default` + `sync` via `tokio-stream`; it does **not** pull `rt`, `net`,
//! `mio`, `macros`, `hyper`, `h2` or `hyper-util` (M13, measured). The manifest
//! declares `tokio` with its own minimal feature set rather than the workspace's
//! `features = ["full"]`, and a test
//! (`winasio_tonic_pulls_in_no_async_runtime_beyond_tokio` in `winasio-tests`)
//! pins the ban so a future dependency bump cannot smuggle a reactor in.
//!
//! **Rejected alternative**: tonic's default features (or its `transport` /
//! `server` / `channel` features) would pull the full hyper + tokio-reactor
//! stack — exactly what this workspace exists to avoid. The client stub is built
//! with `build_transport(false)` so it never references the hyper `Channel`, and
//! the server stub (which needs tonic's `server` feature, hence hyper) is
//! generated in `winasio-tests`, not here.
//!
//! # D3. Duplex is by ordering, and it is an OS property
//!
//! Client-streaming and bidirectional gRPC need the request body to keep being
//! written after the response has started. This crate does nothing special for
//! that: it hands tonic's streaming request body to [`winasio_util::Client`],
//! whose HTTP/2 path already implements the WinHTTP duplex ordering from
//! Microsoft's own `WinHttpHandler` (send headers, start receiving the response
//! before the body finishes — M6). Whether duplex actually works is a property
//! of the host OS (Windows 11 / Server 2022+ full; older server SKUs unary +
//! server-streaming only — M9), detected by the client's auto-chunking
//! capability probe (M8) and exercised, not assumed, by the end-to-end tests.
//!
//! # D4. Only gRPC over TLS
//!
//! Both halves speak gRPC only over HTTP/2, and on these platforms HTTP/2
//! requires TLS — HTTP.sys has no h2c and WinHTTP's gRPC support is TLS-only
//! (M9, M10). Every end-to-end test therefore depends on a provisioned
//! certificate; there is no plaintext gRPC path to offer.

#![cfg(windows)]

mod channel;
pub mod server;

/// The generated stubs for the example `winasio.echo.v1.Echo` service.
///
/// Produced from `proto/echo.proto` by `build.rs` (client stub only — see D2).
/// The service covers all four gRPC call shapes (unary, server-streaming,
/// client-streaming, bidirectional) so the transport is exercised on each.
pub mod echo {
    // The generated code triggers lints that are not ours to fix.
    #![allow(missing_docs)]
    tonic::include_proto!("winasio.echo.v1");
}

pub use channel::WinHttpChannel;
pub use server::{serve_grpc, CurrentThread, Executor, Serve, ThreadPerRequest};

// Re-export the pieces a caller writes bounds against, so depending on
// `winasio-tonic` alone is enough to build a client and server.
pub use tonic;
pub use winasio_util;
