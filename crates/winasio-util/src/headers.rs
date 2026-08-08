// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Moving headers between [`http::HeaderMap`] and WinHTTP's wire formats.
//!
//! Both directions are lossy if done carelessly, and in different ways.
//!
//! # Outbound: why an unrepresentable value is refused, not converted
//!
//! [`http::HeaderValue`] holds arbitrary bytes. WinHTTP wants a UTF-16 string.
//! There is no total function between the two, so something has to give. The
//! choice here is to refuse a value that is not printable ASCII and name the
//! header in the error.
//!
//! The alternative — a lossy conversion — would put bytes on the wire that the
//! caller did not write, under a header name they chose, and the caller would
//! have no way to find out. Measured, the platform refuses such a value anyway:
//! a header block containing a non-ASCII byte makes `WinHttpSendRequest` fail
//! with `ERROR_INVALID_PARAMETER`. So the only thing rejecting early costs is a
//! round trip, and the only thing it buys is an error that says *which* header
//! was the problem instead of a bare error code.
//!
//! # Outbound: why framing headers are refused
//!
//! `Content-Length` and `Transfer-Encoding` are computed by this crate from the
//! body's size hint. Measured: a caller-supplied `Content-Length` in the header
//! block *replaces* the value WinHTTP would have derived from the declared
//! length, so accepting one would let a caller describe a body that was never
//! sent. There is no useful thing a caller can do with these headers that the
//! body type does not already express.
//!
//! # Inbound: why the raw block is parsed rather than queried by name
//!
//! Measured: `Request::header("Set-Cookie")` on a response carrying two of them
//! returns only the first. A by-name query cannot represent a repeated header,
//! and `Set-Cookie` is repeated by design. The raw block keeps them as separate
//! lines, so the block is what gets parsed.
//!
//! The block's exact shape was measured rather than assumed:
//!
//! * the status line is always line 0, and carries the HTTP version;
//! * the block ends `\r\n\r\n`, so splitting on `\r\n` yields two trailing
//!   empty strings;
//! * duplicates are repeated as separate lines, never folded together;
//! * obs-fold continuation lines never reach the caller — WinHTTP unfolds them
//!   onto one line — so there is no fold handling here, because it would be
//!   dead code;
//! * a value containing a colon is preserved intact, so the split is on the
//!   *first* colon only;
//! * surrounding whitespace is already stripped by WinHTTP, and is stripped
//!   again here so that the result does not depend on that.
//!
//! Ordering *between different names* is not preserved by the platform: it
//! hoists headers it has an index for ahead of ones it does not. Order within a
//! single name is preserved, which is the only ordering HTTP gives meaning to.

use http::header::{HeaderName, HeaderValue, CONTENT_LENGTH, TRANSFER_ENCODING};
use http::{HeaderMap, StatusCode, Version};

use crate::error::{BodyError, HeaderReason, RequestError};

/// Encode a header map as the UTF-16 `name: value` block WinHTTP wants.
///
/// Returns `None` for an empty map. That is not a tidy-up: an empty `Vec<u16>`
/// handed to `WinHttpSendRequest` used to fault the process, and while
/// `winasio::winhttp` now normalises it, saying "no headers" when there are
/// none is what the caller meant anyway.
pub(crate) fn encode(headers: &HeaderMap) -> Result<Option<Vec<u16>>, RequestError> {
    let mut block = String::new();
    // `iter` yields one item per value, so a repeated name is emitted once per
    // occurrence rather than collapsed.
    for (name, value) in headers.iter() {
        if name == CONTENT_LENGTH || name == TRANSFER_ENCODING {
            return Err(RequestError::Body(BodyError::FramingHeaderNotAllowed {
                name: name.clone(),
            }));
        }
        let text = value
            .to_str()
            .map_err(|_| RequestError::InvalidRequestHeader {
                name: name.clone(),
                reason: HeaderReason::NotVisibleAscii,
            })?;
        block.push_str(name.as_str());
        block.push_str(": ");
        block.push_str(text);
        block.push_str("\r\n");
    }

    if block.is_empty() {
        return Ok(None);
    }
    Ok(Some(block.encode_utf16().collect()))
}

