// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Taking an [`http::Uri`] apart into the four things WinHTTP asks for.
//!
//! `WinHttpConnect` wants a host and a port; `WinHttpOpenRequest` wants an
//! object name and a secure flag. An `http::Uri` holds all four, but not in
//! that shape, and several of the conversions have a wrong answer that looks
//! plausible.
//!
//! # Decisions, and what they were measured against
//!
//! * **An IPv6 host keeps its brackets.** `Uri::host()` returns `[::1]`, and
//!   the obvious tidy-up — stripping the brackets before handing the string to
//!   `WinHttpConnect` — breaks it: measured, `connect("::1")` fails with
//!   `ERROR_WINHTTP_NAME_NOT_RESOLVED` while `connect("[::1]")` succeeds. The
//!   platform wants the literal form. So the host is passed through untouched,
//!   which is both simpler and the only thing that works.
//! * **A missing path becomes `/`.** `Uri::path_and_query()` is `Some("/")` for
//!   `http://host`, but a `Uri` built by hand can carry an empty path, and
//!   WinHTTP treats an empty object name as the site root only by accident.
//!   Normalising is one line and removes the question.
//! * **A scheme that is not `http` or `https` is refused.** Including no scheme
//!   at all. Treating `ftp://host/x` as `http` would send a request the caller
//!   did not write, to a port they did not name.
//! * **Userinfo is refused rather than dropped.** `http://user:pw@host/` cannot
//!   be expressed through this API, and quietly discarding the credentials
//!   would produce an unauthenticated request that looks exactly like the one
//!   the caller asked for. `http::Uri` has no accessor for userinfo, so it is
//!   detected in the authority string.
//! * **A fragment cannot appear.** `http::Uri` discards it during parsing, and
//!   a fragment is not sent on the wire anyway.

use http::uri::Scheme;
use http::Uri;
use windows::core::HSTRING;

use crate::error::Error;

/// An [`http::Uri`] in the shape `winasio::winhttp` wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The host, verbatim, brackets and all.
    pub host: HSTRING,
    /// The port, explicit or defaulted from the scheme.
    pub port: u16,
    /// The object name: path and query, never empty.
    pub object: HSTRING,
    /// Whether to open the request with `WINHTTP_FLAG_SECURE`.
    pub secure: bool,
}

