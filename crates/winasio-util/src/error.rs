// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! One error type per thing that can fail, and the vocabularies they need.
//!
//! # Why there is no single `Error`
//!
//! There used to be: one enum, sixteen variants, `#[non_exhaustive]`, shared by
//! the WinHTTP client half and the HTTP.sys server half. It was replaced
//! because the sixteen variants were never all reachable at once and the
//! signature never said so. [`ServerSession::new`](crate::ServerSession::new)
//! can produce exactly one of them — an HTTP.sys call failed — yet advertised
//! all sixteen, so a caller could neither tell from the type what might happen
//! nor match without a `_` arm covering fifteen impossible cases.
//!
//! The reachable sets turned out to be genuinely disjoint per API, which is the
//! test for whether a split is real rather than cosmetic. They are now
//! separate types.
//!
//! # Invariants and obligations
//!
//! * **Every variant of every type here is reachable from every API that
//!   returns that type.** This is the rule that decided where to split, and it
//!   is why [`PlatformError`] and [`ClientConfigError`] are structs rather than
//!   enums: five server APIs have exactly one thing that can go wrong, and an
//!   enum with one variant per operation would have re-created the defect being
//!   fixed — [`ServerSession::new`](crate::ServerSession::new) would advertise
//!   an `AddUrl` failure it cannot produce. A struct has no variants, so it is
//!   honest for all five, and the operation vocabulary survives in full as a
//!   field.
//! * **Nothing here is `#[non_exhaustive]`.** Each type documents why closure
//!   is honest for it. The trade is the one made for
//!   [`WinHttpError`](winasio::winhttp::WinHttpError): adding a variant becomes
//!   a breaking change, and in exchange the compiler tells a caller when a new
//!   failure appears. None of these types has a catch-all variant, which is the
//!   condition under which the attribute earns nothing.
//! * **The shared concerns are shared, not copied.** [`BodyError`] holds the
//!   three failures that both halves genuinely produce, and both
//!   [`RequestError`] and [`ResponseError`] wrap it. Copying them into two
//!   enums would guarantee they drift.
//! * **A platform code is never left to speak for itself.** Every type that
//!   carries a `windows::core::Error` also carries what was being done at the
//!   time, because `ERROR_WINHTTP_TIMEOUT` means something different during
//!   [`RequestStage::Connect`] than during a body read. Where the *type* already
//!   says what was being done — [`ResponseBodyError::Read`],
//!   [`AcceptError::Receive`] — there is no stage field, because it could only
//!   have one value.
//! * **`source()` reaches the underlying error wherever there is one.** A
//!   caller walking the chain arrives at the `windows::core::Error` or at the
//!   body's own error, never at a formatted string.
//!
//! # Why the client and server stage vocabularies stay separate
//!
//! A server never connects, never opens a request and never reads a response
//! header block; a client never binds a URL group. Folding [`RequestStage`] and
//! [`ServerOperation`] together would mean carrying variants that can never
//! occur, and a [`Display`](std::fmt::Display) that reads as nonsense on whichever
//! half it was not written for. A test pins that they do not overlap.
//!
//! # Rejected alternatives
//!
//! * **One type per fallible function.** Twelve types, several differing
//!   trivially, and two APIs whose failures a caller handles identically —
//!   [`Server::shutdown`](crate::Server::shutdown) and
//!   [`ShutdownHandle::shutdown`](crate::ShutdownHandle::shutdown), or the five
//!   server APIs that only ever fail in HTTP.sys — would have been forced apart
//!   for no gain. The types are sized to the caller's decision instead.
//! * **Keeping the old `Error` as a deprecated alias.** It could not be a type
//!   alias, because the variant sets differ; it would have had to be a second
//!   enum kept in step by hand, with `From` impls that lose the precision the
//!   split exists to add. The break is taken instead.
//! * **A general `win32()` accessor on every type.** Considered and dropped:
//!   once the types are precise, the server types' only source *is* a Win32
//!   error and reaching it through [`source`](std::error::Error::source) or the
//!   public field is enough, while `win_http()` survives only on the three
//!   client types where a WinHTTP code is genuinely what failed. The old
//!   `Error::win_http` was dead weight for every server caller, and that is the
//!   defect being removed, not one to generalise.
//! * **Folding [`BadContentLength`](ResponseError::BadContentLength) into
//!   [`BodyError`].** It looks like an overlap and is not: the client *refuses*
//!   a caller-set `Content-Length` outright, so only the server ever parses one.
//! * **Folding [`Truncated`](ResponseBodyError::Truncated) into [`BodyError`].**
//!   Likewise not an overlap. HTTP.sys reports a truncated *request* body as a
//!   failed read, so the server half propagates rather than re-detects, and
//!   truncation is constructed in exactly one place on the client.

use std::fmt;

use http::header::HeaderName;
use winasio::winhttp::WinHttpError;

/// `ERROR_INVALID_HANDLE` as an `HRESULT`.
///
/// Written as the code the comparison actually uses rather than imported from
/// `windows::Win32::Foundation`, where it is a `WIN32_ERROR` newtype in a
/// different numeric space: converting it would need both the `.0` and the
/// `0x8007_0000` facility bits, which is more moving parts than the constant it
/// would replace. The same asymmetry is recorded at `ERROR_BUSY_CODE` in
/// `winasio::winhttp`.
const ERROR_INVALID_HANDLE: i32 = 0x8007_0006u32 as i32;

