// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The two sets of headers HTTP.sys recognises.
//!
//! # Why these are separate types
//!
//! HTTP.sys stores recognised headers in a fixed array indexed by a numeric id,
//! and it uses **two different numbering schemes** -- one for requests, one for
//! replies. They agree on ids 0..=19 and disagree on *every one* of 20..=29:
//!
//! | id | on a request | on a reply |
//! |----|--------------|------------|
//! | 20 | `Accept` | `Accept-Ranges` |
//! | 21 | `Accept-Charset` | `Age` |
//! | 22 | `Accept-Encoding` | `ETag` |
//! | 23 | `Accept-Language` | `Location` |
//! | 24 | `Authorization` | `Proxy-Authenticate` |
//! | 25 | `Cookie` | `Retry-After` |
//! | 26 | `Expect` | `Server` |
//! | 27 | `From` | `Set-Cookie` |
//! | 28 | `Host` | `Vary` |
//! | 29 | `If-Match` | `WWW-Authenticate` |
//!
//! The request array has 41 entries and the reply array 30.
//!
//! A single shared header type would therefore read or write *the wrong header*
//! with no diagnostic at all -- asking for `Cookie` on a reply would silently
//! give you `Retry-After`. [`RequestHeader`] and [`ResponseHeader`] are distinct
//! types with no conversion between them, so the mistake cannot be made:
//!
//! ```compile_fail,E0308
//! use winasio::httpsys::{RequestHeader, ResponseHeader};
//! fn wants_a_request_header(_: RequestHeader) {}
//! // `Server` is a reply header; it has no meaning on a request.
//! wants_a_request_header(ResponseHeader::SERVER);
//! ```
//!
//! ```compile_fail,E0308
//! use winasio::httpsys::{RequestHeader, ResponseHeader};
//! fn wants_a_reply_header(_: ResponseHeader) {}
//! // `Cookie` is a request header; the reply-side name at that id is `Retry-After`.
//! wants_a_reply_header(RequestHeader::COOKIE);
//! ```

/// Builds one header-identity type: its constants, its name table and a
/// case-insensitive reverse lookup.
macro_rules! header_set {
    (
        $(#[$meta:meta])*
        $name:ident, $count:expr, $side:literal,
        { $( $konst:ident = $idx:expr, $text:literal ; )* }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u16);

        impl $name {
            $(
                #[doc = concat!("The `", $text, "` ", $side, " header.")]
                pub const $konst: $name = $name($idx);
            )*

            /// How many headers this side recognises.
            pub const COUNT: usize = $count;

            /// Position in the fixed array HTTP.sys uses.
            pub fn index(self) -> usize {
                self.0 as usize
            }

            /// The canonical wire name.
            pub fn name(self) -> &'static str {
                match self.0 {
                    $( $idx => $text, )*
                    // Unreachable: the only constructors are the constants
                    // above and `from_name`, which both stay in range.
                    _ => "",
                }
            }

            /// Look a header up by wire name, ignoring case.
            pub fn from_name(name: &str) -> Option<$name> {
                $(
                    if name.eq_ignore_ascii_case($text) {
                        return Some($name($idx));
                    }
                )*
                None
            }

            /// Every recognised header on this side, in id order.
            pub fn all() -> impl Iterator<Item = $name> {
                (0..Self::COUNT as u16).map($name)
            }
        }

        impl core::fmt::Debug for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.name())
            }
        }
    };
}

header_set! {
    /// A header HTTP.sys recognises **on a request**.
    ///
    /// Deliberately not interchangeable with [`ResponseHeader`]; see the module
    /// documentation for why.
    RequestHeader, 41, "request",
    {
        CACHE_CONTROL = 0, "Cache-Control";
        CONNECTION = 1, "Connection";
        DATE = 2, "Date";
        KEEP_ALIVE = 3, "Keep-Alive";
        PRAGMA = 4, "Pragma";
        TRAILER = 5, "Trailer";
        TRANSFER_ENCODING = 6, "Transfer-Encoding";
        UPGRADE = 7, "Upgrade";
        VIA = 8, "Via";
        WARNING = 9, "Warning";
        ALLOW = 10, "Allow";
        CONTENT_LENGTH = 11, "Content-Length";
        CONTENT_TYPE = 12, "Content-Type";
        CONTENT_ENCODING = 13, "Content-Encoding";
        CONTENT_LANGUAGE = 14, "Content-Language";
        CONTENT_LOCATION = 15, "Content-Location";
        CONTENT_MD5 = 16, "Content-MD5";
        CONTENT_RANGE = 17, "Content-Range";
        EXPIRES = 18, "Expires";
        LAST_MODIFIED = 19, "Last-Modified";
        ACCEPT = 20, "Accept";
        ACCEPT_CHARSET = 21, "Accept-Charset";
        ACCEPT_ENCODING = 22, "Accept-Encoding";
        ACCEPT_LANGUAGE = 23, "Accept-Language";
        AUTHORIZATION = 24, "Authorization";
        COOKIE = 25, "Cookie";
        EXPECT = 26, "Expect";
        FROM = 27, "From";
        HOST = 28, "Host";
        IF_MATCH = 29, "If-Match";
        IF_MODIFIED_SINCE = 30, "If-Modified-Since";
        IF_NONE_MATCH = 31, "If-None-Match";
        IF_RANGE = 32, "If-Range";
        IF_UNMODIFIED_SINCE = 33, "If-Unmodified-Since";
        MAX_FORWARDS = 34, "Max-Forwards";
        PROXY_AUTHORIZATION = 35, "Proxy-Authorization";
        REFERER = 36, "Referer";
        RANGE = 37, "Range";
        TE = 38, "TE";
        TRANSLATE = 39, "Translate";
        USER_AGENT = 40, "User-Agent";
    }
}