/// Append a framing header to an already-encoded block.
///
/// Separate from [`encode`] because the framing decision is the client's, not
/// the caller's, and mixing the two would blur exactly the line that
/// [`BodyError::FramingHeaderNotAllowed`] exists to draw.
pub(crate) fn append(block: Option<Vec<u16>>, line: &str) -> Option<Vec<u16>> {
    let mut encoded: Vec<u16> = block.unwrap_or_default();
    encoded.extend(line.encode_utf16());
    encoded.extend("\r\n".encode_utf16());
    Some(encoded)
}

/// The status line and headers of a response, parsed from the raw block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Head {
    pub version: Version,
    pub headers: HeaderMap,
}

/// Parse the CRLF header block WinHTTP reports for a response.
pub(crate) fn parse(raw: &str) -> Result<Head, RequestError> {
    let mut lines = raw.split("\r\n");

    // Line 0 is the status line. It is not a header, and emitting it as one
    // would put a `HeaderName` on the map that no server sent.
    let status_line = lines.next().unwrap_or_default();
    let version = parse_version(status_line);

    let mut headers = HeaderMap::new();
    for line in lines {
        // Two of these appear at the end of every block, and a response with
        // no headers at all is nothing but the status line and them.
        if line.is_empty() {
            continue;
        }
        let (name, value) =
            line.split_once(':')
                .ok_or_else(|| RequestError::MalformedResponseHeader {
                    line: line.to_string(),
                })?;
        let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| {
            RequestError::MalformedResponseHeader {
                line: line.to_string(),
            }
        })?;
        let value = HeaderValue::from_str(value.trim()).map_err(|_| {
            RequestError::MalformedResponseHeader {
                line: line.to_string(),
            }
        })?;
        // `append`, not `insert`: a repeated name must survive as repeated
        // entries, which is the entire reason this function exists.
        headers.append(name, value);
    }

    Ok(Head { version, headers })
}

/// Parse a trailer block: the same `name: value` lines as [`parse`], but with
/// no status line at the front.
///
/// WinHTTP reports response trailers through the same
/// `WINHTTP_QUERY_RAW_HEADERS_CRLF` format as headers (with the
/// `WINHTTP_QUERY_FLAG_TRAILERS` modifier), but trailers have no status line —
/// they are the fields that arrive *after* the body, which for gRPC is where
/// `grpc-status` and `grpc-message` live. A line that looks like a status line
/// (begins `HTTP/`) is skipped defensively rather than parsed as a header,
/// because some platforms prepend one and a `HeaderName` of `HTTP/1.1` is not a
/// trailer any server sent.
pub(crate) fn parse_trailers(raw: &str) -> Result<HeaderMap, RequestError> {
    let mut headers = HeaderMap::new();
    for line in raw.split("\r\n") {
        if line.is_empty() || line.starts_with("HTTP/") {
            continue;
        }
        let (name, value) =
            line.split_once(':')
                .ok_or_else(|| RequestError::MalformedResponseHeader {
                    line: line.to_string(),
                })?;
        let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| {
            RequestError::MalformedResponseHeader {
                line: line.to_string(),
            }
        })?;
        let value = HeaderValue::from_str(value.trim()).map_err(|_| {
            RequestError::MalformedResponseHeader {
                line: line.to_string(),
            }
        })?;
        headers.append(name, value);
    }
    Ok(headers)
}

/// The HTTP version named by a status line.
///
/// A version the parser does not recognise becomes `HTTP/1.1` rather than an
/// error. WinHTTP does not speak HTTP/2 or HTTP/3 through this API and reports
/// what it negotiated, so an unrecognised token means the status line was
/// malformed — and a malformed status line accompanying a status code and body
/// the platform was happy with is not worth failing an otherwise good response
/// over.
fn parse_version(status_line: &str) -> Version {
    match status_line.split(' ').next().unwrap_or_default() {
        "HTTP/0.9" => Version::HTTP_09,
        "HTTP/1.0" => Version::HTTP_10,
        "HTTP/2.0" | "HTTP/2" => Version::HTTP_2,
        "HTTP/3.0" | "HTTP/3" => Version::HTTP_3,
        _ => Version::HTTP_11,
    }
}

