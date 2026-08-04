// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Buffer traits for operations whose payload is a byte buffer.
//!
//! These are a convenience, **not** a requirement of [`OpCode`](super::OpCode).
//! Many Windows APIs fill a caller-allocated structure rather than a byte slice
//! — `AcceptEx`, `DeviceIoControl`, and the HTTP Server API among them — and
//! those operations simply own their structure directly.

use std::fmt::Debug;
use std::mem::MaybeUninit;

/// A writable byte region that may be uninitialised.
///
/// Handing out `&mut [MaybeUninit<u8>]` would let *safe* code store
/// [`MaybeUninit::uninit`] over bytes a buffer has already initialised, and a
/// later safe read of those bytes is undefined behaviour. This newtype exposes
/// everything an operation needs -- a pointer, a length, a sub-window -- and
/// nothing that can de-initialise a byte, so the buffer's initialised prefix
/// cannot be damaged through it.
///
/// Modelled on [`bytes::buf::UninitSlice`], for the same reason.
///
/// ```compile_fail
/// use std::mem::MaybeUninit;
/// use winasio::iocp::{IoBufMut, UninitSlice};
///
/// let mut v: Vec<u8> = vec![1, 2, 3, 4];
/// let slice: &mut UninitSlice = v.as_uninit();
/// // There is no way back to `&mut [MaybeUninit<u8>]`, so this cannot compile.
/// let raw: &mut [MaybeUninit<u8>] = slice;
/// ```
///
/// [`bytes::buf::UninitSlice`]: https://docs.rs/bytes/latest/bytes/buf/struct.UninitSlice.html
#[repr(transparent)]
pub struct UninitSlice([MaybeUninit<u8>]);

impl UninitSlice {
    /// Wrap a raw pointer and length.
    ///
    /// # Safety
    ///
    /// `ptr` must address `len` writable bytes that stay valid for `'a`, and no
    /// other reference may alias them for that lifetime.
    pub unsafe fn from_raw_parts_mut<'a>(ptr: *mut u8, len: usize) -> &'a mut UninitSlice {
        // SAFETY: the caller guarantees the region; `UninitSlice` is
        // `repr(transparent)` over `[MaybeUninit<u8>]`, which has the same
        // layout as `[u8]`.
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<MaybeUninit<u8>>(), len) };
        unsafe { &mut *(slice as *mut [MaybeUninit<u8>] as *mut UninitSlice) }
    }

    /// Wrap an already-initialised slice.
    ///
    /// Safe, and public, because an implementor backed by a bare `&mut [u8]`,
    /// an array or an arena slab would otherwise have to reach for
    /// [`from_raw_parts_mut`](UninitSlice::from_raw_parts_mut) and pair a
    /// pointer with a length by hand -- the very thing this type exists to
    /// eliminate. Every initialised `u8` is a valid `MaybeUninit<u8>`, and
    /// nothing here can de-initialise it.
    pub fn new(slice: &mut [u8]) -> &mut UninitSlice {
        // SAFETY: the region is valid and initialised for `slice.len()` bytes,
        // and the borrow is preserved.
        unsafe { UninitSlice::from_raw_parts_mut(slice.as_mut_ptr(), slice.len()) }
    }

    /// Address of the first byte, for handing to Windows.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr().cast::<u8>()
    }

    /// Number of writable bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there is anywhere to write.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A sub-window, for buffer adapters.
    ///
    /// # Panics
    ///
    /// If `start > end` or `end` exceeds [`len`](UninitSlice::len).
    pub fn slice_mut(&mut self, start: usize, end: usize) -> &mut UninitSlice {
        let sub = &mut self.0[start..end];
        // SAFETY: `repr(transparent)`, and the borrow is preserved.
        unsafe { &mut *(sub as *mut [MaybeUninit<u8>] as *mut UninitSlice) }
    }
}

impl Debug for UninitSlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UninitSlice")
            .field("len", &self.0.len())
            .finish()
    }
}

/// A stable, owned buffer that can be read from.
///
/// # Safety
///
/// * [`stable_ptr`](IoBuf::stable_ptr) must return the same address for as long
///   as the buffer is owned by an operation, even if the buffer value itself is
///   moved. This is why `Vec<u8>` qualifies and a fixed-size array does not:
///   moving an array moves its bytes.
/// * [`bytes_init`](IoBuf::bytes_init) must not exceed the allocation length.
pub unsafe trait IoBuf: 'static {
    /// Address of the first byte. Stable across moves of `self`.
    fn stable_ptr(&self) -> *const u8;

    /// Number of initialised bytes available to read.
    fn bytes_init(&self) -> usize;
}

