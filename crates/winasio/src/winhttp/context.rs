// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The per-request completion context, and the WinHTTP status callback.
//!
//! This is the part of the module that has to be right. Everything else is a
//! wrapper.

use std::any::Any;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

use windows::core::{Error, HRESULT};
use windows::Win32::Networking::WinHttp::{
    WINHTTP_ASYNC_RESULT, WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE,
    WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING, WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE,
    WINHTTP_CALLBACK_STATUS_READ_COMPLETE, WINHTTP_CALLBACK_STATUS_REQUEST_ERROR,
    WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE, WINHTTP_CALLBACK_STATUS_WRITE_COMPLETE,
};

/// Number of live request contexts.
///
/// Incremented when a context is allocated and decremented when it is finally
/// freed. Tests use the *delta* across a block of work to prove a context was
/// reclaimed; an absolute value would be perturbed by tests running in
/// parallel.
static LIVE_CONTEXTS: AtomicUsize = AtomicUsize::new(0);

/// How many request contexts are currently allocated.
///
/// Exposed for the crate's own tests, which use it to prove that abandoning an
/// operation and dropping a request releases the state WinHTTP was holding.
/// Compare a reading taken before a block of work with one taken after; do not
/// depend on the absolute value.
#[doc(hidden)]
pub fn live_context_count() -> usize {
    LIVE_CONTEXTS.load(Ordering::SeqCst)
}

/// Which transfer an outstanding operation is.
///
/// Used to route an arriving completion to the slot ([`Side`]) holding the
/// operation it belongs to, and to check that the completion matches the
/// operation actually in flight on that side. At most one operation can be
/// outstanding **per side**; a mismatch means the state machine is wrong, and
/// it is better to find that in a test than to resolve the wrong future.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpKind {
    Send,
    Write,
    ReceiveResponse,
    QueryDataAvailable,
    Read,
}

/// Which of a request's two concurrent operation slots an operation belongs to.
///
/// WinHTTP permits **one outstanding write and one outstanding
/// receive/read at the same time** on a single request handle — that is the
/// whole mechanism behind HTTP/2 duplex (M6): the response head can be received
/// while the request body is still being written. To model that, the request's
/// completion state is split into two independent slots. The *write* slot
/// carries `Send` and `Write`; the *read* slot carries `ReceiveResponse`,
/// `QueryDataAvailable` and `Read`. Each slot has its own generation, its own
/// pending/abandoned/complete state and its own waker, so a completion on one
/// side never disturbs an operation in flight on the other.
///
/// For an ordinary HTTP/1.1 request nothing ever uses both slots at once — the
/// caller drives `Send → Write → Receive → Read` strictly in order, and each
/// slot is idle whenever the other is busy — so the split is invisible to the
/// existing single-operation path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// The request-body side: `Send` and `Write`.
    Write,
    /// The response side: `ReceiveResponse`, `QueryDataAvailable` and `Read`.
    Read,
}

impl OpKind {
    /// Which slot this operation completes against.
    pub(crate) fn side(self) -> Side {
        match self {
            OpKind::Send | OpKind::Write => Side::Write,
            OpKind::ReceiveResponse | OpKind::QueryDataAvailable | OpKind::Read => Side::Read,
        }
    }
}

/// The outcome of a completed operation.
pub(crate) enum Completion {
    /// Succeeded, transferring or reporting `len`.
    Done(u32),
    /// Failed.
    Failed(Error),
}

/// State of one of the request's operation slots.
///
/// # Invariant G
///
/// A slot is **never** overwritten by a new submission while it holds
/// `Pending` or `Abandoned`. Only the arrival of the matching terminal
/// notification returns it to `Idle`.
///
/// This invariant is what makes the generation counter sufficient. A completion
/// notification carries no generation of its own — WinHTTP hands back only the
/// context pointer, which is identical for every operation on the request. It
/// is only because at most one operation can be outstanding **per slot**, and
/// because an abandoned one is not replaced until its completion lands, that the
/// generation in the slot identifies the completion unambiguously. The
/// operation's [`OpKind`] additionally identifies *which* slot a completion
/// belongs to (see [`Side`]).
pub(crate) enum OpState {
    /// Nothing outstanding; a submission may proceed.
    Idle,
    /// An operation is in flight and its future is still alive.
    Pending { kind: OpKind, generation: u64 },
    /// An operation is in flight but its future was dropped. Its buffer is in
    /// `retired`. When its completion arrives the slot returns to `Idle` and
    /// the buffer is freed; until then no new operation may be submitted.
    Abandoned { kind: OpKind, generation: u64 },
    /// An operation finished and its result is waiting to be collected.
    Complete {
        generation: u64,
        outcome: Completion,
    },
}

