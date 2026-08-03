// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The asynchronous file type.
//!
//! ```compile_fail
//! use std::rc::Rc;
//! use winasio::fs::{File, OpenOptions};
//! use winasio::iocp::Proactor;
//!
//! fn needs_send<T: Send>(_: T) {}
//!
//! # fn demo(path: &std::path::Path) -> Result<(), winasio::fs::SetupError> {
//! let proactor = Rc::new(Proactor::new().unwrap());
//! let mut options = OpenOptions::new();
//! options.read(true);
//! let file: File<Rc<Proactor>> = options.open(&proactor, path)?;
//! needs_send(file);
//! # Ok(())
//! # }
//! ```

use std::future::Future;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::CancelIoEx;

use crate::iocp::{
    Handle, IntoInner, IoBuf, IoBufMut, OpResult, ReadHandleAt, Submitter, WriteHandleAt,
};

use super::{ReadOutcome, SetupError};

struct Inner<S> {
    handle: Handle,
    submitter: S,
}

/// An overlapped file registered with a completion backend.
pub struct File<S: Submitter> {
    inner: Option<Inner<S>>,
}

impl<S: Submitter> File<S> {
    pub(crate) fn from_parts(handle: Handle, submitter: S) -> Self {
        File {
            inner: Some(Inner { handle, submitter }),
        }
    }

    /// Adopt an existing overlapped file handle and register it.
    ///
    /// On success, responsibility for closing `handle` transfers to the
    /// returned `File`. On registration failure, the handle is closed before the
    /// error is returned.
    ///
    /// # Safety
    ///
    /// The caller must uphold all of the following:
    ///
    /// * `handle` is a valid kernel handle currently owned by the caller.
    /// * The handle was opened for overlapped I/O.
    /// * The handle is not already registered with any completion mechanism.
    /// * The handle has no outstanding overlapped operations owned by anything
    ///   else.
    /// * No other owner may close the handle, and nothing else aliases it as an
    ///   owner.
    /// * The handle's access rights and object type are compatible with a file
    ///   supporting this `File` value's reads and writes.
    pub unsafe fn from_handle<R: crate::iocp::Registrar>(
        registrar: &R,
        handle: HANDLE,
    ) -> Result<File<R::Io>, SetupError> {
        // SAFETY: the caller transfers ownership of a valid, uniquely owned
        // handle under this function's safety contract.
        let handle = unsafe { Handle::from_raw(handle) };
        match registrar.register(handle.raw()) {
            Ok(submitter) => Ok(File::from_parts(handle, submitter)),
            Err(e) => {
                drop(handle);
                Err(SetupError::from(e))
            }
        }
    }

    /// The underlying kernel handle, borrowed for interoperability.
    ///
    /// Ownership is not transferred. Operations built independently from this
    /// value are outside this type's handle-outlives-operation guarantee, must
    /// not outlive the `File`, and will be cancelled if the `File` is dropped.
    pub fn handle(&self) -> HANDLE {
        self.open().handle.raw()
    }

    /// Start a positional read.
    ///
    /// If the returned future is dropped before resolving, cancellation is
    /// requested and the buffer is not returned.
    pub fn read_at<B>(
        &self,
        offset: u64,
        buffer: B,
    ) -> impl Future<Output = OpResult<ReadOutcome, B>>
    where
        B: IoBufMut + Send,
    {
        let open = self.open();
        let submitted =
            open.submitter
                .submit(ReadHandleAt::new(open.handle.clone(), offset, buffer));
        async move {
            let OpResult(result, op) = submitted.await;
            let (result, buffer) = op.finish(result);
            OpResult(result, buffer)
        }
    }

    /// Start a positional write.
    ///
    /// If the returned future is dropped before resolving, cancellation is
    /// requested and the buffer is not returned.
    pub fn write_at<B>(&self, offset: u64, buffer: B) -> impl Future<Output = OpResult<usize, B>>
    where
        B: IoBuf + Send,
    {
        let open = self.open();
        let submitted =
            open.submitter
                .submit(WriteHandleAt::new(open.handle.clone(), offset, buffer));
        async move {
            let OpResult(result, op) = submitted.await;
            OpResult(result, op.into_inner())
        }
    }

    fn open(&self) -> &Inner<S> {
        self.inner.as_ref().expect("file state is present")
    }
}

impl<S: Submitter> std::fmt::Debug for File<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File")
            .field(
                "handle",
                &self.inner.as_ref().map(|inner| inner.handle.raw().0),
            )
            .finish_non_exhaustive()
    }
}

impl<S: Submitter> Drop for File<S> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let Inner { handle, submitter } = inner;
        // SAFETY: this file owns a live handle reference. `None` requests
        // cancellation of every overlapped operation currently on the handle.
        let _ = unsafe { CancelIoEx(handle.raw(), None) };
        drop(submitter);
        drop(handle);
    }
}
