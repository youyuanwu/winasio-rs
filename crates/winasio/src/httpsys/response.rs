// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Composing a reply.

use std::borrow::Cow;

use windows::Win32::Networking::HttpServer::{
    HttpDataChunkFromMemory, HTTP_DATA_CHUNK, HTTP_KNOWN_HEADER, HTTP_RESPONSE_V2,
    HTTP_UNKNOWN_HEADER,
};

use super::header::ResponseHeader;

/// How many unrecognised headers a reply holds without allocating.
pub const INLINE_UNKNOWN_HEADERS: usize = 8;

/// How many body chunks a reply holds without allocating.
pub const INLINE_CHUNKS: usize = 4;

/// A value a reply refers to: either borrowed from `'static` data or owned.
///
/// Borrowed values are what make a reply built from compile-time constants
/// allocation-free.
pub type Value = Cow<'static, [u8]>;

/// Fixed-capacity storage that spills to the heap once full.
struct Slots<T, const N: usize> {
    inline: [Option<T>; N],
    len: usize,
    spill: Vec<T>,
}

impl<T, const N: usize> Slots<T, N> {
    fn new() -> Self {
        Slots {
            inline: std::array::from_fn(|_| None),
            len: 0,
            spill: Vec::new(),
        }
    }

    fn push(&mut self, value: T) {
        if self.len < N {
            self.inline[self.len] = Some(value);
            self.len += 1;
        } else {
            // Documented spill: costs one allocation, leaving the reply's
            // zero-allocation budget.
            self.spill.push(value);
        }
    }

    fn total(&self) -> usize {
        self.len + self.spill.len()
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.inline[..self.len]
            .iter()
            .map(|s| s.as_ref().expect("occupied slot"))
            .chain(self.spill.iter())
    }

    fn clear(&mut self) {
        for slot in &mut self.inline[..self.len] {
            *slot = None;
        }
        self.len = 0;
        self.spill.clear();
    }
}

/// A reply: status, reason phrase, headers and body.
///
/// # Why the pointers are built at send time
///
/// The structure HTTP.sys reads holds pointers to the reason phrase, every
/// header value and the chunk array. If those were computed when the caller set
/// them, they could point into this value's own inline storage and would dangle
/// the moment it moved.
///
/// Instead every pointer is derived from `&mut self` inside the send
/// operation's `operate`, which runs only after the operation has reached its
/// final heap address and cannot move again. That is what lets a `Response` be
/// built, moved and stored freely before it is sent, while still using inline
/// storage -- so a reply of constants costs no allocation at all.
pub struct Response {
    status: u16,
    reason: Value,
    known: [Option<Value>; ResponseHeader::COUNT],
    unknown: Slots<(Value, Value), INLINE_UNKNOWN_HEADERS>,
    chunks: Slots<Value, INLINE_CHUNKS>,

    // Scratch the FFI call reads. Only ever written inside `operate`, and
    // cleared again when the reply leaves the operation.
    raw: HTTP_RESPONSE_V2,
    unknown_raw: [HTTP_UNKNOWN_HEADER; INLINE_UNKNOWN_HEADERS],
    unknown_raw_spill: Vec<HTTP_UNKNOWN_HEADER>,
    chunks_raw: [HTTP_DATA_CHUNK; INLINE_CHUNKS],
    chunks_raw_spill: Vec<HTTP_DATA_CHUNK>,
}

// SAFETY: a response owns everything it refers to, and the raw mirror is only
// populated while the operation holds it exclusively.
unsafe impl Send for Response {}
unsafe impl Sync for Response {}

impl Default for Response {
    fn default() -> Self {
        Response::new(200)
    }
}

impl Response {
    /// A reply with the given status and no headers or body.
    pub fn new(status: u16) -> Response {
        Response {
            status,
            reason: Cow::Borrowed(b""),
            known: std::array::from_fn(|_| None),
            unknown: Slots::new(),
            chunks: Slots::new(),
            raw: HTTP_RESPONSE_V2::default(),
            unknown_raw: [HTTP_UNKNOWN_HEADER::default(); INLINE_UNKNOWN_HEADERS],
            unknown_raw_spill: Vec::new(),
            chunks_raw: [HTTP_DATA_CHUNK::default(); INLINE_CHUNKS],
            chunks_raw_spill: Vec::new(),
        }
    }

    /// Set the status code.
    pub fn set_status(&mut self, status: u16) -> &mut Self {
        self.status = status;
        self
    }

    /// The status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Set the reason phrase. A `&'static` value does not allocate.
    pub fn set_reason(&mut self, reason: impl Into<Value>) -> &mut Self {
        self.reason = reason.into();
        self
    }

    /// Set a header HTTP.sys recognises. A `&'static` value does not allocate.
    ///
    /// Setting the same header twice replaces the earlier value.
    pub fn set_header(&mut self, name: ResponseHeader, value: impl Into<Value>) -> &mut Self {
        self.known[name.index()] = Some(value.into());
        self
    }

    /// Remove a recognised header.
    pub fn remove_header(&mut self, name: ResponseHeader) -> &mut Self {
        self.known[name.index()] = None;
        self
    }