/// Take a URI apart, or say why it cannot be used.
pub(crate) fn decompose(uri: &Uri) -> Result<Target, Error> {
    let secure = match uri.scheme() {
        Some(scheme) if *scheme == Scheme::HTTP => false,
        Some(scheme) if *scheme == Scheme::HTTPS => true,
        Some(scheme) => {
            return Err(Error::UnsupportedScheme {
                scheme: Some(scheme.as_str().to_string()),
            })
        }
        None => return Err(Error::UnsupportedScheme { scheme: None }),
    };

    // `http://:8080/x` parses, and reports an empty host rather than none —
    // measured. An empty host is not a host.
    let host = match uri.host() {
        Some(host) if !host.is_empty() => host,
        _ => return Err(Error::MissingHost),
    };

    // There is no `Uri::userinfo`. The authority is the only place it shows.
    if uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err(Error::UserinfoNotSupported);
    }

    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });

    let object = match uri.path_and_query().map(|target| target.as_str()) {
        Some("") | None => "/",
        Some(target) => target,
    };

    Ok(Target {
        host: HSTRING::from(host),
        port,
        object: HSTRING::from(object),
        secure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(text: &str) -> Result<Target, Error> {
        decompose(&text.parse::<Uri>().expect("the test URI should parse"))
    }

    fn parts(text: &str) -> (String, u16, String, bool) {
        let target = target(text).expect("the test URI should decompose");
        (
            target.host.to_string(),
            target.port,
            target.object.to_string(),
            target.secure,
        )
    }

    /// Host, port, object name, secure — the four things a `Target` is.
    type Expected = (&'static str, u16, &'static str, bool);

    #[test]
    fn a_uri_is_split_into_the_four_things_winhttp_asks_for() {
        // A table, because each row is a case that has a plausible wrong
        // answer and the differences between them are one character wide.
        let cases: &[(&str, Expected)] = &[
            ("http://example.com/", ("example.com", 80, "/", false)),
            ("https://example.com/", ("example.com", 443, "/", true)),
            // No path at all: the object name must still be a path.
            ("http://example.com", ("example.com", 80, "/", false)),
            // An explicit port wins over the scheme default, even when it
            // happens to be the default.
            ("http://example.com:80/", ("example.com", 80, "/", false)),
            (
                "http://example.com:8080/a",
                ("example.com", 8080, "/a", false),
            ),
            ("https://example.com:80/a", ("example.com", 80, "/a", true)),
            // The query is part of the object name, not a separate parameter.
            (
                "http://example.com/search?q=winasio&n=1",
                ("example.com", 80, "/search?q=winasio&n=1", false),
            ),
            // An empty query is still a query, and the `?` must survive.
            ("http://example.com/a?", ("example.com", 80, "/a?", false)),
            // An IPv6 literal keeps its brackets: the platform wants them.
            ("http://[::1]:8080/x", ("[::1]", 8080, "/x", false)),
            ("https://[fe80::1]/", ("[fe80::1]", 443, "/", true)),
            // An IPv4 literal is just a host.
            ("http://127.0.0.1:9/", ("127.0.0.1", 9, "/", false)),
            // The scheme is matched case-insensitively by `http::Uri`, but the
            // host's case is preserved as written. DNS does not care, and
            // rewriting it would be a change the caller did not ask for.
            ("HTTP://Example.COM/A", ("Example.COM", 80, "/A", false)),
        ];

        for (uri, expected) in cases {
            let (host, port, object, secure) = parts(uri);
            assert_eq!(
                (host.as_str(), port, object.as_str(), secure),
                *expected,
                "for {uri}"
            );
        }
    }

    #[test]
    fn a_scheme_that_is_not_http_is_refused_rather_than_assumed() {
        assert!(matches!(
            target("ftp://example.com/x"),
            Err(Error::UnsupportedScheme { scheme: Some(s) }) if s == "ftp"
        ));
        // A `Uri` with no scheme is the shape a caller gets from writing a
        // bare path, which is exactly the mistake worth catching.
        assert!(matches!(
            target("/just/a/path"),
            Err(Error::UnsupportedScheme { scheme: None })
        ));
    }

    #[test]
    fn a_uri_with_no_host_is_refused() {
        // `http://:8080/x` parses and reports `Some("")` — a plausible-looking
        // empty string rather than the `None` one would expect.
        assert!(matches!(target("http://:8080/x"), Err(Error::MissingHost)));
    }

    #[test]
    fn userinfo_is_refused_rather_than_silently_dropped() {
        assert!(matches!(
            target("http://user:pw@example.com/x"),
            Err(Error::UserinfoNotSupported)
        ));
        assert!(matches!(
            target("http://user@example.com/x"),
            Err(Error::UserinfoNotSupported)
        ));
    }

    #[test]
    fn an_absent_path_becomes_a_root_path() {
        // Measured: `http` 1.x cannot represent an empty path-and-query at all
        // — `"".parse::<PathAndQuery>()` is `InvalidUri(Empty)` and
        // `Uri::from_parts` with `path_and_query: None` alongside an authority
        // is `PathAndQueryMissing`. So the normalisation in `decompose` is
        // defence against a shape the type system already prevents, and the
        // reachable case is a URI written without a path at all.
        assert_eq!(parts("http://example.com").2, "/");
        assert_eq!(
            "http://example.com"
                .parse::<Uri>()
                .unwrap()
                .path_and_query()
                .map(|target| target.as_str()),
            Some("/"),
            "http::Uri is expected to supply the root path itself"
        );
    }
}
