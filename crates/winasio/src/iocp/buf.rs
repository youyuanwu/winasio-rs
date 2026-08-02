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
/// * [`stable_mut_ptr`](IoBufMut::stable_mut_ptr) must return the same address
///   as [`IoBuf::stable_ptr`].
/// * [`bytes_total`](IoBufMut::bytes_total) must not exceed the allocation
///   length, since Windows may write that many bytes.
/// * [`set_init`](IoBufMut::set_init) must make the first `len` bytes readable.
pub unsafe trait IoBufMut: IoBuf {
    /// Mutable address of the first byte. Stable across moves of `self`.
    ///
    /// A zero-capacity buffer may return a non-null but non-dereferenceable
    /// pointer; callers must respect [`bytes_total`](IoBufMut::bytes_total).
    fn stable_mut_ptr(&mut self) -> *mut u8;

    /// Capacity available for Windows to write into.
    fn bytes_total(&self) -> usize;

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
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn bytes_total(&self) -> usize {
        self.capacity()
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
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn bytes_total(&self) -> usize {
        self.len()
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
        let ptr = v.stable_mut_ptr();
        assert!(!ptr.is_null());
        unsafe { v.set_init(10) };
        assert_eq!(v.bytes_init(), 10);
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
