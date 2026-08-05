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
/// Used to check that an arriving completion matches the operation actually in
/// flight. Only one transfer can be outstanding on a request at a time, so this
/// is a consistency check rather than a demultiplexer — but a mismatch means
/// the state machine is wrong, and it is better to find that in a test than to
/// resolve the wrong future.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpKind {
    Send,
    Write,
    ReceiveResponse,
    QueryDataAvailable,
    Read,
}

/// The outcome of a completed operation.
pub(crate) enum Completion {
    /// Succeeded, transferring or reporting `len`.
    Done(u32),
    /// Failed.
    Failed(Error),
}

/// State of the request's single operation slot.
///
/// # Invariant G
///
/// The slot is **never** overwritten by a new submission while it holds
/// `Pending` or `Abandoned`. Only the arrival of the matching terminal
/// notification returns it to `Idle`.
///
/// This invariant is what makes the generation counter sufficient. A completion
/// notification carries no generation of its own — WinHTTP hands back only the
/// context pointer, which is identical for every operation on the request. It
/// is only because at most one operation can be outstanding, and because an
/// abandoned one is not replaced until its completion lands, that the
/// generation in the slot identifies the completion unambiguously.
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

pub(crate) struct Inner {
    /// Monotonic counter; a fresh value is taken for each submission and each
    /// abandonment.
    pub(crate) next_generation: u64,
    pub(crate) op: OpState,
    pub(crate) waker: Option<Waker>,
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
    /// Whether a new operation may be submitted.
    ///
    /// `Complete` counts as busy: an uncollected result belongs to a future
    /// that has not been polled since its completion landed, and overwriting it
    /// would lose that future's answer.
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self.op, OpState::Idle)
    }

    /// Claim the slot for a new operation. Invariant G is upheld by the caller,
    /// which checks [`Inner::is_idle`] first under the same lock.
    pub(crate) fn begin(&mut self, kind: OpKind, generation: u64) {
        self.op = OpState::Pending { kind, generation };
    }

    /// Undo a claim whose WinHTTP call failed synchronously.
    ///
    /// Only if the slot is still ours *and* still pending. A synchronous
    /// failure can race an inline completion for the very same operation;
    /// clobbering a `Complete` here would strand the future forever.
    pub(crate) fn rollback(&mut self, generation: u64) {
        if let OpState::Pending { generation: g, .. } = self.op {
            if g == generation {
                self.op = OpState::Idle;
            }
        }
    }

    /// Collect the outcome of `generation`, if it has landed.
    pub(crate) fn take_completion(&mut self, generation: u64) -> Option<Completion> {
        match &self.op {
            OpState::Complete { generation: g, .. } if *g == generation => {
                match std::mem::replace(&mut self.op, OpState::Idle) {
                    OpState::Complete { outcome, .. } => Some(outcome),
                    // Unreachable: just matched. Restore and report nothing
                    // rather than panicking, because this type is reachable
                    // from the callback.
                    other => {
                        self.op = other;
                        None
                    }
                }
            }
            _ => None,
        }
    }

    /// A future for `generation` is being dropped.
    pub(crate) fn abandon(&mut self, generation: u64, buffer: Option<Box<dyn Any + Send>>) {
        match &self.op {
            OpState::Pending {
                kind,
                generation: g,
            } if *g == generation => {
                // Still in flight. WinHTTP owns the buffer until its completion
                // arrives, so the buffer is retired rather than freed, and the
                // slot stays occupied so that no later operation can be
                // confused with this one.
                let kind = *kind;
                self.op = OpState::Abandoned { kind, generation };
                if let Some(buffer) = buffer {
                    self.retired.push((generation, buffer));
                }
            }
            OpState::Complete { generation: g, .. } if *g == generation => {
                // Already finished; nothing is holding the buffer. Discard the
                // uncollected result and reopen the slot.
                self.op = OpState::Idle;
            }
            // Never submitted, or belongs to someone else. The buffer, if any,
            // is dropped normally by the caller.
            _ => {}
        }
    }
}

