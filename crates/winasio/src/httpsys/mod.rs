// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A safe, asynchronous wrapper over the Windows HTTP Server API (HTTP.sys).
//!
//! This is a building block, not a framework: it gives you everything needed to
//! interpret a request, read its body, compose a reply and send it, and leaves
//! the accept loop to you.
//!
//! Operations are built on [`crate::iocp`], so they are runtime-agnostic.
//!
//! # Alignment
//!
//! HTTP.sys writes an [`HTTP_REQUEST_V2`] into a caller-supplied buffer, and
//! that structure needs stricter alignment than a byte buffer provides. Getting
//! it wrong is reported by the operating system as `ERROR_NOACCESS`, but forming
//! an under-aligned pointer to the structure is undefined behaviour in Rust
//! regardless -- so the receive buffer is allocated through an element type wide
//! enough to guarantee it, checked below at compile time.
//!
//! [`HTTP_REQUEST_V2`]: windows::Win32::Networking::HttpServer::HTTP_REQUEST_V2

mod error;
mod header;
mod init;
mod ops;
mod queue;
mod request;
mod session;

pub use header::{RequestHeader, ResponseHeader};
pub use init::HttpInitializer;
pub use ops::cancel::CancelRequest;
pub use ops::receive::ReceiveRequest;
pub use queue::{ReceiveConfig, ReceiveError, RequestQueue};
pub use request::{Method, Request, RequestId, UnknownHeaders, MIN_CAPACITY};
pub use session::{ServerSession, UrlGroup};

/// The element the request buffer is allocated as.
///
/// Its alignment must cover `HTTP_REQUEST_V2`'s; see the module documentation.
pub(crate) type BufferUnit = u64;

// Phase 0 measured `align_of::<HTTP_REQUEST_V2>() == 8` on x86_64. This turns
// any future divergence -- a new architecture, or a bindings change -- into a
// build failure rather than silent undefined behaviour.
const _: () = assert!(
    std::mem::align_of::<BufferUnit>()
        >= std::mem::align_of::<windows::Win32::Networking::HttpServer::HTTP_REQUEST_V2>(),
    "the request buffer's element type is not aligned enough for HTTP_REQUEST_V2"
);
