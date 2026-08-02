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
//! # Interior mutability
//!
//! The allocation is shared through an [`Arc`], yet Windows writes the
//! `OVERLAPPED` and the operation itself is mutated by `operate`, `cancel`, and
//! `on_complete`. Those fields therefore live behind [`UnsafeCell`] or a lock;
//! forming an ordinary `&`/`&mut` to their *contents* outside the exclusivity
//! windows documented on each accessor is undefined behaviour.
//!
//! The operation is guarded by a [`Mutex`] rather than a bare `UnsafeCell`
//! because `cancel` (called from the future's `Drop`) and `on_complete` (called
//! from the completion) genuinely can run concurrently. The lock is uncontended
//! in the common case and is dwarfed by the syscall it accompanies.
//!
//! # Publication order
//!
//! The completion result is written to its slot **before** the state is
//! advanced to [`OpState::Completed`]. A thread that observes `Completed` is
//! therefore guaranteed a written result. Doing this in the other order is a
//! data race that hands uninitialised memory to the caller.

// The driver in `proactor.rs` and `threadpool.rs` is the only consumer of this
// module. Until those land, the unit tests are the sole callers, so the
// non-test build sees these items as unused.
#![allow(dead_code)]

use std::cell::UnsafeCell;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

use windows::core::Result;
use windows::Win32::System::IO::OVERLAPPED;

use super::op::{OpCode, OpType};

/// Guards against acting on a packet whose memory has been corrupted or
/// mis-attributed. Defence in depth: the driver establishes ownership from the
/// completion key first.
const OP_MAGIC: u32 = 0x5741_5349; // "WASI"

/// Operation lifecycle.
///
/// Advanced by compare-exchange so the terminal transition happens exactly once
/// however completion and cancellation interleave. The state never passes
/// through [`OpState::Completed`] unless a result has already been written.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpState {
    /// In flight; the future is still interested.
    Submitted = 0,
    /// The future was dropped. A completion is still expected, and its result
    /// will be discarded rather than stored.
    Abandoned = 1,
    /// A result has been written and is available to the future.
    Completed = 2,
    /// The result has been taken, or discarded after abandonment.
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
    complete: unsafe fn(*mut OVERLAPPED, Result<usize>),
}

/// Fixed-layout prefix following `OVERLAPPED`, letting the completion path
/// dispatch without knowing `T`.
///
/// Its offset within [`RawOp`] does not depend on `T`: `#[repr(C)]` places it
/// at `size_of::<OVERLAPPED>()` rounded up to its own alignment, and the two
/// alignments coincide on every supported target. The unit tests assert this.
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
    /// Only readable once `state` reads [`OpState::Completed`].
    result: UnsafeCell<MaybeUninit<Result<usize>>>,
    waker: Mutex<Option<Waker>>,
    /// Locked by `operate`, `cancel`, and `on_complete`, which can race.
    op: Mutex<T>,
}

// SAFETY: `OVERLAPPED` contains raw pointers, which makes `RawOp<T>` `!Send`
// and `!Sync` by inference for every `T`. That inference is too conservative
// here: the kernel owns the `OVERLAPPED` exclusively between submission and
// completion and this crate never aliases it, the operation is behind a
// `Mutex`, and every other mutable field is guarded by the state machine.
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
        // A result that was written but never taken must still be dropped.
        // `Completed` is the only state in which the slot holds a live value.
        if OpState::from_u32(self.state.load(Ordering::Acquire)) == OpState::Completed {
            unsafe { (*self.result.get()).assume_init_drop() };
        }
        #[cfg(any(test, feature = "test-util"))]
        counter::dec();
    }
}

fn vtable_of<T: OpCode>() -> &'static OpVTable {
    trait HasVTable {
        const VTABLE: OpVTable;
    }
    impl<T: OpCode> HasVTable for T {
        const VTABLE: OpVTable = OpVTable {
            complete: complete_erased::<T>,
        };
    }
    &<T as HasVTable>::VTABLE
}