/// `ERROR_OPERATION_ABORTED` as an `HRESULT`. See [`ERROR_INVALID_HANDLE`].
const ERROR_OPERATION_ABORTED: i32 = 0x8007_03E3u32 as i32;

/// Whether a platform code is the one every operation gets once the request
/// queue has been closed.
fn is_closed_handle(source: &windows::core::Error) -> bool {
    source.code().0 == ERROR_INVALID_HANDLE
}

// ---------------------------------------------------------------------------
// Shared vocabulary
// ---------------------------------------------------------------------------

/// Why a request header could not be sent.
///
/// Exhaustive, and expected to stay that way: it names the one property
/// [`http::HeaderValue`] permits and WinHTTP does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderReason {
    /// The value is not printable ASCII.
    ///
    /// [`http::HeaderValue`] holds arbitrary bytes; WinHTTP wants a UTF-16
    /// string. There is no honest conversion between the two, so the value is
    /// refused rather than mangled. See the crate documentation for why this is
    /// a rejection and not a lossy conversion.
    NotVisibleAscii,
}

impl fmt::Display for HeaderReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderReason::NotVisibleAscii => f.write_str("the value is not printable ASCII"),
        }
    }
}

/// Why an inbound request could not be expressed as an [`http::Request`].
///
/// Exhaustive: an HTTP request head has exactly these four parts that can be
/// unrepresentable, and HTTP.sys has already parsed the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestReason {
    /// The request line named something that is not an HTTP method.
    Method,
    /// The request target is not a URI this crate can parse.
    Target,
    /// A header name is not a valid HTTP field name.
    HeaderName,
    /// A header value contains a byte [`http::HeaderValue`] forbids.
    ///
    /// In practice CR, LF or NUL. HTTP.sys hands the bytes over verbatim, so
    /// the only rejection is the one the `http` crate makes.
    HeaderValue,
}

impl fmt::Display for RequestReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            RequestReason::Method => "the method",
            RequestReason::Target => "the request target",
            RequestReason::HeaderName => "a header name",
            RequestReason::HeaderValue => "a header value",
        };
        f.write_str(text)
    }
}

/// A failure of message framing, or of the body itself.
///
/// The one type both halves of this crate share, because framing an
/// [`http_body::Body`] is the same problem in both directions: each side
/// declares a length, each can be handed a body that breaks its own
/// [`size_hint`](http_body::Body::size_hint), and each can be handed a body that
/// fails outright. Wrapped by [`RequestError::Body`] and [`ResponseError::Body`]
/// and never returned on its own.
///
/// All three variants are reachable through both wrappers, which is why this is
/// one type rather than three variants copied into two enums.
///
/// # Why this is exhaustive
///
/// The set is closed by what an [`http_body::Body`] can do wrong relative to a
/// framing decision: promise a length and not keep it, fail, or have its framing
/// decided for it by a header the caller was not entitled to set. There is no
/// catch-all variant, so a caller that matches all three has genuinely covered
/// the type, and a fourth would be a real change a caller should be told about.
#[derive(Debug)]
pub enum BodyError {
    /// The caller set a header that decides message framing.
    ///
    /// `Transfer-Encoding` is chosen by this crate: the server half frames
    /// chunks itself and the client half declares a length, so a
    /// caller-supplied transfer coding would describe a body that was never
    /// encoded that way. On the client a `Content-Length` is refused for the
    /// same reason; on the server it is *checked* instead — see
    /// [`ResponseError::BadContentLength`] — because an `axum::Router` sets one
    /// and refusing it would make a real router unservable.
    FramingHeaderNotAllowed {
        /// The offending header.
        name: HeaderName,
    },
    /// The body produced a different number of bytes than it promised.
    ///
    /// Caught here rather than left to the platform. Measured, WinHTTP reports
    /// an under-written request body as a send timeout thirty seconds later and
    /// an over-written one not at all, and HTTP.sys puts a silently truncated
    /// message on the wire.
    LengthMismatch {
        /// What the size hint, or the caller's `Content-Length`, said.
        declared: u64,
        /// What the body actually produced.
        actual: u64,
    },
    /// The body's own [`poll_frame`](http_body::Body::poll_frame) failed.
    Source(Box<dyn std::error::Error + Send + Sync>),
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyError::FramingHeaderNotAllowed { name } => write!(
                f,
                "header `{name}` decides message framing and is chosen by this crate"
            ),
            BodyError::LengthMismatch { declared, actual } => {
                write!(f, "body promised {declared} bytes and produced {actual}")
            }
            BodyError::Source(source) => write!(f, "body failed: {source}"),
        }
    }
}

impl std::error::Error for BodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BodyError::Source(source) => Some(source.as_ref()),
            BodyError::FramingHeaderNotAllowed { .. } | BodyError::LengthMismatch { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Client: configuration
// ---------------------------------------------------------------------------

/// Which call failed while opening a WinHTTP session.
///
/// More precise than the single `Configure` stage it replaces: the three calls
/// [`ClientBuilder::build`](crate::ClientBuilder::build) makes fail for quite
/// different reasons, and a caller reading a log should not have to guess which
/// one produced the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientConfigStage {
    /// `WinHttpOpen`: creating the session itself.
    OpenSession,
    /// Applying the resolve, connect, send and receive deadlines.
    SetTimeouts,
    /// Applying the redirect policy.
    SetRedirectPolicy,
}

impl fmt::Display for ClientConfigStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ClientConfigStage::OpenSession => "opening the WinHTTP session",
            ClientConfigStage::SetTimeouts => "setting the session timeouts",
            ClientConfigStage::SetRedirectPolicy => "setting the redirect policy",
        };
        f.write_str(text)
    }
}

