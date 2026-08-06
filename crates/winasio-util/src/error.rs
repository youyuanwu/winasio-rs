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
    /// `Content-Length` and `Transfer-Encoding` are chosen by this crate from
    /// the body's size hint. A caller-supplied value would replace the
    /// computed one on the wire and describe a body that was never sent.
    FramingHeaderNotAllowed {
        /// The offending header.
        name: HeaderName,
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
}

impl Error {
    pub(crate) fn transport(stage: Stage) -> impl Fn(windows::core::Error) -> Error {
        move |source| Error::Transport { stage, source }
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
                "request header `{name}` decides message framing and is chosen from the body"
            ),
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
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::BodyError(source) => Some(source.as_ref()),
            Error::Transport { source, .. } => Some(source),
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
                name: HeaderName::from_static("content-length"),
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
        ];
        let mut seen: Vec<&str> = messages.iter().map(String::as_str).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "two variants share a message");
    }
}