/// Store a completion result and wake the future, if one is still waiting.
///
/// # Safety
///
/// `optr` must be a pointer produced by [`Key::leak`] for an operation of type
/// `T` whose leaked reference has not yet been reclaimed. That reference is
/// consumed here.
unsafe fn complete_erased<T: OpCode>(optr: *mut OVERLAPPED, result: Result<usize>) {
    // Reclaim the reference the kernel held. The allocation cannot be freed
    // before this function returns, because we now own a strong count.
    let arc: Arc<RawOp<T>> = unsafe { Arc::from_raw(optr as *const RawOp<T>) };

    // Let the operation read back whatever Windows filled in. The lock makes
    // this exclusive with respect to a concurrent `cancel` from the future's
    // `Drop`, which is the one call that can genuinely race us.
    {
        let mut op = arc.op.lock().unwrap();
        unsafe { op.on_complete(&result) };
    }

    // Write the result *before* publishing `Completed`. Any thread that
    // observes `Completed` is then guaranteed to find a written value.
    unsafe { (*arc.result.get()).write(result) };

    match arc.state.compare_exchange(
        OpState::Submitted as u32,
        OpState::Completed as u32,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            // Release the lock before waking: an executor that polls inline on
            // the waking thread would otherwise re-enter `set_waker` and
            // deadlock on this non-reentrant mutex.
            let waker = arc.waker.lock().unwrap().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
        Err(previous) => {
            debug_assert_eq!(
                OpState::from_u32(previous),
                OpState::Abandoned,
                "an operation completed twice"
            );
            // Nobody is waiting. Take the value straight back out and drop it,
            // so the state never advertises a result that will not be read.
            let discarded = unsafe { (*arc.result.get()).assume_init_read() };
            drop(discarded);
            arc.state.store(OpState::Terminal as u32, Ordering::Release);
        }
    }
    // `arc` drops here, releasing the kernel's reference.
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
                    vtable: vtable_of::<T>(),
                },
                state: AtomicU32::new(OpState::Submitted as u32),
                result: UnsafeCell::new(MaybeUninit::uninit()),
                waker: Mutex::new(None),
                op: Mutex::new(op),
            }),
        }
    }

    /// The pointer Windows is given.
    ///
    /// Derived from the allocation base so it carries provenance over the whole
    /// `RawOp`, which the completion path relies on when it reaches fields
    /// beyond the `OVERLAPPED`.
    pub(crate) fn overlapped_ptr(&self) -> *mut OVERLAPPED {
        Arc::as_ptr(&self.inner) as *mut OVERLAPPED
    }

    /// Leak one reference for the kernel to hold.
    ///
    /// Must be called *before* the operation is started: a thread-pool callback
    /// can fire before the initiating call returns, and would otherwise try to
    /// reclaim a reference that does not exist yet.
    pub(crate) fn leak(&self) -> *mut OVERLAPPED {
        Arc::into_raw(Arc::clone(&self.inner)) as *mut OVERLAPPED
    }

    /// Reclaim a reference leaked by [`Key::leak`] for an operation that never
    /// started.
    ///
    /// # Safety
    ///
    /// `optr` must be the value returned by a matching [`Key::leak`], and the
    /// operation must not have completed.
    pub(crate) unsafe fn unleak(optr: *mut OVERLAPPED) {
        drop(unsafe { Arc::from_raw(optr as *const RawOp<T>) });
    }

    /// Run the operation's start routine.
    ///
    /// # Safety
    ///
    /// Must be called at most once, before the operation is in flight.
    pub(crate) unsafe fn operate(&self) -> std::task::Poll<Result<usize>> {
        let optr = self.overlapped_ptr();
        let mut op = self.inner.op.lock().unwrap();
        unsafe { op.operate(optr) }
    }

    /// Ask the operation to cancel itself.
    ///
    /// Safe to call concurrently with a completion: the operation lock keeps the
    /// two from aliasing, and cancelling an already-finished operation is a
    /// no-op at the Windows level.
    pub(crate) fn cancel(&self) -> Result<()> {
        let optr = self.overlapped_ptr();
        let mut op = self.inner.op.lock().unwrap();
        unsafe { op.cancel(optr) }
    }

    /// Read the operation's declared type.
    pub(crate) fn op_type(&self) -> OpType {
        self.inner.op.lock().unwrap().op_type()
    }

    /// Install the waker to be signalled on completion.
    pub(crate) fn set_waker(&self, waker: &Waker) {
        let mut slot = self.inner.waker.lock().unwrap();
        match slot.as_ref() {
            Some(existing) if existing.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
    }

    /// Take the completion result, if one has been written.
    pub(crate) fn take_result(&self) -> Option<Result<usize>> {
        self.inner
            .state
            .compare_exchange(
                OpState::Completed as u32,
                OpState::Terminal as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            // SAFETY: the CAS succeeded, so the state was `Completed`, which is
            // only published after the result has been written. The CAS also
            // guarantees no other thread can take it.
            .map(|_| unsafe { (*self.inner.result.get()).assume_init_read() })
    }

    /// Mark the operation abandoned because the future was dropped.
    ///
    /// Returns `true` if it was still in flight, meaning a completion is still
    /// expected and cancellation is worth requesting.
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
        let raw = match Arc::try_unwrap(self.inner) {
            Ok(raw) => raw,
            Err(inner) => return Err(Key { inner }),
        };

        // `Arc::try_unwrap` proved uniqueness, so nothing else can observe this
        // allocation. Suppress the `Drop` impl and dispose of each field by
        // hand, because `op` must be moved out rather than dropped.
        let raw = ManuallyDrop::new(raw);

        if OpState::from_u32(raw.state.load(Ordering::Acquire)) == OpState::Completed {
            unsafe { (*raw.result.get()).assume_init_drop() };
        }

        // SAFETY: unique ownership; each field is read or dropped exactly once.
        // `overlapped`, `header` and `state` are plain data with no drop glue.
        let op = unsafe { std::ptr::read(&raw.op) };
        unsafe { std::ptr::drop_in_place(&raw.waker as *const _ as *mut Mutex<Option<Waker>>) };

        #[cfg(any(test, feature = "test-util"))]
        counter::dec();

        Ok(op.into_inner().unwrap_or_else(|e| e.into_inner()))
    }
}

