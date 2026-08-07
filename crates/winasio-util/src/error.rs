// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The one error type this crate returns, and the vocabulary it needs.

use std::fmt;

use http::header::HeaderName;
use winasio::winhttp::WinHttpError;

/// What the client was doing when a platform call failed.
///
/// Carried by [`Error::Transport`] so that a `windows` error code is not left
/// to speak for itself. `ERROR_WINHTTP_TIMEOUT` means something quite different
/// during [`Stage::Connect`] than during [`Stage::ReadBody`], and the caller
/// should not have to guess which one it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Resolving the host and opening a connection.
    Connect,
    /// Creating the request handle.
    OpenRequest,
    /// Applying session or request options before sending.
    Configure,
    /// Submitting the request head.
    Send,
    /// Writing a chunk of the request body.
    Write,
    /// Waiting for the response head.
    ReceiveResponse,
    /// Reading the status line and header block.
    ReadHeaders,
    /// Reading the response body.
    ReadBody,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Stage::Connect => "connecting",
            Stage::OpenRequest => "opening the request",
            Stage::Configure => "configuring the request",
            Stage::Send => "sending the request",
            Stage::Write => "writing the request body",
            Stage::ReceiveResponse => "receiving the response",
            Stage::ReadHeaders => "reading the response headers",
            Stage::ReadBody => "reading the response body",
        };
        f.write_str(text)
    }
}

/// What the server was doing when a platform call failed.
///
/// The server's counterpart to [`Stage`], and deliberately a *separate* enum.
/// A server never connects, never opens a request and never reads a response
/// header block, so folding the two would mean carrying eight variants that can
/// never occur alongside the eleven that can — and a [`Display`](fmt::Display)
/// for the client's stages that would read as nonsense on a server.
///
/// The error *type* is still shared: a caller using both halves of this crate
/// gets one `Result` type, which is the part that was worth keeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStage {
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
    /// Waiting for the next request.
    Receive,
    /// Reading the request body.
    ReadBody,
    /// Sending the response head.
    SendHead,
    /// Sending a piece of the response body.
    SendBody,
    /// Refusing a request without answering it.
    Reject,
    /// Shutting the request queue down.
    Shutdown,
}

impl fmt::Display for ServerStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ServerStage::Initialize => "starting the HTTP Server API",
            ServerStage::CreateSession => "creating the server session",
            ServerStage::CreateUrlGroup => "creating the URL group",
            ServerStage::AddUrl => "registering a URL",
            ServerStage::CreateQueue => "creating the request queue",
            ServerStage::BindUrlGroup => "binding the URL group",
            ServerStage::Receive => "receiving a request",
            ServerStage::ReadBody => "reading the request body",
            ServerStage::SendHead => "sending the response head",
            ServerStage::SendBody => "sending the response body",
            ServerStage::Reject => "rejecting a request",
            ServerStage::Shutdown => "shutting the request queue down",
        };
        f.write_str(text)
    }
}

/// Why an inbound request could not be expressed as an [`http::Request`].
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

/// Why a request header could not be sent.
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

