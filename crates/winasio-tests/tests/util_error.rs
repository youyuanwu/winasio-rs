//! Compile-level proof that `winasio-util`'s error types are exhaustively
//! matchable from another crate.
//!
//! These tests assert almost nothing at runtime. Their value is that they *stop
//! compiling* if a variant is added to any of the eight public error types, and
//! that is the whole point of dropping `#[non_exhaustive]`. They live in
//! `winasio-tests` rather than beside the types on purpose: `#[non_exhaustive]`
//! has no effect within the defining crate, so an exhaustive match in
//! `winasio-util`'s own unit tests would compile either way and prove nothing.
//!
//! There is no `_` arm anywhere below, and none may be added. Adding a variant
//! must break this file with `E0004`; that break is the semver-major signal the
//! attribute would otherwise have suppressed. The fix is to bump the major
//! version and add an arm, never to add a catch-all.
//!
//! # Invariants and obligations
//!
//! * Every `match` in this file must remain exhaustive without a wildcard.
//! * Every public error type of `winasio-util` must appear here. A new type that
//!   is not listed is a gap in the proof, not a type that is exempt.
//! * The two types that are structs -- `ClientConfigError` and `PlatformError`
//!   -- have no variants, so they are proved differently: their *vocabulary*
//!   enums (`ClientConfigStage`, `ServerOperation`) are what a caller matches
//!   on, and those are what is matched here.

#![cfg(windows)]

use winasio_util::{
    AcceptError, BodyError, ClientConfigStage, HeaderReason, RequestError, RequestReason,
    RequestStage, ResponseBodyError, ResponseError, SendStage, ServeError, ServerOperation,
};

/// Verified by temporarily adding a probe variant during development: the
/// defining crate's own matches broke first (as documented above -- the
/// attribute is inert in-crate), and once those were satisfied *this* function
/// failed with `E0004: non-exhaustive patterns`. That is the guarantee.
fn describe_body(error: &BodyError) -> &'static str {
    match error {
        BodyError::FramingHeaderNotAllowed { .. } => "framing header",
        BodyError::LengthMismatch { .. } => "length mismatch",
        BodyError::Source(_) => "source",
    }
}

fn describe_request(error: &RequestError) -> &'static str {
    match error {
        RequestError::UnsupportedScheme { .. } => "scheme",
        RequestError::MissingHost => "host",
        RequestError::UserinfoNotSupported => "userinfo",
        RequestError::InvalidRequestHeader { .. } => "header",
        RequestError::BodyTooLarge { .. } => "too large",
        RequestError::WriteStalled { .. } => "stalled",
        RequestError::MalformedResponseHeader { .. } => "response header",
        RequestError::Body(inner) => describe_body(inner),
        RequestError::Transport { .. } => "transport",
    }
}

fn describe_response_body(error: &ResponseBodyError) -> &'static str {
    match error {
        ResponseBodyError::Truncated { .. } => "truncated",
        ResponseBodyError::Read(_) => "read",
        ResponseBodyError::Write(_) => "write",
    }
}

fn describe_accept(error: &AcceptError) -> &'static str {
    match error {
        AcceptError::RequestTooLarge { .. } => "too large",
        AcceptError::MalformedRequest { .. } => "malformed",
        AcceptError::Receive(_) => "receive",
    }
}

fn describe_response(error: &ResponseError) -> &'static str {
    match error {
        ResponseError::BadContentLength { .. } => "bad content-length",
        ResponseError::Body(inner) => describe_body(inner),
        ResponseError::Send { .. } => "send",
    }
}

fn describe_serve(error: &ServeError) -> &'static str {
    match error {
        ServeError::Accept(inner) => describe_accept(inner),
        ServeError::Service(_) => "service",
        ServeError::Response(inner) => describe_response(inner),
    }
}

