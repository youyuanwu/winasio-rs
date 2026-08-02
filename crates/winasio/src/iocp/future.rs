// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The future returned by submitting an operation.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use windows::core::Result;

use super::buf::BufResult;
use super::op::OpCode;
use super::raw::Key;

/// An in-flight operation.
///
/// Resolves to the operation's result paired with the state it owned.
///
/// This is [`Send`] whenever `T` is, so it can be awaited on any runtime. It
/// deliberately holds no reference to the backend that produced it.
///
/// # Dropping before completion
///
/// Dropping this future requests cancellation immediately, but **the operation's
/// state is not returned** and its memory is retained until Windows delivers the
/// completion — usually promptly, but unbounded on a stalled handle. This is
/// inherent to completion-based I/O: the kernel may still be writing into the
/// buffer, so releasing it earlier would be a use-after-free.
///
/// Callers who must recover a buffer should await the operation rather than
/// dropping it.
pub struct Submit<T: OpCode> {
    key: Option<Key<T>>,
    /// Result captured when the operation completed inline.
    inline: Option<Result<usize>>,
}

impl<T: OpCode> Submit<T> {
    pub(crate) fn pending(key: Key<T>) -> Self {
        Submit {
            key: Some(key),
            inline: None,
        }
    }

    pub(crate) fn ready(key: Key<T>, result: Result<usize>) -> Self {
        Submit {
            key: Some(key),
            inline: Some(result),
        }
    }

    /// Whether the operation already finished, without awaiting.
    pub fn is_ready(&self) -> bool {
        self.inline.is_some()
    }

    fn finish(&mut self, result: Result<usize>) -> BufResult<usize, T> {
        let key = self.key.take().expect("operation resolved twice");
        match key.try_into_op() {
            Ok(op) => BufResult::new(result, op),
            Err(_still_shared) => unreachable!(
                "operation state was still shared after completion; the completion \
                 path releases its reference before waking"
            ),
        }
    }
}

impl<T: OpCode> Future for Submit<T> {
    type Output = BufResult<usize, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if let Some(result) = this.inline.take() {
            return Poll::Ready(this.finish(result));
        }

        let key = this.key.as_ref().expect("polled after completion");

        // Install the waker before checking, so a completion landing between the
        // check and the installation still wakes us.
        key.set_waker(cx.waker());

        match key.take_result() {
            Some(result) => Poll::Ready(this.finish(result)),
            None => Poll::Pending,
        }
    }
}

impl<T: OpCode> Drop for Submit<T> {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        // Already resolved inline: nothing is in flight.
        if self.inline.is_some() {
            return;
        }
        if key.abandon() {
            // Still in flight. Ask Windows to cancel; the completion arrives
            // regardless and is what finally releases the allocation. The
            // driver removes its bookkeeping entry when that packet lands.
            let _ = key.cancel();
        }
        // Dropping `key` releases only this reference. The kernel's leaked
        // reference keeps the allocation alive until the completion lands.
    }
}