/// A stable, owned buffer that can be written into.
///
/// # Safety
///
/// In addition to [`IoBuf`]'s requirements:
///
/// * [`as_uninit`](IoBufMut::as_uninit) must return the same address as
///   [`IoBuf::stable_ptr`], and that address must stay the same for as long as
///   the buffer is owned by an operation.
/// * The returned region is the one [`set_init`](IoBufMut::set_init)'s length is
///   measured against. An adapter presenting a sub-window of an inner buffer is
///   fine, provided it translates `set_init` back to the inner buffer's own
///   coordinates -- see this crate's internal `TailBuf`.
/// * [`set_init`](IoBufMut::set_init) must make the first `len` bytes readable.
///
/// The predecessor of `as_uninit` returned a bare pointer whose length the
/// caller carried separately, and recombining the two into a `&mut [u8]`
/// produced a reference over uninitialised spare capacity. Returning a
/// [`UninitSlice`] keeps pointer, length and lifetime together, so that mistake
/// is no longer expressible, and -- unlike a plain `&mut [MaybeUninit<u8>]` --
/// it also cannot be used to de-initialise bytes the buffer has already
/// initialised. Windows is given the pointer and the length directly; it never
/// sees a Rust slice.
pub unsafe trait IoBufMut: IoBuf {
    /// The whole buffer -- initialised prefix and uninitialised tail alike.
    ///
    /// A zero-capacity buffer returns an empty region, which may have a
    /// non-null but non-dereferenceable address.
    fn as_uninit(&mut self) -> &mut UninitSlice;

    /// Capacity available for Windows to write into.
    ///
    /// Provided so that implementors need not keep two values in sync. Nothing
    /// in this crate passes this value to Windows -- transfer lengths always
    /// come from [`as_uninit`](IoBufMut::as_uninit) itself -- so overriding it
    /// inconsistently cannot widen a transfer.
    fn bytes_total(&mut self) -> usize {
        self.as_uninit().len()
    }

    /// Record how many bytes Windows actually initialised.
    ///
    /// # Safety
    ///
    /// The first `len` bytes must genuinely have been initialised, and `len`
    /// must not exceed [`bytes_total`](IoBufMut::bytes_total).
    unsafe fn set_init(&mut self, len: usize);
}

unsafe impl IoBuf for Vec<u8> {
    fn stable_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }
}

unsafe impl IoBufMut for Vec<u8> {
    fn as_uninit(&mut self) -> &mut UninitSlice {
        let capacity = self.capacity();
        // SAFETY: `Vec` guarantees the pointer is valid for `capacity` bytes.
        // The region covers the whole allocation, initialised prefix included,
        // because `set_init` publishes an absolute length and `TailBuf` indexes
        // from the start. `UninitSlice` cannot de-initialise that prefix.
        unsafe { UninitSlice::from_raw_parts_mut(self.as_mut_ptr(), capacity) }
    }

    unsafe fn set_init(&mut self, len: usize) {
        assert!(
            len <= self.capacity(),
            "set_init past capacity is immediate undefined behaviour"
        );
        unsafe { self.set_len(len) };
    }
}

unsafe impl IoBuf for Box<[u8]> {
    fn stable_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }
}

unsafe impl IoBufMut for Box<[u8]> {
    fn as_uninit(&mut self) -> &mut UninitSlice {
        // A boxed slice is fully initialised, so no uninitialised region exists
        // to protect -- but the same type is used for uniformity.
        UninitSlice::new(self)
    }

    unsafe fn set_init(&mut self, _len: usize) {
        // A boxed slice is fully initialised on creation; length cannot change.
    }
}

unsafe impl IoBuf for &'static [u8] {
    fn stable_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }
}

/// An operation's result, paired with the state that was handed in.
///
/// The state comes back whether the operation succeeded or failed, so a failed
/// write does not consume the buffer it was writing.
///
/// The state is a buffer only sometimes — a wait carries none, and the HTTP
/// Server API operations carry a kernel-filled structure — which is why this is
/// named for the operation rather than the buffer.
///
/// Both fields are public, so destructuring is the idiomatic access:
///
/// ```
/// # use winasio::iocp::OpResult;
/// let outcome: OpResult<usize, Vec<u8>> = OpResult(Ok(3), vec![1, 2, 3]);
/// let OpResult(result, state) = outcome;
/// assert_eq!(result.unwrap(), 3);
/// assert_eq!(state, vec![1, 2, 3]);
/// ```
#[derive(Debug)]
#[must_use = "the operation's state is returned here and would otherwise be dropped"]
pub struct OpResult<T, S>(pub windows::core::Result<T>, pub S);

impl<T, S> OpResult<T, S> {
    /// Split into the result and the state.
    pub fn into_parts(self) -> (windows::core::Result<T>, S) {
        (self.0, self.1)
    }

    /// Apply a function to the state, keeping the result.
    pub fn map_state<U>(self, f: impl FnOnce(S) -> U) -> OpResult<T, U> {
        OpResult(self.0, f(self.1))
    }
}

