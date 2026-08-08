// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Generate the gRPC client + server stubs for the end-to-end tonic tests.
//!
//! # Why the proto and all codegen live here, not in `winasio-tonic`
//!
//! `winasio-tonic` is a **transport**, not a service. It has no build script and
//! runs no codegen, so building it needs no `protoc` — a consumer of the crate
//! should not have to install a protobuf compiler to get a `WinHttpChannel`.
//! The example `Echo` service exists purely to exercise that transport, so its
//! `.proto` and every stub generated from it belong to this test crate, which is
//! the only place they are used.
//!
//! Both stubs are generated here (`build_client(true).build_server(true)`)
//! because the e2e tests must both **call** and **serve** the service. Doing it
//! in one compilation means the client and server share a single set of message
//! types, so the typed round-trip in the tests is self-consistent. A test crate
//! is also allowed the async runtime that tonic's `server` feature pulls, which
//! D2 bans from `winasio-tonic`'s graph.
//!
//! Generating a real `EchoClient<WinHttpChannel>` here is also what type-checks
//! the transport against tonic's *actual* contract (see `grpc_tls.rs`'s
//! `build_client`), so nothing was lost by dropping the equivalent compile-time
//! check that used to live in `winasio-tonic`.
//!
//! `build_transport(false)` suppresses the generated `connect()` convenience
//! that would reference tonic's hyper-based `Channel`; the transport under test
//! is winasio-tonic's `WinHttpChannel`, constructed by hand in the tests.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Owned by this crate: the Echo service is a test fixture, not part of
    // winasio-tonic's API.
    let proto = "proto/echo.proto";
    let include = "proto";

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        // Keep tonic's hyper `Channel` out of the generated code; the tests use
        // winasio-tonic's WinHttpChannel as the transport.
        .build_transport(false)
        .compile_protos(&[proto], &[include])?;

    println!("cargo:rerun-if-changed={proto}");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