/// A [`Client`](crate::Client) could not be built.
///
/// Returned by [`Client::new`](crate::Client::new) and
/// [`ClientBuilder::build`](crate::ClientBuilder::build), which have exactly one
/// thing that can go wrong: a WinHTTP call failed. That is why this is a struct
/// and not an enum — there are no variants to be unreachable, and the stage is
/// diagnostic rather than a branch point. A caller that wants to branch has
/// [`win_http`](ClientConfigError::win_http).
#[derive(Debug)]
pub struct ClientConfigError {
    /// Which call failed.
    pub stage: ClientConfigStage,
    /// The error WinHTTP reported.
    pub source: windows::core::Error,
}

impl ClientConfigError {
    /// The WinHTTP-specific meaning of the failure.
    ///
    /// Not an `Option`, unlike the `Error::win_http` this replaces: every
    /// failure this type can carry came out of a WinHTTP call, so there is
    /// always a WinHTTP reading — [`WinHttpError::Other`] for a code WinHTTP
    /// does not name.
    pub fn win_http(&self) -> WinHttpError {
        WinHttpError::from_error(&self.source)
    }

    pub(crate) fn at(
        stage: ClientConfigStage,
    ) -> impl Fn(windows::core::Error) -> ClientConfigError {
        move |source| ClientConfigError { stage, source }
    }
}

impl fmt::Display for ClientConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.stage, self.source)
    }
}

impl std::error::Error for ClientConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

// ---------------------------------------------------------------------------
// Client: one request
// ---------------------------------------------------------------------------

/// What the client was doing when a platform call failed during a request.
///
/// Carried by [`RequestError::Transport`] so that a `windows` error code is not
/// left to speak for itself. `ERROR_WINHTTP_TIMEOUT` means something quite
/// different during [`RequestStage::Connect`] than during
/// [`RequestStage::ReceiveResponse`], and the caller should not have to guess
/// which one it got.
///
/// Reading the response *body* is deliberately not here: it happens after
/// [`Client::request`](crate::Client::request) has returned, and its failures
/// are [`ResponseBodyError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStage {
    /// Resolving the host and opening a connection.
    Connect,
    /// Creating the request handle.
    OpenRequest,
    /// Applying per-request options before sending.
    ///
    /// Only the certificate relaxations reach this stage; the session-wide
    /// options are applied at build time and fail as [`ClientConfigError`].
    Configure,
    /// Submitting the request head.
    Send,
    /// Writing a chunk of the request body.
    Write,
    /// Waiting for the response head.
    ReceiveResponse,
    /// Reading the status line and header block.
    ReadHeaders,
}

impl fmt::Display for RequestStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            RequestStage::Connect => "connecting",
            RequestStage::OpenRequest => "opening the request",
            RequestStage::Configure => "configuring the request",
            RequestStage::Send => "sending the request",
            RequestStage::Write => "writing the request body",
            RequestStage::ReceiveResponse => "receiving the response",
            RequestStage::ReadHeaders => "reading the response headers",
        };
        f.write_str(text)
    }
}

/// [`Client::request`](crate::Client::request) could not deliver a response
/// head.
///
/// Covers everything from parsing the caller's URI to reading the response
/// header block. It stops there: the response body is read after this call has
/// returned, and fails as [`ResponseBodyError`].
///
/// # Why this is exhaustive
///
/// Every variant is a distinct decision a caller might make — the first four say
/// the *request* was malformed and no retry will help, `BodyTooLarge` and `Body`
/// say the caller's body is the problem, and `Transport` says the network or the
/// peer is. There is no catch-all, so an exhaustive match is a real guarantee
/// rather than a formality, and if a future client capability adds a failure the
/// compiler will say so at every match. That is a breaking change and it is
/// accepted rather than worked around.
#[derive(Debug)]
pub enum RequestError {
    /// The request URI names a scheme other than `http` or `https`, or names
    /// none at all.
    ///
    /// Not silently treated as `http`: a caller that wrote `ftp://` or
    /// `/relative/path` made a mistake, and guessing on their behalf would send
    /// a request somewhere they did not ask for.
    UnsupportedScheme {
        /// The scheme as written, if there was one.
        scheme: Option<String>,
    },
    /// The request URI has no host, or an empty one.
    MissingHost,
    /// The request URI carries userinfo (`user:password@host`).
    ///
    /// Refused rather than dropped, because dropping it would send an
    /// unauthenticated request that looks like the one the caller wrote.
    UserinfoNotSupported,
    /// A request header could not be represented on the wire.
    InvalidRequestHeader {
        /// The offending header.
        name: HeaderName,
        /// What was wrong with it.
        reason: HeaderReason,
    },
    /// The request body declared a length WinHTTP cannot express.
    ///
    /// `WinHttpSendRequest` takes the total length as a `u32`.
    BodyTooLarge {
        /// The length the body's size hint promised.
        length: u64,
    },
    /// A write of the request body reported success and no progress.
    ///
    /// Never observed — a single one-megabyte write was measured to complete
    /// whole — but the platform reports a count rather than a completion, so
    /// the possibility is reported rather than looped on forever.
    WriteStalled {
        /// How much of the buffer was still unwritten.
        remaining: usize,
    },
    /// A line of the response header block could not be parsed.
    MalformedResponseHeader {
        /// The line, as the platform reported it.
        line: String,
    },
    /// The request body could not be framed, or failed.
    Body(BodyError),
    /// A WinHTTP call failed.
    Transport {
        /// What the client was doing.
        stage: RequestStage,
        /// The error the platform reported.
        source: windows::core::Error,
    },
}