header_set! {
    /// A header HTTP.sys recognises **on a reply**.
    ///
    /// Deliberately not interchangeable with [`RequestHeader`]; see the module
    /// documentation for why.
    ResponseHeader, 30, "reply",
    {
        CACHE_CONTROL = 0, "Cache-Control";
        CONNECTION = 1, "Connection";
        DATE = 2, "Date";
        KEEP_ALIVE = 3, "Keep-Alive";
        PRAGMA = 4, "Pragma";
        TRAILER = 5, "Trailer";
        TRANSFER_ENCODING = 6, "Transfer-Encoding";
        UPGRADE = 7, "Upgrade";
        VIA = 8, "Via";
        WARNING = 9, "Warning";
        ALLOW = 10, "Allow";
        CONTENT_LENGTH = 11, "Content-Length";
        CONTENT_TYPE = 12, "Content-Type";
        CONTENT_ENCODING = 13, "Content-Encoding";
        CONTENT_LANGUAGE = 14, "Content-Language";
        CONTENT_LOCATION = 15, "Content-Location";
        CONTENT_MD5 = 16, "Content-MD5";
        CONTENT_RANGE = 17, "Content-Range";
        EXPIRES = 18, "Expires";
        LAST_MODIFIED = 19, "Last-Modified";
        ACCEPT_RANGES = 20, "Accept-Ranges";
        AGE = 21, "Age";
        ETAG = 22, "ETag";
        LOCATION = 23, "Location";
        PROXY_AUTHENTICATE = 24, "Proxy-Authenticate";
        RETRY_AFTER = 25, "Retry-After";
        SERVER = 26, "Server";
        SET_COOKIE = 27, "Set-Cookie";
        VARY = 28, "Vary";
        WWW_AUTHENTICATE = 29, "WWW-Authenticate";
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The array sizes must match the bindings' `HttpHeaderRequestMaximum` (41)
    /// and `HttpHeaderResponseMaximum` (30).
    #[test]
    fn counts_match_the_api() {
        assert_eq!(RequestHeader::COUNT, 41);
        assert_eq!(ResponseHeader::COUNT, 30);
    }

    /// Ids 0..=19 mean the same header on both sides.
    #[test]
    fn low_ids_agree() {
        for id in 0..20u16 {
            let req = RequestHeader::all().nth(id as usize).unwrap();
            let resp = ResponseHeader::all().nth(id as usize).unwrap();
            assert_eq!(
                req.name(),
                resp.name(),
                "id {id} should mean the same on both sides"
            );
        }
    }

    /// Every id in 20..=29 means something *different* on each side. This is the
    /// property that makes a shared header type unsafe, so it is asserted
    /// exhaustively: a bindings change that renumbered them would fail here.
    #[test]
    fn divergent_ids_really_diverge() {
        let expected = [
            (20, "Accept", "Accept-Ranges"),
            (21, "Accept-Charset", "Age"),
            (22, "Accept-Encoding", "ETag"),
            (23, "Accept-Language", "Location"),
            (24, "Authorization", "Proxy-Authenticate"),
            (25, "Cookie", "Retry-After"),
            (26, "Expect", "Server"),
            (27, "From", "Set-Cookie"),
            (28, "Host", "Vary"),
            (29, "If-Match", "WWW-Authenticate"),
        ];
        for (id, req_name, resp_name) in expected {
            let req = RequestHeader::all().nth(id).unwrap();
            let resp = ResponseHeader::all().nth(id).unwrap();
            assert_eq!(req.name(), req_name, "request id {id}");
            assert_eq!(resp.name(), resp_name, "reply id {id}");
            assert_ne!(req.name(), resp.name(), "id {id} must diverge");
        }
    }

    /// The specific confusions the divergence would cause.
    #[test]
    fn the_dangerous_pairs_have_distinct_indices_per_side() {
        assert_eq!(RequestHeader::COOKIE.index(), 25);
        assert_eq!(ResponseHeader::RETRY_AFTER.index(), 25);
        assert_eq!(RequestHeader::ACCEPT.index(), 20);
        assert_eq!(ResponseHeader::ACCEPT_RANGES.index(), 20);
    }

    #[test]
    fn lookup_by_name_is_case_insensitive() {
        assert_eq!(
            RequestHeader::from_name("content-type"),
            Some(RequestHeader::CONTENT_TYPE)
        );
        assert_eq!(
            ResponseHeader::from_name("SERVER"),
            Some(ResponseHeader::SERVER)
        );
        assert_eq!(RequestHeader::from_name("X-Custom"), None);
        // A reply-only name is not a request header, and vice versa.
        assert_eq!(RequestHeader::from_name("Set-Cookie"), None);
        assert_eq!(ResponseHeader::from_name("Cookie"), None);
    }

    #[test]
    fn every_id_has_a_name() {
        for h in RequestHeader::all() {
            assert!(!h.name().is_empty(), "request id {} has no name", h.index());
        }
        for h in ResponseHeader::all() {
            assert!(!h.name().is_empty(), "reply id {} has no name", h.index());
        }
    }

    #[test]
    fn debug_shows_the_side_and_name() {
        assert_eq!(
            format!("{:?}", RequestHeader::COOKIE),
            "RequestHeader(Cookie)"
        );
        assert_eq!(
            format!("{:?}", ResponseHeader::RETRY_AFTER),
            "ResponseHeader(Retry-After)"
        );
    }
}