/// Everything that can go wrong in this crate.
///
/// Deliberately not a single opaque type wrapping a string. Each variant names
/// a distinct failure that a caller might reasonably want to handle
/// differently, and in particular a body that was cut off
/// ([`Error::TruncatedBody`]) is nothing like a body that ended.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
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
    /// The caller set a header that decides message framing.
    ///
    /// `Transfer-Encoding` is chosen by this crate: the server half frames
    /// chunks itself and the client half declares a length, so a caller-supplied
    /// transfer coding would describe a body that was never encoded that way.
    ///
    /// A response `Content-Length` is *not* refused — it is read as a
    /// declaration and checked against the body, because real services such as
    /// an `axum::Router` set it themselves.
    FramingHeaderNotAllowed {
        /// The offending header.
        name: HeaderName,
    },
    /// A response `Content-Length` this crate could not use.
    ///
    /// Either the value is not a number, or the same response carries two that
    /// disagree. Both would put a wrong length on the wire, and measured,
    /// HTTP.sys honours a wrong length verbatim rather than correcting it.
    BadContentLength {
        /// The value as the caller wrote it, or both values when they clash.
        value: String,
    },
    /// The request body declared a length WinHTTP cannot express.
    ///
    /// `WinHttpSendRequest` takes the total length as a `u32`.
    BodyTooLarge {
        /// The length the body's size hint promised.
        length: u64,
    },
    /// The request body produced a different number of bytes than it promised.
    ///
    /// Caught here rather than left to the platform, which reports it as a
    /// send timeout thirty seconds later.
    BodyLengthMismatch {
        /// What the size hint said.
        declared: u64,
        /// What the body actually produced.
        actual: u64,
    },
    /// The request body itself failed.
    BodyError(Box<dyn std::error::Error + Send + Sync>),
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
    /// The response body ended before it delivered the length it declared.
    ///
    /// This is the failure this crate exists to catch. WinHTTP reports a
    /// connection closed gracefully in the middle of a body as a body that
    /// ended: `WinHttpQueryDataAvailable` returns zero and nothing fails. A
    /// caller that trusted that would parse a truncated document and never
    /// know.
    TruncatedBody {
        /// The length the response declared.
        expected: u64,
        /// How much of it arrived.
        received: u64,
    },
    /// A platform call failed.
    Transport {
        /// What the client was doing.
        stage: Stage,
        /// The error the platform reported.
        source: windows::core::Error,
    },
    /// An HTTP.sys call failed while serving.
    ///
    /// Separate from [`Error::Transport`] because the stage vocabularies do not
    /// overlap and the error codes come from different subsystems: these are
    /// Win32 codes, so [`Error::win_http`] does not try to interpret them.
    Platform {
        /// What the server was doing.
        stage: ServerStage,
        /// The error the platform reported.
        source: windows::core::Error,
    },
    /// A request arrived that would not fit in the buffer offered for it.
    ///
    /// `winasio::httpsys` grows its buffer and retries a bounded number of
    /// times, then discards the request rather than looping forever. The
    /// request is gone by the time this is reported — there is nothing to
    /// answer and nothing to retry — so an accept loop must treat it as one bad
    /// request and carry on, not as a reason to re-receive.
    RequestTooLarge {
        /// The largest buffer that was offered.
        capacity: usize,
    },
    /// An inbound request could not be expressed as an [`http::Request`].
    ///
    /// Reported rather than repaired: a request whose target is not a URI is
    /// not a request this crate can hand to a router, and guessing a
    /// replacement would route a caller somewhere nobody asked for.
    MalformedRequest {
        /// Which part of the request was the problem.
        reason: RequestReason,
        /// The offending text, lossily decoded for the message.
        value: String,
    },
    /// The service returned an error instead of a response.
    ///
    /// A bodiless `500` is put on the wire on a best-effort basis before this
    /// is returned, so the peer is not left waiting; the error itself is still
    /// handed back rather than swallowed.
    Service(Box<dyn std::error::Error + Send + Sync>),
}

impl Error {
    pub(crate) fn transport(stage: Stage) -> impl Fn(windows::core::Error) -> Error {
        move |source| Error::Transport { stage, source }
    }

    pub(crate) fn platform(stage: ServerStage) -> impl Fn(windows::core::Error) -> Error {
        move |source| Error::Platform { stage, source }
    }

    /// Whether this is the failure a closed request queue produces.
    ///
    /// [`Server::shutdown`](crate::Server::shutdown) closes the queue, and
    /// measured, there are two distinct ways that surfaces:
    ///
    /// - an operation *started after* the close fails with
    ///   `ERROR_INVALID_HANDLE`;
    /// - a `receive` that was *already waiting* when the close happened fails
    ///   with `ERROR_OPERATION_ABORTED`.
    ///
    /// Both are shutdown, and an accept loop needs to tell them apart from a
    /// genuine fault so that it can exit quietly instead of reporting shutdown
    /// as an error.
    ///
    /// The abort code is only read as shutdown at [`ServerStage::Receive`],
    /// because the same code means something entirely different elsewhere:
    /// measured, a request body that the peer truncated fails its *read* with
    /// `ERROR_OPERATION_ABORTED`, and that is a fault, not a shutdown.
    pub fn is_queue_closed(&self) -> bool {
        const ERROR_INVALID_HANDLE: i32 = 0x8007_0006u32 as i32;
        const ERROR_OPERATION_ABORTED: i32 = 0x8007_03E3u32 as i32;
        match self {
            Error::Platform { stage, source } => {
                source.code().0 == ERROR_INVALID_HANDLE
                    || (source.code().0 == ERROR_OPERATION_ABORTED
                        && matches!(stage, ServerStage::Receive))
            }
            _ => false,
        }
    }