/// One independently-drivable operation slot, with its own state and waker.
///
/// A request has two of these — see [`Side`] — so that a write and a
/// receive/read can be outstanding at the same time (HTTP/2 duplex, M6).
pub(crate) struct Slot {
    pub(crate) op: OpState,
    pub(crate) waker: Option<Waker>,
}

impl Slot {
    fn new() -> Self {
        Slot {
            op: OpState::Idle,
            waker: None,
        }
    }

    fn is_idle(&self) -> bool {
        matches!(self.op, OpState::Idle)
    }
}

pub(crate) struct Inner {
    /// Monotonic counter; a fresh value is taken for each submission and each
    /// abandonment. Shared across both slots so a generation is unique across
    /// the whole request, which keeps the `retired` bookkeeping unambiguous.
    pub(crate) next_generation: u64,
    /// The request-body slot: `Send` and `Write`.
    pub(crate) write: Slot,
    /// The response slot: `ReceiveResponse`, `QueryDataAvailable` and `Read`.
    pub(crate) read: Slot,
    /// Buffers belonging to abandoned operations, keyed by the generation that
    /// owned them. WinHTTP may still be writing into these, so they are held
    /// until the matching completion arrives — or until `HANDLE_CLOSING`, which
    /// is the backstop for a handle closed before the completion landed.
    pub(crate) retired: Vec<(u64, Box<dyn Any + Send>)>,
    /// Request bodies and header blocks handed to `WinHttpSendRequest`.
    ///
    /// WinHTTP documents `lpOptional` as having to remain valid until
    /// `WinHttpReceiveResponse` completes, not merely until
    /// `SENDREQUEST_COMPLETE` arrives — it may re-send the body without asking,
    /// for instance to follow a redirect or to answer an authentication
    /// challenge. Releasing at send-complete therefore leaves the platform
    /// reading freed memory on exactly the paths that are hardest to provoke in
    /// a test, so the body is held here for the longer of the two lifetimes.
    ///
    /// Cleared when a receive-response completes, and again at
    /// `HANDLE_CLOSING` for requests whose response was never received.
    pub(crate) send_retention: Vec<Box<dyn Any + Send>>,
}

/// State shared between the awaiting task and whichever thread runs the
/// callback.
pub(crate) struct RequestContext {
    pub(crate) inner: Mutex<Inner>,
}

impl Inner {
    /// The slot for a [`Side`].
    pub(crate) fn slot_mut(&mut self, side: Side) -> &mut Slot {
        match side {
            Side::Write => &mut self.write,
            Side::Read => &mut self.read,
        }
    }

    /// Whether a new operation may be submitted on `side`.
    ///
    /// `Complete` counts as busy: an uncollected result belongs to a future
    /// that has not been polled since its completion landed, and overwriting it
    /// would lose that future's answer.
    pub(crate) fn is_idle(&self, side: Side) -> bool {
        match side {
            Side::Write => self.write.is_idle(),
            Side::Read => self.read.is_idle(),
        }
    }

    /// Claim the appropriate slot for a new operation. Invariant G is upheld by
    /// the caller, which checks [`Inner::is_idle`] first under the same lock.
    pub(crate) fn begin(&mut self, kind: OpKind, generation: u64) {
        self.slot_mut(kind.side()).op = OpState::Pending { kind, generation };
    }

    /// Undo a claim whose WinHTTP call failed synchronously.
    ///
    /// Only if the slot is still ours *and* still pending. A synchronous
    /// failure can race an inline completion for the very same operation;
    /// clobbering a `Complete` here would strand the future forever.
    pub(crate) fn rollback(&mut self, side: Side, generation: u64) {
        let slot = self.slot_mut(side);
        if let OpState::Pending { generation: g, .. } = slot.op {
            if g == generation {
                slot.op = OpState::Idle;
            }
        }
    }

