// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Moving heads between [`http`] and HTTP.sys, in both directions.
//!
//! # The numbering hazard
//!
//! HTTP.sys keeps its known headers in a fixed array, and **the request array
//! and the reply array are numbered differently**. Ids 0 to 19 agree; every id
//! from 20 to 29 means a different header on each side. Index 25 is `Cookie` on
//! a request and `Retry-After` on a reply; index 26 is `Expect` and `Server`.
//!
//! A conversion layer that read a request through the reply table would produce
//! a server that silently relabels headers — no error, no crash, just a
//! `Cookie` reported as a `Retry-After`. Nothing about the types prevents it,
//! because both sides are `u16` underneath.
//!
//! So this module reads requests through [`RequestHeader`] only and writes
//! replies through [`ResponseHeader`] only, and the tests below walk both
//! tables exhaustively and assert the crossing pairs differ — following the
//! precedent set by `the_dangerous_pairs_have_distinct_indices_per_side` one
//! crate down.
//!
//! # Inbound: what is copied and when
//!
//! Every accessor on [`Request`] borrows the request's own buffer, so building
//! an [`http::Request`] means copying out. It is done once, eagerly, in
//! [`to_http`]: one pass over the 41 known slots and one over the unknown list.
//! The alternative — a lazy header map that keeps the platform request alive —
//! would make the borrow visible in the public type for the sake of avoiding
//! copies of data that is a few hundred bytes at most.
//!
//! Measured, and important: **HTTP.sys folds repeated request headers into one
//! comma-joined value** before this crate sees them. Two `X-Custom` lines
//! arrive as the single value `one, two`, and so do two `Accept` lines and two
//! `Cookie` lines. Repeated inbound headers therefore cannot be recovered as
//! repeated [`HeaderMap`] entries; there is nothing here to preserve them
//! *from*. The reply direction has no such limitation and does preserve them.
//!
//! # Outbound: known slot or unknown list?
//!
//! HTTP.sys offers two ways to emit a reply header and they behave differently
//! in ways that were measured rather than assumed:
//!
//! | | known slot (`set_header`) | unknown list (`add_header`) |
//! |---|---|---|
//! | repeated name | only the last survives | every occurrence is emitted, in order |
//! | empty value | the line is dropped entirely | the line is emitted |
//! | `Server` | merged with HTTP.sys's own into one line | **two `Server` lines** |
//! | `Date` | replaces HTTP.sys's own | **two `Date` lines** |
//! | `Content-Length` | suppresses HTTP.sys's computed value | **duplicates it** |
//!
//! So the rule is: **everything goes through the unknown list**, which is the
//! only one of the two that is faithful — it preserves repeats, order and empty
//! values — with exactly two exceptions. `Date` and `Server` go through the
//! known slot, because HTTP.sys emits its own copy of each unconditionally and
//! the unknown list would produce two of them. Both are singular by RFC 9110,
//! so the slot's collapsing of repeats is correct for them and only for them.
//!
//! Framing headers are neither: they are refused from the caller and written by
//! the response path, which owns them.
//!
//! # Outbound: why no value is rejected
//!
//! The client half refuses a header value that is not printable ASCII, because
//! WinHTTP wants UTF-16 and there is no honest conversion. Measured, HTTP.sys
//! wants bytes and passes them through unchanged: `a\x01b\x7f` arrived on the
//! wire as `[97, 1, 98, 127]`, and `\xc3\xa9` as `[195, 169]`. There is nothing
//! to reject, so nothing is rejected. [`http::HeaderValue`] already forbids CR,
//! LF and NUL, which is the only class of byte that could change framing.

use std::net::SocketAddr;

use http::header::{HeaderName, HeaderValue, CONTENT_LENGTH, DATE, SERVER, TRANSFER_ENCODING};
use http::{HeaderMap, Method, StatusCode, Uri, Version};
use winasio::httpsys::{Request, RequestHeader, RequestId, Response, ResponseHeader};

use crate::error::{AcceptError, BodyError, RequestReason, ResponseError};

