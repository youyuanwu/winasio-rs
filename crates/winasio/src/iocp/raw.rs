// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The per-operation allocation.
//!
//! # The invariant
//!
//! Everything in this module exists to uphold one rule:
//!
//! > The allocation backing an in-flight operation stays alive until Windows
//! > delivers its completion — even if the awaiting future was dropped long
//! > before.
//!
//! It is enforced with a reference count. Submission leaks one count to the
//! kernel; the completion path reclaims it. The future holds a second count.
//! Whichever drops last frees the allocation, so neither a cancelled future nor
//! a late completion can free memory the other still refers to.
//!
//! # Layout
//!
//! [`RawOp`] is `#[repr(C)]` with `OVERLAPPED` first, so the pointer Windows
//! hands back *is* the allocation's address. The completion path recovers the
//! operation by pointer arithmetic; there is no lookup table.
//!
//! Fields Windows or a completion callback may write are wrapped in
//! [`UnsafeCell`], because they are mutated while the allocation is shared
//! through an [`Arc`]. Code in this module must never construct a `&RawOp<T>`
//! that spans them.

// The driver in `proactor.rs` and `threadpool.rs` is the only consumer of this
// module. Until those land, the unit tests are the sole callers, so the
// non-test build sees these items as unused.
#![allow(dead_code)]

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

use windows::core::Result;
use windows::Win32::System::IO::OVERLAPPED;

use super::op::OpCode;

/// Distinguishes our completion packets from anything else that may arrive on a
/// completion port, including packets posted by unrelated code.
const OP_MAGIC: u32 = 0x5741_5349; // "WASI"

/// Operation lifecycle. One `AtomicU32`, advanced by compare-exchange, so the
/// terminal transition happens exactly once no matter how completion and
/// cancellation interleave.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpState {
    /// In flight; the future is still interested.
    Submitted = 0,
    /// The future was dropped. A completion is still expected.
    Abandoned = 1,
    /// A result has been stored and the waker signalled.
    Completed = 2,
    /// The result has been taken by the future, or discarded after abandonment.
    Terminal = 3,
}

impl OpState {
    fn from_u32(v: u32) -> Self {
        match v {
            0 => OpState::Submitted,
            1 => OpState::Abandoned,
            2 => OpState::Completed,
            _ => OpState::Terminal,
        }
    }
}

/// Type-erased operations the completion path performs without knowing `T`.
pub(crate) struct OpVTable {
    /// Store the result, run `on_complete`, and wake the future if any.
    pub(crate) complete: unsafe fn(*mut OVERLAPPED, Result<usize>),
    /// Reclaim the leaked reference without completing (used to unwind a
    /// submission that never actually started).
    pub(crate) reclaim: unsafe fn(*mut OVERLAPPED),
}

/// Fixed-layout prefix that follows `OVERLAPPED`, letting the completion path
/// dispatch without knowing `T`.
#[repr(C)]
pub(crate) struct ErasedHeader {
    magic: u32,
    vtable: &'static OpVTable,
}

/// The heap allocation shared between the future and the kernel.
#[repr(C)]
pub(crate) struct RawOp<T> {
    /// Must stay first: Windows holds a pointer to this field.
    overlapped: UnsafeCell<OVERLAPPED>,
    header: ErasedHeader,
    state: AtomicU32,
    result: UnsafeCell<MaybeUninit<Result<usize>>>,
    waker: Mutex<Option<Waker>>,
    op: UnsafeCell<T>,
}

// SAFETY: `OVERLAPPED` contains raw pointers, which makes `RawOp<T>` `!Send` and
// `!Sync` by inference for every `T`. That inference is too conservative here:
// the kernel owns the `OVERLAPPED` exclusively between submission and
// completion, and this crate never aliases it. Access to `op` is serialised by
// the state machine — `operate` runs before the operation is in flight, and
// `on_complete` runs only on the single thread that wins the terminal CAS.
// Bounding on `T: Send` keeps the thread-pool backend's requirement meaningful;
// a blanket impl would silently erase it.
unsafe impl<T: Send> Send for RawOp<T> {}
unsafe impl<T: Send> Sync for RawOp<T> {}

#[cfg(any(test, feature = "test-util"))]
mod counter {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LIVE: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn inc() {
        LIVE.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn dec() {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }

