// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Owned wrapper over a Windows I/O completion port.

use std::time::Duration;

use windows::core::{Error, Result};
use windows::Win32::Foundation::{
    CloseHandle, RtlNtStatusToDosError, ERROR_INVALID_PARAMETER, HANDLE, INVALID_HANDLE_VALUE,
    NTSTATUS, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::SetFileCompletionNotificationModes;
use windows::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatusEx, PostQueuedCompletionStatus,
    OVERLAPPED_ENTRY,
};

/// Do not queue a completion packet when an operation succeeds inline.
///
/// Not exported by the `windows` crate; value from `winbase.h`.
const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: u8 = 0x1;

/// Do not set the file handle's event on completion, which IOCP code never
/// uses. Value from `winbase.h`.
const FILE_SKIP_SET_EVENT_ON_HANDLE: u8 = 0x2;

/// `WAIT_TIMEOUT` as returned by `GetQueuedCompletionStatusEx`.
const WAIT_TIMEOUT_CODE: u32 = 258;

/// Completion key marking packets that belong to a `winasio` proactor.
pub(crate) const KEY_OPERATION: usize = 0x7761_7331;

/// Completion key used by the wakeup sentinel.
pub(crate) const KEY_WAKEUP: usize = 0x7761_7332;

/// Why registering a handle failed.
///
/// A handle can be associated with exactly one completion mechanism, for its
/// entire lifetime. Attempting to register twice — with either backend, in
/// either order — is reported as [`RegistrationError::AlreadyRegistered`].
#[derive(Debug)]
pub enum RegistrationError {
    /// The handle is already associated with a completion port or thread-pool
    /// I/O object. Association is permanent and cannot be undone.
    ///
    /// Windows reports every such case as `ERROR_INVALID_PARAMETER`, which is
    /// not by itself specific. This crate controls every argument at the
    /// registration call, so that failure is attributed here.
    AlreadyRegistered(Error),
    /// Any other failure.
    Os(Error),
}

impl RegistrationError {
    pub(crate) fn from_association(err: Error) -> Self {
        if err.code() == ERROR_INVALID_PARAMETER.to_hresult() {
            RegistrationError::AlreadyRegistered(err)
        } else {
            RegistrationError::Os(err)
        }
    }

    /// The underlying Windows error.
    pub fn as_error(&self) -> &Error {
        match self {
            RegistrationError::AlreadyRegistered(e) | RegistrationError::Os(e) => e,
        }
    }
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistrationError::AlreadyRegistered(e) => write!(
                f,
                "handle is already registered with a completion mechanism: {e}"
            ),
            RegistrationError::Os(e) => write!(f, "handle registration failed: {e}"),
        }
    }
}

impl std::error::Error for RegistrationError {}

impl From<RegistrationError> for Error {
    fn from(value: RegistrationError) -> Self {
        match value {
            RegistrationError::AlreadyRegistered(e) | RegistrationError::Os(e) => e,
        }
    }
}

/// An owned completion port.
pub(crate) struct CompletionPort {
    handle: HANDLE,
}

// SAFETY: a completion port handle is usable from any thread; Windows
// serialises access internally.
unsafe impl Send for CompletionPort {}
unsafe impl Sync for CompletionPort {}

impl CompletionPort {
    /// Create a port. `concurrency` of 1 suits a single-threaded driver.
    pub(crate) fn new() -> Result<Self> {
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1) }?;
        Ok(CompletionPort { handle })
    }

    /// The raw port handle. Needed by the thread-pool backend in a later phase.
    #[allow(dead_code)]
    pub(crate) fn raw(&self) -> HANDLE {
        self.handle
    }

    /// Associate a handle with this port.
    ///
    /// Returns whether synchronous completions will bypass the port. When
    /// [`SetFileCompletionNotificationModes`] is unsupported for the handle
    /// type, this is `false` and an inline success still produces a packet, so
    /// the caller must treat it as pending.
    pub(crate) fn attach(&self, handle: HANDLE) -> std::result::Result<bool, RegistrationError> {
        unsafe { CreateIoCompletionPort(handle, Some(self.handle), KEY_OPERATION, 0) }
            .map_err(RegistrationError::from_association)?;

        // Skipping the port on inline success avoids a second delivery for an
        // operation the submitter already saw complete. Not every handle type
        // supports it; failure is not fatal, it just changes the contract.
        let skip = unsafe {
            SetFileCompletionNotificationModes(
                handle,
                FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE,
            )
        }
        .is_ok();

        Ok(skip)
    }

    /// Post the wakeup sentinel, unblocking a waiting [`CompletionPort::poll`].
    ///
    /// The public path is [`Notify`](crate::iocp::Notify), which can cross
    /// threads; this is the direct form.
    #[allow(dead_code)]
    pub(crate) fn wake(&self) -> Result<()> {
        unsafe { PostQueuedCompletionStatus(self.handle, 0, KEY_WAKEUP, None) }
    }

    /// Post a completion packet for an operation that did not originate in the
    /// I/O subsystem. Used by event waits, which are driven by the Win32 wait
    /// infrastructure but must surface through the same completion path.
    #[allow(dead_code)]
    pub(crate) fn post_operation(
        &self,
        optr: *mut windows::Win32::System::IO::OVERLAPPED,
    ) -> Result<()> {
        unsafe { PostQueuedCompletionStatus(self.handle, 0, KEY_OPERATION, Some(optr)) }
    }

    /// Retrieve up to `entries.len()` completions.
    ///
    /// Returns the number written. A timeout yields `Ok(0)`.
    pub(crate) fn poll(
        &self,
        entries: &mut [OVERLAPPED_ENTRY],
        timeout: Option<Duration>,
    ) -> Result<usize> {
        let millis = match timeout {
            None => u32::MAX, // INFINITE
            Some(d) => d.as_millis().min(u32::MAX as u128 - 1) as u32,
        };
        let mut removed: u32 = 0;
        let res = unsafe {
            GetQueuedCompletionStatusEx(self.handle, entries, &mut removed, millis, false)
        };
        match res {
            Ok(()) => Ok(removed as usize),
            Err(e) if e.code() == WIN32_ERROR(WAIT_TIMEOUT_CODE).to_hresult() => Ok(0),
            Err(e) => Err(e),
        }
    }
}

