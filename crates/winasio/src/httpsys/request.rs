// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! A received request and the storage its metadata lives in.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use windows::Win32::Networking::HttpServer::{
    HTTP_REQUEST_FLAG_MORE_ENTITY_BODY_EXISTS, HTTP_REQUEST_V2, HTTP_UNKNOWN_HEADER, HTTP_VERB,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6};

use super::header::RequestHeader;
use super::BufferUnit;

/// Identifies a request within its queue.
///
/// Needed to reply to a request, to read its body, and to reject it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RequestId(pub(crate) u64);

impl RequestId {
    /// The sentinel meaning "whichever request comes next".
    pub(crate) const NEXT: RequestId = RequestId(0);

    /// The raw value, for interoperating with the Windows API directly.
    pub fn get(self) -> u64 {
        self.0
    }

    /// Rebuild an identifier from a raw value.
    ///
    /// Useful when an identifier has been carried across a boundary that cannot
    /// hold the type itself.
    pub fn from_raw(id: u64) -> RequestId {
        RequestId(id)
    }
}

/// The request's method.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method<'a> {
    Options,
    Get,
    Head,
    Post,
    Put,
    Delete,
    Trace,
    Connect,
    Track,
    Move,
    Copy,
    PropFind,
    PropPatch,
    MkCol,
    Lock,
    Unlock,
    Search,
    /// An extension method HTTP.sys does not recognise, with its literal text.
    Unknown(&'a [u8]),
    /// HTTP.sys reported the verb as invalid or unparsed.
    Invalid,
}

impl Method<'_> {
    /// The method's text. Allocation-free for every recognised method.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Method::Options => b"OPTIONS",
            Method::Get => b"GET",
            Method::Head => b"HEAD",
            Method::Post => b"POST",
            Method::Put => b"PUT",
            Method::Delete => b"DELETE",
            Method::Trace => b"TRACE",
            Method::Connect => b"CONNECT",
            Method::Track => b"TRACK",
            Method::Move => b"MOVE",
            Method::Copy => b"COPY",
            Method::PropFind => b"PROPFIND",
            Method::PropPatch => b"PROPPATCH",
            Method::MkCol => b"MKCOL",
            Method::Lock => b"LOCK",
            Method::Unlock => b"UNLOCK",
            Method::Search => b"SEARCH",
            Method::Unknown(raw) => raw,
            Method::Invalid => b"",
        }
    }
}

/// The smallest receive capacity that can yield anything useful.
///
/// Phase 0 measured that a buffer below this returns `ERROR_INSUFFICIENT_BUFFER`
/// with no request identifier at all -- so a smaller buffer cannot even be
/// retried or rejected. Enforced rather than merely documented.
pub const MIN_CAPACITY: usize = std::mem::size_of::<HTTP_REQUEST_V2>();

/// A received request.
///
/// # Why this is an ordinary movable value
///
/// HTTP.sys writes the URL, headers and addresses into the tail of a
/// caller-supplied buffer and stores pointers to that tail inside the
/// `HTTP_REQUEST_V2` header at its start. Those pointers are what every accessor
/// below follows.
///
/// The buffer is a heap allocation of its own, so moving a `Request` moves a
/// pointer rather than the bytes -- the pointed-to block never moves, and every
/// interior pointer stays valid. That is why this type needs no `Pin`, unlike
/// the design it replaces, which embedded the buffer *inline in the struct* and
/// so had to be handed back as `Pin<Box<Request>>` with a `PhantomPinned`.
///
/// Three properties keep that soundness, and are why this type deliberately
/// offers no way to break them: the allocation is fixed and never grown in
/// place, no `&mut` to the buffer escapes, and the type is not `Clone` (a copy
/// would duplicate the header with pointers into the *original* buffer).
pub struct Request {
    /// Fixed-size, zero-initialised, aligned for `HTTP_REQUEST_V2`.
    buffer: Box<[BufferUnit]>,
    capacity: usize,
    retries: u32,
}