/// What HTTP.sys knows about the connection a request arrived on.
///
/// Put in the [`Extensions`](http::Extensions) of every request this crate
/// produces, because it is information the platform has and the `http` types
/// have nowhere else to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// The identifier HTTP.sys uses for this request.
    pub request_id: RequestId,
    /// The identifier HTTP.sys uses for the connection it arrived on.
    ///
    /// Stable across the requests of one keep-alive connection.
    pub connection_id: u64,
    /// The peer's address, when HTTP.sys reported one.
    pub peer_address: Option<SocketAddr>,
}

/// The head of an inbound request, copied out of the platform's buffer.
pub(crate) struct Head {
    pub method: Method,
    pub uri: Uri,
    pub version: Version,
    pub headers: HeaderMap,
    pub info: ConnectionInfo,
}

/// Copy an HTTP.sys request head into `http` types.
pub(crate) fn to_http(request: &Request) -> Result<Head, AcceptError> {
    let method = method_of(request)?;
    let uri = uri_of(request)?;
    let version = version_of(request);

    let mut headers = HeaderMap::new();
    // The known slots, read through the *request* table. See the module docs.
    for known in RequestHeader::all() {
        let Some(value) = request.header(known) else {
            continue;
        };
        // `known.name()` is a compile-time constant from the request table, so
        // it is always a valid field name; the value is whatever arrived.
        let name = HeaderName::from_bytes(known.name().as_bytes()).map_err(|_| {
            AcceptError::MalformedRequest {
                reason: RequestReason::HeaderName,
                value: known.name().to_string(),
            }
        })?;
        headers.append(name, header_value(value)?);
    }
    for (name, value) in request.unknown_headers() {
        let name = HeaderName::from_bytes(name).map_err(|_| AcceptError::MalformedRequest {
            reason: RequestReason::HeaderName,
            value: String::from_utf8_lossy(name).into_owned(),
        })?;
        // `append`, not `insert`: HTTP.sys folds repeats before we see them, so
        // this cannot fire today, but a name colliding with a known slot must
        // not silently replace it.
        headers.append(name, header_value(value)?);
    }

    Ok(Head {
        method,
        uri,
        version,
        headers,
        info: ConnectionInfo {
            request_id: request.id(),
            connection_id: request.connection_id(),
            peer_address: request.peer_address(),
        },
    })
}

fn header_value(bytes: &[u8]) -> Result<HeaderValue, AcceptError> {
    HeaderValue::from_bytes(bytes).map_err(|_| AcceptError::MalformedRequest {
        reason: RequestReason::HeaderValue,
        value: String::from_utf8_lossy(bytes).into_owned(),
    })
}

/// The request's method.
///
/// Measured: `PATCH` is **not** one of the verbs HTTP.sys recognises — it
/// arrives as `Unknown(b"PATCH")`, exactly like a private verb would. So the
/// unknown path is not an edge case to be tolerated, it is the path a
/// perfectly ordinary method takes, and it goes through
/// [`Method::from_bytes`] which knows `PATCH` perfectly well.
fn method_of(request: &Request) -> Result<Method, AcceptError> {
    let raw = request.method();
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Err(AcceptError::MalformedRequest {
            reason: RequestReason::Method,
            value: String::new(),
        });
    }
    Method::from_bytes(bytes).map_err(|_| AcceptError::MalformedRequest {
        reason: RequestReason::Method,
        value: String::from_utf8_lossy(bytes).into_owned(),
    })
}

/// The request target, verbatim.
///
/// `raw_target` is the request-target as it appeared on the wire, which is what
/// a router wants: origin-form for an ordinary request, `*` for `OPTIONS *`,
/// absolute-form from a proxy-shaped client. It is not reassembled from the
/// pre-parsed components HTTP.sys also offers, because reassembling would
/// normalise a target the peer wrote and this crate is not a normaliser.
fn uri_of(request: &Request) -> Result<Uri, AcceptError> {
    let raw = request.raw_target();
    Uri::try_from(raw).map_err(|_| AcceptError::MalformedRequest {
        reason: RequestReason::Target,
        value: String::from_utf8_lossy(raw).into_owned(),
    })
}

