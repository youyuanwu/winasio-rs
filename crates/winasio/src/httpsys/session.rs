// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Server sessions and URL groups.

use windows::core::{Result, HSTRING};
use windows::Win32::Networking::HttpServer::{
    HttpAddUrlToUrlGroup, HttpCloseServerSession, HttpCloseUrlGroup, HttpCreateServerSession,
    HttpCreateUrlGroup, HttpSetUrlGroupProperty, HTTP_SERVER_PROPERTY,
};

use super::error::check;
use super::init::VERSION;

/// A server-side session, which owns URL groups.
#[derive(Debug)]
pub struct ServerSession {
    id: u64,
}

impl ServerSession {
    /// Create a session.
    pub fn new() -> Result<ServerSession> {
        let mut id: u64 = 0;
        let code = unsafe { HttpCreateServerSession(VERSION, &mut id, None) };
        check(code)?;
        Ok(ServerSession { id })
    }
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        // Ignored: a panic in `Drop` aborts if it happens during unwinding.
        let _ = unsafe { HttpCloseServerSession(self.id) };
    }
}

/// A set of URLs bound to a listener.
///
/// Borrows its session, so the session cannot be closed while a group still
/// refers to it.
#[derive(Debug)]
pub struct UrlGroup<'a> {
    _session: &'a ServerSession,
    id: u64,
}

impl<'a> UrlGroup<'a> {
    /// Create a URL group within `session`.
    pub fn new(session: &'a ServerSession) -> Result<UrlGroup<'a>> {
        let mut id: u64 = 0;
        let code = unsafe { HttpCreateUrlGroup(session.id, &mut id, None) };
        check(code)?;
        Ok(UrlGroup {
            _session: session,
            id,
        })
    }

    /// Register a URL prefix with this group.
    ///
    /// Fails if the prefix is already reserved by another process, or if the
    /// caller lacks the necessary reservation.
    pub fn add_url(&self, url: &HSTRING) -> Result<()> {
        check(unsafe { HttpAddUrlToUrlGroup(self.id, url, 0, None) })
    }

    /// # Safety
    ///
    /// `information` must point at `length` bytes matching what `property`
    /// expects.
    pub(crate) unsafe fn set_property(
        &self,
        property: HTTP_SERVER_PROPERTY,
        information: *const core::ffi::c_void,
        length: u32,
    ) -> Result<()> {
        check(unsafe { HttpSetUrlGroupProperty(self.id, property, information, length) })
    }
}

impl Drop for UrlGroup<'_> {
    fn drop(&mut self) {
        // Ignored, as above.
        let _ = unsafe { HttpCloseUrlGroup(self.id) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SC-004: a lifecycle value whose release step fails must be dropped
    /// without a panic escaping.
    ///
    /// Constructed in-crate because it needs an invalid identifier, which the
    /// public constructors cannot produce.
    #[test]
    fn dropping_a_session_with_an_invalid_id_does_not_panic() {
        let bogus = ServerSession { id: u64::MAX };
        drop(bogus); // `HttpCloseServerSession` fails; nothing may escape.
    }

    #[test]
    fn dropping_a_url_group_with_an_invalid_id_does_not_panic() {
        // A session value is needed only to satisfy the borrow.
        let session = ServerSession { id: u64::MAX };
        let bogus = UrlGroup {
            _session: &session,
            id: u64::MAX,
        };
        drop(bogus);
        drop(session);
    }
}