// SAFETY: the request owns its buffer outright, and the pointers within it point
// into that same owned allocation. Nothing is shared with another thread.
unsafe impl Send for Request {}
unsafe impl Sync for Request {}

impl Request {
    /// Allocate a receive buffer of at least `capacity` bytes.
    ///
    /// The capacity is raised to [`MIN_CAPACITY`] and rounded up to whole
    /// allocation units.
    pub(crate) fn with_capacity(capacity: usize) -> Request {
        let capacity = capacity.max(MIN_CAPACITY);
        let units = capacity.div_ceil(std::mem::size_of::<BufferUnit>());
        Request {
            // `vec![0; n]` zero-initialises, which matters: after a partial fill
            // the untouched tail must not be uninitialised memory.
            buffer: vec![0 as BufferUnit; units].into_boxed_slice(),
            capacity: units * std::mem::size_of::<BufferUnit>(),
            retries: 0,
        }
    }

    /// Bytes available for HTTP.sys to write into.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many retries the receive performed before succeeding.
    ///
    /// Zero means the configured capacity was sufficient first time.
    pub fn retries(&self) -> u32 {
        self.retries
    }

    pub(crate) fn set_retries(&mut self, retries: u32) {
        self.retries = retries;
    }

    /// Pointer HTTP.sys writes through. Not exposed: taking `&mut` to the
    /// buffer anywhere else would let the allocation be reallocated or aliased.
    pub(crate) fn write_ptr(&mut self) -> *mut HTTP_REQUEST_V2 {
        self.buffer.as_mut_ptr() as *mut HTTP_REQUEST_V2
    }

    /// Single base pointer every accessor derives from.
    ///
    /// Accessors use raw-pointer reads rather than materialising overlapping
    /// references over the same allocation, which keeps provenance simple.
    fn base(&self) -> *const HTTP_REQUEST_V2 {
        self.buffer.as_ptr() as *const HTTP_REQUEST_V2
    }

    /// The parsed request header, for callers needing something outside this API.
    ///
    /// # Safety
    ///
    /// The returned structure contains raw pointers into this request's buffer.
    /// They are valid only while the `Request` is alive, and only if the buffer
    /// is not modified.
    pub unsafe fn raw(&self) -> &HTTP_REQUEST_V2 {
        unsafe { &*self.base() }
    }

    /// This request's identifier, for replying, reading its body or rejecting it.
    pub fn id(&self) -> RequestId {
        RequestId(unsafe { (*self.base()).Base.RequestId })
    }

    /// The connection this request arrived on.
    pub fn connection_id(&self) -> u64 {
        unsafe { (*self.base()).Base.ConnectionId }
    }