    /// Collect the outcome of `generation` on `side`, if it has landed.
    pub(crate) fn take_completion(&mut self, side: Side, generation: u64) -> Option<Completion> {
        let slot = self.slot_mut(side);
        match &slot.op {
            OpState::Complete { generation: g, .. } if *g == generation => {
                match std::mem::replace(&mut slot.op, OpState::Idle) {
                    OpState::Complete { outcome, .. } => Some(outcome),
                    // Unreachable: just matched. Restore and report nothing
                    // rather than panicking, because this type is reachable
                    // from the callback.
                    other => {
                        slot.op = other;
                        None
                    }
                }
            }
            _ => None,
        }
    }

    /// A future for `generation` on `side` is being dropped.
    pub(crate) fn abandon(
        &mut self,
        side: Side,
        generation: u64,
        buffer: Option<Box<dyn Any + Send>>,
    ) {
        // Decide the transition against the slot, then touch `retired` only
        // after the slot borrow has ended (they are different fields of the
        // same `Inner`, so the borrow checker will not let both live at once).
        let retire = {
            let slot = self.slot_mut(side);
            match &slot.op {
                OpState::Pending {
                    kind,
                    generation: g,
                } if *g == generation => {
                    // Still in flight. WinHTTP owns the buffer until its
                    // completion arrives, so the buffer is retired rather than
                    // freed, and the slot stays occupied so that no later
                    // operation can be confused with this one.
                    let kind = *kind;
                    slot.op = OpState::Abandoned { kind, generation };
                    true
                }
                OpState::Complete { generation: g, .. } if *g == generation => {
                    // Already finished; nothing is holding the buffer. Discard
                    // the uncollected result and reopen the slot.
                    slot.op = OpState::Idle;
                    false
                }
                // Never submitted, or belongs to someone else. The buffer, if
                // any, is dropped normally by the caller.
                _ => false,
            }
        };
        if retire {
            if let Some(buffer) = buffer {
                self.retired.push((generation, buffer));
            }
        }
    }
}

impl RequestContext {
    pub(crate) fn new() -> Arc<Self> {
        LIVE_CONTEXTS.fetch_add(1, Ordering::SeqCst);
        Arc::new(RequestContext {
            inner: Mutex::new(Inner {
                next_generation: 1,
                write: Slot::new(),
                read: Slot::new(),
                retired: Vec::new(),
                send_retention: Vec::new(),
            }),
        })
    }