#[test]
fn every_error_variant_can_be_matched_exhaustively_from_another_crate() {
    assert_eq!(describe_request(&RequestError::MissingHost), "host");
    assert_eq!(
        describe_request(&RequestError::Body(BodyError::LengthMismatch {
            declared: 1,
            actual: 2
        })),
        "length mismatch"
    );
    assert_eq!(
        describe_response_body(&ResponseBodyError::Truncated {
            expected: 5,
            received: 1
        }),
        "truncated"
    );
    assert_eq!(
        describe_accept(&AcceptError::RequestTooLarge { capacity: 1 }),
        "too large"
    );
    assert_eq!(
        describe_response(&ResponseError::BadContentLength {
            value: "x".to_string()
        }),
        "bad content-length"
    );
    assert_eq!(
        describe_serve(&ServeError::Service(Box::new(std::io::Error::other("x")))),
        "service"
    );
}

#[test]
fn every_vocabulary_enum_can_be_matched_exhaustively_from_another_crate() {
    // The two struct-shaped types carry their vocabulary in a field rather than
    // in variants, so this is where their exhaustiveness guarantee lives.
    fn client_stage(stage: ClientConfigStage) -> &'static str {
        match stage {
            ClientConfigStage::OpenSession => "open",
            ClientConfigStage::SetTimeouts => "timeouts",
            ClientConfigStage::SetRedirectPolicy => "redirects",
        }
    }
    fn request_stage(stage: RequestStage) -> &'static str {
        match stage {
            RequestStage::Connect => "connect",
            RequestStage::OpenRequest => "open",
            RequestStage::Configure => "configure",
            RequestStage::Send => "send",
            RequestStage::Write => "write",
            RequestStage::ReceiveResponse => "receive",
            RequestStage::ReadHeaders => "headers",
        }
    }
    fn operation(operation: ServerOperation) -> &'static str {
        match operation {
            ServerOperation::Initialize => "initialize",
            ServerOperation::CreateSession => "session",
            ServerOperation::CreateUrlGroup => "url group",
            ServerOperation::AddUrl => "add url",
            ServerOperation::CreateQueue => "queue",
            ServerOperation::BindUrlGroup => "bind",
            ServerOperation::ReadBody => "read body",
            ServerOperation::Reject => "reject",
            ServerOperation::Shutdown => "shutdown",
        }
    }
    fn send_stage(stage: SendStage) -> &'static str {
        match stage {
            SendStage::Head => "head",
            SendStage::Body => "body",
        }
    }
    fn header_reason(reason: HeaderReason) -> &'static str {
        match reason {
            HeaderReason::NotVisibleAscii => "ascii",
        }
    }
    fn request_reason(reason: RequestReason) -> &'static str {
        match reason {
            RequestReason::Method => "method",
            RequestReason::Target => "target",
            RequestReason::HeaderName => "header name",
            RequestReason::HeaderValue => "header value",
        }
    }

    assert_eq!(client_stage(ClientConfigStage::OpenSession), "open");
    assert_eq!(request_stage(RequestStage::Connect), "connect");
    assert_eq!(operation(ServerOperation::AddUrl), "add url");
    assert_eq!(send_stage(SendStage::Head), "head");
    assert_eq!(header_reason(HeaderReason::NotVisibleAscii), "ascii");
    assert_eq!(request_reason(RequestReason::Method), "method");
}

/// The defect this whole change exists to remove, expressed as a test.
///
/// `ServerSession::new` can only fail one way: an HTTP.sys call returned an
/// error. Under the old single `Error` its signature advertised sixteen
/// variants, fifteen of which it could not produce, and a caller could not match
/// without a `_` arm. `PlatformError` is a struct, so there is nothing to match
/// and nothing to be wrong about -- and the operation vocabulary survives in a
/// field for the caller that wants it.
#[test]
fn a_construction_failure_needs_no_wildcard_arm() {
    let session = winasio_util::ServerSession::new();
    match session {
        Ok(session) => {
            // The field is a plain enum, matched exhaustively above.
            drop(session);
        }
        Err(error) => {
            assert!(!error.is_queue_closed(), "{error}");
            let _: ServerOperation = error.operation;
            let _: &windows::core::Error = &error.source;
        }
    }
}
