// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The operation abstraction.
//!
//! An [`OpCode`] describes one asynchronous Windows operation: how to start it,
//! how to cancel it, and what state it owns for the duration. Implementing it is
//! the only thing required to make an arbitrary Windows overlapped API awaitable
//! — no change to this crate is needed.

use std::task::Poll;

use windows::core::{Error, Result};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::OVERLAPPED;

/// How an operation's completion is signalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpType {
    /// A true overlapped operation. `operate` passes the `OVERLAPPED` pointer to
    /// a Windows API which completes through an I/O completion port or a
    /// thread-pool I/O callback.
    Overlapped,
    /// A wait on a signalable handle, driven by the Win32 wait infrastructure
    /// rather than by an I/O completion.
    Event(HANDLE),
}

/// One asynchronous Windows operation.
///
/// # Ownership
///
/// The operation owns every buffer and structure the kernel touches. Ownership
/// transfers to the driver on submission and comes back to the caller on
/// completion. This is not a stylistic choice: with completion-based I/O the
/// kernel retains the pointers until the operation finishes, which a borrowed
/// slice cannot express soundly.
///
/// # Safety
///
/// Implementors must uphold all of the following.
///
/// * `operate` must derive every pointer it hands to Windows from `&mut self`.
///   The operation already lives at a stable heap address; pointers into locals,
///   or into values that may later move, are undefined behaviour.
/// * `operate` must actually initiate the operation using the `optr` it is given,
///   and must not retain `optr` beyond the call.
/// * If `operate` can return `Poll::Ready(Ok(_))` — completing inline — and the
///   handle it used does not have `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` set,
///   then [`OpCode::handle`] must return that handle. Otherwise the driver will
///   release the operation while Windows is still holding a pointer to it.
/// * `cancel` must request cancellation of the operation started by `operate`
///   (normally `CancelIoEx`) and must not free anything.
/// * The implementor must not assume `cancel` prevents completion. Cancellation
///   is asynchronous; a completion still arrives and is the point at which the
///   operation's state is released.
/// * If `op_type` returns [`OpType::Event`], the handle must remain valid until
///   the operation reaches a terminal state.
pub unsafe trait OpCode: 'static {
    /// How this operation completes. Defaults to a true overlapped operation.
    fn op_type(&self) -> OpType {
        OpType::Overlapped
    }

    /// Which handle this operation runs on, if any.
    ///
    /// **This has no default, deliberately.** The driver uses it to decide,
    /// after [`OpCode::operate`] returns `Poll::Ready(Ok(_))`, whether a
    /// completion packet is *also* coming — which depends on whether the handle
    /// skips the completion port on success.
    ///
    /// Getting it wrong is a use-after-free, so every implementor is required
    /// to state the answer rather than inherit one:
    ///
    /// * `Some(handle)` — the operation runs on this handle. Always correct.
    /// * `None` — the operation cannot complete inline, so the question does
    ///   not arise. Only sound if [`OpCode::operate`] never returns
    ///   `Poll::Ready(Ok(_))`.
    ///
    /// When in doubt, return `Some`.
    fn handle(&self) -> Option<HANDLE>;

    /// Start the operation.
    ///
    /// Returns [`Poll::Pending`] if it started asynchronously and a completion
    /// will arrive, or [`Poll::Ready`] if it finished inline — either
    /// successfully with a transferred count, or with an error.
    ///
    /// # Safety
    ///
    /// `optr` must point at the `OVERLAPPED` embedded in this operation's own
    /// allocation. The caller guarantees this; implementors simply pass it
    /// through to the Windows API.
    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>>;

    /// Request cancellation of an in-flight operation.
    ///
    /// Called when the awaiting future is dropped. A completion still arrives
    /// afterwards, normally carrying `ERROR_OPERATION_ABORTED`.
    ///
    /// # Safety
    ///
    /// `optr` is the same pointer previously passed to [`OpCode::operate`].
    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        let _ = optr;
        Ok(())
    }

    /// Observe the completion result before it is handed to the caller.
    ///
    /// Lets an operation read back fields Windows filled in — transferred
    /// lengths, address sizes, flags — while it still has `&mut self`.
    ///
    /// Runs on whichever path resolved the operation: the completion packet, or
    /// the inline path when [`OpCode::operate`] finished synchronously and no
    /// packet will follow. An operation that must classify a condition the
    /// platform can report either way therefore needs no separate inline hook.
    ///
    /// **Not guaranteed to run exactly once.** An operation that classifies its
    /// own inline result inside `operate` will also see the driver's inline
    /// call, so implementations must be idempotent rather than assuming a
    /// single invocation.
    ///
    /// # Safety
    ///
    /// Called only while the driver holds exclusive access to the operation,
    /// before its state is released or returned.
    unsafe fn on_complete(&mut self, result: &Result<usize>) {
        let _ = result;
    }

    /// Observe the completion result together with the byte count Windows
    /// reported, even when the status is a failure.
    ///
    /// Some conditions are failures that still transferred data — a named pipe
    /// message that did not fit the buffer reports `ERROR_MORE_DATA` *and* the
    /// bytes it delivered. [`OpCode::on_complete`] cannot see that count,
    /// because a failure status carries no value in the `Result`.
    ///
    /// The default forwards to [`OpCode::on_complete`], so operations that do
    /// not care are unaffected and existing implementations keep working
    /// unchanged. Override this instead of `on_complete` when the count matters
    /// on the failure path; the driver calls this one, never both.
    ///
    /// `transferred` is what the platform reported. It is **not** validated
    /// against any buffer length: an implementation that uses it to publish
    /// initialised bytes must clamp it to its own capacity first.
    ///
    /// Called under the same conditions as [`OpCode::on_complete`] — at most
    /// once, from either the completion path or the inline path.
    ///
    /// # Safety
    ///
    /// Called only while the driver holds exclusive access to the operation,
    /// before its state is released or returned.
    unsafe fn on_complete_with(&mut self, result: &Result<usize>, transferred: usize) {
        let _ = transferred;
        // SAFETY: this call inherits this method's own contract — the driver
        // holds exclusive access and the state has not been released.
        unsafe { self.on_complete(result) }
    }
}

/// Extracts the meaningful result out of a completed operation.
///
/// Operations commonly wrap a buffer or a filled structure; this returns it to
/// the caller. Deliberately not a supertrait of [`OpCode`] — an operation may
/// own state that has no natural "inner" value.
pub trait IntoInner {
    /// What the caller gets back.
    type Inner;

    /// Consume the operation and yield its result value.
    fn into_inner(self) -> Self::Inner;
}

/// Converts a Win32 success flag into the [`Poll`] result `operate` must return,
/// treating `ERROR_IO_PENDING` as "started asynchronously".
///
/// On inline success the transferred count is read from the `OVERLAPPED`'s
/// `InternalHigh` field, which Windows fills in for a synchronously completed
/// operation. Callers must not perform any other Windows call between the API
/// they are wrapping and this function, since it inspects the thread's last
/// error.
///
/// # Safety
///
/// `optr` must be the pointer that was passed to the wrapped API.
pub unsafe fn win32_result(started_ok: bool, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
    if started_ok {
        // Completed inline; Windows recorded the byte count in the OVERLAPPED.
        let transferred = unsafe { (*optr).InternalHigh };
        return Poll::Ready(Ok(transferred));
    }
    let err = Error::from_thread();
    if err.code() == windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult() {
        Poll::Pending
    } else {
        Poll::Ready(Err(err))
    }
}
