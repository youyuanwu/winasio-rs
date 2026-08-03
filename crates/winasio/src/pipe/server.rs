// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Named-pipe server builder and unconnected typestate.
//!
//! ```compile_fail
//! use winasio::iocp::ThreadPool;
//! use winasio::pipe::ServerOptions;
//!
//! # fn demo() -> Result<(), winasio::pipe::SetupError> {
//! let server = ServerOptions::new("compile_fail_unconnected_read").create(&ThreadPool)?;
//! let _ = server.read(Vec::with_capacity(8));
//! # Ok(())
//! # }
//! ```

use std::future::Future;

use windows::core::{Error, Result};
use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, PIPE_ACCESS_INBOUND,
    PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::fs::SetupError;
use crate::iocp::{ConnectPipe, Handle, OpResult, Registrar, Submitter};

use super::connected::{drop_inner, Inner, InnerGuard};
use super::name::local_pipe_path;
use super::NamedPipe;

/// Direction requested when opening a named pipe endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessDirection {
    /// The server reads and the client writes.
    Inbound,
    /// The server writes and the client reads.
    Outbound,
    /// Both endpoints may read and write.
    #[default]
    Duplex,
}

/// Builder for creating an overlapped byte-mode named-pipe server instance.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    name: String,
    access: AccessDirection,
    max_instances: u32,
    in_buffer_size: u32,
    out_buffer_size: u32,
    default_timeout: u32,
    first_instance: bool,
}

impl ServerOptions {
    /// Create options for the bare pipe `name`.
    pub fn new(name: impl Into<String>) -> Self {
        ServerOptions {
            name: name.into(),
            access: AccessDirection::Duplex,
            max_instances: 1,
            in_buffer_size: 4096,
            out_buffer_size: 4096,
            default_timeout: 0,
            first_instance: false,
        }
    }

    /// Replace the bare pipe name.
    pub fn name(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = name.into();
        self
    }

    /// Set the access direction.
    pub fn access(&mut self, access: AccessDirection) -> &mut Self {
        self.access = access;
        self
    }

    /// Set the maximum number of instances for this pipe name.
    pub fn max_instances(&mut self, max_instances: u32) -> &mut Self {
        self.max_instances = max_instances;
        self
    }

    /// Set the inbound buffer size.
    pub fn in_buffer_size(&mut self, size: u32) -> &mut Self {
        self.in_buffer_size = size;
        self
    }

    /// Set the outbound buffer size.
    pub fn out_buffer_size(&mut self, size: u32) -> &mut Self {
        self.out_buffer_size = size;
        self
    }

    /// Set the default timeout, in milliseconds.
    pub fn default_timeout(&mut self, timeout: u32) -> &mut Self {
        self.default_timeout = timeout;
        self
    }

    /// Require this create call to create the first instance for the name.
    pub fn first_instance(&mut self, first_instance: bool) -> &mut Self {
        self.first_instance = first_instance;
        self
    }

    /// Create and register an unconnected server instance.
    pub fn create<R: Registrar>(
        &self,
        registrar: &R,
    ) -> std::result::Result<NamedPipeServer<R::Io>, SetupError> {
        let name = local_pipe_path(&self.name)?;
        let mut open_mode = self.server_access_mode() | FILE_FLAG_OVERLAPPED;
        if self.first_instance {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT;

        // SAFETY: the name was validated and composed as a local, NUL-free
        // pipe path. All other arguments are values owned by this builder.
        let raw = unsafe {
            CreateNamedPipeW(
                &name,
                open_mode,
                pipe_mode,
                self.max_instances,
                self.out_buffer_size,
                self.in_buffer_size,
                self.default_timeout,
                None,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(SetupError::from_windows(Error::from_thread()));
        }

        // SAFETY: `CreateNamedPipeW` returned a newly owned handle, and
        // ownership of closing it transfers into `Handle`.
        let handle = unsafe { Handle::from_raw(raw) };
        match registrar.register(handle.raw()) {
            Ok(submitter) => Ok(NamedPipeServer::from_inner(Inner::new(handle, submitter))),
            Err(e) => {
                drop(handle);
                Err(SetupError::from(e))
            }
        }
    }

    pub(crate) fn server_access_mode(
        &self,
    ) -> windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES {
        match self.access {
            AccessDirection::Inbound => PIPE_ACCESS_INBOUND,
            AccessDirection::Outbound => PIPE_ACCESS_OUTBOUND,
            AccessDirection::Duplex => PIPE_ACCESS_DUPLEX,
        }
    }
}

/// An unconnected server-side named-pipe instance.
pub struct NamedPipeServer<S: Submitter> {
    pub(crate) inner: Option<Inner<S>>,
}

impl<S: Submitter> NamedPipeServer<S> {
    pub(crate) fn from_inner(inner: Inner<S>) -> Self {
        NamedPipeServer { inner: Some(inner) }
    }

    /// Start accepting a client on this server instance.
    ///
    /// If a client connected before this call, the returned future resolves
    /// successfully without waiting for a completion packet.
    ///
    /// Dropping the returned future before it resolves cancels the accept and
    /// tears the instance down in the documented order; as with every operation
    /// in this crate, nothing is handed back in that case.
    pub fn connect(mut self) -> impl Future<Output = Result<NamedPipe<S>>> {
        let inner = self.take_inner();
        let submitted = inner
            .submitter
            .submit(ConnectPipe::new(inner.handle.clone()));
        // Parked in the future: if the caller drops it before completion, the
        // guard restores the teardown ordering an implicit field drop would skip.
        let mut guard = InnerGuard::new(inner);
        async move {
            let OpResult(result, _op) = submitted.await;
            match result {
                // Taking the state disarms the guard.
                Ok(_) => Ok(NamedPipe::from_inner(guard.take())),
                // Left armed: the guard tears the instance down on the way out.
                Err(e) => Err(e),
            }
        }
    }

    fn take_inner(&mut self) -> Inner<S> {
        self.inner.take().expect("pipe state is present")
    }
}

impl<S: Submitter> std::fmt::Debug for NamedPipeServer<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedPipeServer")
            .field(
                "handle",
                &self.inner.as_ref().map(|inner| inner.handle.raw().0),
            )
            .finish_non_exhaustive()
    }
}

impl<S: Submitter> Drop for NamedPipeServer<S> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        drop_inner(inner);
    }
}