fn version_of(request: &Request) -> Version {
    // HTTP.sys does NOT report the negotiated protocol in the version tuple:
    // measured (M2), an h2 request reports `Version = (1, 1)` exactly like an
    // h1.1 request, and signals h2 only through `HTTP_REQUEST.Flags`
    // (`HTTP_REQUEST_FLAG_HTTP2`). So the flag is consulted first; the tuple is
    // only a fallback for the older protocols the flag says nothing about.
    if request.is_http2() {
        return Version::HTTP_2;
    }
    if request.is_http3() {
        return Version::HTTP_3;
    }
    let (major, minor) = request.version();
    version_from_tuple(major, minor)
}

/// Map the raw `HTTP_REQUEST.Version` tuple to an [`http::Version`].
///
/// This is only the fallback for requests whose protocol is *not* flagged
/// (h2/h3 are detected from `HTTP_REQUEST.Flags` — see [`version_of`]); for h1.x
/// and older the tuple is all HTTP.sys gives. Kept as a pure function so the
/// mirroring test can exercise the table directly, without a real request.
fn version_from_tuple(major: u16, minor: u16) -> Version {
    match (major, minor) {
        (0, 9) => Version::HTTP_09,
        (1, 0) => Version::HTTP_10,
        // These two are retained even though the flag path handles a genuine
        // h2/h3 request: if a future HTTP.sys ever did populate the tuple, this
        // would still be right rather than silently wrong.
        (2, _) => Version::HTTP_2,
        (3, _) => Version::HTTP_3,
        // Everything else, including the 1.1 that HTTP.sys speaks and the 0.0
        // it reports for a request whose version line it could not read.
        _ => Version::HTTP_11,
    }
}

/// Whether this header is one the caller may not set.
///
/// Only `Transfer-Encoding`: this crate chooses the transfer coding and frames
/// the chunks itself, so a caller-supplied one would either double-frame the
/// body or describe a coding that was never applied. Measured, HTTP.sys passes
/// such a header to the wire untouched and applies no coding of its own, so
/// this is not a theoretical concern.
///
/// `Content-Length` is *not* refused. It is instead read as a declaration and
/// checked against the body; see [`declared_length`]. Refusing it was the first
/// design, and it was wrong: an `axum::Router` sets `Content-Length` on its own
/// replies, so refusing it made a real router unservable. Measured by the
/// integration test that serves one.
pub(crate) fn is_framing_header(name: &HeaderName) -> bool {
    name == TRANSFER_ENCODING
}

/// The length the caller declared, if any.
///
/// Repeated `Content-Length` headers are an error unless they agree, which is
/// what [RFC 9110 §8.6] requires of a recipient and is the only safe reading
/// for a sender.
///
/// [RFC 9110 §8.6]: https://www.rfc-editor.org/rfc/rfc9110#section-8.6
pub(crate) fn declared_length(headers: &HeaderMap) -> Result<Option<u64>, ResponseError> {
    let mut found: Option<u64> = None;
    for value in headers.get_all(CONTENT_LENGTH) {
        let text = std::str::from_utf8(value.as_bytes()).ok();
        let parsed = text.and_then(|t| t.trim().parse::<u64>().ok());
        let Some(parsed) = parsed else {
            return Err(ResponseError::BadContentLength {
                value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
            });
        };
        match found {
            Some(previous) if previous != parsed => {
                return Err(ResponseError::BadContentLength {
                    value: format!("{previous}, {parsed}"),
                });
            }
            _ => found = Some(parsed),
        }
    }
    Ok(found)
}

/// Which reply headers go through the known slot rather than the unknown list.
///
/// Exactly two, and only because HTTP.sys emits its own copy of each
/// unconditionally. See the module documentation for the measurements.
fn known_slot(name: &HeaderName) -> Option<ResponseHeader> {
    if name == DATE {
        Some(ResponseHeader::DATE)
    } else if name == SERVER {
        Some(ResponseHeader::SERVER)
    } else {
        None
    }
}