    /// A recognised header's value, if set.
    pub fn header(&self, name: ResponseHeader) -> Option<&[u8]> {
        self.known[name.index()].as_deref()
    }

    /// Add a header HTTP.sys does not recognise.
    ///
    /// The first [`INLINE_UNKNOWN_HEADERS`] cost no allocation; beyond that the
    /// storage spills to the heap.
    pub fn add_header(&mut self, name: impl Into<Value>, value: impl Into<Value>) -> &mut Self {
        self.unknown.push((name.into(), value.into()));
        self
    }

    /// Append an in-memory body chunk.
    ///
    /// The first [`INLINE_CHUNKS`] cost no allocation.
    pub fn add_body(&mut self, body: impl Into<Value>) -> &mut Self {
        self.chunks.push(body.into());
        self
    }

    /// Remove all body chunks, keeping status and headers.
    pub fn clear_body(&mut self) -> &mut Self {
        self.chunks.clear();
        self
    }

    /// The body chunks, in order.
    pub fn body_chunks(&self) -> impl Iterator<Item = &[u8]> {
        self.chunks.iter().map(|c| c.as_ref())
    }

    /// Total body length across all chunks.
    pub fn body_len(&self) -> usize {
        self.chunks.iter().map(|c| c.len()).sum()
    }

    /// Build the structure HTTP.sys reads, deriving every pointer from `self`.
    ///
    /// # Safety
    ///
    /// The returned pointer is valid only while `self` neither moves nor is
    /// modified. Callers must be inside an operation whose address is already
    /// final -- see the type documentation.
    pub(crate) unsafe fn build(&mut self) -> *const HTTP_RESPONSE_V2 {
        self.raw = HTTP_RESPONSE_V2::default();
        self.raw.Base.StatusCode = self.status;

        if !self.reason.is_empty() {
            self.raw.Base.ReasonLength = self.reason.len() as u16;
            self.raw.Base.pReason = windows::core::PCSTR(self.reason.as_ptr());
        }

        for (index, value) in self.known.iter().enumerate() {
            if let Some(v) = value {
                self.raw.Base.Headers.KnownHeaders[index] = HTTP_KNOWN_HEADER {
                    RawValueLength: v.len() as u16,
                    pRawValue: windows::core::PCSTR(v.as_ptr()),
                };
            }
        }

        let unknown_total = self.unknown.total();
        if unknown_total > 0 {
            // Windows needs one contiguous array. Within the inline capacity it
            // is filled in place; beyond it, a heap array is allocated -- the
            // documented cost of spilling.
            if unknown_total <= INLINE_UNKNOWN_HEADERS {
                for (slot, (name, value)) in self.unknown_raw.iter_mut().zip(self.unknown.iter()) {
                    *slot = HTTP_UNKNOWN_HEADER {
                        NameLength: name.len() as u16,
                        RawValueLength: value.len() as u16,
                        pName: windows::core::PCSTR(name.as_ptr()),
                        pRawValue: windows::core::PCSTR(value.as_ptr()),
                    };
                }
                self.raw.Base.Headers.pUnknownHeaders = self.unknown_raw.as_mut_ptr();
            } else {
                self.unknown_raw_spill = self
                    .unknown
                    .iter()
                    .map(|(name, value)| HTTP_UNKNOWN_HEADER {
                        NameLength: name.len() as u16,
                        RawValueLength: value.len() as u16,
                        pName: windows::core::PCSTR(name.as_ptr()),
                        pRawValue: windows::core::PCSTR(value.as_ptr()),
                    })
                    .collect();
                self.raw.Base.Headers.pUnknownHeaders = self.unknown_raw_spill.as_mut_ptr();
            }
            self.raw.Base.Headers.UnknownHeaderCount = unknown_total as u16;
        }

        let chunk_total = self.chunks.total();
        if chunk_total > 0 {
            if chunk_total <= INLINE_CHUNKS {
                for (slot, body) in self.chunks_raw.iter_mut().zip(self.chunks.iter()) {
                    *slot = memory_chunk(body);
                }
                self.raw.Base.pEntityChunks = self.chunks_raw.as_mut_ptr();
            } else {
                self.chunks_raw_spill = self.chunks.iter().map(|b| memory_chunk(b)).collect();
                self.raw.Base.pEntityChunks = self.chunks_raw_spill.as_mut_ptr();
            }
            self.raw.Base.EntityChunkCount = chunk_total as u16;
        }

        &self.raw
    }

    /// Drop every pointer from the raw mirror.
    ///
    /// Called as the reply leaves its operation. The operation's storage is
    /// moved out on completion, so the pointers recorded during `build` no
    /// longer describe anything -- and this value is handed back to the caller
    /// even when the send failed, with an `unsafe` accessor that would otherwise
    /// expose them.
    pub(crate) fn invalidate(&mut self) {
        self.raw = HTTP_RESPONSE_V2::default();
        self.unknown_raw = [HTTP_UNKNOWN_HEADER::default(); INLINE_UNKNOWN_HEADERS];
        self.chunks_raw = [HTTP_DATA_CHUNK::default(); INLINE_CHUNKS];
        self.unknown_raw_spill = Vec::new();
        self.chunks_raw_spill = Vec::new();
    }