    /// Number of operation allocations currently alive.
    ///
    /// Test support: lets a soak assert that every submitted operation was
    /// eventually released.
    pub fn live_operations() -> usize {
        LIVE.load(Ordering::SeqCst)
    }
}

#[cfg(any(test, feature = "test-util"))]
pub use counter::live_operations;

impl<T> Drop for RawOp<T> {
    fn drop(&mut self) {
        // If a result was stored but never taken, drop it.
        if OpState::from_u32(self.state.load(Ordering::Acquire)) == OpState::Completed {
            unsafe { (*self.result.get()).assume_init_drop() };
        }
        #[cfg(any(test, feature = "test-util"))]
        counter::dec();
    }
}

impl<T: OpCode> RawOp<T> {
    fn vtable() -> &'static OpVTable {
        trait HasVTable {
            const VTABLE: OpVTable;
        }
        impl<T: OpCode> HasVTable for T {
            const VTABLE: OpVTable = OpVTable {
                complete: complete_erased::<T>,
                reclaim: reclaim_erased::<T>,
            };
        }
        &<T as HasVTable>::VTABLE
    }
}

/// Store a completion result and wake the future, if it is still waiting.
///
/// # Safety
///
/// `optr` must have come from [`Key::into_raw`] for an operation of type `T`,
/// and the leaked reference it represents is consumed here.
unsafe fn complete_erased<T: OpCode>(optr: *mut OVERLAPPED, result: Result<usize>) {
    // Reclaim the reference the kernel held.
    let arc: Arc<RawOp<T>> = unsafe { Arc::from_raw(optr as *const RawOp<T>) };

    // Let the operation read back anything Windows filled in. Sole access:
    // the operation is no longer in flight and only this thread reaches here.
    unsafe { (*arc.op.get()).on_complete(&result) };

    // Claim the terminal transition. Whether the future is still interested
    // decides if we store the result or discard it.
    let previous = arc.state.swap(OpState::Completed as u32, Ordering::AcqRel);
    match OpState::from_u32(previous) {
        OpState::Submitted => {
            unsafe { (*arc.result.get()).write(result) };
            // Publish before waking.
            if let Some(waker) = arc.waker.lock().unwrap().take() {
                waker.wake();
            }
        }
        OpState::Abandoned => {
            // Nobody is waiting. Drop the result and mark it taken so `Drop`
            // does not try to drop it again.
            drop(result);
            arc.state.store(OpState::Terminal as u32, Ordering::Release);
        }
        OpState::Completed | OpState::Terminal => {
            debug_assert!(false, "operation completed twice");
            drop(result);
        }
    }
    // `arc` drops here, releasing the kernel's reference.
}

/// Reclaim a leaked reference for an operation that never started.
///
/// # Safety
///
/// Same contract as [`complete_erased`].
unsafe fn reclaim_erased<T: OpCode>(optr: *mut OVERLAPPED) {
    drop(unsafe { Arc::from_raw(optr as *const RawOp<T>) });
}

/// An owning handle to an operation allocation.
pub(crate) struct Key<T: OpCode> {
    inner: Arc<RawOp<T>>,
}

impl<T: OpCode> Key<T> {
    /// Allocate an operation.
    pub(crate) fn new(op: T) -> Self {
        #[cfg(any(test, feature = "test-util"))]
        counter::inc();

        Key {
            inner: Arc::new(RawOp {
                overlapped: UnsafeCell::new(OVERLAPPED::default()),
                header: ErasedHeader {
                    magic: OP_MAGIC,
                    vtable: RawOp::<T>::vtable(),
                },
                state: AtomicU32::new(OpState::Submitted as u32),
                result: UnsafeCell::new(MaybeUninit::uninit()),
                waker: Mutex::new(None),
                op: UnsafeCell::new(op),
            }),
        }
    }

    /// The pointer Windows is given. Identical to the allocation's address,
    /// because `OVERLAPPED` is the first field of a `#[repr(C)]` struct.
    pub(crate) fn overlapped_ptr(&self) -> *mut OVERLAPPED {
        self.inner.overlapped.get()
    }

    /// Leak one reference for the kernel to hold.
    ///
    /// Must be called *before* the operation is started: a thread-pool callback
    /// can fire before the initiating call returns, and would otherwise try to
    /// reclaim a reference that does not exist yet.
    pub(crate) fn leak(&self) -> *mut OVERLAPPED {
        let cloned = Arc::clone(&self.inner);
        Arc::into_raw(cloned) as *mut OVERLAPPED
    }