impl RequestError {
    /// The WinHTTP-specific meaning of a transport failure, if this *is* one.
    ///
    /// `None` for every other variant, and for a transport failure whose code is
    /// not one WinHTTP defines.
    pub fn win_http(&self) -> Option<WinHttpError> {
        match self {
            RequestError::Transport { source, .. } => Some(WinHttpError::from_error(source)),
            _ => None,
        }
    }

    pub(crate) fn transport(stage: RequestStage) -> impl Fn(windows::core::Error) -> RequestError {
        move |source| RequestError::Transport { stage, source }
    }
}

impl From<BodyError> for RequestError {
    fn from(error: BodyError) -> RequestError {
        RequestError::Body(error)
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::UnsupportedScheme { scheme: Some(s) } => {
                write!(f, "unsupported URI scheme `{s}`: expected http or https")
            }
            RequestError::UnsupportedScheme { scheme: None } => {
                f.write_str("the request URI has no scheme: expected http or https")
            }
            RequestError::MissingHost => f.write_str("the request URI has no host"),
            RequestError::UserinfoNotSupported => {
                f.write_str("the request URI carries userinfo, which is not supported")
            }
            RequestError::InvalidRequestHeader { name, reason } => {
                write!(f, "request header `{name}` cannot be sent: {reason}")
            }
            RequestError::BodyTooLarge { length } => write!(
                f,
                "request body of {length} bytes exceeds the {} the platform can declare",
                u32::MAX
            ),
            RequestError::WriteStalled { remaining } => write!(
                f,
                "a request body write made no progress with {remaining} bytes left"
            ),
            RequestError::MalformedResponseHeader { line } => {
                write!(f, "malformed response header line {line:?}")
            }
            RequestError::Body(source) => write!(f, "request {source}"),
            RequestError::Transport { stage, source } => write!(f, "{stage} failed: {source}"),
        }
    }
}

impl std::error::Error for RequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RequestError::Body(source) => Some(source),
            RequestError::Transport { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Client: the response body
// ---------------------------------------------------------------------------

/// Reading a [`ResponseBody`](crate::ResponseBody) failed.
///
/// Two things can go wrong while draining a response body and they are nothing
/// like each other: the connection ended before the declared length arrived, or
/// the platform refused the read.
///
/// # Why this is exhaustive
///
/// Two variants, both specific, and the set is closed by the shape of the
/// operation: a read either fails or completes, and a completed read either
/// satisfies the declared length or does not. There is nothing a third variant
/// could describe without a change to what reading a body means.
///
/// # Why there is no stage
///
/// [`RequestError::Transport`] needs one because seven different calls can
/// produce it. This type has exactly one: reading the body. The type already
/// says so, and a stage field with a single possible value would be noise.
#[derive(Debug)]
pub enum ResponseBodyError {
    /// The response body ended before it delivered the length it declared.
    ///
    /// This is the failure this crate exists to catch. WinHTTP reports a
    /// connection closed gracefully in the middle of a body as a body that
    /// ended: `WinHttpQueryDataAvailable` returns zero and nothing fails. A
    /// caller that trusted that would parse a truncated document and never
    /// know.
    ///
    /// A body of *unknown* length cannot be checked and never produces this;
    /// see the crate documentation.
    Truncated {
        /// The length the response declared.
        expected: u64,
        /// How much of it arrived.
        received: u64,
    },
    /// A WinHTTP read failed.
    Read(windows::core::Error),
}

impl ResponseBodyError {
    /// The WinHTTP-specific meaning of a read failure, if this *is* one.
    ///
    /// `None` for [`ResponseBodyError::Truncated`], which no platform code
    /// describes — that is the whole point of it.
    pub fn win_http(&self) -> Option<WinHttpError> {
        match self {
            ResponseBodyError::Read(source) => Some(WinHttpError::from_error(source)),
            ResponseBodyError::Truncated { .. } => None,
        }
    }
}

impl fmt::Display for ResponseBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResponseBodyError::Truncated { expected, received } => write!(
                f,
                "the response body ended after {received} of {expected} declared bytes"
            ),
            ResponseBodyError::Read(source) => {
                write!(f, "reading the response body failed: {source}")
            }
        }
    }
}

impl std::error::Error for ResponseBodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResponseBodyError::Read(source) => Some(source),
            ResponseBodyError::Truncated { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Server: the calls whose only failure is HTTP.sys
// ---------------------------------------------------------------------------

/// Which HTTP.sys call failed.
///
/// The server's counterpart to [`RequestStage`], and deliberately a separate
/// vocabulary: a server never connects and never reads a response header block,
/// so folding the two would mean carrying variants that can never occur and a
/// [`Display`](std::fmt::Display) that reads as nonsense on whichever half it was not
/// written for. A test pins that the two do not overlap.
///
/// Receiving a request and sending a response are *not* here. They have their
/// own types — [`AcceptError`] and [`ResponseError`] — because those two
/// operations can fail for reasons other than a platform call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerOperation {
    /// Starting the HTTP Server API.
    Initialize,
    /// Creating the server session.
    CreateSession,
    /// Creating the URL group.
    CreateUrlGroup,
    /// Registering a URL with the group.
    ///
    /// The usual cause is a missing reservation: HTTP.sys refuses a URL that
    /// neither an elevated process nor a `netsh http add urlacl` entry allows.
    AddUrl,
    /// Creating the request queue.
    CreateQueue,
    /// Binding the URL group to the request queue.
    BindUrlGroup,
    /// Reading the request body.
    ReadBody,
    /// Refusing a request without answering it.
    Reject,
    /// Shutting the request queue down.
    Shutdown,
}