    /// Lock the shared state, recovering from a poisoned mutex.
    ///
    /// A poisoned mutex means some other thread panicked while holding it.
    /// Panicking again here is not an option: this is called from the callback,
    /// and a panic there would unwind into `extern "system"` and abort. The
    /// state is a plain state machine with no invariant a partial update could
    /// break in a way that matters, so recovering is strictly better than
    /// aborting.
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Drop for RequestContext {
    fn drop(&mut self) {
        LIVE_CONTEXTS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Record a completion against the operation slot, and return the waker to be
/// woken *after* the lock is released.
///
/// Returns `None` — doing nothing at all — when the completion does not match
/// what is outstanding. That is the right response to a notification the state
/// machine did not expect: dropping it on the floor cannot corrupt anything,
/// whereas guessing could resolve the wrong future.
/// Record a completion against the slot the operation belongs to, and return
/// the waker to be woken *after* the lock is released.
///
/// Returns `None` — doing nothing at all — when the completion does not match
/// what is outstanding on that slot. That is the right response to a
/// notification the state machine did not expect: dropping it on the floor
/// cannot corrupt anything, whereas guessing could resolve the wrong future.
///
/// `expected` names the exact operation, which also selects the slot
/// ([`OpKind::side`]). Terminal *errors* (`REQUEST_ERROR`) do not carry a kind
/// and are handled by [`record_error`] instead.
fn record(context: &RequestContext, expected: OpKind, outcome: Completion) -> Option<Waker> {
    let mut inner = context.lock();
    // A completed receive-response is the point at which WinHTTP is documented
    // to be finished with the request body it was given at send time. Freeing
    // here rather than at send-complete is the whole reason `send_retention`
    // exists; freeing is deferred to the end of the borrow so that no user
    // destructor runs while the lock is held.
    let release = if expected == OpKind::ReceiveResponse {
        std::mem::take(&mut inner.send_retention)
    } else {
        Vec::new()
    };
    let side = expected.side();
    // The generation of an abandoned operation whose retired buffer can now be
    // freed — decided under the slot borrow, acted on once it has ended.
    let mut free_generation: Option<u64> = None;
    let waker = {
        let slot = inner.slot_mut(side);
        match slot.op {
            OpState::Pending { kind, generation } if kind == expected => {
                slot.op = OpState::Complete {
                    generation,
                    outcome,
                };
                slot.waker.take()
            }
            OpState::Abandoned { kind, generation } if kind == expected => {
                // The future that started this is gone. Discard the status, the
                // length and the error: attributing any of them to a later
                // operation is precisely the defect this design exists to
                // prevent.
                //
                // WinHTTP has now delivered the terminal notification for this
                // operation, so it is provably finished with the buffer and the
                // buffer can be freed here rather than being held until the
                // handle closes.
                slot.op = OpState::Idle;
                free_generation = Some(generation);
                // Wake anything waiting: a later submission may be parked behind
                // `OperationInProgress`.
                slot.waker.take()
            }
            _ => None,
        }
    };
    if let Some(generation) = free_generation {
        inner.retired.retain(|(g, _)| *g != generation);
    }
    drop(inner);
    // Outside the lock: dropping these runs caller-supplied destructors, and a
    // destructor that re-entered this context would deadlock against a lock
    // still held here.
    drop(release);
    waker
}

/// Fault every slot that has an operation in flight, and return their wakers.
///
/// `REQUEST_ERROR` carries no [`OpKind`], and with two slots active either or
/// both of a write and a receive/read could be the operation that failed. A
/// request error is terminal for the whole handle, though — WinHTTP will not
/// let a surviving operation make progress once one has faulted — so the
/// correct and simplest response is to fail *both* slots with the same error
/// and wake both futures. Waking only one would leave the other parked forever
/// (its completion is never coming), which is exactly the hang this avoids.
///
/// A slot that is `Idle` or already `Complete` is left untouched: there is no
/// live future there to fault.
fn record_error(context: &RequestContext, error: Error) -> [Option<Waker>; 2] {
    let mut inner = context.lock();
    // A terminal error ends the request, so the body WinHTTP was reading is no
    // longer needed; release it here as a receive-response would.
    let release = std::mem::take(&mut inner.send_retention);
    let mut free: Vec<u64> = Vec::new();
    let mut wakers = [None, None];
    for (index, side) in [Side::Write, Side::Read].into_iter().enumerate() {
        let slot = inner.slot_mut(side);
        wakers[index] = match slot.op {
            OpState::Pending { generation, .. } => {
                slot.op = OpState::Complete {
                    generation,
                    outcome: Completion::Failed(error.clone()),
                };
                slot.waker.take()
            }
            OpState::Abandoned { generation, .. } => {
                slot.op = OpState::Idle;
                free.push(generation);
                slot.waker.take()
            }
            _ => None,
        };
    }
    if !free.is_empty() {
        inner.retired.retain(|(g, _)| !free.contains(g));
    }
    drop(inner);
    drop(release);
    wakers
}

/// The WinHTTP status callback.
///
/// # Obligations this function meets
///
/// * **It cannot unwind.** There is no `panic!`, `unwrap`, `expect`, `assert`,
///   indexing or slicing anywhere in it or in anything it calls. An unwind
///   reaching this `extern "system"` boundary is a guaranteed process abort,
///   raised from a thread the caller does not own and cannot guard.
/// * **It ignores what it does not recognise.** The catch-all arm returns. The
///   previous implementation panicked there.
/// * **It never holds the lock across a call into WinHTTP**, and it never calls
///   into WinHTTP at all. It also releases the lock before waking, because
///   `Waker::wake` runs arbitrary executor code.
/// * **It is not `#[no_mangle]`.** The previous implementation exported a
///   symbol called `AsyncCallback` from every binary that linked this crate.
///
/// # Safety
///
/// Registered with WinHTTP, which is the only caller. `context` is either zero
/// or the value installed with `WINHTTP_OPTION_CONTEXT_VALUE`, which is a
/// pointer produced by `Arc::into_raw` on a `RequestContext`.
pub(crate) unsafe extern "system" fn status_callback(
    handle: *mut c_void,
    context: usize,
    status: u32,
    information: *mut c_void,
    information_length: u32,
) {
    // The body is entirely panic-free by construction, but "entirely" spans
    // two destructors that run caller-supplied code: dropping a retired buffer
    // runs `B`'s `Drop`, and `waker.wake()` runs the executor. Neither is this
    // module's to audit, and an unwind out of an `extern "system"` function is
    // undefined behaviour, so the boundary is sealed here rather than argued
    // about. A panic is swallowed: there is no thread to propagate it to and
    // aborting the user's process is a worse outcome than a lost notification.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: forwarded unchanged from this function's own contract.
        unsafe { dispatch_callback(handle, context, status, information, information_length) }
    }));
    drop(outcome);
}

