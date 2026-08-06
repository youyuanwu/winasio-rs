// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Naming the boxed read future once per completion backend.
//!
//! # Why this trait exists
//!
//! [`IncomingBody`](super::IncomingBody) implements
//! [`poll_frame`](http_body::Body::poll_frame) by hand, so it must hold an
//! in-flight read across polls, so the read has to have a name. `read_body` is
//! an `async fn` whose future is anonymous, so the name has to be a box.
//!
//! One box will not do for both backends. `Pin<Box<dyn Future + Send>>` needs
//! the future to be `Send`, which needs `Arc<RequestQueue<S>>: Send + Sync`,
//! which is false for `S = Rc<Proactor>`. `Pin<Box<dyn Future>>` works for both
//! and makes *every* body `!Send`, including the thread-pool one — and a
//! `!Send` body cannot be moved onto a spawned task, which is the entire
//! concurrency story this crate offers.
//!
//! So the box is an associated type and each backend names its own. The result
//! is that `IncomingBody<ThreadPoolIo>` is `Send` and
//! `IncomingBody<Rc<Proactor>>` compiles, which is what `winasio::httpsys`
//! supports and therefore what this crate must not narrow.
//!
//! # Why it is sealed
//!
//! The trait names an implementation detail. There are exactly two completion
//! backends in `winasio`, this crate supports both, and a third would come from
//! there rather than from a downstream crate. Sealing it means the associated
//! type can change without a breaking release.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use winasio::httpsys::{RequestId, RequestQueue};
use winasio::iocp::{OpResult, Proactor, Submitter, ThreadPoolIo};

/// What one read of the request body produced, with its buffer handed back.
///
/// The buffer travels through the future in both directions so that the body
/// can reuse a single allocation for the whole request rather than one per
/// frame.
pub(crate) type ReadStep = (Vec<u8>, Result<usize, windows::core::Error>);

pub(crate) mod sealed {
    /// Not nameable outside this crate, so [`Backend`](super::Backend) cannot
    /// be implemented outside it either.
    pub trait Sealed {}
}

/// A completion backend this crate can read a request body on.
///
/// Implemented for `winasio`'s two backends and sealed against anything else.
/// Callers rarely name it: it appears as the defaulted `S` parameter of
/// [`Server`](crate::Server) and friends, where the default is [`ThreadPoolIo`].
///
/// See the module documentation for why a trait is needed at all.
pub trait Backend: Submitter + Sized + 'static + sealed::Sealed {
    /// The concrete type of an in-flight body read on this backend.
    #[doc(hidden)]
    type Read: Future<Output = ReadStep> + Unpin + 'static;

    /// Start one read, owning everything it needs so that the future is
    /// self-contained and can be stored across polls.
    #[doc(hidden)]
    fn read_body(queue: Arc<RequestQueue<Self>>, id: RequestId, buffer: Vec<u8>) -> Self::Read;
}

impl sealed::Sealed for ThreadPoolIo {}

impl Backend for ThreadPoolIo {
    /// `Send`, so that a body — and therefore a whole
    /// [`Accepted`](crate::server::Accepted) — can be moved onto a task the
    /// caller spawned.
    type Read = Pin<Box<dyn Future<Output = ReadStep> + Send>>;

    fn read_body(queue: Arc<RequestQueue<Self>>, id: RequestId, buffer: Vec<u8>) -> Self::Read {
        Box::pin(async move {
            let OpResult(read, buffer) = queue.read_body(id, buffer).await;
            (buffer, read)
        })
    }
}

impl sealed::Sealed for Rc<Proactor> {}

impl Backend for Rc<Proactor> {
    /// Not `Send`: a [`Proactor`] is driven by one thread and an `Rc` cannot
    /// leave it. That is the point of this backend, not a shortcoming of it.
    type Read = Pin<Box<dyn Future<Output = ReadStep>>>;

    fn read_body(queue: Arc<RequestQueue<Self>>, id: RequestId, buffer: Vec<u8>) -> Self::Read {
        Box::pin(async move {
            let OpResult(read, buffer) = queue.read_body(id, buffer).await;
            (buffer, read)
        })
    }
}
