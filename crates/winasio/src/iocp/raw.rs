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
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

use windows::core::Result;
use windows::Win32::System::IO::OVERLAPPED;

use super::op::{OpCode, OpType};

/// Guards against acting on a packet whose memory has been corrupted.
///
/// Ownership is established by [`is_ours`] before anything is dereferenced;
/// this tag is a second line of defence against corruption, not the primary
/// check.
const OP_MAGIC: u32 = 0x5741_5349; // "WASI"

/// Addresses of operation allocations currently owned by the kernel.
///
/// A completion packet arriving on a port or thread-pool registration is **not**
/// necessarily ours: the completion key is set per *handle*, so any overlapped
/// call the user makes on a registered handle produces a packet carrying it.
/// Dereferencing such a pointer to look for our header would read past the
/// caller's `OVERLAPPED` — an out-of-bounds read, and a potential indirect call
/// through foreign memory.
///
/// So membership is tested by address, before any dereference.
mod live {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    fn set() -> &'static Mutex<HashSet<usize>> {
        static SET: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
        SET.get_or_init(|| Mutex::new(HashSet::new()))
    }

    fn lock() -> std::sync::MutexGuard<'static, HashSet<usize>> {
        set().lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record that the kernel now owns this allocation.
    pub(super) fn insert(addr: usize) {
        lock().insert(addr);
    }

    /// Claim a completion. Returns `false` if the address is not one of ours,
    /// or if another thread already claimed it.
    pub(super) fn claim(addr: usize) -> bool {
        lock().remove(&addr)
    }
}

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
    /// Store the result, run the completion hook, and wake the future if any.
    ///
    /// The third argument is the byte count the platform reported, which is
    /// carried separately because a failure status still cannot hold one.
    complete: unsafe fn(*mut OVERLAPPED, Result<usize>, usize),
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
    ///
    /// `Option` so the operation can be moved out by whichever thread wins the
    /// terminal transition, without requiring sole ownership of the allocation.
    /// The completion path may still hold its own reference at that moment.
    op: Mutex<Option<T>>,
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
unsafe fn complete_erased<T: OpCode>(
    optr: *mut OVERLAPPED,
    result: Result<usize>,
    transferred: usize,
) {
    // Reclaim the reference the kernel held. The allocation cannot be freed
    // before this function returns, because we now own a strong count.
    let arc: Arc<RawOp<T>> = unsafe { Arc::from_raw(optr as *const RawOp<T>) };

    // Let the operation read back whatever Windows filled in. The lock makes
    // this exclusive with respect to a concurrent `cancel` from the future's
    // `Drop`, which is the one call that can genuinely race us.
    {
        let mut op = arc.op.lock().unwrap();
        if let Some(op) = op.as_mut() {
            unsafe { op.on_complete_with(&result, transferred) };
        }
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
            // Take the waker out of the lock, then release *our* reference,
            // and only then wake. An executor that polls inline on this thread
            // would otherwise find the allocation still shared and be unable to
            // reclaim the operation state.
            let waker = arc.waker.lock().unwrap().take();
            drop(arc);
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
}

/// An owning handle to an operation allocation.
pub(crate) struct Key<T: OpCode> {
    inner: Arc<RawOp<T>>,
}

pub(crate) struct ErasedCancel {
    ptr: *const (),
    vtable: &'static ErasedCancelVTable,
}

struct ErasedCancelVTable {
    cancel: unsafe fn(*const ()) -> Result<()>,
    drop_ref: unsafe fn(*const ()),
}

impl ErasedCancel {
    pub(crate) fn cancel(&self) -> Result<()> {
        // SAFETY: `ptr` was produced by `Key::erased_cancel` for this vtable.
        unsafe { (self.vtable.cancel)(self.ptr) }
    }
}

impl Drop for ErasedCancel {
    fn drop(&mut self) {
        // SAFETY: `ptr` is the strong reference owned by this value.
        unsafe { (self.vtable.drop_ref)(self.ptr) };
    }
}

fn erased_cancel_vtable_of<T: OpCode>() -> &'static ErasedCancelVTable {
    trait HasVTable {
        const VTABLE: ErasedCancelVTable;
    }
    impl<T: OpCode> HasVTable for T {
        const VTABLE: ErasedCancelVTable = ErasedCancelVTable {
            cancel: cancel_erased::<T>,
            drop_ref: drop_erased::<T>,
        };
    }
    &<T as HasVTable>::VTABLE
}