impl<T, S: crate::iocp::op::IntoInner> OpResult<T, S> {
    /// Split into the result and the operation's *meaningful* value — usually
    /// the buffer — rather than the operation struct itself.
    ///
    /// Destructuring cannot do this, because it applies
    /// [`IntoInner`](crate::iocp::IntoInner).
    pub fn into_inner_parts(self) -> (windows::core::Result<T>, S::Inner) {
        (self.0, self.1.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_buffer_reports_capacity_and_init() {
        let mut v: Vec<u8> = Vec::with_capacity(64);
        assert_eq!(v.bytes_total(), 64);
        assert_eq!(v.bytes_init(), 0);
        let ptr = v.as_uninit().as_mut_ptr();
        assert!(!ptr.is_null());
        unsafe { v.set_init(10) };
        assert_eq!(v.bytes_init(), 10);
    }

    /// `bytes_total` is provided in terms of `as_uninit`, so an implementor
    /// need not keep two values in sync -- which is what made the old split
    /// pointer/length pair recombinable into a slice over uninitialised memory.
    #[test]
    fn capacity_always_matches_the_uninit_region() {
        let mut v: Vec<u8> = Vec::with_capacity(64);
        assert_eq!(v.as_uninit().len(), v.bytes_total());
        unsafe { v.set_init(10) };
        assert_eq!(
            v.as_uninit().len(),
            v.bytes_total(),
            "publishing an initialised prefix must not shrink the writable region"
        );

        let mut b: Box<[u8]> = vec![0u8; 24].into_boxed_slice();
        assert_eq!(b.as_uninit().len(), b.bytes_total());
    }

    /// The region must start at the buffer's first byte, not at its initialised
    /// prefix: `set_init` publishes an absolute length and `TailBuf` indexes
    /// from the start.
    #[test]
    fn uninit_region_starts_at_the_first_byte() {
        let mut v: Vec<u8> = Vec::with_capacity(8);
        let base = v.stable_ptr();
        unsafe { v.set_init(4) };
        assert_eq!(v.as_uninit().as_mut_ptr().cast_const(), base);
        assert_eq!(v.as_uninit().len(), 8);
    }

    /// The reason `as_uninit` returns [`UninitSlice`] rather than
    /// `&mut [MaybeUninit<u8>]`.
    ///
    /// `as_uninit` is a *safe* method covering the whole allocation, so a raw
    /// `&mut [MaybeUninit<u8>]` would let safe code store `MaybeUninit::uninit`
    /// over bytes the buffer has already initialised, and the next safe read of
    /// those bytes would be undefined behaviour. `UninitSlice` exposes a
    /// pointer, a length and a sub-window, and nothing that can write an
    /// uninitialised value.
    ///
    /// The compile-time half of this is the `compile_fail` doctest on
    /// [`UninitSlice`]; what is checked here is that the initialised prefix
    /// genuinely survives a round trip through the region.
    #[test]
    fn the_initialised_prefix_cannot_be_damaged_through_the_region() {
        let mut v: Vec<u8> = vec![1, 2, 3, 4];
        v.reserve(4);

        let region = v.as_uninit();
        assert!(region.len() >= 8, "prefix plus spare capacity");
        // Only a pointer and a length come out; there is no safe path from here
        // to writing `MaybeUninit::uninit` into the first four bytes.
        let _ptr = region.as_mut_ptr();

        assert_eq!(&v[..], &[1, 2, 3, 4]);
    }

    #[test]
    fn sub_windows_are_bounds_checked() {
        let mut v: Vec<u8> = Vec::with_capacity(16);
        let region = v.as_uninit();
        assert_eq!(region.slice_mut(4, 12).len(), 8);
        assert_eq!(region.slice_mut(16, 16).len(), 0);
        assert!(region.slice_mut(16, 16).is_empty());
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn a_sub_window_past_the_end_panics_rather_than_aliasing() {
        let mut v: Vec<u8> = Vec::with_capacity(16);
        let _ = v.as_uninit().slice_mut(0, 17);
    }

    /// The other documented panic: std words this case differently, so the
    /// `end > len` test above does not cover it.
    #[test]
    #[should_panic(expected = "slice index starts at")]
    fn an_inverted_sub_window_panics() {
        let mut v: Vec<u8> = Vec::with_capacity(16);
        let _ = v.as_uninit().slice_mut(8, 4);
    }

    #[test]
    fn vec_pointer_is_stable_across_move() {
        let v: Vec<u8> = Vec::with_capacity(32);
        let before = v.stable_ptr();
        let moved = v;
        // Moving the Vec moves only the three-word header; the heap block,
        // which is what Windows is given, does not move.
        assert_eq!(before, moved.stable_ptr());
    }

    #[test]
    fn op_result_returns_state_on_error() {
        let outcome: OpResult<usize, Vec<u8>> = OpResult(
            Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                -1,
            ))),
            vec![1, 2, 3],
        );
        let (result, state) = outcome.into_parts();
        assert!(result.is_err());
        assert_eq!(state, vec![1, 2, 3]);
    }

    #[test]
    fn op_result_destructures_directly() {
        let OpResult(result, state) = OpResult(Ok(7usize), vec![9u8]);
        assert_eq!(result.unwrap(), 7);
        assert_eq!(state, vec![9]);
    }
}