    /// The request method.
    pub fn method(&self) -> Method<'_> {
        let verb = unsafe { (*self.base()).Base.Verb };
        match verb {
            HTTP_VERB(3) => Method::Options,
            HTTP_VERB(4) => Method::Get,
            HTTP_VERB(5) => Method::Head,
            HTTP_VERB(6) => Method::Post,
            HTTP_VERB(7) => Method::Put,
            HTTP_VERB(8) => Method::Delete,
            HTTP_VERB(9) => Method::Trace,
            HTTP_VERB(10) => Method::Connect,
            HTTP_VERB(11) => Method::Track,
            HTTP_VERB(12) => Method::Move,
            HTTP_VERB(13) => Method::Copy,
            HTTP_VERB(14) => Method::PropFind,
            HTTP_VERB(15) => Method::PropPatch,
            HTTP_VERB(16) => Method::MkCol,
            HTTP_VERB(17) => Method::Lock,
            HTTP_VERB(18) => Method::Unlock,
            HTTP_VERB(19) => Method::Search,
            // `HttpVerbUnknown`: the literal text is in `pUnknownVerb`.
            HTTP_VERB(1) => {
                let (ptr, len) = unsafe {
                    let b = &(*self.base()).Base;
                    (b.pUnknownVerb.0, b.UnknownVerbLength as usize)
                };
                Method::Unknown(self.slice_u8(ptr, len))
            }
            _ => Method::Invalid,
        }
    }

    /// The request target exactly as it arrived, without allocating.
    pub fn raw_target(&self) -> &[u8] {
        let (ptr, len) = unsafe {
            let b = &(*self.base()).Base;
            (b.pRawUrl.0, b.RawUrlLength as usize)
        };
        self.slice_u8(ptr, len)
    }

    /// The request target as text, if it is valid UTF-8. Does not allocate.
    pub fn target(&self) -> Option<&str> {
        std::str::from_utf8(self.raw_target()).ok()
    }

    /// The full URL HTTP.sys parsed, as UTF-16 code units.
    ///
    /// The pre-parsed components are wide strings, so unlike [`raw_target`] they
    /// cannot be borrowed as `&str` without converting -- which allocates. Use
    /// these when a borrow is enough, and the `*_lossy` forms when it is not.
    ///
    /// [`raw_target`]: Request::raw_target
    pub fn full_url_wide(&self) -> &[u16] {
        let cooked = unsafe { &(*self.base()).Base.CookedUrl };
        self.slice_u16(cooked.pFullUrl.0, cooked.FullUrlLength as usize / 2)
    }

    /// The host component, as UTF-16 code units.
    pub fn host_wide(&self) -> &[u16] {
        let cooked = unsafe { &(*self.base()).Base.CookedUrl };
        self.slice_u16(cooked.pHost.0, cooked.HostLength as usize / 2)
    }

    /// The absolute path component, as UTF-16 code units.
    pub fn path_wide(&self) -> &[u16] {
        let cooked = unsafe { &(*self.base()).Base.CookedUrl };
        self.slice_u16(cooked.pAbsPath.0, cooked.AbsPathLength as usize / 2)
    }

    /// The query string component, as UTF-16 code units.
    pub fn query_wide(&self) -> &[u16] {
        let cooked = unsafe { &(*self.base()).Base.CookedUrl };
        self.slice_u16(cooked.pQueryString.0, cooked.QueryStringLength as usize / 2)
    }

    /// The full URL as a `String`. Allocates; see [`full_url_wide`].
    ///
    /// [`full_url_wide`]: Request::full_url_wide
    pub fn full_url_lossy(&self) -> String {
        String::from_utf16_lossy(self.full_url_wide())
    }

    /// The host as a `String`. Allocates.
    pub fn host_lossy(&self) -> String {
        String::from_utf16_lossy(self.host_wide())
    }

    /// The absolute path as a `String`. Allocates.
    pub fn path_lossy(&self) -> String {
        String::from_utf16_lossy(self.path_wide())
    }

    /// The query string as a `String`. Allocates.
    pub fn query_lossy(&self) -> String {
        String::from_utf16_lossy(self.query_wide())
    }

    /// The HTTP version, as `(major, minor)`.
    pub fn version(&self) -> (u16, u16) {
        let v = unsafe { (*self.base()).Base.Version };
        (v.MajorVersion, v.MinorVersion)
    }

    /// A header HTTP.sys recognises, or `None` if it was absent.
    ///
    /// A present but empty header returns `Some(&[])`, which is why this is not
    /// merely a length check.
    pub fn header(&self, name: RequestHeader) -> Option<&[u8]> {
        let known = unsafe { &(*self.base()).Base.Headers.KnownHeaders };
        let entry = known.get(name.index())?;
        if entry.pRawValue.is_null() {
            return None;
        }
        Some(self.slice_u8(entry.pRawValue.0, entry.RawValueLength as usize))
    }

    /// A recognised header as text, if present and valid UTF-8.
    pub fn header_str(&self, name: RequestHeader) -> Option<&str> {
        std::str::from_utf8(self.header(name)?).ok()
    }

    /// Every header HTTP.sys did not recognise, in arrival order.
    ///
    /// Repeated names each yield their own entry.
    pub fn unknown_headers(&self) -> UnknownHeaders<'_> {
        let (ptr, count) = unsafe {
            let h = &(*self.base()).Base.Headers;
            (h.pUnknownHeaders, h.UnknownHeaderCount as usize)
        };
        UnknownHeaders {
            request: self,
            ptr,
            count,
            index: 0,
        }
    }

    /// Look up an unrecognised header by name, ignoring case.
    ///
    /// Where a name repeats, this yields the first occurrence; use
    /// [`unknown_headers`] to reach the rest.
    ///
    /// [`unknown_headers`]: Request::unknown_headers
    pub fn unknown_header(&self, name: &str) -> Option<&[u8]> {
        let wanted = name.as_bytes();
        self.unknown_headers().find_map(|(n, v)| {
            // Length first: it rejects most candidates without comparing bytes.
            (n.len() == wanted.len()
                && n.iter().zip(wanted).all(|(a, b)| a.eq_ignore_ascii_case(b)))
            .then_some(v)
        })
    }

    /// An unrecognised header as text, if present and valid UTF-8.
    pub fn unknown_header_str(&self, name: &str) -> Option<&str> {
        std::str::from_utf8(self.unknown_header(name)?).ok()
    }

    /// Whether HTTP.sys reported that more body data exists.
    ///
    /// This does not distinguish a request with no body from one with an empty
    /// body -- the API reports both the same way. A caller needing that
    /// distinction reads the entity headers via [`header`].
    ///
    /// [`header`]: Request::header
    pub fn has_more_body(&self) -> bool {
        let flags = unsafe { (*self.base()).Base.Flags };
        flags & HTTP_REQUEST_FLAG_MORE_ENTITY_BODY_EXISTS != 0
    }

    /// Total bytes HTTP.sys has received for this request so far.
    pub fn bytes_received(&self) -> u64 {
        unsafe { (*self.base()).Base.BytesReceived }
    }

    /// The address of the peer that sent the request.
    ///
    /// Allocation-free.
    pub fn peer_address(&self) -> Option<SocketAddr> {
        let ptr = unsafe { (*self.base()).Base.Address.pRemoteAddress };
        if ptr.is_null() {
            return None;
        }
        // SAFETY: HTTP.sys wrote a sockaddr into this request's own buffer; the
        // family tells us which layout to read.
        let family = unsafe { (*ptr).sa_family };
        if family == AF_INET {
            let v4 = ptr as *const SOCKADDR_IN;
            let (addr, port) = unsafe { ((*v4).sin_addr, (*v4).sin_port) };
            let octets = unsafe { addr.S_un.S_addr }.to_ne_bytes();
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(octets)),
                u16::from_be(port),
            ))
        } else if family == AF_INET6 {
            let v6 = ptr as *const SOCKADDR_IN6;
            let (addr, port) = unsafe { ((*v6).sin6_addr, (*v6).sin6_port) };
            let octets = unsafe { addr.u.Byte };
            Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(octets)),
                u16::from_be(port),
            ))
        } else {
            None
        }
    }

    /// Borrow `len` bytes at `ptr`, which must lie within this buffer.
    fn slice_u8(&self, ptr: *const u8, len: usize) -> &[u8] {
        if ptr.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: HTTP.sys wrote these bytes into this request's own buffer,
        // which outlives the borrow and is never mutated after the receive.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }

    /// Borrow `len` UTF-16 code units at `ptr`, which must lie within this buffer.
    fn slice_u16(&self, ptr: *const u16, len: usize) -> &[u16] {
        if ptr.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: as `slice_u8`; the cooked URL is written as wide characters.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("id", &self.id())
            .field("method", &self.method())
            .field("target", &String::from_utf8_lossy(self.raw_target()))
            .field("capacity", &self.capacity)
            .field("retries", &self.retries)
            .finish()
    }
}

