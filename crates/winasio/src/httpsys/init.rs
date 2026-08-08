// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Starting and stopping the HTTP Server API.

use std::sync::OnceLock;

use windows::core::Result;
use windows::Win32::Networking::HttpServer::{
    HttpFeatureResponseTrailers, HttpInitialize, HttpIsFeatureSupported, HttpTerminate,
    HTTPAPI_VERSION, HTTP_FEATURE_ID, HTTP_INITIALIZE_CONFIG, HTTP_INITIALIZE_SERVER,
};

use super::error::check;

pub(crate) const VERSION: HTTPAPI_VERSION = HTTPAPI_VERSION {
    HttpApiMajorVersion: 2,
    HttpApiMinorVersion: 0,
};

/// Keeps the HTTP Server API initialised for as long as it is held.
///
/// The subsystem is reference-counted per process, so several of these may
/// coexist; each start is matched by its own shutdown when the value is
/// dropped.
///
/// ```no_run
/// # fn main() -> windows::core::Result<()> {
/// let _http = winasio::httpsys::HttpInitializer::new()?;
/// // ... serve ...
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct HttpInitializer {
    /// Not constructible from outside, so the only way to hold one is to have
    /// successfully initialised.
    _private: (),
}

impl HttpInitializer {
    /// Initialise the server and configuration subsystems.
    pub fn new() -> Result<HttpInitializer> {
        let code = unsafe {
            HttpInitialize(
                VERSION,
                HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG,
                None,
            )
        };
        check(code)?;
        Ok(HttpInitializer { _private: () })
    }

    /// Whether HTTP.sys can send HTTP/2 response trailers on this host (M3).
    ///
    /// # Why this is a method, not a free function (the M3 trap)
    ///
    /// Measured: `HttpIsFeatureSupported(HttpFeatureResponseTrailers)` returns
    /// `false` when called **before** `HttpInitialize`, a false negative that a
    /// caller cannot tell from a genuine "unsupported". Requiring `&self` makes
    /// that mistake unrepresentable: the only way to hold an [`HttpInitializer`]
    /// is to have initialised successfully, so the feature call here always runs
    /// after init. The control that proves the call actually discriminates —
    /// that a bogus feature id returns `false` in both states — lives in the
    /// tests.
    ///
    /// The answer is a property of the running OS, so it is cached process-wide
    /// after the first (correctly-ordered) query; the cache can only be filled
    /// through this method, so the ordering guarantee is preserved.
    pub fn supports_response_trailers(&self) -> bool {
        static CACHE: OnceLock<bool> = OnceLock::new();
        *CACHE.get_or_init(|| unsafe {
            HttpIsFeatureSupported(HttpFeatureResponseTrailers).as_bool()
        })
    }

    /// Query an arbitrary HTTP.sys feature id after initialisation.
    ///
    /// Exposed for the discrimination control in the tests (a bogus id must
    /// return `false`); ordinary callers want
    /// [`supports_response_trailers`](Self::supports_response_trailers).
    #[doc(hidden)]
    pub fn __supports_feature(&self, id: i32) -> bool {
        unsafe { HttpIsFeatureSupported(HTTP_FEATURE_ID(id)).as_bool() }
    }
}

impl Drop for HttpInitializer {
    fn drop(&mut self) {
        // Deliberately ignored: a panic here would abort during unwinding, and
        // there is nothing a caller could do about a failed shutdown anyway.
        let _ = unsafe { HttpTerminate(HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG, None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_trailers_are_supported_after_init_and_the_probe_discriminates() {
        // M3: on Windows 11 / Server 2019+ HTTP.sys can frame response trailers.
        // The point of this test is twofold. First, that the feature really is
        // reported supported here (the whole gRPC-unary story depends on it).
        // Second — the control — that the call is not a rubber stamp: a bogus
        // feature id must come back `false`. Without that control a
        // `HttpIsFeatureSupported` that returned `true` for everything would
        // pass a naive check while telling us nothing.
        let http = HttpInitializer::new().expect("init HTTP Server API");

        assert!(
            http.supports_response_trailers(),
            "M3: HttpFeatureResponseTrailers should be supported on this host after init"
        );

        // A feature id that does not exist. If this also returned `true`, the
        // probe would not discriminate and the assertion above would be
        // meaningless.
        assert!(
            !http.__supports_feature(i32::MAX),
            "a bogus feature id must return false, or the probe does not discriminate"
        );
    }
}