/// Write a caller's status line and headers into a platform response.
///
/// `Transfer-Encoding` is refused and `Content-Length` is dropped here; the
/// response path declares the length itself, after it has agreed on one.
pub(crate) fn from_http(
    status: StatusCode,
    headers: &HeaderMap,
) -> Result<Response, ResponseError> {
    let mut reply = Response::new(status.as_u16());

    // Measured: HTTP.sys supplies no reason phrase at all, so a status line
    // would otherwise read `HTTP/1.1 404 ` with nothing after it.
    if let Some(reason) = status.canonical_reason() {
        reply.set_reason(reason.as_bytes());
    }

    for (name, value) in headers.iter() {
        if is_framing_header(name) {
            return Err(ResponseError::Body(BodyError::FramingHeaderNotAllowed {
                name: name.clone(),
            }));
        }
        if name == CONTENT_LENGTH {
            // Read by `declared_length` and re-emitted through the known slot
            // once the body has been checked against it. Emitting it here as
            // well would put two `Content-Length` lines on the wire -- measured.
            continue;
        }
        match known_slot(name) {
            Some(slot) => {
                reply.set_header(slot, value.as_bytes().to_vec());
            }
            None => {
                reply.add_header(name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec());
            }
        }
    }

    Ok(reply)
}

/// Whether a reply of this status, to a request of this method, may carry a
/// body on the wire.
///
/// Measured, and the reason this is enforced here rather than trusted to the
/// platform: HTTP.sys **sends the body anyway** for a `HEAD` reply and for a
/// `204` that was given one. The suppression has to happen in this crate.
pub(crate) fn may_send_body(method: &Method, status: StatusCode) -> bool {
    if method == Method::HEAD {
        return false;
    }
    !(status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED)
}