impl<T: OpCode> Clone for Key<T> {
    fn clone(&self) -> Self {
        Key {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Deliver a completion to the operation that owns `optr`.
///
/// Returns `false` if the packet does not carry our magic tag, which indicates
/// corruption or a mis-attributed packet rather than a routine occurrence.
///
/// # Safety
///
/// The caller must already have established that this packet belongs to this
/// crate — the driver does so from the completion key it set when associating
/// the handle. `optr` must be a pointer produced by [`Key::leak`] whose leaked
/// reference has not been reclaimed.
///
/// The magic check performed here is defence in depth against corruption, not a
/// validity oracle: reading it requires `optr` to already point at a live
/// [`RawOp`] allocation.
pub(crate) unsafe fn dispatch_completion(optr: *mut OVERLAPPED, result: Result<usize>) -> bool {
    if optr.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees `optr` points at one of our allocations, so
    // the header is in bounds.
    let header = unsafe { read_header(optr) };
    if header.0 != OP_MAGIC {
        return false;
    }
    unsafe { (header.1.complete)(optr, result) };
    true
}

/// Read the erased header that follows `OVERLAPPED`.
///
/// Returns the raw fields rather than a reference, so no borrow with an
/// unconstrained lifetime is ever materialised.
///
/// # Safety
///
/// `optr` must point at a live [`RawOp`] allocation.
unsafe fn read_header(optr: *mut OVERLAPPED) -> (u32, &'static OpVTable) {
    let base = optr as *const u8;
    let header = unsafe { base.add(std::mem::size_of::<OVERLAPPED>()) } as *const ErasedHeader;
    let magic = unsafe { std::ptr::read(std::ptr::addr_of!((*header).magic)) };
    let vtable = unsafe { std::ptr::read(std::ptr::addr_of!((*header).vtable)) };
    (magic, vtable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::task::Poll;

    struct NoopOp {
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

    /// Records that erased dispatch reached the right monomorphisation.
    static REACHED: AtomicBool = AtomicBool::new(false);
    static REACHED_LEN: AtomicU32 = AtomicU32::new(0);

    struct ObservingOp;

    unsafe impl OpCode for ObservingOp {
        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            Poll::Pending
        }
        unsafe fn on_complete(&mut self, result: &Result<usize>) {
            REACHED.store(true, Ordering::SeqCst);
            if let Ok(n) = result {
                REACHED_LEN.store(*n as u32, Ordering::SeqCst);
            }
        }
    }

    fn noop(len: usize) -> NoopOp {
        NoopOp {
            _buf: vec![0u8; len],
        }
    }

    fn assert_send<T: Send>() {}

    /// `live_operations` is process-global, so tests that assert on it must not
    /// observe each other's allocations. Cargo runs tests in parallel by
    /// default, so they take this lock.
    ///
    /// Phase 4's soak has the same constraint.
    static COUNTER_LOCK: Mutex<()> = Mutex::new(());

    fn counter_guard() -> std::sync::MutexGuard<'static, ()> {
        COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn raw_op_is_send_when_op_is_send() {
        // Load-bearing: without the explicit `unsafe impl`s, `OVERLAPPED`'s raw
        // pointers make this fail to compile for every `T`, which would leave
        // the submission future unspawnable on a multi-threaded runtime.
        assert_send::<RawOp<NoopOp>>();
        assert_send::<Key<NoopOp>>();
        assert_send::<Arc<RawOp<NoopOp>>>();
    }

    #[test]
    fn overlapped_is_at_offset_zero() {
        assert_eq!(std::mem::offset_of!(RawOp<NoopOp>, overlapped), 0);
        assert_eq!(std::mem::offset_of!(RawOp<BigOp>, overlapped), 0);

        let key = Key::new(noop(8));
        assert_eq!(
            Arc::as_ptr(&key.inner) as usize,
            key.overlapped_ptr() as usize,
            "the kernel's pointer must be the allocation address"
        );
    }

    #[test]
    fn erased_header_offset_is_type_independent() {
        // The completion path reaches the header by adding this exact offset to
        // a pointer it received from Windows, without knowing `T`.
        let expected = std::mem::size_of::<OVERLAPPED>();
        assert_eq!(std::mem::offset_of!(RawOp<NoopOp>, header), expected);
        assert_eq!(std::mem::offset_of!(RawOp<BigOp>, header), expected);
        assert_eq!(std::mem::offset_of!(RawOp<ObservingOp>, header), expected);
    }

    #[test]
    fn erased_dispatch_reaches_the_right_type() {
        REACHED.store(false, Ordering::SeqCst);
        REACHED_LEN.store(0, Ordering::SeqCst);

        let key = Key::new(ObservingOp);
        let optr = key.leak();
        assert!(unsafe { dispatch_completion(optr, Ok(4242)) });

        assert!(
            REACHED.load(Ordering::SeqCst),
            "on_complete must run on the concrete type"
        );
        assert_eq!(REACHED_LEN.load(Ordering::SeqCst), 4242);
        assert_eq!(key.take_result().unwrap().unwrap(), 4242);
    }

    #[test]
    fn leak_and_complete_frees_exactly_once() {
        let _guard = counter_guard();
        let before = live_operations();
        let key = Key::new(noop(3));
        assert_eq!(live_operations(), before + 1);

        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(3)) };

        assert_eq!(key.take_result().unwrap().unwrap(), 3);
        drop(key);
        assert_eq!(live_operations(), before, "allocation must be released");
    }

    #[test]
    fn abandoned_operation_survives_until_completion() {
        let _guard = counter_guard();
        let before = live_operations();
        let key = Key::new(noop(64));
        let optr = key.leak();

        assert!(key.abandon(), "first abandon wins");
        drop(key);
        assert_eq!(
            live_operations(),
            before + 1,
            "the kernel reference must keep it alive"
        );

        unsafe { dispatch_completion(optr, Ok(0)) };
        assert_eq!(
            live_operations(),
            before,
            "released once the completion arrives"
        );
    }

    #[test]
    fn completed_but_never_taken_is_dropped_once() {
        let _guard = counter_guard();
        let before = live_operations();
        let key = Key::new(noop(1));
        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(7)) };
        // Never call take_result: `Drop` must dispose of the stored result.
        drop(key);
        assert_eq!(live_operations(), before);
    }

