// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Connected named-pipe endpoint.

use std::future::Future;

use windows::core::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::DisconnectNamedPipe;
use windows::Win32::System::IO::CancelIoEx;

use crate::iocp::{
    Handle, IntoInner, IoBuf, IoBufMut, OpResult, ReadHandle, Submitter, WriteHandle,
};

use super::server::NamedPipeServer;
use super::ReadOutcome;

pub(crate) struct Inner<S> {
    pub(crate) handle: Handle,
    pub(crate) submitter: S,
}

impl<S> Inner<S> {
    pub(crate) fn new(handle: Handle, submitter: S) -> Self {
        Inner { handle, submitter }
    }
}

pub(crate) fn drop_inner<S>(inner: Inner<S>) {
    let Inner { handle, submitter } = inner;
    // SAFETY: this owner holds a live handle reference. `None` requests
    // cancellation of every overlapped operation currently on the handle.
    let _ = unsafe { CancelIoEx(handle.raw(), None) };
    drop(submitter);
    drop(handle);
}

/// Keeps [`drop_inner`]'s ordering when state is parked inside a future.
///
/// A typestate transition moves the state into the future that will produce the
/// next state, so for the duration of that future nothing else owns it. If the
/// caller drops the future before it resolves, the state must still be torn down
/// in the documented order — dropping the fields implicitly would skip the
/// cancellation step and release the handle reference in declaration order
/// instead of last. Taking the state out disarms the guard.
pub(crate) struct InnerGuard<S>(Option<Inner<S>>);

impl<S> InnerGuard<S> {
    pub(crate) fn new(inner: Inner<S>) -> Self {
        InnerGuard(Some(inner))
    }

    /// Take the state, disarming the guard.
    pub(crate) fn take(&mut self) -> Inner<S> {
        self.0.take().expect("pipe state is present")
    }
}

impl<S> Drop for InnerGuard<S> {
    fn drop(&mut self) {
        if let Some(inner) = self.0.take() {
            drop_inner(inner);
        }
    }
}

/// A connected named-pipe endpoint.
pub struct NamedPipe<S: Submitter> {
    pub(crate) inner: Option<Inner<S>>,
}

impl<S: Submitter> NamedPipe<S> {
    pub(crate) fn from_inner(inner: Inner<S>) -> Self {
        NamedPipe { inner: Some(inner) }
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn into_file_parts(mut self) -> (Handle, S) {
        let inner = self.take_inner();
        (inner.handle, inner.submitter)
    }

    /// The underlying kernel handle, borrowed for interoperability.
    ///
    /// Ownership is not transferred. Operations built independently from this
    /// value are outside this type's handle-outlives-operation guarantee, must
    /// not outlive the `NamedPipe`, and will be cancelled if the `NamedPipe` is
    /// dropped.
    pub fn handle(&self) -> HANDLE {
        self.open().handle.raw()
    }

    /// Start a read.
    ///
    /// On a message-read-mode endpoint, an oversized message resolves to
    /// [`ReadOutcome::MoreData`] carrying the delivered byte count. The
    /// operation does not grow the buffer or retry; read again to retrieve the
    /// remainder.
    ///
    /// If the returned future is dropped before resolving, cancellation is
    /// requested and the buffer is not returned.
    pub fn read<B>(&self, buffer: B) -> impl Future<Output = OpResult<ReadOutcome, B>>
    where
        B: IoBufMut + Send,
    {
        let open = self.open();
        let submitted = open
            .submitter
            .submit(ReadHandle::new(open.handle.clone(), buffer));
        async move {
            let OpResult(result, op) = submitted.await;
            let (result, buffer) = op.finish(result);
            OpResult(result, buffer)
        }
    }

    /// Start a write.
    ///
    /// If the returned future is dropped before resolving, cancellation is
    /// requested and the buffer is not returned.
    pub fn write<B>(&self, buffer: B) -> impl Future<Output = OpResult<usize, B>>
    where
        B: IoBuf + Send,
    {
        let open = self.open();
        let submitted = open
            .submitter
            .submit(WriteHandle::new(open.handle.clone(), buffer));
        async move {
            let OpResult(result, op) = submitted.await;
            OpResult(result, op.into_inner())
        }
    }

    /// Write the whole buffer to the pipe.
    ///
    /// The helper submits as many writes as are needed to transfer the full
    /// buffer.
    ///
    /// If the returned future is dropped before resolving, bytes already
    /// transferred to the peer are not undone, and neither the buffer nor the
    /// transferred count is returned.
    pub fn write_all<B>(&self, buffer: B) -> impl Future<Output = crate::io::TransferResult<B>> + '_
    where
        B: IoBuf + Send,
    {
        crate::io::write_all(self, 0, buffer)
    }

    /// Fill the buffer's full capacity from the pipe.
    ///
    /// A closed peer before the capacity is filled is reported as
    /// [`crate::io::TransferError::ClosedPeer`], with the partially filled
    /// buffer and transferred count returned in the result. A stream that
    /// simply ends without a peer closing — which a pipe cannot do, but a file
    /// can — reports [`crate::io::TransferError::UnexpectedEof`] instead.
    ///
    /// If the returned future is dropped before resolving, bytes already
    /// transferred from the peer are not undone, and neither the buffer nor the
    /// transferred count is returned.
    pub fn read_exact<B>(
        &self,
        buffer: B,
    ) -> impl Future<Output = crate::io::TransferResult<B>> + '_
    where
        B: IoBufMut + Send,
    {
        crate::io::read_exact(self, 0, buffer)
    }

    /// Read from the pipe until the peer closes into `buffer`.
    ///
    /// The returned count is the number of bytes appended by this helper.
    ///
    /// If the returned future is dropped before resolving, bytes already
    /// transferred from the peer are not undone, and neither the buffer nor the
    /// transferred count is returned.
    pub fn read_to_end(
        &self,
        buffer: Vec<u8>,
    ) -> impl Future<Output = crate::io::TransferResult<Vec<u8>>> + '_ {
        crate::io::read_to_end(self, 0, buffer)
    }

    /// Disconnect this server-side pipe and return the reusable server state.
    ///
    /// This consumes the pipe, so it requires exclusive ownership. If the pipe
    /// has been shared for concurrent I/O, wait for those operations to finish
    /// and recover the sole owner before disconnecting.
    pub fn disconnect(mut self) -> Result<NamedPipeServer<S>> {
        let inner = self.take_inner();
        // SAFETY: `inner.handle` is a connected server-side named-pipe handle
        // owned by this value.
        match unsafe { DisconnectNamedPipe(inner.handle.raw()) } {
            Ok(()) => Ok(NamedPipeServer::from_inner(inner)),
            Err(e) => {
                drop_inner(inner);
                Err(e)
            }
        }
    }

    fn take_inner(&mut self) -> Inner<S> {
        self.inner.take().expect("pipe state is present")
    }

    fn open(&self) -> &Inner<S> {
        self.inner.as_ref().expect("pipe state is present")
    }
}

impl<S: Submitter> std::fmt::Debug for NamedPipe<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedPipe")
            .field(
                "handle",
                &self.inner.as_ref().map(|inner| inner.handle.raw().0),
            )
            .finish_non_exhaustive()
    }
}

impl<S: Submitter> Drop for NamedPipe<S> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        drop_inner(inner);
    }
}