impl RequestContext {
    pub(crate) fn new() -> Arc<Self> {
        LIVE_CONTEXTS.fetch_add(1, Ordering::SeqCst);
        Arc::new(RequestContext {
            inner: Mutex::new(Inner {
                next_generation: 1,
                op: OpState::Idle,
                waker: None,
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
fn record(
    context: &RequestContext,
    expected: Option<OpKind>,
    outcome: Completion,
) -> Option<Waker> {
    let mut inner = context.lock();
    // A completed receive-response is the point at which WinHTTP is documented
    // to be finished with the request body it was given at send time. Freeing
    // here rather than at send-complete is the whole reason `send_retention`
    // exists; freeing is deferred to the end of the borrow so that no user
    // destructor runs while the lock is held.
    let release = if expected == Some(OpKind::ReceiveResponse) {
        std::mem::take(&mut inner.send_retention)
    } else {
        Vec::new()
    };
    let waker = match inner.op {
        OpState::Pending { kind, generation } if expected.is_none_or(|k| k == kind) => {
            inner.op = OpState::Complete {
                generation,
                outcome,
            };
            inner.waker.take()
        }
        OpState::Abandoned { kind, generation } if expected.is_none_or(|k| k == kind) => {
            // The future that started this is gone. Discard the status, the
            // length and the error: attributing any of them to a later
            // operation is precisely the defect this design exists to prevent.
            //
            // WinHTTP has now delivered the terminal notification for this
            // operation, so it is provably finished with the buffer and the
            // buffer can be freed here rather than being held until the handle
            // closes.
            inner.op = OpState::Idle;
            inner.retired.retain(|(g, _)| *g != generation);
            // Wake anything waiting: a later submission may be parked behind
            // `OperationInProgress`.
            inner.waker.take()
        }
        _ => None,
    };
    drop(inner);
    // Outside the lock: dropping these runs caller-supplied destructors, and a
    // destructor that re-entered this context would deadlock against a lock
    // still held here.
    drop(release);
    waker
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

    let waker = match status {
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
            let waker = inner.waker.take();
            drop(inner);
            drop(release);
            waker
        }

        WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE => {
            record(&context, Some(OpKind::Send), Completion::Done(0))
        }

        WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE => {
            record(&context, Some(OpKind::ReceiveResponse), Completion::Done(0))
        }

        WINHTTP_CALLBACK_STATUS_WRITE_COMPLETE => {
            // `lpvStatusInformation` points at a DWORD holding the byte count.
            let written = read_u32(information, information_length);
            record(&context, Some(OpKind::Write), Completion::Done(written))
        }

        WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE => {
            // Likewise a DWORD, holding the number of bytes available.
            let available = read_u32(information, information_length);
            record(
                &context,
                Some(OpKind::QueryDataAvailable),
                Completion::Done(available),
            )
        }

        WINHTTP_CALLBACK_STATUS_READ_COMPLETE => {
            // Here the *length* parameter carries the byte count, and
            // `lpvStatusInformation` points at the caller's own buffer. This
            // asymmetry with WRITE_COMPLETE is WinHTTP's, not ours.
            record(
                &context,
                Some(OpKind::Read),
                Completion::Done(information_length),
            )
        }

        WINHTTP_CALLBACK_STATUS_REQUEST_ERROR => {
            let error = read_async_error(information, information_length);
            // No expected kind: any outstanding operation can fail this way,
            // and only one can be outstanding.
            record(&context, None, Completion::Failed(error))
        }

        // Progress notifications — resolving, connecting, sending, secure
        // failure, handle created, redirects — and anything a future version of
        // WinHTTP invents. Ignored, deliberately and silently. The previous
        // implementation panicked here, from a thread it did not own.
        _ => None,
    };

    // Outside the lock: `wake` may run arbitrary executor code, including code
    // that immediately polls the future and takes this same lock.
    if let Some(waker) = waker {
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
            super::consts::ERROR_WINHTTP_CONNECTION_ERROR,
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
        context.lock().op = OpState::Pending {
            kind: OpKind::Read,
            generation: 7,
        };
        let waker = record(&context, Some(OpKind::Write), Completion::Done(99));
        assert!(waker.is_none());
        assert!(matches!(
            context.lock().op,
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
            inner.op = OpState::Abandoned {
                kind: OpKind::Read,
                generation: 4,
            };
            inner.retired.push((4, Box::new(vec![0u8; 16])));
            inner.retired.push((5, Box::new(vec![0u8; 16])));
        }

        record(&context, Some(OpKind::Read), Completion::Done(16));

        let inner = context.lock();
        assert!(matches!(inner.op, OpState::Idle));
        assert_eq!(inner.retired.len(), 1, "only generation 4 should be freed");
        assert_eq!(inner.retired[0].0, 5);
    }

    #[test]
    fn a_request_error_matches_whatever_is_outstanding() {
        // Any operation can fail this way and only one can be outstanding, so
        // the error arm deliberately does not check the kind.
        for kind in [
            OpKind::Send,
            OpKind::Write,
            OpKind::ReceiveResponse,
            OpKind::QueryDataAvailable,
            OpKind::Read,
        ] {
            let context = RequestContext::new();
            context.lock().op = OpState::Pending {
                kind,
                generation: 1,
            };
            record(
                &context,
                None,
                Completion::Failed(Error::from_hresult(HRESULT::from_win32(12017))),
            );
            assert!(
                matches!(context.lock().op, OpState::Complete { .. }),
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