impl Drop for CompletionPort {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

/// The byte count a completion packet reported, regardless of its status.
///
/// [`entry_result`] cannot carry this on the failure path, but some failures —
/// `ERROR_MORE_DATA` above all — still transferred data.
pub(crate) fn entry_transferred(entry: &OVERLAPPED_ENTRY) -> usize {
    entry.dwNumberOfBytesTransferred as usize
}

/// Convert the `Internal` field of a completion entry into a result.
///
/// `Internal` carries an `NTSTATUS`, not a Win32 error code, so it must be
/// translated rather than reinterpreted.
pub(crate) fn entry_result(entry: &OVERLAPPED_ENTRY) -> Result<usize> {
    let status = NTSTATUS(entry.Internal as i32);
    if status.is_ok() {
        Ok(entry.dwNumberOfBytesTransferred as usize)
    } else {
        let win32 = unsafe { RtlNtStatusToDosError(status) };
        Err(Error::from_hresult(WIN32_ERROR(win32).to_hresult()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_can_be_created_and_woken() {
        let port = CompletionPort::new().unwrap();
        port.wake().unwrap();

        let mut entries = [OVERLAPPED_ENTRY::default(); 4];
        let n = port
            .poll(&mut entries, Some(Duration::from_millis(500)))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(entries[0].lpCompletionKey, KEY_WAKEUP);
    }

    #[test]
    fn poll_times_out_cleanly() {
        let port = CompletionPort::new().unwrap();
        let mut entries = [OVERLAPPED_ENTRY::default(); 4];
        let n = port
            .poll(&mut entries, Some(Duration::from_millis(20)))
            .unwrap();
        assert_eq!(n, 0, "a timeout is not an error");
    }

    #[test]
    fn success_status_maps_to_transferred_count() {
        let entry = OVERLAPPED_ENTRY {
            lpCompletionKey: KEY_OPERATION,
            lpOverlapped: std::ptr::null_mut(),
            Internal: 0,
            dwNumberOfBytesTransferred: 42,
        };
        assert_eq!(entry_result(&entry).unwrap(), 42);
    }

    #[test]
    fn ntstatus_is_translated_not_reinterpreted() {
        // STATUS_CANCELLED, what an aborted operation reports.
        const STATUS_CANCELLED: usize = 0xC000_0120;
        let entry = OVERLAPPED_ENTRY {
            lpCompletionKey: KEY_OPERATION,
            lpOverlapped: std::ptr::null_mut(),
            Internal: STATUS_CANCELLED,
            dwNumberOfBytesTransferred: 0,
        };
        let err = entry_result(&entry).unwrap_err();
        // Must come back as ERROR_OPERATION_ABORTED (995), not as the raw
        // NTSTATUS bit pattern.
        assert_eq!(
            err.code(),
            WIN32_ERROR(995).to_hresult(),
            "NTSTATUS must be translated to a Win32 code"
        );
    }

    #[test]
    fn registration_error_classifies_duplicate_association() {
        let dup = RegistrationError::from_association(Error::from_hresult(
            ERROR_INVALID_PARAMETER.to_hresult(),
        ));
        assert!(matches!(dup, RegistrationError::AlreadyRegistered(_)));

        let other =
            RegistrationError::from_association(Error::from_hresult(WIN32_ERROR(5).to_hresult()));
        assert!(matches!(other, RegistrationError::Os(_)));
    }
}
