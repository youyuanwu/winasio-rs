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

use super::buf::OpResult;
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
}

impl<T: OpCode> Future for Submit<T> {
    type Output = OpResult<usize, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if let Some(result) = this.inline.take() {
            let key = this.key.take().expect("resolved once");
            // Nothing is in flight, so the state can be taken directly.
            return Poll::Ready(OpResult(result, key.take_op_inline()));
        }

        let key = this.key.as_ref().expect("polled after completion");

        // Install the waker before checking, so a completion landing between the
        // check and the installation still wakes us.
        key.set_waker(cx.waker());

        // Takes the result and the operation together under one exclusive
        // transition, so this does not depend on the completion path having
        // already released its own reference to the allocation.
        match key.take_completion() {
            Some((result, op)) => {
                this.key = None;
                Poll::Ready(OpResult(result, op))
            }
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
