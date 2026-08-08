// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Generate the gRPC client + server stubs for the end-to-end tonic tests.
//!
//! # Why the test crate generates its own stubs
//!
//! `winasio-tonic` generates only the *client* stub (`build_server(false)`),
//! because the tonic `server` feature pulls a full async runtime that D2 bans
//! from that crate's graph. The e2e tests, however, must both **call** and
//! **serve** the Echo service, so they need the server stub too. A test crate is
//! allowed a runtime, so here we generate `build_client(true)
//! .build_server(true)` from the *same* checked-in proto
//! (`../winasio-tonic/proto/echo.proto`). Because both stubs come from one proto
//! compilation, the request/reply message types are a single set shared by the
//! generated client and server — the typed round-trip in the tests is
//! self-consistent.
//!
//! `build_transport(false)` suppresses the generated `connect()` convenience
//! that would reference tonic's hyper-based `Channel`; the transport under test
//! is winasio-tonic's `WinHttpChannel`, constructed by hand in the tests.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The proto is owned by winasio-tonic (checked in there, D3); the tests
    // reuse it so client and server speak the identical schema.
    let proto = "../winasio-tonic/proto/echo.proto";
    let include = "../winasio-tonic/proto";

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