/// The body of [`status_callback`], separated so the unwind guard above wraps
/// every path through it including its destructors.
///
/// # Safety
///
/// See [`status_callback`].
unsafe fn dispatch_callback(
    _handle: *mut c_void,
    context: usize,
    status: u32,
    information: *mut c_void,
    information_length: u32,
) {
    if context == 0 {
        // A handle whose context was never installed — for example the session
        // or connection handle, which inherit the callback. Nothing to do.
        return;
    }
    let raw = context as *const RequestContext;

    // Borrow WinHTTP's reference without consuming it, then take our own
    // strong reference for the duration of this invocation.
    //
    // The clone is defence in depth rather than a fix for a demonstrated race.
    // A probe deliberately parked a callback for two seconds on a pool thread
    // and closed the handle 300 ms into that window: `WinHttpCloseHandle`
    // returned in 0.3 ms, and `HANDLE_CLOSING` was not delivered until the
    // parked callback returned — on that same thread. WinHTTP appears to
    // serialise callbacks per handle and to drain in-flight invocations before
    // delivering `HANDLE_CLOSING`, so the interleaving this clone guards
    // against was not reproducible.
    //
    // It is kept anyway because that serialisation is an observation, not a
    // documented guarantee, and the cost is one uncontended atomic against a
    // use-after-free in a callback that cannot be made to fail safe.
    //
    // SAFETY: `raw` came from `Arc::into_raw` and WinHTTP's reference is still
    // live at entry — it is only released in the `HANDLE_CLOSING` arm below,
    // and that arm keeps `context` alive through its own clone.
    let borrowed = ManuallyDrop::new(unsafe { Arc::from_raw(raw) });
    let context: Arc<RequestContext> = ManuallyDrop::into_inner(borrowed.clone());

    let wakers: Vec<Waker> = match status {
        WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING => {
            // The one and only place WinHTTP's reference is released. This
            // notification is delivered exactly once per handle, is the last
            // one that handle produces, and arrives even for a request on
            // which no operation ever ran — which is why the context can
            // safely be installed at request-open time.
            //
            // SAFETY: this arm runs at most once for this handle, and the
            // reference it consumes was created by `Arc::into_raw` when the
            // request was opened.
            drop(unsafe { Arc::from_raw(raw) });

            // Backstop: free any buffer still held for an operation whose
            // completion never arrived because the handle was closed first.
            // WinHTTP is finished with the handle by now, so nothing can still
            // be writing into them.
            let mut inner = context.lock();
            inner.retired.clear();
            // Likewise for a request body whose response was never received:
            // the handle is gone, so WinHTTP cannot re-send it.
            let release = std::mem::take(&mut inner.send_retention);
            // Wake whatever is parked on *either* slot: the handle is gone, so
            // both a stalled writer and a stalled reader must be woken to learn
            // their operations will never complete.
            let mut wakers = Vec::new();
            wakers.extend(inner.write.waker.take());
            wakers.extend(inner.read.waker.take());
            drop(inner);
            drop(release);
            wakers
        }

        WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE => {
            record(&context, OpKind::Send, Completion::Done(0))
                .into_iter()
                .collect()
        }

        WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE => {
            record(&context, OpKind::ReceiveResponse, Completion::Done(0))
                .into_iter()
                .collect()
        }

        WINHTTP_CALLBACK_STATUS_WRITE_COMPLETE => {
            // `lpvStatusInformation` points at a DWORD holding the byte count.
            let written = read_u32(information, information_length);
            record(&context, OpKind::Write, Completion::Done(written))
                .into_iter()
                .collect()
        }

        WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE => {
            // Likewise a DWORD, holding the number of bytes available.
            let available = read_u32(information, information_length);
            record(
                &context,
                OpKind::QueryDataAvailable,
                Completion::Done(available),
            )
            .into_iter()
            .collect()
        }

        WINHTTP_CALLBACK_STATUS_READ_COMPLETE => {
            // Here the *length* parameter carries the byte count, and
            // `lpvStatusInformation` points at the caller's own buffer. This
            // asymmetry with WRITE_COMPLETE is WinHTTP's, not ours.
            record(&context, OpKind::Read, Completion::Done(information_length))
                .into_iter()
                .collect()
        }

        WINHTTP_CALLBACK_STATUS_REQUEST_ERROR => {
            let error = read_async_error(information, information_length);
            // No expected kind: a terminal error can fault either or both of an
            // outstanding write and receive/read, so both slots are failed and
            // both wakers collected. See [`record_error`].
            record_error(&context, error)
                .into_iter()
                .flatten()
                .collect()
        }

        // Progress notifications — resolving, connecting, sending, secure
        // failure, handle created, redirects — and anything a future version of
        // WinHTTP invents. Ignored, deliberately and silently. The previous
        // implementation panicked here, from a thread it did not own.
        _ => Vec::new(),
    };

    // Outside the lock: `wake` may run arbitrary executor code, including code
    // that immediately polls the future and takes this same lock.
    for waker in wakers {
        waker.wake();
    }
}