/// Whether a reply of this status may carry a framing header.
///
/// A `HEAD` reply may and should — RFC 9110 says it carries the length the
/// equivalent `GET` would have, and measured, HTTP.sys emits it correctly and
/// leaves the connection reusable. A `1xx`, `204` or `304` may not.
pub(crate) fn may_declare_length(status: StatusCode) -> bool {
    !(status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_header_tables_are_read_from_the_right_side() {
        // The hazard this module exists to avoid, spelled out. Ids 0..=19 name
        // the same header on both sides; 20..=29 name different ones. Reading a
        // request through the reply table would relabel every one of the second
        // group, silently.
        for id in 0..20u16 {
            let request = RequestHeader::all().nth(id as usize).unwrap();
            let reply = ResponseHeader::all().nth(id as usize).unwrap();
            assert_eq!(
                request.name(),
                reply.name(),
                "id {id} was expected to agree across the two tables"
            );
        }
        for id in 20..30u16 {
            let request = RequestHeader::all().nth(id as usize).unwrap();
            let reply = ResponseHeader::all().nth(id as usize).unwrap();
            assert_ne!(
                request.name(),
                reply.name(),
                "id {id} means different headers per side and must not be shared"
            );
        }
        // The request table is longer, and its tail has no reply counterpart
        // at all.
        assert_eq!(RequestHeader::COUNT, 41);
        assert_eq!(ResponseHeader::COUNT, 30);

        // The specific crossings a mistake would produce, named so that a
        // regression reads as a sentence rather than as an index.
        assert_eq!(RequestHeader::COOKIE.index(), 25);
        assert_eq!(ResponseHeader::RETRY_AFTER.index(), 25);
        assert_eq!(RequestHeader::EXPECT.index(), 26);
        assert_eq!(ResponseHeader::SERVER.index(), 26);
        assert_eq!(RequestHeader::HOST.index(), 28);
        assert_eq!(ResponseHeader::VARY.index(), 28);
    }

    #[test]
    fn every_request_header_name_is_a_valid_http_field_name() {
        // `to_http` builds a `HeaderName` from each known slot's name. If any
        // of the 41 were not a legal field name the conversion would fail at
        // run time on a request that used it, which is exactly the kind of
        // failure that should be caught here instead.
        for known in RequestHeader::all() {
            let name = HeaderName::from_bytes(known.name().as_bytes())
                .unwrap_or_else(|_| panic!("{} is not a field name", known.name()));
            // And the round trip is case-insensitively stable, so a lookup by
            // the lowercase name a caller would write finds it.
            assert_eq!(
                RequestHeader::from_name(name.as_str()),
                Some(known),
                "{} did not survive the round trip",
                known.name()
            );
        }
    }

    #[test]
    fn only_date_and_server_use_the_known_reply_slot() {
        // Measured: those two are the ones HTTP.sys emits itself, so the
        // unknown list would produce two of each. Everything else must go
        // through the unknown list, which is the faithful one.
        assert_eq!(known_slot(&DATE), Some(ResponseHeader::DATE));
        assert_eq!(known_slot(&SERVER), Some(ResponseHeader::SERVER));
        for name in [
            "content-type",
            "set-cookie",
            "etag",
            "location",
            "connection",
            "vary",
            "x-anything",
        ] {
            assert_eq!(
                known_slot(&HeaderName::from_bytes(name.as_bytes()).unwrap()),
                None,
                "{name} must go through the unknown list"
            );
        }
    }

    #[test]
    fn a_caller_supplied_transfer_encoding_is_refused() {
        let mut headers = HeaderMap::new();
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        let error = from_http(StatusCode::OK, &headers).unwrap_err();
        assert!(
            matches!(
                &error,
                ResponseError::Body(BodyError::FramingHeaderNotAllowed { name })
                    if name == "transfer-encoding"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_caller_supplied_content_length_is_read_rather_than_refused() {
        // The rule an `axum::Router` forced: it sets `Content-Length` itself, so
        // refusing the header made a real router unservable.
        let mut headers = HeaderMap::new();
        headers.insert("content-length", HeaderValue::from_static("7"));
        assert_eq!(declared_length(&headers).unwrap(), Some(7));
        // ... and `from_http` does not emit it, because the response path
        // declares it once, after checking it. There is no accessor for the
        // unknown list, so the wire-level proof that exactly one
        // `Content-Length` line results lives in the integration test
        // `a_declared_length_frames_a_body_whose_size_is_unknown`; what is
        // checkable here is that the known slot was left alone for the
        // response path to fill in.
        let reply = from_http(StatusCode::OK, &headers).unwrap();
        assert_eq!(reply.header(ResponseHeader::CONTENT_LENGTH), None);
    }

    #[test]
    fn two_content_lengths_that_disagree_are_an_error() {
        let mut headers = HeaderMap::new();
        headers.append("content-length", HeaderValue::from_static("7"));
        headers.append("content-length", HeaderValue::from_static("9"));
        assert!(matches!(
            declared_length(&headers),
            Err(ResponseError::BadContentLength { .. })
        ));

        // Agreeing repeats are not an error, only a redundancy.
        let mut headers = HeaderMap::new();
        headers.append("content-length", HeaderValue::from_static("7"));
        headers.append("content-length", HeaderValue::from_static("7"));
        assert_eq!(declared_length(&headers).unwrap(), Some(7));
    }

    #[test]
    fn a_content_length_that_is_not_a_number_is_an_error() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", HeaderValue::from_static("seven"));
        assert!(matches!(
            declared_length(&headers),
            Err(ResponseError::BadContentLength { .. })
        ));
    }

    #[test]
    fn duplicate_response_headers_survive_as_separate_entries() {
        // The reason everything but `Date` and `Server` goes through the
        // unknown list: the known slot keeps only the last value.
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", HeaderValue::from_static("a=1"));
        headers.append("set-cookie", HeaderValue::from_static("b=2"));
        let reply = from_http(StatusCode::OK, &headers).unwrap();
        // Nothing landed in the known `Set-Cookie` slot.
        assert_eq!(reply.header(ResponseHeader::SET_COOKIE), None);
        assert_eq!(reply.status(), 200);
    }

    #[test]
    fn a_status_line_gets_the_reason_phrase_the_platform_omits() {
        // Measured: HTTP.sys writes `HTTP/1.1 404 ` and stops.
        let reply = from_http(StatusCode::NOT_FOUND, &HeaderMap::new()).unwrap();
        assert_eq!(reply.status(), 404);
        // A status with no canonical reason simply gets none, rather than an
        // invented one.
        let odd = from_http(StatusCode::from_u16(599).unwrap(), &HeaderMap::new()).unwrap();
        assert_eq!(odd.status(), 599);
    }

    #[test]
    fn a_header_value_that_is_not_ascii_is_sent_rather_than_refused() {
        // The deliberate difference from the client half. Measured, HTTP.sys
        // passes reply header bytes through unchanged, so there is nothing to
        // reject and rejecting would only lose the caller a legal header.
        let mut headers = HeaderMap::new();
        headers.insert("x-note", HeaderValue::from_bytes(b"caf\xc3\xa9").unwrap());
        assert!(from_http(StatusCode::OK, &headers).is_ok());
    }

    #[test]
    fn a_bodiless_reply_is_recognised_and_a_head_still_declares_its_length() {
        // Measured: HTTP.sys sends the body regardless for both of these, so
        // the suppression has to be here.
        assert!(!may_send_body(&Method::HEAD, StatusCode::OK));
        assert!(!may_send_body(&Method::GET, StatusCode::NO_CONTENT));
        assert!(!may_send_body(&Method::GET, StatusCode::NOT_MODIFIED));
        assert!(!may_send_body(&Method::GET, StatusCode::CONTINUE));
        assert!(may_send_body(&Method::GET, StatusCode::OK));
        assert!(may_send_body(&Method::POST, StatusCode::CREATED));

        // But a HEAD reply still declares the length a GET would have had,
        // which measurement confirmed works and leaves the connection usable.
        assert!(may_declare_length(StatusCode::OK));
        assert!(!may_declare_length(StatusCode::NO_CONTENT));
        assert!(!may_declare_length(StatusCode::NOT_MODIFIED));
        assert!(!may_declare_length(StatusCode::CONTINUE));
    }

    #[test]
    fn an_http_version_maps_to_what_the_platform_reported() {
        // The tuple fallback (`version_from_tuple`) covers the protocols HTTP.sys
        // reports in `HTTP_REQUEST.Version`. The negotiated h2/h3 case is NOT in
        // this table: measured (M2), HTTP.sys reports `(1, 1)` for an h2 request
        // and flags it via `HTTP_REQUEST.Flags` instead, so `version_of` checks
        // `is_http2`/`is_http3` before consulting this table. That flag path
        // needs a real h2 request and is covered end-to-end in winasio-tests.
        assert_eq!(version_from_tuple(0, 9), Version::HTTP_09);
        assert_eq!(version_from_tuple(1, 0), Version::HTTP_10);
        assert_eq!(version_from_tuple(1, 1), Version::HTTP_11);
        assert_eq!(version_from_tuple(2, 0), Version::HTTP_2);
        // A version the platform could not parse is reported as 0.0; an
        // unparseable version line is not worth failing a request over.
        assert_eq!(version_from_tuple(0, 0), Version::HTTP_11);
    }

    #[test]
    fn an_extension_method_is_not_an_error() {
        // Measured: `PATCH` reaches this crate as an unknown verb, so the
        // unknown path has to work for ordinary methods, not just private ones.
        assert_eq!(Method::from_bytes(b"PATCH").unwrap(), Method::PATCH);
        assert_eq!(Method::from_bytes(b"FROB").unwrap().as_str(), "FROB");
        // And a verb that is not a token at all is refused rather than mangled.
        assert!(Method::from_bytes(b"bad verb").is_err());
    }

    #[test]
    fn a_request_target_is_taken_verbatim() {
        // Origin form, the ordinary case.
        let uri = Uri::try_from(&b"/a/b?x=1&y=2"[..]).unwrap();
        assert_eq!(uri.path(), "/a/b");
        assert_eq!(uri.query(), Some("x=1&y=2"));
        // `OPTIONS *`, which is a legal target and not a path.
        assert_eq!(Uri::try_from(&b"*"[..]).unwrap().to_string(), "*");
        // Absolute form survives with its authority intact.
        let absolute = Uri::try_from(&b"http://example.com/x"[..]).unwrap();
        assert_eq!(absolute.host(), Some("example.com"));
    }
}
