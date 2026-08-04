// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Named-pipe client builder.

use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE,
    OPEN_EXISTING,
};
use windows::Win32::System::Pipes::{
    SetNamedPipeHandleState, PIPE_READMODE_BYTE, PIPE_READMODE_MESSAGE,
};

use crate::fs::SetupError;
use crate::iocp::{Handle, Registrar};

use super::connected::Inner;
use super::name::local_pipe_path;
use super::server::{AccessDirection, PipeMode};
use super::NamedPipe;

/// Builder for connecting an overlapped named-pipe client.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    name: String,
    access: AccessDirection,
    read_mode: PipeMode,
}

impl ClientOptions {
    /// Create options for the bare pipe `name`.
    pub fn new(name: impl Into<String>) -> Self {
        ClientOptions {
            name: name.into(),
            access: AccessDirection::Duplex,
            read_mode: PipeMode::Byte,
        }
    }

    /// Replace the bare pipe name.
    pub fn name(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = name.into();
        self
    }

    /// Set the client access direction.
    pub fn access(&mut self, access: AccessDirection) -> &mut Self {
        self.access = access;
        self
    }

    /// Set the client's read mode. Byte read mode is the default.
    ///
    /// Message read mode is applied inside [`ClientOptions::connect`], before
    /// the connected pipe is returned.
    pub fn read_mode(&mut self, read_mode: PipeMode) -> &mut Self {
        self.read_mode = read_mode;
        self
    }

    /// Open and register a connected client endpoint.
    ///
    /// If all server instances are busy, this returns [`SetupError::Busy`]
    /// immediately; waiting and retrying is the caller's responsibility.
    pub fn connect<R: Registrar>(
        &self,
        registrar: &R,
    ) -> std::result::Result<NamedPipe<R::Io>, SetupError> {
        let name = local_pipe_path(&self.name)?;

        // SAFETY: the name was validated and composed as a local, NUL-free
        // pipe path. The handle is always opened for overlapped I/O.
        let raw = unsafe {
            CreateFileW(
                &name,
                self.desired_access(),
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(SetupError::from_windows)?;

        // SAFETY: `CreateFileW` returned a newly owned handle, and ownership of
        // closing it transfers into `Handle`.
        let handle = unsafe { Handle::from_raw(raw) };
        if let Err(e) = self.apply_read_mode(handle.raw()) {
            drop(handle);
            return Err(SetupError::from_windows(e));
        }
        match registrar.register(handle.raw()) {
            Ok(submitter) => Ok(NamedPipe::from_inner(Inner::new(handle, submitter))),
            Err(e) => {
                drop(handle);
                Err(SetupError::from(e))
            }
        }
    }

    fn desired_access(&self) -> u32 {
        match self.access {
            AccessDirection::Inbound => FILE_GENERIC_READ.0,
            AccessDirection::Outbound => FILE_GENERIC_WRITE.0,
            AccessDirection::Duplex => FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
        }
    }

    fn apply_read_mode(
        &self,
        handle: windows::Win32::Foundation::HANDLE,
    ) -> windows::core::Result<()> {
        let mode = match self.read_mode {
            PipeMode::Byte => PIPE_READMODE_BYTE,
            PipeMode::Message => PIPE_READMODE_MESSAGE,
        };
        if mode == PIPE_READMODE_BYTE {
            return Ok(());
        }
        // SAFETY: `handle` is a live named-pipe handle just returned by
        // `CreateFileW`. The mode pointer is valid for the duration of the
        // call, and the collection/time-out pointers are intentionally absent.
        unsafe { SetNamedPipeHandleState(handle, Some(&mode), None, None) }
    }
}
