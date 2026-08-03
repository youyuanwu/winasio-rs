// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Starting and stopping the HTTP Server API.

use windows::core::Result;
use windows::Win32::Networking::HttpServer::{
    HttpInitialize, HttpTerminate, HTTPAPI_VERSION, HTTP_INITIALIZE_CONFIG, HTTP_INITIALIZE_SERVER,
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
}

impl Drop for HttpInitializer {
    fn drop(&mut self) {
        // Deliberately ignored: a panic here would abort during unwinding, and
        // there is nothing a caller could do about a failed shutdown anyway.
        let _ = unsafe { HttpTerminate(HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG, None) };
    }
}