/// Iterator over a request's unrecognised headers.
pub struct UnknownHeaders<'a> {
    request: &'a Request,
    ptr: *mut HTTP_UNKNOWN_HEADER,
    count: usize,
    index: usize,
}

impl<'a> Iterator for UnknownHeaders<'a> {
    /// `(name, value)`, both borrowed from the request's buffer.
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.count || self.ptr.is_null() {
            return None;
        }
        // SAFETY: `pUnknownHeaders` points at `UnknownHeaderCount` entries
        // inside the request's buffer.
        let entry = unsafe { &*self.ptr.add(self.index) };
        self.index += 1;
        Some((
            self.request
                .slice_u8(entry.pName.0, entry.NameLength as usize),
            self.request
                .slice_u8(entry.pRawValue.0, entry.RawValueLength as usize),
        ))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count.saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl std::iter::ExactSizeIterator for UnknownHeaders<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_is_aligned_for_the_request_header() {
        for cap in [MIN_CAPACITY, MIN_CAPACITY + 1, 4096, 65536] {
            let req = Request::with_capacity(cap);
            let addr = req.base() as usize;
            assert_eq!(
                addr % std::mem::align_of::<HTTP_REQUEST_V2>(),
                0,
                "capacity {cap} produced a misaligned buffer"
            );
        }
    }