    /// The raw reply structure, for callers needing something outside this API.
    ///
    /// # Safety
    ///
    /// Meaningful only between a [`build`](Response::build) and the end of the
    /// operation that performed it. Outside that window every pointer in it has
    /// been cleared.
    pub unsafe fn raw(&self) -> &HTTP_RESPONSE_V2 {
        &self.raw
    }
}

/// Describe an in-memory body chunk.
fn memory_chunk(body: &[u8]) -> HTTP_DATA_CHUNK {
    let mut chunk = HTTP_DATA_CHUNK {
        DataChunkType: HttpDataChunkFromMemory,
        ..Default::default()
    };
    chunk.Anonymous.FromMemory.BufferLength = body.len() as u32;
    chunk.Anonymous.FromMemory.pBuffer = body.as_ptr() as *mut core::ffi::c_void;
    chunk
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("reason", &String::from_utf8_lossy(&self.reason))
            .field(
                "known_headers",
                &self.known.iter().filter(|h| h.is_some()).count(),
            )
            .field("unknown_headers", &self.unknown.total())
            .field("chunks", &self.chunks.total())
            .field("body_len", &self.body_len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_spill_after_the_inline_capacity() {
        let mut s: Slots<u32, 2> = Slots::new();
        s.push(1);
        s.push(2);
        assert_eq!(s.total(), 2);
        assert!(s.spill.is_empty(), "within capacity must stay inline");
        s.push(3);
        assert_eq!(s.total(), 3);
        assert_eq!(s.spill.len(), 1);
        assert_eq!(s.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
        s.clear();
        assert_eq!(s.total(), 0);
    }

    #[test]
    fn setting_a_header_twice_replaces_it() {
        let mut r = Response::new(200);
        r.set_header(ResponseHeader::SERVER, &b"a"[..]);
        r.set_header(ResponseHeader::SERVER, &b"b"[..]);
        assert_eq!(r.header(ResponseHeader::SERVER), Some(&b"b"[..]));
        r.remove_header(ResponseHeader::SERVER);
        assert_eq!(r.header(ResponseHeader::SERVER), None);
    }

    #[test]
    fn body_chunks_are_kept_in_order() {
        let mut r = Response::new(200);
        r.add_body(&b"one"[..]).add_body(&b"two"[..]);
        assert_eq!(
            r.body_chunks().collect::<Vec<_>>(),
            vec![&b"one"[..], &b"two"[..]]
        );
        assert_eq!(r.body_len(), 6);
        r.clear_body();
        assert_eq!(r.body_len(), 0);
    }

    /// `build` must derive its pointers from the value's *current* location, and
    /// `invalidate` must leave nothing behind.
    #[test]
    fn build_then_invalidate_clears_every_pointer() {
        let mut r = Response::new(204);
        r.set_reason(&b"No Content"[..])
            .set_header(ResponseHeader::SERVER, &b"winasio"[..])
            .add_header(&b"X-Trace"[..], &b"1"[..])
            .add_body(&b"hello"[..]);

        unsafe {
            let raw = r.build();
            assert_eq!((*raw).Base.StatusCode, 204);
            assert_eq!((*raw).Base.ReasonLength, 10);
            assert!(!(*raw).Base.pReason.is_null());
            assert_eq!((*raw).Base.Headers.UnknownHeaderCount, 1);
            assert!(!(*raw).Base.Headers.pUnknownHeaders.is_null());
            assert_eq!((*raw).Base.EntityChunkCount, 1);
            assert!(!(*raw).Base.pEntityChunks.is_null());
        }

        r.invalidate();
        unsafe {
            let raw = r.raw();
            assert_eq!(raw.Base.StatusCode, 0);
            assert!(raw.Base.pReason.is_null());
            assert_eq!(raw.Base.Headers.UnknownHeaderCount, 0);
            assert!(raw.Base.Headers.pUnknownHeaders.is_null());
            assert_eq!(raw.Base.EntityChunkCount, 0);
            assert!(raw.Base.pEntityChunks.is_null());
        }

        // The logical state survives, so the reply can be inspected and re-sent.
        assert_eq!(r.status(), 204);
        assert_eq!(r.header(ResponseHeader::SERVER), Some(&b"winasio"[..]));
        assert_eq!(r.body_len(), 5);
    }

    /// Rebuilding after a move must pick up the new address.
    #[test]
    fn rebuilding_after_a_move_is_consistent() {
        let mut r = Response::new(200);
        r.add_body(&b"body"[..]);
        unsafe { r.build() };

        let mut moved = r;
        let raw = unsafe { moved.build() };
        // The chunk descriptor must point at the (heap) body, and the chunk
        // array at the moved value's own inline storage.
        unsafe {
            let chunks = (*raw).Base.pEntityChunks;
            assert_eq!(chunks, moved.chunks_raw.as_mut_ptr());
            assert_eq!(
                (*chunks).Anonymous.FromMemory.pBuffer as *const u8,
                b"body".as_ptr()
            );
        }
    }
}