impl fmt::Display for ServerOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ServerOperation::Initialize => "starting the HTTP Server API",
            ServerOperation::CreateSession => "creating the server session",
            ServerOperation::CreateUrlGroup => "creating the URL group",
            ServerOperation::AddUrl => "registering a URL",
            ServerOperation::CreateQueue => "creating the request queue",
            ServerOperation::BindUrlGroup => "binding the URL group",
            ServerOperation::ReadBody => "reading the request body",
            ServerOperation::Reject => "rejecting a request",
            ServerOperation::Shutdown => "shutting the request queue down",
        };
        f.write_str(text)
    }
}

/// An HTTP.sys call failed, and that is the only thing that could have.
///
/// Returned by the five server APIs with a singleton reachable set:
/// [`ServerSession::new`](crate::ServerSession::new),
/// [`ServerBuilder::build`](crate::ServerBuilder::build),
/// [`Server::shutdown`](crate::Server::shutdown),
/// [`ShutdownHandle::shutdown`](crate::ShutdownHandle::shutdown),
/// [`Responder::reject`](crate::Responder::reject), and every poll of an
/// [`IncomingBody`](crate::IncomingBody).
///
/// A struct rather than an enum, and that is the design decision worth
/// recording. An enum with one variant per [`ServerOperation`] would put
/// `AddUrl` in the signature of `ServerSession::new`, which cannot produce it —
/// exactly the defect this module exists to remove. A struct has no variants, so
/// it is honest for all five APIs at once, and the operation vocabulary survives
/// in full as a field rather than being dropped for tidiness.
#[derive(Debug)]
pub struct PlatformError {
    /// What the server was doing.
    pub operation: ServerOperation,
    /// The error HTTP.sys reported.
    pub source: windows::core::Error,
}

impl PlatformError {
    /// Whether this is the failure a closed request queue produces.
    ///
    /// [`Server::shutdown`](crate::Server::shutdown) closes the queue, and
    /// measured, an operation *started after* the close fails with
    /// `ERROR_INVALID_HANDLE`. That is the only code read as shutdown here.
    ///
    /// The other shutdown code, `ERROR_OPERATION_ABORTED`, belongs to
    /// [`AcceptError::is_queue_closed`] and to nothing else: measured, a
    /// `receive` that was *already waiting* when the close happened fails with
    /// it, but so does the *read* of a request body the peer truncated — and
    /// that is a fault, not a shutdown. The single `Error` this replaces had to
    /// enforce that with a runtime comparison against a stage field. Here the
    /// type enforces it: a [`PlatformError`] cannot have come from a receive, so
    /// it cannot read an abort as shutdown even by mistake.
    pub fn is_queue_closed(&self) -> bool {
        is_closed_handle(&self.source)
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.operation, self.source)
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn platform(
    operation: ServerOperation,
) -> impl Fn(windows::core::Error) -> PlatformError {
    move |source| PlatformError { operation, source }
}

// ---------------------------------------------------------------------------
// Server: accepting
// ---------------------------------------------------------------------------

/// [`Server::accept`](crate::Server::accept) could not produce a request.
///
/// A caller's decision here is three-way, and the variants are exactly that
/// three-way split: the queue is shut down and the loop should exit quietly (see
/// [`is_queue_closed`](AcceptError::is_queue_closed)); one request was bad and
/// the loop should carry on; or the platform failed and the loop should report.
///
/// # Why this is exhaustive
///
/// The three outcomes above are the whole space of what accepting can do wrong,
/// and each maps to a different action. There is no catch-all, so matching all
/// three is a real guarantee.
///
/// # Why there is no stage
///
/// Only one platform call happens here. [`AcceptError::Receive`] names it, and a
/// stage field with one value would be noise — but see
/// [`is_queue_closed`](AcceptError::is_queue_closed), because that single value
/// is *load-bearing*.
#[derive(Debug)]
pub enum AcceptError {
    /// A request arrived that would not fit in the buffer offered for it.
    ///
    /// `winasio::httpsys` grows its buffer and retries a bounded number of
    /// times, then discards the request rather than looping forever. The request
    /// is gone by the time this is reported — there is nothing to answer and
    /// nothing to retry — so an accept loop must treat it as one bad request and
    /// carry on, not as a reason to re-receive.
    RequestTooLarge {
        /// The largest buffer that was offered.
        capacity: usize,
    },
    /// An inbound request could not be expressed as an [`http::Request`].
    ///
    /// Reported rather than repaired: a request whose target is not a URI is not
    /// a request this crate can hand to a router, and guessing a replacement
    /// would route a caller somewhere nobody asked for. A bodiless `400` is put
    /// on the wire on a best-effort basis first, so the peer is not left waiting
    /// out HTTP.sys's request timeout.
    MalformedRequest {
        /// Which part of the request was the problem.
        reason: RequestReason,
        /// The offending text, lossily decoded for the message.
        value: String,
    },
    /// The `receive` call itself failed.
    Receive(windows::core::Error),
}

impl AcceptError {
    /// Whether this is the failure a closed request queue produces.
    ///
    /// Measured, there are two distinct ways a close surfaces at a receive:
    ///
    /// - a receive *started after* the close fails with `ERROR_INVALID_HANDLE`;
    /// - a receive that was *already waiting* when the close happened fails with
    ///   `ERROR_OPERATION_ABORTED`.
    ///
    /// Both are shutdown, and an accept loop needs to tell them apart from a
    /// genuine fault so that it can exit quietly instead of reporting shutdown
    /// as an error.
    ///
    /// This is the only place `ERROR_OPERATION_ABORTED` is read as shutdown,
    /// because elsewhere it means something entirely different: measured, a
    /// request body that the peer truncated fails its *read* with the same code,
    /// and that is a fault. Under the single `Error` this replaces that rule was
    /// a runtime `matches!` against a stage field; here it is the type, and a
    /// [`PlatformError`] carrying a body read has no way to reach this code path
    /// at all.
    pub fn is_queue_closed(&self) -> bool {
        match self {
            AcceptError::Receive(source) => {
                is_closed_handle(source) || source.code().0 == ERROR_OPERATION_ABORTED
            }
            AcceptError::RequestTooLarge { .. } | AcceptError::MalformedRequest { .. } => false,
        }
    }
}

impl fmt::Display for AcceptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AcceptError::RequestTooLarge { capacity } => write!(
                f,
                "a request did not fit in {capacity} bytes and was discarded unanswered"
            ),
            AcceptError::MalformedRequest { reason, value } => {
                write!(f, "the inbound request is unusable: {reason} {value:?}")
            }
            AcceptError::Receive(source) => write!(f, "receiving a request failed: {source}"),
        }
    }
}