    /// Reclaim a reference leaked by [`Key::leak`] for an operation that never
    /// started.
    ///
    /// # Safety
    ///
    /// `optr` must be the value returned by a matching [`Key::leak`], and the
    /// operation must not have completed.
    pub(crate) unsafe fn unleak(optr: *mut OVERLAPPED) {
        unsafe { reclaim_erased::<T>(optr) };
    }

    /// Run the operation's start routine.
    ///
    /// # Safety
    ///
    /// Must be called at most once, before the operation is in flight.
    pub(crate) unsafe fn operate(&self) -> std::task::Poll<Result<usize>> {
        let optr = self.overlapped_ptr();
        unsafe { (*self.inner.op.get()).operate(optr) }
    }

    /// Ask the operation to cancel itself.
    ///
    /// # Safety
    ///
    /// Only valid while the operation is in flight.
    pub(crate) unsafe fn cancel(&self) -> Result<()> {
        let optr = self.overlapped_ptr();
        unsafe { (*self.inner.op.get()).cancel(optr) }
    }

    /// Read the operation's declared type.
    pub(crate) fn op_type(&self) -> super::op::OpType {
        unsafe { (*self.inner.op.get()).op_type() }
    }

    /// Install the waker to be signalled on completion.
    pub(crate) fn set_waker(&self, waker: &Waker) {
        let mut slot = self.inner.waker.lock().unwrap();
        match slot.as_ref() {
            Some(existing) if existing.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
    }

    /// Take the completion result, if one has been stored.
    pub(crate) fn take_result(&self) -> Option<Result<usize>> {
        let swapped = self.inner.state.compare_exchange(
            OpState::Completed as u32,
            OpState::Terminal as u32,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if swapped.is_ok() {
            Some(unsafe { (*self.inner.result.get()).assume_init_read() })
        } else {
            None
        }
    }

    /// Mark the operation abandoned because the future was dropped.
    ///
    /// Returns `true` if the operation was still in flight, meaning a
    /// completion is still expected and cancellation should be requested.
    pub(crate) fn abandon(&self) -> bool {
        self.inner
            .state
            .compare_exchange(
                OpState::Submitted as u32,
                OpState::Abandoned as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Consume the key and yield the operation, if this is the last reference.
    pub(crate) fn try_into_op(self) -> std::result::Result<T, Self> {
        match Arc::try_unwrap(self.inner) {
            Ok(raw) => {
                // Prevent `Drop` from running against a partially moved value.
                let raw = std::mem::ManuallyDrop::new(raw);
                if OpState::from_u32(raw.state.load(Ordering::Acquire)) == OpState::Completed {
                    unsafe { (*raw.result.get()).assume_init_drop() };
                }
                #[cfg(any(test, feature = "test-util"))]
                counter::dec();
                Ok(unsafe { std::ptr::read(raw.op.get()) })
            }
            Err(inner) => Err(Key { inner }),
        }
    }
}

impl<T: OpCode> Clone for Key<T> {
    fn clone(&self) -> Self {
        Key {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Deliver a completion to whichever operation owns `optr`.
///
/// Returns `false` if the pointer is not one of ours, which happens for packets
/// posted by unrelated code sharing the port.
///
/// # Safety
///
/// `optr` must either be null, or point at an [`ErasedHeader`]-bearing
/// allocation produced by [`Key::leak`].
pub(crate) unsafe fn dispatch_completion(optr: *mut OVERLAPPED, result: Result<usize>) -> bool {
    if optr.is_null() {
        return false;
    }
    let header = unsafe { header_of(optr) };
    if header.magic != OP_MAGIC {
        return false;
    }
    unsafe { (header.vtable.complete)(optr, result) };
    true
}

/// Locate the erased header that follows `OVERLAPPED` in the allocation.
///
/// # Safety
///
/// `optr` must point at a live [`RawOp`] allocation.
unsafe fn header_of<'a>(optr: *mut OVERLAPPED) -> &'a ErasedHeader {
    // The header sits immediately after `OVERLAPPED`, at an offset that does not
    // depend on `T` because `#[repr(C)]` lays out fields in declaration order.
    let base = optr as *const u8;
    let offset = std::mem::size_of::<OVERLAPPED>();
    unsafe { &*(base.add(offset) as *const ErasedHeader) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;

    struct NoopOp {
        _tag: u32,
        _buf: Vec<u8>,
    }

    unsafe impl OpCode for NoopOp {
        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            Poll::Pending
        }
    }

    struct BigOp {
        _pad: [u8; 512],
    }

    unsafe impl OpCode for BigOp {
        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            Poll::Pending
        }
    }

    fn assert_send<T: Send>() {}

    #[test]
    fn raw_op_is_send_when_op_is_send() {
        // Load-bearing: without the explicit `unsafe impl`s above, `OVERLAPPED`'s
        // raw pointers make this fail to compile for every `T`, which would make
        // `Submit<T>` unspawnable on a multi-threaded runtime.
        assert_send::<RawOp<NoopOp>>();
        assert_send::<Key<NoopOp>>();
        assert_send::<Arc<RawOp<NoopOp>>>();
    }

    #[test]
    fn overlapped_is_at_offset_zero() {
        let key = Key::new(NoopOp {
            _tag: 7,
            _buf: vec![0u8; 8],
        });
        let alloc = Arc::as_ptr(&key.inner) as usize;
        let ovl = key.overlapped_ptr() as usize;
        assert_eq!(alloc, ovl, "OVERLAPPED must be at offset 0 of RawOp<T>");
    }

    #[test]
    fn erased_header_offset_is_type_independent() {
        let small = Key::new(NoopOp {
            _tag: 1,
            _buf: Vec::new(),
        });
        let big = Key::new(BigOp { _pad: [0u8; 512] });

        let expected = std::mem::size_of::<OVERLAPPED>();
        for (base, ovl) in [
            (
                Arc::as_ptr(&small.inner) as usize,
                small.overlapped_ptr() as usize,
            ),
            (
                Arc::as_ptr(&big.inner) as usize,
                big.overlapped_ptr() as usize,
            ),
        ] {
            assert_eq!(base, ovl);
        }

        // The header must be readable at the same offset for both types.
        let h1 = unsafe { header_of(small.overlapped_ptr()) };
        let h2 = unsafe { header_of(big.overlapped_ptr()) };
        assert_eq!(h1.magic, OP_MAGIC);
        assert_eq!(h2.magic, OP_MAGIC);
        assert_eq!(expected, std::mem::size_of::<OVERLAPPED>());
    }

    #[test]
    fn leak_and_complete_frees_exactly_once() {
        let before = live_operations();
        let key = Key::new(NoopOp {
            _tag: 3,
            _buf: vec![1, 2, 3],
        });
        assert_eq!(live_operations(), before + 1);

        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(3)) };

        // The future still holds its reference, so the result is available.
        assert_eq!(key.take_result().unwrap().unwrap(), 3);
        drop(key);
        assert_eq!(live_operations(), before, "allocation must be released");
    }

    #[test]
    fn abandoned_operation_survives_until_completion() {
        let before = live_operations();
        let key = Key::new(NoopOp {
            _tag: 4,
            _buf: vec![9; 64],
        });
        let optr = key.leak();

        assert!(key.abandon(), "first abandon wins");
        drop(key);
        assert_eq!(
            live_operations(),
            before + 1,
            "kernel reference must keep it alive"
        );

        unsafe { dispatch_completion(optr, Ok(0)) };
        assert_eq!(
            live_operations(),
            before,
            "released once the completion arrives"
        );
    }

    #[test]
    fn foreign_pointer_is_rejected() {
        let mut stray = OVERLAPPED::default();
        let ok = unsafe { dispatch_completion(std::ptr::addr_of_mut!(stray), Ok(0)) };
        assert!(!ok, "a packet without our magic must be ignored");

        let ok = unsafe { dispatch_completion(std::ptr::null_mut(), Ok(0)) };
        assert!(!ok, "a null pointer must be ignored");
    }

    #[test]
    fn take_result_is_exactly_once() {
        let key = Key::new(NoopOp {
            _tag: 5,
            _buf: Vec::new(),
        });
        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(11)) };
        assert_eq!(key.take_result().unwrap().unwrap(), 11);
        assert!(key.take_result().is_none(), "result must be taken once");
    }

    #[test]
    fn unstarted_operation_can_be_reclaimed() {
        let before = live_operations();
        let key = Key::new(NoopOp {
            _tag: 6,
            _buf: Vec::new(),
        });
        let optr = key.leak();
        unsafe { Key::<NoopOp>::unleak(optr) };
        drop(key);
        assert_eq!(live_operations(), before);
    }

    #[test]
    fn op_state_round_trips() {
        for s in [
            OpState::Submitted,
            OpState::Abandoned,
            OpState::Completed,
            OpState::Terminal,
        ] {
            assert_eq!(OpState::from_u32(s as u32), s);
        }
    }
}