    #[test]
    fn foreign_pointer_is_rejected() {
        // Embed the OVERLAPPED in a larger zeroed allocation so the header probe
        // stays in bounds, and poison the following bytes so the test does not
        // pass merely because adjacent memory happened to be zero.
        #[repr(C)]
        struct Foreign {
            overlapped: OVERLAPPED,
            poison: [u8; 64],
        }
        let mut foreign = Foreign {
            overlapped: OVERLAPPED::default(),
            poison: [0xAB; 64],
        };
        let ok = unsafe { dispatch_completion(std::ptr::addr_of_mut!(foreign.overlapped), Ok(0)) };
        assert!(!ok, "a packet without our magic must be rejected");

        let ok = unsafe { dispatch_completion(std::ptr::null_mut(), Ok(0)) };
        assert!(!ok, "a null pointer must be rejected");
    }

    #[test]
    fn take_result_is_exactly_once() {
        let key = Key::new(noop(0));
        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(11)) };
        assert_eq!(key.take_result().unwrap().unwrap(), 11);
        assert!(key.take_result().is_none(), "the result is taken once");
    }

    #[test]
    fn unstarted_operation_can_be_reclaimed() {
        let _guard = counter_guard();
        let before = live_operations();
        let key = Key::new(noop(0));
        let optr = key.leak();
        unsafe { Key::<NoopOp>::unleak(optr) };
        drop(key);
        assert_eq!(live_operations(), before);
    }

    #[test]
    fn try_into_op_returns_state_and_releases_the_allocation() {
        let _guard = counter_guard();
        let before = live_operations();
        let key = Key::new(noop(16));

        // Install a waker so the field is non-empty; it must still be released.
        let waker = futures_noop_waker();
        key.set_waker(&waker);

        let op = key.try_into_op().unwrap_or_else(|_| panic!("unique"));
        assert_eq!(op._buf.len(), 16);
        assert_eq!(live_operations(), before, "allocation must be released");
    }

    #[test]
    fn try_into_op_after_completion_drops_the_result() {
        let _guard = counter_guard();
        let before = live_operations();
        let key = Key::new(noop(2));
        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(2)) };
        // Result written but deliberately not taken.
        let op = key.try_into_op().unwrap_or_else(|_| panic!("unique"));
        assert_eq!(op._buf.len(), 2);
        assert_eq!(live_operations(), before);
    }

    #[test]
    fn try_into_op_fails_while_shared() {
        let key = Key::new(noop(1));
        let clone = key.clone();
        assert!(
            key.try_into_op().is_err(),
            "shared keys cannot be unwrapped"
        );
        drop(clone);
    }

    #[test]
    fn waker_is_signalled_on_completion() {
        use std::sync::atomic::AtomicBool;
        static WOKEN: AtomicBool = AtomicBool::new(false);
        WOKEN.store(false, Ordering::SeqCst);

        let waker = counting_waker(&WOKEN);
        let key = Key::new(noop(0));
        key.set_waker(&waker);

        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(1)) };
        assert!(WOKEN.load(Ordering::SeqCst), "the waker must be signalled");
    }

    #[test]
    fn abandoned_completion_does_not_wake() {
        use std::sync::atomic::AtomicBool;
        static WOKEN: AtomicBool = AtomicBool::new(false);
        WOKEN.store(false, Ordering::SeqCst);

        let waker = counting_waker(&WOKEN);
        let key = Key::new(noop(0));
        key.set_waker(&waker);
        let optr = key.leak();

        assert!(key.abandon());
        unsafe { dispatch_completion(optr, Ok(1)) };
        assert!(
            !WOKEN.load(Ordering::SeqCst),
            "an abandoned operation has nobody to wake"
        );
        assert!(key.take_result().is_none(), "its result is discarded");
    }

    #[test]
    fn concurrent_abandon_and_complete_release_exactly_once() {
        // Exercises the interleaving the state machine exists to make safe.
        let _guard = counter_guard();
        let before = live_operations();
        for _ in 0..2_000 {
            let key = Key::new(noop(8));
            let optr = key.leak();
            let moved = key.clone();
            let h = std::thread::spawn(move || {
                if moved.abandon() {
                    let _ = moved.cancel();
                }
            });
            unsafe { dispatch_completion(optr, Ok(8)) };
            h.join().unwrap();
            drop(key);
        }
        assert_eq!(
            live_operations(),
            before,
            "every allocation must be released exactly once"
        );
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

    // --- test wakers -------------------------------------------------------

    fn futures_noop_waker() -> Waker {
        use std::task::{RawWaker, RawWakerVTable};
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    fn counting_waker(flag: &'static AtomicBool) -> Waker {
        use std::task::{RawWaker, RawWakerVTable};
        unsafe fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        unsafe fn wake(p: *const ()) {
            unsafe { (*(p as *const AtomicBool)).store(true, Ordering::SeqCst) };
        }
        unsafe fn wake_by_ref(p: *const ()) {
            unsafe { wake(p) };
        }
        unsafe fn drop_fn(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_fn);
        unsafe {
            Waker::from_raw(RawWaker::new(
                flag as *const AtomicBool as *const (),
                &VTABLE,
            ))
        }
    }
}