impl std::error::Error for AcceptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AcceptError::Receive(source) => Some(source),
            AcceptError::RequestTooLarge { .. } | AcceptError::MalformedRequest { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Server: responding
// ---------------------------------------------------------------------------

/// Which half of a response was on the wire when HTTP.sys refused it.
///
/// Two calls can fail while answering a request, and the difference matters to a
/// caller: a head that never went out leaves the peer with nothing, while a body
/// that stopped half way leaves it with a message it will read as truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStage {
    /// Sending the response head.
    Head,
    /// Sending a piece of the response body.
    Body,
}

impl fmt::Display for SendStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            SendStage::Head => "sending the response head",
            SendStage::Body => "sending the response body",
        };
        f.write_str(text)
    }
}

/// [`Responder::send`](crate::Responder::send) could not put a response on the
/// wire.
///
/// Note what is *not* here: a failing service. That is [`ServeError::Service`],
/// and it belongs to the APIs that call a service —
/// [`Server::serve_one`](crate::Server::serve_one),
/// [`Server::serve`](crate::Server::serve) and
/// [`Accepted::serve`](crate::Accepted::serve). A caller who builds the response
/// themselves and hands it to `send` cannot get one, and the type says so.
///
/// # Why this is exhaustive
///
/// Three variants covering the three things that can go wrong once a response
/// exists: its declared length is unusable, its body will not frame, or the
/// platform refused it. No catch-all.
#[derive(Debug)]
pub enum ResponseError {
    /// A response `Content-Length` this crate could not use.
    ///
    /// Either the value is not a number, or the same response carries two that
    /// disagree. Both would put a wrong length on the wire, and measured,
    /// HTTP.sys honours a wrong length verbatim rather than correcting it.
    ///
    /// Deliberately not folded into [`BodyError`]: it looks like an overlap and
    /// is not, because the client half *refuses* a caller-set `Content-Length`
    /// rather than parsing one, so only the server can produce this.
    BadContentLength {
        /// The value as the caller wrote it, or both values when they clash.
        value: String,
    },
    /// The response body could not be framed, or failed.
    Body(BodyError),
    /// An HTTP.sys send failed.
    Send {
        /// Which half of the response was going out.
        stage: SendStage,
        /// The error HTTP.sys reported.
        source: windows::core::Error,
    },
}

impl ResponseError {
    /// Whether this is the failure a closed request queue produces.
    ///
    /// Only `ERROR_INVALID_HANDLE`. `ERROR_OPERATION_ABORTED` is a fault here —
    /// see [`AcceptError::is_queue_closed`] for why that code is read as
    /// shutdown at a receive and nowhere else.
    pub fn is_queue_closed(&self) -> bool {
        match self {
            ResponseError::Send { source, .. } => is_closed_handle(source),
            ResponseError::BadContentLength { .. } | ResponseError::Body(_) => false,
        }
    }

    pub(crate) fn send(stage: SendStage) -> impl Fn(windows::core::Error) -> ResponseError {
        move |source| ResponseError::Send { stage, source }
    }
}

impl From<BodyError> for ResponseError {
    fn from(error: BodyError) -> ResponseError {
        ResponseError::Body(error)
    }
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResponseError::BadContentLength { value } => write!(
                f,
                "the response declares `Content-Length: {value}`, which cannot be used"
            ),
            ResponseError::Body(source) => write!(f, "response {source}"),
            ResponseError::Send { stage, source } => write!(f, "{stage} failed: {source}"),
        }
    }
}