unsafe fn cancel_erased<T: OpCode>(ptr: *const ()) -> Result<()> {
    // SAFETY: `ptr` is a strong `Arc<RawOp<T>>` reference created by
    // `Key::erased_cancel`. `ManuallyDrop` lets us borrow it without consuming
    // the pending table's reference.
    let arc = std::mem::ManuallyDrop::new(unsafe { Arc::from_raw(ptr as *const RawOp<T>) });
    let optr = Arc::as_ptr(&arc) as *mut OVERLAPPED;
    let mut guard = arc.op.lock().unwrap();
    match guard.as_mut() {
        // SAFETY: `optr` is this operation's own OVERLAPPED pointer.
        Some(op) => unsafe { op.cancel(optr) },
        None => Ok(()),
    }
}

unsafe fn drop_erased<T: OpCode>(ptr: *const ()) {
    // SAFETY: `ptr` is the strong reference owned by `ErasedCancel`.
    drop(unsafe { Arc::from_raw(ptr as *const RawOp<T>) });
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
                op: Mutex::new(Some(op)),
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
    ///
    /// Also records the allocation as kernel-owned, so a completion carrying
    /// this address can be recognised as ours without dereferencing it.
    pub(crate) fn leak(&self) -> *mut OVERLAPPED {
        let raw = Arc::into_raw(Arc::clone(&self.inner)) as *mut OVERLAPPED;
        live::insert(raw as usize);
        raw
    }

    /// Clone a type-erased reference that can cancel this operation.
    pub(crate) fn erased_cancel(&self) -> ErasedCancel {
        ErasedCancel {
            ptr: Arc::into_raw(Arc::clone(&self.inner)) as *const (),
            vtable: erased_cancel_vtable_of::<T>(),
        }
    }

    /// Reclaim a reference leaked by [`Key::leak`] for an operation that never
    /// started.
    ///
    /// # Safety
    ///
    /// `optr` must be the value returned by a matching [`Key::leak`], and the
    /// operation must not have completed.
    pub(crate) unsafe fn unleak(optr: *mut OVERLAPPED) {
        if live::claim(optr as usize) {
            drop(unsafe { Arc::from_raw(optr as *const RawOp<T>) });
        }
    }

    /// Run the operation's start routine.
    ///
    /// # Safety
    ///
    /// Must be called at most once, before the operation is in flight.
    pub(crate) unsafe fn operate(&self) -> std::task::Poll<Result<usize>> {
        let optr = self.overlapped_ptr();
        let mut op = self.inner.op.lock().unwrap();
        let op = op.as_mut().expect("an operation is started once");
        unsafe { op.operate(optr) }
    }

    /// Ask the operation to cancel itself.
    ///
    /// Safe to call concurrently with a completion: the operation lock keeps the
    /// two from aliasing, and cancelling an already-finished operation is a
    /// no-op at the Windows level.
    pub(crate) fn cancel(&self) -> Result<()> {
        let optr = self.overlapped_ptr();
        let mut guard = self.inner.op.lock().unwrap();
        match guard.as_mut() {
            Some(op) => unsafe { op.cancel(optr) },
            None => Ok(()),
        }
    }

    /// Read the operation's declared type.
    pub(crate) fn op_type(&self) -> Option<OpType> {
        self.inner.op.lock().unwrap().as_ref().map(|o| o.op_type())
    }

    /// The handle this operation targets, if it reports one.
    pub(crate) fn handle(&self) -> Option<windows::Win32::Foundation::HANDLE> {
        self.inner
            .op
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|o| o.handle())
    }

    /// Run the operation's completion hook for a result produced inline.
    ///
    /// The completion path does this itself; this is for the synchronous case,
    /// where no packet is delivered.
    pub(crate) fn on_complete_inline(&self, result: &Result<usize>) {
        let transferred = *result.as_ref().unwrap_or(&0);
        self.on_complete_inline_with(result, transferred);
    }

    /// As [`Key::on_complete_inline`], but carrying the platform's byte count
    /// alongside the status.
    pub(crate) fn on_complete_inline_with(&self, result: &Result<usize>, transferred: usize) {
        let mut op = self.inner.op.lock().unwrap();
        if let Some(op) = op.as_mut() {
            unsafe { op.on_complete_with(result, transferred) };
        }
    }

    /// Install the waker to be signalled on completion.
    pub(crate) fn set_waker(&self, waker: &Waker) {
        let mut slot = self.inner.waker.lock().unwrap();
        match slot.as_ref() {
            Some(existing) if existing.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
    }

    /// Take the completion result together with the operation state.
    ///
    /// The compare-exchange makes this exclusive, so the operation can be moved
    /// out without requiring sole ownership of the allocation — the completion
    /// path may still hold its reference at this moment.
    pub(crate) fn take_completion(&self) -> Option<(Result<usize>, T)> {
        self.inner
            .state
            .compare_exchange(
                OpState::Completed as u32,
                OpState::Terminal as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;

        // SAFETY: the CAS succeeded, so the state was `Completed`, which is only
        // published after the result has been written, and no other thread can
        // take it.
        let result = unsafe { (*self.inner.result.get()).assume_init_read() };
        let op = self
            .inner
            .op
            .lock()
            .unwrap()
            .take()
            .expect("the terminal transition happens once");
        Some((result, op))
    }

    /// Take the operation state for a result produced inline, where no
    /// completion is coming and no state transition is involved.
    pub(crate) fn take_op_inline(&self) -> T {
        self.inner
            .op
            .lock()
            .unwrap()
            .take()
            .expect("an inline result is taken once")
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
}

impl<T: OpCode> Clone for Key<T> {
    fn clone(&self) -> Self {
        Key {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
impl<T: OpCode> Key<T> {
    /// How many references exist. Test support only.
    fn strong_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }
}

/// Deliver a completion to the operation that owns `optr`.
///
/// Returns `false` if the packet is not ours — which is a routine occurrence,
/// not an error: the completion key is set per handle, so any overlapped call
/// the caller makes on a registered handle produces a packet carrying it.
///
/// Ownership is decided by looking the address up in the set of allocations the
/// kernel currently owns, **before** anything is dereferenced. A foreign
/// `OVERLAPPED` is therefore never read through, which matters because it may
/// sit at the end of a page.
///
/// # Safety
///
/// `optr` may be any pointer, including one this crate has never seen. It is
/// only dereferenced once membership has been established.
pub(crate) unsafe fn dispatch_completion(optr: *mut OVERLAPPED, result: Result<usize>) -> bool {
    // Without a separately reported count, the only count available is the one
    // in the result itself, which is zero for a failure.
    let transferred = *result.as_ref().unwrap_or(&0);
    unsafe { dispatch_completion_with(optr, result, transferred) }
}

/// As [`dispatch_completion`], but carrying the byte count the platform
/// reported alongside the status.
///
/// Backends use this so an operation can still learn how much was transferred
/// when the status is a failure — `ERROR_MORE_DATA` being the case that matters.
///
/// # Safety
///
/// `optr` may be any pointer, including one this crate has never seen. It is
/// only dereferenced once membership has been established.
pub(crate) unsafe fn dispatch_completion_with(
    optr: *mut OVERLAPPED,
    result: Result<usize>,
    transferred: usize,
) -> bool {
    if optr.is_null() {
        return false;
    }
    // Address-based ownership test. Claiming also guarantees exactly one
    // dispatcher acts on this allocation.
    if !live::claim(optr as usize) {
        return false;
    }

    // SAFETY: the address was registered by `Key::leak` and has not been
    // reclaimed, so it points at a live `RawOp` allocation and the header is in
    // bounds.
    let header = unsafe { read_header(optr) };
    if header.0 != OP_MAGIC {
        debug_assert!(false, "operation allocation header is corrupt");
        return false;
    }
    unsafe { (header.1.complete)(optr, result, transferred) };
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

/// Serialises tests that observe the process-global operation counter.
///
/// `live_operations` counts every operation in the process, and cargo runs tests
/// in parallel, so a test asserting on it must not observe another test's
/// allocations. Crucially this is **not** confined to this module: any test
/// anywhere in the crate that submits an operation bumps the same counter, so it
/// must take this lock too.
#[cfg(test)]
pub(crate) fn counter_guard() -> std::sync::MutexGuard<'static, ()> {
    static COUNTER_LOCK: Mutex<()> = Mutex::new(());
    COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::task::Poll;

    struct NoopOp {
        _buf: Vec<u8>,
    }

    unsafe impl OpCode for NoopOp {
        fn handle(&self) -> Option<windows::Win32::Foundation::HANDLE> {
            None
        }

        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            Poll::Pending
        }
    }

    struct BigOp {
        _pad: [u8; 512],
    }

    unsafe impl OpCode for BigOp {
        fn handle(&self) -> Option<windows::Win32::Foundation::HANDLE> {
            None
        }

        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            Poll::Pending
        }
    }

    /// Records that erased dispatch reached the right monomorphisation.
    static REACHED: AtomicBool = AtomicBool::new(false);
    static REACHED_LEN: AtomicU32 = AtomicU32::new(0);

    struct ObservingOp;

    unsafe impl OpCode for ObservingOp {
        fn handle(&self) -> Option<windows::Win32::Foundation::HANDLE> {
            None
        }

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

    use super::counter_guard;

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
        // Creating an operation bumps the process-global counter the
        // assertions in this module depend on.
        let _guard = counter_guard();
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

    /// Records the count handed to `on_complete_with`, including on failure.
    static COUNTING_TRANSFERRED: AtomicUsize = AtomicUsize::new(usize::MAX);

    struct CountingOp;

    unsafe impl OpCode for CountingOp {
        fn handle(&self) -> Option<windows::Win32::Foundation::HANDLE> {
            None
        }

        unsafe fn operate(&mut self, _optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
            Poll::Pending
        }

        unsafe fn on_complete_with(&mut self, _result: &Result<usize>, transferred: usize) {
            COUNTING_TRANSFERRED.store(transferred, Ordering::SeqCst);
        }
    }

    impl crate::iocp::op::IntoInner for CountingOp {
        type Inner = ();
        fn into_inner(self) {}
    }

    #[test]
    fn failure_completion_still_carries_its_byte_count() {
        // A named-pipe message that does not fit reports ERROR_MORE_DATA *and*
        // the bytes it delivered. Without this the count is unrecoverable, and
        // the caller cannot tell which bytes of their own buffer are valid.
        let _guard = counter_guard();
        COUNTING_TRANSFERRED.store(usize::MAX, Ordering::SeqCst);

        let key = Key::new(CountingOp);
        let optr = key.leak();
        let err = windows::core::Error::from_hresult(
            windows::Win32::Foundation::ERROR_MORE_DATA.to_hresult(),
        );
        assert!(unsafe { dispatch_completion_with(optr, Err(err), 10) });

        assert_eq!(
            COUNTING_TRANSFERRED.load(Ordering::SeqCst),
            10,
            "the count must survive a non-success status"
        );
        // The status itself is untouched: a truncated message is still an error
        // at this layer, and classifying it is the operation's job.
        assert!(key.take_completion().unwrap().0.is_err());
    }

    #[test]
    fn plain_dispatch_still_reports_the_success_count() {
        // The count-less entry point must keep behaving exactly as before, so
        // no existing operation observes a change.
        let _guard = counter_guard();
        COUNTING_TRANSFERRED.store(usize::MAX, Ordering::SeqCst);

        let key = Key::new(CountingOp);
        let optr = key.leak();
        assert!(unsafe { dispatch_completion(optr, Ok(7)) });
        assert_eq!(COUNTING_TRANSFERRED.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn erased_dispatch_reaches_the_right_type() {
        // Creating an operation bumps the process-global counter the
        // assertions in this module depend on.
        let _guard = counter_guard();
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
        assert_eq!(key.take_completion().unwrap().0.unwrap(), 4242);
    }

    #[test]
    fn leak_and_complete_frees_exactly_once() {
        let _guard = counter_guard();
        let before = live_operations();
        let key = Key::new(noop(3));
        assert_eq!(live_operations(), before + 1);

        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(3)) };

        assert_eq!(key.take_completion().unwrap().0.unwrap(), 3);
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
        // Creating an operation bumps the process-global counter the
        // assertions in this module depend on.
        let _guard = counter_guard();
        let key = Key::new(noop(0));
        let optr = key.leak();
        unsafe { dispatch_completion(optr, Ok(11)) };
        assert_eq!(key.take_completion().unwrap().0.unwrap(), 11);
        assert!(key.take_completion().is_none(), "the result is taken once");
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

        let op = key.take_op_inline();
        assert_eq!(op._buf.len(), 16);
        drop(key);
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
        let (_result, op) = key.take_completion().expect("a result was written");
        assert_eq!(op._buf.len(), 2);
        drop(key);
        assert_eq!(live_operations(), before);
    }

    #[test]
    fn completion_can_be_taken_while_the_allocation_is_still_shared() {
        // The completion path may still hold its reference when the future is
        // woken, so taking the result must not require sole ownership.
        let _guard = counter_guard();
        let key = Key::new(noop(1));
        let optr = key.leak();
        let shadow = key.clone();

        unsafe { dispatch_completion(optr, Ok(1)) };

        let (result, op) = key
            .take_completion()
            .expect("takeable even with another reference alive");
        assert_eq!(result.unwrap(), 1);
        assert_eq!(op._buf.len(), 1);
        drop(shadow);
    }

    #[test]
    fn waker_is_signalled_on_completion() {
        // Creating an operation bumps the process-global counter the
        // assertions in this module depend on.
        let _guard = counter_guard();
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
        // Creating an operation bumps the process-global counter the
        // assertions in this module depend on.
        let _guard = counter_guard();
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
        assert!(key.take_completion().is_none(), "its result is discarded");
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
    fn completion_releases_its_reference_before_waking() {
        // Creating an operation bumps the process-global counter the
        // assertions in this module depend on.
        let _guard = counter_guard();
        // The future's `finish` unwraps the allocation to hand back the
        // operation state. If the completion path still held a reference when
        // it woke, an executor polling inline would find the allocation shared
        // and be unable to unwrap it.
        let key = Key::new(noop(4));
        let optr = key.leak();
        assert_eq!(key.strong_count(), 2, "future + kernel");

        unsafe { dispatch_completion(optr, Ok(4)) };

        assert_eq!(
            key.strong_count(),
            1,
            "the completion path must release its reference before waking"
        );
        assert!(
            key.clone().take_completion().is_some(),
            "and the result must be available"
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