/// Read a `DWORD` out of a status-information pointer, defensively.
fn read_u32(information: *const c_void, information_length: u32) -> u32 {
    if information.is_null() || (information_length as usize) < size_of::<u32>() {
        return 0;
    }
    // SAFETY: non-null, and WinHTTP reports at least four bytes at that
    // address. Read unaligned because nothing documents the alignment.
    unsafe { information.cast::<u32>().read_unaligned() }
}

/// Read the failure out of a `REQUEST_ERROR` notification.
///
/// Two defects of the previous implementation are fixed here, and both were
/// confirmed by measurement rather than reasoning.
///
/// 1. `lpvStatusInformation` is a pointer **to** a `WINHTTP_ASYNC_RESULT`. The
///    old code cast it to `*mut &WINHTTP_ASYNC_RESULT` and dereferenced twice,
///    so it read the structure's first field — `dwResult`, a small integer such
///    as 5 — and used *that* as an address.
/// 2. `dwError` is a Win32 code and needs `HRESULT::from_win32`. The old code
///    built `HRESULT(dwError as i32)`, which for 12002 is `0x00002EE2`: sign
///    bit clear, therefore a **success** HRESULT. Downstream that tripped an
///    `assert!(is_err())` and aborted the process on every async failure.
fn read_async_error(information: *const c_void, information_length: u32) -> Error {
    if information.is_null() || (information_length as usize) < size_of::<WINHTTP_ASYNC_RESULT>() {
        // Nothing usable arrived. Report a failure rather than inventing a
        // success: a caller must never see a transport failure as an empty
        // successful body.
        return Error::from_hresult(HRESULT::from_win32(
            windows::Win32::Networking::WinHttp::ERROR_WINHTTP_CONNECTION_ERROR,
        ));
    }
    // SAFETY: non-null, and WinHTTP reports at least `size_of` bytes there.
    let result = unsafe { information.cast::<WINHTTP_ASYNC_RESULT>().read_unaligned() };
    Error::from_hresult(HRESULT::from_win32(result.dwError))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_context_is_counted_while_it_lives() {
        let before = live_context_count();
        let context = RequestContext::new();
        assert_eq!(live_context_count(), before + 1);
        drop(context);
        assert_eq!(live_context_count(), before);
    }

    #[test]
    fn a_completion_for_the_wrong_operation_is_ignored() {
        // A mismatch means the state machine is wrong. The callback must not
        // guess: resolving the wrong future is worse than hanging, because a
        // hang shows up in a test and a wrong answer does not.
        let context = RequestContext::new();
        context.lock().read.op = OpState::Pending {
            kind: OpKind::Read,
            generation: 7,
        };
        // A write completion routes to the write slot, which is idle, so the
        // read slot is untouched.
        let waker = record(&context, OpKind::Write, Completion::Done(99));
        assert!(waker.is_none());
        assert!(matches!(
            context.lock().read.op,
            OpState::Pending {
                kind: OpKind::Read,
                generation: 7
            }
        ));
    }

    #[test]
    fn a_wrong_kind_on_the_same_slot_is_ignored() {
        // Two kinds share the read slot; a completion for the wrong one must
        // still be ignored rather than resolve the operation in flight.
        let context = RequestContext::new();
        context.lock().read.op = OpState::Pending {
            kind: OpKind::Read,
            generation: 7,
        };
        let waker = record(&context, OpKind::ReceiveResponse, Completion::Done(0));
        assert!(waker.is_none());
        assert!(matches!(
            context.lock().read.op,
            OpState::Pending {
                kind: OpKind::Read,
                generation: 7
            }
        ));
    }

    #[test]
    fn an_abandoned_operations_completion_frees_its_buffer_and_unblocks_the_slot() {
        // The interleaving that the previous implementation got wrong: a late
        // completion must not signal the *next* operation, and the slot must
        // reopen only when that late completion actually lands.
        let context = RequestContext::new();
        {
            let mut inner = context.lock();
            inner.read.op = OpState::Abandoned {
                kind: OpKind::Read,
                generation: 4,
            };
            inner.retired.push((4, Box::new(vec![0u8; 16])));
            inner.retired.push((5, Box::new(vec![0u8; 16])));
        }

        record(&context, OpKind::Read, Completion::Done(16));

        let inner = context.lock();
        assert!(matches!(inner.read.op, OpState::Idle));
        assert_eq!(inner.retired.len(), 1, "only generation 4 should be freed");
        assert_eq!(inner.retired[0].0, 5);
    }

    #[test]
    fn a_write_and_a_receive_may_be_outstanding_at_once_and_complete_in_either_order() {
        // The whole point of the two-slot design (HTTP/2 duplex, M6): a write
        // on the request body and a receive of the response head are in flight
        // together, and their completions arrive independently — here, the
        // receive completes first, then the write — without either disturbing
        // the other.
        let context = RequestContext::new();
        {
            let mut inner = context.lock();
            inner.write.op = OpState::Pending {
                kind: OpKind::Write,
                generation: 1,
            };
            inner.read.op = OpState::Pending {
                kind: OpKind::ReceiveResponse,
                generation: 2,
            };
        }

        // Receive completes first.
        record(&context, OpKind::ReceiveResponse, Completion::Done(0));
        {
            let inner = context.lock();
            assert!(
                matches!(inner.read.op, OpState::Complete { generation: 2, .. }),
                "the receive should be complete"
            );
            assert!(
                matches!(inner.write.op, OpState::Pending { generation: 1, .. }),
                "the write must be untouched by the receive completing"
            );
        }

        // Then the write completes.
        record(&context, OpKind::Write, Completion::Done(42));
        let inner = context.lock();
        assert!(matches!(
            inner.write.op,
            OpState::Complete {
                generation: 1,
                outcome: Completion::Done(42)
            }
        ));
        assert!(matches!(
            inner.read.op,
            OpState::Complete { generation: 2, .. }
        ));
    }

    #[test]
    fn a_request_error_faults_both_slots_at_once() {
        // A terminal error can fault either or both of an outstanding write and
        // receive/read; both are failed so neither future is left parked on a
        // completion that will never arrive.
        let context = RequestContext::new();
        {
            let mut inner = context.lock();
            inner.write.op = OpState::Pending {
                kind: OpKind::Write,
                generation: 1,
            };
            inner.read.op = OpState::Pending {
                kind: OpKind::Read,
                generation: 2,
            };
        }
        record_error(&context, Error::from_hresult(HRESULT::from_win32(12017)));
        let inner = context.lock();
        assert!(
            matches!(inner.write.op, OpState::Complete { .. }),
            "the write slot should be failed"
        );
        assert!(
            matches!(inner.read.op, OpState::Complete { .. }),
            "the read slot should be failed"
        );
    }

    #[test]
    fn a_request_error_faults_whichever_single_slot_is_outstanding() {
        // Only one operation is outstanding: the error faults that slot and
        // leaves the idle slot alone.
        for kind in [
            OpKind::Send,
            OpKind::Write,
            OpKind::ReceiveResponse,
            OpKind::QueryDataAvailable,
            OpKind::Read,
        ] {
            let context = RequestContext::new();
            context.lock().slot_mut(kind.side()).op = OpState::Pending {
                kind,
                generation: 1,
            };
            record_error(&context, Error::from_hresult(HRESULT::from_win32(12017)));
            assert!(
                matches!(
                    context.lock().slot_mut(kind.side()).op,
                    OpState::Complete { .. }
                ),
                "{kind:?} should have been completed"
            );
        }
    }

    #[test]
    fn a_truncated_request_error_is_still_a_failure() {
        // Never classify a failure as a success. If the notification does not
        // carry a usable result there is still no response, and reporting one
        // would hand the caller a silently empty body.
        let error = read_async_error(std::ptr::null(), 0);
        assert!(error.code().is_err());
    }
}