    /// The WinHTTP-specific meaning of a transport failure, if there is one.
    ///
    /// Returns `None` for every non-transport variant, and for a transport
    /// failure whose code is not one WinHTTP defines.
    pub fn win_http(&self) -> Option<WinHttpError> {
        match self {
            Error::Transport { source, .. } => Some(WinHttpError::from_error(source)),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnsupportedScheme { scheme: Some(s) } => {
                write!(f, "unsupported URI scheme `{s}`: expected http or https")
            }
            Error::UnsupportedScheme { scheme: None } => {
                f.write_str("the request URI has no scheme: expected http or https")
            }
            Error::MissingHost => f.write_str("the request URI has no host"),
            Error::UserinfoNotSupported => {
                f.write_str("the request URI carries userinfo, which is not supported")
            }
            Error::InvalidRequestHeader { name, reason } => {
                write!(f, "request header `{name}` cannot be sent: {reason}")
            }
            Error::FramingHeaderNotAllowed { name } => write!(
                f,
                "header `{name}` decides message framing and is chosen by this crate"
            ),
            Error::BadContentLength { value } => {
                write!(
                    f,
                    "the response declares `Content-Length: {value}`, which cannot be used"
                )
            }
            Error::BodyTooLarge { length } => write!(
                f,
                "request body of {length} bytes exceeds the {} the platform can declare",
                u32::MAX
            ),
            Error::BodyLengthMismatch { declared, actual } => write!(
                f,
                "request body promised {declared} bytes and produced {actual}"
            ),
            Error::BodyError(source) => write!(f, "the request body failed: {source}"),
            Error::WriteStalled { remaining } => write!(
                f,
                "a request body write made no progress with {remaining} bytes left"
            ),
            Error::MalformedResponseHeader { line } => {
                write!(f, "malformed response header line {line:?}")
            }
            Error::TruncatedBody { expected, received } => write!(
                f,
                "the response body ended after {received} of {expected} declared bytes"
            ),
            Error::Transport { stage, source } => write!(f, "{stage} failed: {source}"),
            Error::Platform { stage, source } => write!(f, "{stage} failed: {source}"),
            Error::RequestTooLarge { capacity } => write!(
                f,
                "a request did not fit in {capacity} bytes and was discarded unanswered"
            ),
            Error::MalformedRequest { reason, value } => {
                write!(f, "the inbound request is unusable: {reason} {value:?}")
            }
            Error::Service(source) => write!(f, "the service failed: {source}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::BodyError(source) => Some(source.as_ref()),
            Error::Service(source) => Some(source.as_ref()),
            Error::Transport { source, .. } => Some(source),
            Error::Platform { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transport_error_keeps_the_platform_code_reachable() {
        // The whole reason `Transport` holds a `windows::core::Error` rather
        // than a formatted string: a caller that wants to distinguish a
        // timeout from a refused connection can still do so.
        let error = Error::Transport {
            stage: Stage::ReadBody,
            source: windows::core::Error::from_hresult(windows::core::HRESULT(
                0x8007_2EE2u32 as i32,
            )),
        };
        assert_eq!(error.win_http(), Some(WinHttpError::Timeout));
    }

    #[test]
    fn a_non_transport_error_has_no_platform_code() {
        let error = Error::TruncatedBody {
            expected: 10,
            received: 3,
        };
        assert_eq!(error.win_http(), None);
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn every_variant_says_something_specific() {
        // A `Display` that produced the same text for two different failures
        // would defeat the point of having separate variants at all.
        let messages = [
            Error::UnsupportedScheme {
                scheme: Some("ftp".into()),
            }
            .to_string(),
            Error::UnsupportedScheme { scheme: None }.to_string(),
            Error::MissingHost.to_string(),
            Error::UserinfoNotSupported.to_string(),
            Error::InvalidRequestHeader {
                name: HeaderName::from_static("x-note"),
                reason: HeaderReason::NotVisibleAscii,
            }
            .to_string(),
            Error::FramingHeaderNotAllowed {
                name: HeaderName::from_static("transfer-encoding"),
            }
            .to_string(),
            Error::BadContentLength {
                value: "seven".into(),
            }
            .to_string(),
            Error::BodyTooLarge { length: 1 << 33 }.to_string(),
            Error::BodyLengthMismatch {
                declared: 4,
                actual: 2,
            }
            .to_string(),
            Error::MalformedResponseHeader {
                line: "nonsense".into(),
            }
            .to_string(),
            Error::TruncatedBody {
                expected: 10,
                received: 3,
            }
            .to_string(),
            Error::RequestTooLarge { capacity: 4096 }.to_string(),
            Error::MalformedRequest {
                reason: RequestReason::Target,
                value: "not a uri".into(),
            }
            .to_string(),
        ];
        let mut seen: Vec<&str> = messages.iter().map(String::as_str).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "two variants share a message");
    }

    fn platform(code: u32) -> windows::core::Error {
        windows::core::Error::from_hresult(windows::core::HRESULT(code as i32))
    }

    #[test]
    fn a_closed_queue_is_distinguishable_from_a_real_fault() {
        // The whole point of `is_queue_closed`: an accept loop must exit
        // quietly on shutdown and report anything else. Measured, a closed
        // queue answers every operation with ERROR_INVALID_HANDLE.
        let closed = Error::Platform {
            stage: ServerStage::Receive,
            source: platform(0x8007_0006),
        };
        assert!(closed.is_queue_closed());

        // A `receive` that was already waiting when the close happened is
        // aborted rather than invalidated -- measured, and the reason this
        // needs two codes rather than one.
        let aborted = Error::Platform {
            stage: ServerStage::Receive,
            source: platform(0x8007_03E3),
        };
        assert!(aborted.is_queue_closed());

        // The same code on a body read is a truncated request, which is a
        // fault. Reading it as shutdown would silently swallow a broken body.
        let truncated = Error::Platform {
            stage: ServerStage::ReadBody,
            source: platform(0x8007_03E3),
        };
        assert!(!truncated.is_queue_closed());

        // ERROR_NETNAME_DELETED, which a second send on one request produces,
        // is a fault and must not look like shutdown.
        let fault = Error::Platform {
            stage: ServerStage::SendBody,
            source: platform(0x8007_04CD),
        };
        assert!(!fault.is_queue_closed());

        // A client-side transport error is never a closed server queue, even
        // if the code happened to coincide.
        let client = Error::Transport {
            stage: Stage::ReadBody,
            source: platform(0x8007_0006),
        };
        assert!(!client.is_queue_closed());

        assert!(!Error::MissingHost.is_queue_closed());
    }

    #[test]
    fn a_server_error_is_not_read_as_a_winhttp_code() {
        // `win_http` interprets WinHTTP's numbering. HTTP.sys reports Win32
        // codes in the same numeric space, so interpreting them would invent a
        // WinHTTP meaning for an error WinHTTP never produced.
        let error = Error::Platform {
            stage: ServerStage::SendHead,
            source: platform(0x8007_2EE2),
        };
        assert_eq!(error.win_http(), None);
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn the_two_stage_vocabularies_do_not_overlap() {
        // They are separate enums precisely so that a server never reports
        // "connecting" and a client never reports "binding the URL group".
        // If a future edit merged them, this comparison would stop compiling
        // or start matching.
        let client: Vec<String> = [
            Stage::Connect,
            Stage::OpenRequest,
            Stage::Configure,
            Stage::Send,
            Stage::Write,
            Stage::ReceiveResponse,
            Stage::ReadHeaders,
            Stage::ReadBody,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let server: Vec<String> = [
            ServerStage::Initialize,
            ServerStage::CreateSession,
            ServerStage::CreateUrlGroup,
            ServerStage::AddUrl,
            ServerStage::CreateQueue,
            ServerStage::BindUrlGroup,
            ServerStage::Receive,
            ServerStage::ReadBody,
            ServerStage::SendHead,
            ServerStage::SendBody,
            ServerStage::Reject,
            ServerStage::Shutdown,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();

        // Every server stage says something distinct.
        let mut unique = server.clone();
        unique.sort_unstable();
        let total = unique.len();
        unique.dedup();
        assert_eq!(unique.len(), total, "two server stages share a message");

        // "reading the response body" and "reading the request body" are the
        // near-miss pair: they must not be the same string.
        for text in &server {
            assert!(
                !client.contains(text),
                "{text:?} appears in both stage vocabularies"
            );
        }
    }
}