impl std::error::Error for ResponseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResponseError::Body(source) => Some(source),
            ResponseError::Send { source, .. } => Some(source),
            ResponseError::BadContentLength { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Server: accepting and responding together
// ---------------------------------------------------------------------------

/// Serving one request through a [`tower_service::Service`] failed.
///
/// Returned by the three APIs that do the whole job —
/// [`Server::serve_one`](crate::Server::serve_one),
/// [`Server::serve`](crate::Server::serve) and
/// [`Accepted::serve`](crate::Accepted::serve) — and it is a union of the two
/// halves plus the one failure only these APIs can have: the service itself.
///
/// A union of the two types rather than a flattened enum, so that a caller who
/// already handles an [`AcceptError`] or a [`ResponseError`] elsewhere handles
/// the same type here.
///
/// # Why this is exhaustive
///
/// Serving is accept, then call, then respond. Three variants, one per step, and
/// nothing else happens. No catch-all.
#[derive(Debug)]
pub enum ServeError {
    /// The request could not be accepted.
    Accept(AcceptError),
    /// The service returned an error instead of a response, or was never ready.
    ///
    /// A bodiless `500` is put on the wire on a best-effort basis before this is
    /// returned, so the peer is not left waiting; the error itself is still
    /// handed back rather than swallowed.
    Service(Box<dyn std::error::Error + Send + Sync>),
    /// The response could not be sent.
    Response(ResponseError),
}

impl ServeError {
    /// Whether this is the failure a closed request queue produces.
    ///
    /// Delegates: shutdown can surface either at the receive or at the send of a
    /// request that was already in hand. A service failure is never shutdown.
    pub fn is_queue_closed(&self) -> bool {
        match self {
            ServeError::Accept(error) => error.is_queue_closed(),
            ServeError::Response(error) => error.is_queue_closed(),
            ServeError::Service(_) => false,
        }
    }
}

impl From<AcceptError> for ServeError {
    fn from(error: AcceptError) -> ServeError {
        ServeError::Accept(error)
    }
}

impl From<ResponseError> for ServeError {
    fn from(error: ResponseError) -> ServeError {
        ServeError::Response(error)
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServeError::Accept(error) => write!(f, "{error}"),
            ServeError::Service(source) => write!(f, "the service failed: {source}"),
            ServeError::Response(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServeError::Accept(error) => Some(error),
            ServeError::Service(source) => Some(source.as_ref()),
            ServeError::Response(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_code(code: u32) -> windows::core::Error {
        windows::core::Error::from_hresult(windows::core::HRESULT(code as i32))
    }

    #[test]
    fn a_transport_error_keeps_the_platform_code_reachable() {
        // The whole reason `Transport` holds a `windows::core::Error` rather
        // than a formatted string: a caller that wants to distinguish a timeout
        // from a refused connection can still do so.
        let error = RequestError::Transport {
            stage: RequestStage::ReceiveResponse,
            source: platform_code(0x8007_2EE2),
        };
        assert_eq!(error.win_http(), Some(WinHttpError::Timeout));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn a_client_config_failure_always_has_a_winhttp_reading() {
        // Not an `Option`, unlike the `Error::win_http` this replaces: every
        // failure this type carries came out of a WinHTTP call.
        let error = ClientConfigError {
            stage: ClientConfigStage::OpenSession,
            source: platform_code(0x8007_2EE2),
        };
        assert_eq!(error.win_http(), WinHttpError::Timeout);
        // A code WinHTTP does not name still reads as *something*.
        let odd = ClientConfigError {
            stage: ClientConfigStage::SetTimeouts,
            source: platform_code(0x8007_0006),
        };
        assert!(matches!(odd.win_http(), WinHttpError::Other(_)));
    }

    #[test]
    fn a_truncated_body_is_not_a_platform_failure() {
        // Truncation is the failure no platform code describes -- that is the
        // entire point of it.
        let error = ResponseBodyError::Truncated {
            expected: 10,
            received: 3,
        };
        assert_eq!(error.win_http(), None);
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn a_response_body_read_failure_keeps_its_winhttp_meaning() {
        let error = ResponseBodyError::Read(platform_code(0x8007_2EE2));
        assert_eq!(error.win_http(), Some(WinHttpError::Timeout));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn a_server_error_is_not_read_as_a_winhttp_code() {
        // The `Error::win_http` this replaces had to return `None` for every
        // server variant, which made it dead weight for every server caller. The
        // server types simply do not have the accessor now, and this records
        // that the platform error is still reachable through `source`.
        let error = PlatformError {
            operation: ServerOperation::Shutdown,
            source: platform_code(0x8007_2EE2),
        };
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn a_closed_queue_is_distinguishable_from_a_real_fault() {
        // Measured, a closed queue answers every operation started afterwards
        // with ERROR_INVALID_HANDLE.
        assert!(PlatformError {
            operation: ServerOperation::Shutdown,
            source: platform_code(0x8007_0006),
        }
        .is_queue_closed());
        assert!(AcceptError::Receive(platform_code(0x8007_0006)).is_queue_closed());
        assert!(ResponseError::Send {
            stage: SendStage::Head,
            source: platform_code(0x8007_0006),
        }
        .is_queue_closed());

        // A `receive` that was already waiting when the close happened is
        // aborted rather than invalidated -- measured, and the reason this needs
        // two codes rather than one.
        assert!(AcceptError::Receive(platform_code(0x8007_03E3)).is_queue_closed());

        // The same code on a body read is a truncated request, which is a fault.
        // Reading it as shutdown would silently swallow a broken body. Under the
        // single `Error` this replaces the rule was a runtime comparison against
        // a stage field; now `PlatformError` has no way to reach the abort rule
        // at all.
        assert!(!PlatformError {
            operation: ServerOperation::ReadBody,
            source: platform_code(0x8007_03E3),
        }
        .is_queue_closed());

        // ERROR_NETNAME_DELETED, which a second send on one request produces, is
        // a fault and must not look like shutdown.
        assert!(!ResponseError::Send {
            stage: SendStage::Body,
            source: platform_code(0x8007_04CD),
        }
        .is_queue_closed());

        // Nor is an aborted send a shutdown.
        assert!(!ResponseError::Send {
            stage: SendStage::Body,
            source: platform_code(0x8007_03E3),
        }
        .is_queue_closed());

        // Non-platform variants are never shutdown, whatever else they say.
        assert!(!AcceptError::RequestTooLarge { capacity: 4096 }.is_queue_closed());
        assert!(!AcceptError::MalformedRequest {
            reason: RequestReason::Target,
            value: "x".into(),
        }
        .is_queue_closed());
        assert!(!ResponseError::BadContentLength { value: "x".into() }.is_queue_closed());
    }

    #[test]
    fn a_shutdown_seen_through_serve_is_still_a_shutdown() {
        // `Server::serve` exits quietly on shutdown, and shutdown can surface
        // either at the receive or at the send of a request already in hand.
        assert!(
            ServeError::Accept(AcceptError::Receive(platform_code(0x8007_03E3))).is_queue_closed()
        );
        assert!(ServeError::Response(ResponseError::Send {
            stage: SendStage::Head,
            source: platform_code(0x8007_0006),
        })
        .is_queue_closed());
        // A failing service is never a shutdown, however the loop is written.
        assert!(!ServeError::Service("nope".into()).is_queue_closed());
    }

    #[test]
    fn every_variant_says_something_specific() {
        // A `Display` that produced the same text for two different failures
        // would defeat the point of having separate variants at all. Checked
        // across *all* the types together, because they share a namespace in a
        // caller's log even though they no longer share a type.
        let messages = [
            RequestError::UnsupportedScheme {
                scheme: Some("ftp".into()),
            }
            .to_string(),
            RequestError::UnsupportedScheme { scheme: None }.to_string(),
            RequestError::MissingHost.to_string(),
            RequestError::UserinfoNotSupported.to_string(),
            RequestError::InvalidRequestHeader {
                name: HeaderName::from_static("x-note"),
                reason: HeaderReason::NotVisibleAscii,
            }
            .to_string(),
            RequestError::BodyTooLarge { length: 1 << 33 }.to_string(),
            RequestError::WriteStalled { remaining: 5 }.to_string(),
            RequestError::MalformedResponseHeader {
                line: "nonsense".into(),
            }
            .to_string(),
            RequestError::Body(BodyError::FramingHeaderNotAllowed {
                name: HeaderName::from_static("transfer-encoding"),
            })
            .to_string(),
            RequestError::Body(BodyError::LengthMismatch {
                declared: 4,
                actual: 2,
            })
            .to_string(),
            ResponseBodyError::Truncated {
                expected: 10,
                received: 3,
            }
            .to_string(),
            AcceptError::RequestTooLarge { capacity: 4096 }.to_string(),
            AcceptError::MalformedRequest {
                reason: RequestReason::Target,
                value: "not a uri".into(),
            }
            .to_string(),
            ResponseError::BadContentLength {
                value: "seven".into(),
            }
            .to_string(),
            // The same `BodyError` under the other wrapper must not read the
            // same: "request body promised" and "response body promised" are
            // different facts, and the wrapper is what supplies the difference.
            ResponseError::Body(BodyError::LengthMismatch {
                declared: 4,
                actual: 2,
            })
            .to_string(),
            ResponseError::Body(BodyError::FramingHeaderNotAllowed {
                name: HeaderName::from_static("transfer-encoding"),
            })
            .to_string(),
        ];
        let mut seen: Vec<&str> = messages.iter().map(String::as_str).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "two variants share a message");
    }

    #[test]
    fn the_client_and_server_stage_vocabularies_do_not_overlap() {
        // They are separate enums precisely so that a server never reports
        // "connecting" and a client never reports "binding the URL group". If a
        // future edit merged them, this comparison would stop compiling or start
        // matching.
        let mut client: Vec<String> = [
            RequestStage::Connect,
            RequestStage::OpenRequest,
            RequestStage::Configure,
            RequestStage::Send,
            RequestStage::Write,
            RequestStage::ReceiveResponse,
            RequestStage::ReadHeaders,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        client.extend(
            [
                ClientConfigStage::OpenSession,
                ClientConfigStage::SetTimeouts,
                ClientConfigStage::SetRedirectPolicy,
            ]
            .iter()
            .map(ToString::to_string),
        );

        let mut server: Vec<String> = [
            ServerOperation::Initialize,
            ServerOperation::CreateSession,
            ServerOperation::CreateUrlGroup,
            ServerOperation::AddUrl,
            ServerOperation::CreateQueue,
            ServerOperation::BindUrlGroup,
            ServerOperation::ReadBody,
            ServerOperation::Reject,
            ServerOperation::Shutdown,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        server.extend(
            [SendStage::Head, SendStage::Body]
                .iter()
                .map(ToString::to_string),
        );

        // Every stage on each side says something distinct.
        for side in [&client, &server] {
            let mut unique = side.clone();
            unique.sort_unstable();
            let total = unique.len();
            unique.dedup();
            assert_eq!(unique.len(), total, "two stages share a message");
        }

        // "reading the response body" and "reading the request body" were the
        // near-miss pair under the single-`Error` design; the first has since
        // moved into `ResponseBodyError`, which has no stage at all. The rule
        // still holds for everything that remains.
        for text in &server {
            assert!(
                !client.contains(text),
                "{text:?} appears in both stage vocabularies"
            );
        }
    }
}
