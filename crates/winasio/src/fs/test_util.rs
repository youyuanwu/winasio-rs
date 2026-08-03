// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Test-only helpers for exercising file teardown paths.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use windows::core::{Error, HSTRING};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::iocp::{Handle, IoBuf, IoBufMut, Registrar};

use super::{File, SetupError};

/// The client side of a pipe used to keep a server read pending.
pub struct PendingReadPeer {
    _client: Handle,
}

/// Create a registered handle whose reads remain pending until cancelled.
///
/// The returned `File` wraps the server end of a byte-mode named pipe. The peer
/// is kept open but never writes, so a read submitted through the file is
/// genuinely in flight rather than an immediate EOF.
pub fn pending_read_file<R: Registrar>(
    registrar: &R,
) -> Result<(File<R::Io>, PendingReadPeer), SetupError> {
    let name = unique_pipe_name();

    // SAFETY: creating a local byte-mode named pipe with default security. The
    // returned handle is checked before ownership is transferred.
    let server = unsafe {
        CreateNamedPipeW(
            &name,
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            None,
        )
    };
    if server == INVALID_HANDLE_VALUE {
        return Err(SetupError::from_windows(Error::from_thread()));
    }
    // SAFETY: `server` is a newly created, uniquely owned handle.
    let server = unsafe { Handle::from_raw(server) };

    // SAFETY: opening the client end of the pipe name created above.
    let client = unsafe {
        CreateFileW(
            &name,
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
    }
    .map_err(SetupError::from_windows)?;
    // SAFETY: `client` is a newly opened, uniquely owned handle.
    let client = unsafe { Handle::from_raw(client) };

    // SAFETY: the client is already connected, so this reports either success
    // or `ERROR_PIPE_CONNECTED` without starting an overlapped connect.
    match unsafe { ConnectNamedPipe(server.raw(), None) } {
        Ok(()) => {}
        Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
        Err(e) => return Err(SetupError::from_windows(e)),
    }

    let submitter = registrar.register(server.raw()).map_err(SetupError::from)?;
    Ok((
        File::from_parts(server, submitter),
        PendingReadPeer { _client: client },
    ))
}

fn unique_pipe_name() -> HSTRING {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    HSTRING::from(format!(r"\\.\pipe\winasio_fs_pending_{pid:x}_{n:x}"))
}

/// A mutable buffer that records when it is dropped.
pub struct DropProbeBuf {
    bytes: Vec<u8>,
    drops: Arc<AtomicUsize>,
}

impl DropProbeBuf {
    /// Allocate a buffer with `capacity` and increment `drops` on drop.
    pub fn with_capacity(capacity: usize, drops: Arc<AtomicUsize>) -> Self {
        DropProbeBuf {
            bytes: Vec::with_capacity(capacity),
            drops,
        }
    }
}

impl Drop for DropProbeBuf {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

// SAFETY: the buffer is a `Vec<u8>`, whose allocation address is stable across
// moves, and `bytes_init` never exceeds the allocation length.
unsafe impl IoBuf for DropProbeBuf {
    fn stable_ptr(&self) -> *const u8 {
        self.bytes.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.bytes.len()
    }
}

// SAFETY: the mutable pointer is the same `Vec<u8>` allocation reported by
// `stable_ptr`; capacity bounds the writable region; and `set_init` delegates to
// `Vec::set_len` after checking that bound.
unsafe impl IoBufMut for DropProbeBuf {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    fn bytes_total(&self) -> usize {
        self.bytes.capacity()
    }

    unsafe fn set_init(&mut self, len: usize) {
        assert!(len <= self.bytes.capacity());
        // SAFETY: the caller guarantees the first `len` bytes were initialised.
        unsafe { self.bytes.set_len(len) };
    }
}