    #[test]
    fn capacity_is_raised_to_the_minimum_and_rounded_up() {
        // Below the minimum: raised. Phase 0 showed a smaller buffer yields
        // ERROR_INSUFFICIENT_BUFFER with no request id.
        assert_eq!(Request::with_capacity(0).capacity(), MIN_CAPACITY);
        assert_eq!(Request::with_capacity(64).capacity(), MIN_CAPACITY);
        // Rounded up to whole units.
        let unit = std::mem::size_of::<BufferUnit>();
        let req = Request::with_capacity(MIN_CAPACITY + 1);
        assert!(req.capacity() > MIN_CAPACITY);
        assert_eq!(req.capacity() % unit, 0);
    }

    #[test]
    fn buffer_is_zero_initialised() {
        let req = Request::with_capacity(4096);
        assert!(
            req.buffer.iter().all(|&b| b == 0),
            "a partially filled buffer must not expose uninitialised memory"
        );
    }

    /// The structural half of FR-028: metadata lives *outside* the struct.
    ///
    /// If any of it moved back inline, the struct's own size would grow with the
    /// configured capacity -- and moving a `Request` would then dangle every
    /// pointer HTTP.sys wrote. SC-022 covers the behavioural half.
    #[test]
    fn request_size_is_independent_of_capacity() {
        let small = Request::with_capacity(MIN_CAPACITY);
        let large = Request::with_capacity(1024 * 1024);
        assert_eq!(
            std::mem::size_of_val(&small),
            std::mem::size_of_val(&large),
            "request metadata must not be stored inline"
        );
        assert!(large.capacity() > small.capacity());
    }

    /// Moving the value must not move the buffer it points into.
    #[test]
    fn moving_a_request_keeps_the_buffer_address() {
        let req = Request::with_capacity(4096);
        let before = req.base() as usize;
        let moved = req;
        assert_eq!(before, moved.base() as usize);
        let boxed = Box::new(moved);
        assert_eq!(before, boxed.base() as usize);
    }

    #[test]
    fn accessors_on_an_unfilled_request_are_empty_not_unsound() {
        let req = Request::with_capacity(4096);
        assert!(req.raw_target().is_empty());
        assert_eq!(req.target(), Some(""));
        assert!(req.full_url_wide().is_empty());
        assert!(req.header(RequestHeader::HOST).is_none());
        assert_eq!(req.unknown_headers().count(), 0);
        assert!(!req.has_more_body());
        assert_eq!(req.peer_address(), None);
        assert_eq!(req.retries(), 0);
    }

    #[test]
    fn method_text_is_allocation_free() {
        assert_eq!(Method::Get.as_bytes(), b"GET");
        assert_eq!(Method::PropPatch.as_bytes(), b"PROPPATCH");
        assert_eq!(Method::Unknown(b"FROBNICATE").as_bytes(), b"FROBNICATE");
    }
}