/// Whether a response of this status, to a request of this method, may carry a
/// body at all.
///
/// Measured: a `204` or a `HEAD` response carrying `Content-Length: 10` reports
/// zero available immediately. Treating that declared length as a promise would
/// turn every such response into a spurious truncation error, so the length is
/// only believed when the message may have a body in the first place.
pub(crate) fn may_have_body(method: &http::Method, status: StatusCode) -> bool {
    if method == http::Method::HEAD {
        return false;
    }
    !(status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED)
}

/// The declared body length, if the response declared one this crate can use.
///
/// A `Content-Length` that does not parse, or that appears more than once with
/// differing values, yields `None`: an unenforceable declaration is better than
/// an enforced guess. A repeated but consistent value is accepted, because that
/// is a legal if pointless thing for a server to send.
pub(crate) fn content_length(headers: &HeaderMap) -> Option<u64> {
    let mut declared: Option<u64> = None;
    for value in headers.get_all(CONTENT_LENGTH) {
        let parsed = value.to_str().ok()?.trim().parse::<u64>().ok()?;
        match declared {
            Some(existing) if existing != parsed => return None,
            _ => declared = Some(parsed),
        }
    }
    declared
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal block probe 1 measured, so the parser is tested against
    /// bytes the platform actually produced rather than bytes invented here.
    const MEASURED: &str = "HTTP/1.1 201 Created\r\n\
Connection: close\r\n\
Content-Length: 5\r\n\
Content-Type: text/plain\r\n\
Set-Cookie: a=1\r\n\
Set-Cookie: b=2\r\n\
X-Colon: scheme://host:8080/x\r\n\
X-Folded: first  \tsecond\r\n\
X-Empty: \r\n\
X-Spaces: padded\r\n\r\n";

    fn text(map: &HeaderMap, name: &str) -> Vec<String> {
        map.get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_measured_header_block_parses_as_the_server_wrote_it() {
        let head = parse(MEASURED).unwrap();
        assert_eq!(head.version, Version::HTTP_11);
        // The status line is not a header.
        assert!(head.headers.get("HTTP/1.1").is_none());
        assert_eq!(text(&head.headers, "content-type"), ["text/plain"]);
        // A value containing colons survives whole: the split is on the first.
        assert_eq!(text(&head.headers, "x-colon"), ["scheme://host:8080/x"]);
        // An unfolded continuation is one value, tab and all.
        assert_eq!(text(&head.headers, "x-folded"), ["first  \tsecond"]);
        // An empty value is an entry with an empty value, not an absent one.
        assert_eq!(text(&head.headers, "x-empty"), [""]);
        assert_eq!(text(&head.headers, "x-spaces"), ["padded"]);
        // The two trailing empty lines produce nothing, so the count is the
        // nine header lines the block actually carried.
        assert_eq!(head.headers.len(), 9);
    }

    #[test]
    fn duplicate_headers_survive_as_separate_entries() {
        // The reason this module parses the block instead of querying by name.
        let head = parse(MEASURED).unwrap();
        assert_eq!(text(&head.headers, "set-cookie"), ["a=1", "b=2"]);
    }

    #[test]
    fn a_response_with_no_headers_at_all_parses_to_an_empty_map() {
        // Measured shape: nothing but the status line and the terminator.
        let head = parse("HTTP/1.1 200 OK\r\n\r\n").unwrap();
        assert!(head.headers.is_empty());
        assert_eq!(head.version, Version::HTTP_11);
    }

    #[test]
    fn the_status_line_names_the_version() {
        assert_eq!(
            parse("HTTP/1.0 200 OK\r\n\r\n").unwrap().version,
            Version::HTTP_10
        );
        assert_eq!(
            parse("HTTP/1.1 200 OK\r\n\r\n").unwrap().version,
            Version::HTTP_11
        );
        // A status line with no reason phrase is legal and must not confuse
        // the split.
        assert_eq!(
            parse("HTTP/1.0 204\r\n\r\n").unwrap().version,
            Version::HTTP_10
        );
        // An unrecognised version is not worth failing a good response over.
        assert_eq!(parse("garbage\r\n\r\n").unwrap().version, Version::HTTP_11);
    }

    #[test]
    fn a_line_with_no_colon_is_reported_with_the_line_in_it() {
        let error = parse("HTTP/1.1 200 OK\r\nnonsense\r\n\r\n").unwrap_err();
        assert!(matches!(
            &error,
            RequestError::MalformedResponseHeader { line } if line == "nonsense"
        ));
    }

    #[test]
    fn a_name_that_is_not_a_header_name_is_reported() {
        let error = parse("HTTP/1.1 200 OK\r\nbad name: x\r\n\r\n").unwrap_err();
        assert!(matches!(
            error,
            RequestError::MalformedResponseHeader { .. }
        ));
    }

    #[test]
    fn headers_encode_as_a_crlf_block() {
        let mut map = HeaderMap::new();
        map.insert("accept", HeaderValue::from_static("text/plain"));
        let encoded = encode(&map).unwrap().unwrap();
        assert_eq!(
            String::from_utf16(&encoded).unwrap(),
            "accept: text/plain\r\n"
        );
    }

    #[test]
    fn trailers_parse_without_a_status_line() {
        // The gRPC case: `grpc-status`/`grpc-message` arrive after the body,
        // with no status line in the block.
        let map = parse_trailers("grpc-status: 0\r\ngrpc-message: \r\n\r\n").unwrap();
        assert_eq!(text(&map, "grpc-status"), ["0"]);
        assert_eq!(text(&map, "grpc-message"), [""]);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn a_status_line_prefixing_a_trailer_block_is_ignored() {
        // Defensive: if a platform prepends a status line, it must not become a
        // `HeaderName` of `HTTP/1.1`.
        let map = parse_trailers("HTTP/2 200\r\ngrpc-status: 5\r\n\r\n").unwrap();
        assert!(map.get("HTTP/2").is_none());
        assert_eq!(text(&map, "grpc-status"), ["5"]);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn an_empty_trailer_block_parses_to_an_empty_map() {
        assert!(parse_trailers("").unwrap().is_empty());
        assert!(parse_trailers("\r\n\r\n").unwrap().is_empty());
    }

    #[test]
    fn a_repeated_request_header_is_emitted_once_per_value() {
        let mut map = HeaderMap::new();
        map.append("x-tag", HeaderValue::from_static("one"));
        map.append("x-tag", HeaderValue::from_static("two"));
        let encoded = encode(&map).unwrap().unwrap();
        assert_eq!(
            String::from_utf16(&encoded).unwrap(),
            "x-tag: one\r\nx-tag: two\r\n"
        );
    }

    #[test]
    fn an_empty_header_map_encodes_to_no_block_at_all() {
        // Not `Some(vec![])`, which used to fault the process one crate down.
        assert!(encode(&HeaderMap::new()).unwrap().is_none());
    }

    #[test]
    fn a_non_ascii_header_value_is_rejected_and_names_its_header() {
        let mut map = HeaderMap::new();
        // Legal to construct — `HeaderValue` holds bytes — and impossible to
        // send.
        map.insert("x-note", HeaderValue::from_bytes(b"caf\xc3\xa9").unwrap());
        let error = encode(&map).unwrap_err();
        assert!(matches!(
            &error,
            RequestError::InvalidRequestHeader { name, reason }
                if name == "x-note" && *reason == HeaderReason::NotVisibleAscii
        ));
    }

    #[test]
    fn a_caller_supplied_framing_header_is_rejected() {
        for name in ["content-length", "transfer-encoding"] {
            let mut map = HeaderMap::new();
            map.insert(name, HeaderValue::from_static("7"));
            assert!(
                matches!(
                    encode(&map),
                    Err(RequestError::Body(
                        BodyError::FramingHeaderNotAllowed { .. }
                    ))
                ),
                "{name} should have been refused"
            );
        }
    }

    #[test]
    fn a_framing_header_is_appended_after_the_callers_own() {
        let mut map = HeaderMap::new();
        map.insert("accept", HeaderValue::from_static("*/*"));
        let block = append(encode(&map).unwrap(), "Transfer-Encoding: chunked");
        assert_eq!(
            String::from_utf16(&block.unwrap()).unwrap(),
            "accept: */*\r\nTransfer-Encoding: chunked\r\n"
        );
    }

    #[test]
    fn a_framing_header_can_be_the_only_header() {
        let block = append(None, "Transfer-Encoding: chunked");
        assert_eq!(
            String::from_utf16(&block.unwrap()).unwrap(),
            "Transfer-Encoding: chunked\r\n"
        );
    }

    #[test]
    fn a_header_value_cannot_smuggle_a_second_header_into_the_block() {
        // The header block is assembled by concatenating CRLF-separated lines,
        // so a value containing a CRLF would be a request-splitting hole. It
        // is not reachable: `HeaderValue` refuses CR and LF at construction,
        // which is the gate this test exists to notice the removal of.
        for smuggled in [
            &b"ok\r\nX-Injected: yes"[..],
            &b"ok\nX-Injected: yes"[..],
            &b"ok\rX-Injected: yes"[..],
            &b"ok\0"[..],
        ] {
            assert!(
                HeaderValue::from_bytes(smuggled).is_err(),
                "{smuggled:?} should not be a header value at all"
            );
        }

        // A header *name* cannot carry one either, so neither half of a line
        // is attacker-shaped.
        assert!(HeaderName::from_bytes(b"x\r\nX-Injected").is_err());
        assert!(HeaderName::from_bytes(b"x: y").is_err());

        // What a value legitimately may contain still round-trips: a tab, and
        // a colon, neither of which changes the framing.
        let mut map = HeaderMap::new();
        map.insert("x-note", HeaderValue::from_bytes(b"a\tb: c").unwrap());
        assert_eq!(
            String::from_utf16(&encode(&map).unwrap().unwrap()).unwrap(),
            "x-note: a\tb: c\r\n"
        );
    }

    #[test]
    fn a_bodiless_response_is_recognised_whatever_it_declares() {
        // Measured: all of these report zero bytes available even with a
        // `Content-Length` header, so believing the header would invent a
        // truncation that did not happen.
        assert!(!may_have_body(&http::Method::HEAD, StatusCode::OK));
        assert!(!may_have_body(&http::Method::GET, StatusCode::NO_CONTENT));
        assert!(!may_have_body(&http::Method::GET, StatusCode::NOT_MODIFIED));
        assert!(!may_have_body(&http::Method::GET, StatusCode::CONTINUE));
        assert!(may_have_body(&http::Method::GET, StatusCode::OK));
        assert!(may_have_body(&http::Method::POST, StatusCode::CREATED));
        // A 304 to a HEAD is bodiless twice over.
        assert!(!may_have_body(
            &http::Method::HEAD,
            StatusCode::NOT_MODIFIED
        ));
    }

    #[test]
    fn a_declared_length_is_read_only_when_it_is_unambiguous() {
        let head = parse(MEASURED).unwrap();
        assert_eq!(content_length(&head.headers), Some(5));

        assert_eq!(content_length(&HeaderMap::new()), None);

        let mut nonsense = HeaderMap::new();
        nonsense.insert(CONTENT_LENGTH, HeaderValue::from_static("banana"));
        assert_eq!(content_length(&nonsense), None);

        let mut negative = HeaderMap::new();
        negative.insert(CONTENT_LENGTH, HeaderValue::from_static("-1"));
        assert_eq!(content_length(&negative), None);

        let mut contradictory = HeaderMap::new();
        contradictory.append(CONTENT_LENGTH, HeaderValue::from_static("5"));
        contradictory.append(CONTENT_LENGTH, HeaderValue::from_static("6"));
        assert_eq!(content_length(&contradictory), None);

        let mut consistent = HeaderMap::new();
        consistent.append(CONTENT_LENGTH, HeaderValue::from_static("5"));
        consistent.append(CONTENT_LENGTH, HeaderValue::from_static("5"));
        assert_eq!(content_length(&consistent), Some(5));
    }
}
