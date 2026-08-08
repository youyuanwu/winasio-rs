// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Codegen from the checked-in `.proto` (D3).
//!
//! Uses `tonic-prost-build` (tonic 0.14 moved prost codegen out of `tonic-build`
//! into this separate crate — M13). It shells out to `protoc`, so the build
//! environment must provide one: on PATH, or via the `PROTOC` environment
//! variable. CI installs it explicitly; see the workflow's protoc step (D3).
//!
//! The generated client and server stubs are written to `OUT_DIR` and pulled in
//! with `tonic::include_proto!` from `lib.rs`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Rerun only when the schema changes, not on every source edit.
    println!("cargo:rerun-if-changed=proto/echo.proto");
    println!("cargo:rerun-if-changed=build.rs");

    tonic_prost_build::configure()
        // Client stub only. The server stub needs tonic's `server` feature,
        // which pulls a full runtime (h2/hyper/tokio-net) and would violate D2;
        // the server-side stub is generated in `winasio-tests` instead, where a
        // runtime is allowed. This crate's server story is generic glue over
        // `winasio-axum` (see `server.rs`) and needs no generated server type.
        .build_client(true)
        .build_server(false)
        // Do not emit the `connect()`/`tonic::transport::Channel` convenience:
        // that is the built-in hyper transport this crate exists to replace, and
        // referencing it would require the `transport` feature (hyper). Our
        // transport is `WinHttpChannel` (see `channel.rs`).
        .build_transport(false)
        .compile_protos(&["proto/echo.proto"], &["proto"])?;

    Ok(())
}
