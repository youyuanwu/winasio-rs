//! A minimal owned wrapper over a Windows manual-reset event.
//!
//! Awaiting a signalable handle is an operation now; see
//! [`WaitForHandle`](crate::iocp::WaitForHandle).

use windows::{
    core::Error,
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::Threading::{CreateEventA, ResetEvent, SetEvent},
    },
};

// wrapper for windows event
#[derive(Default)]
pub struct ManualResetEvent {
    h: HANDLE,
}

impl ManualResetEvent {
    pub fn new() -> ManualResetEvent {
        // do not expect this to fail since this is un-named event.
        let h = unsafe { CreateEventA(None, true, false, None) }.unwrap();
        ManualResetEvent { h }
    }

    pub fn assign(&mut self, h: HANDLE) {
        assert!(self.h.is_invalid());
        self.h = h;
    }

    // set the event
    pub fn set(&self) -> Result<(), Error> {
        assert!(!self.h.is_invalid());
        unsafe { SetEvent(self.h) }
    }

    pub fn reset(&self) -> Result<(), Error> {
        assert!(!self.h.is_invalid());
        unsafe { ResetEvent(self.h) }
    }

    // releases the ownership of the handle
    pub fn release(&mut self) -> HANDLE {
        let h = self.h;
        assert!(!h.is_invalid());
        self.h = HANDLE::default();
        h
    }

    // get the private view of handle
    pub fn get(&self) -> HANDLE {
        assert!(!self.h.is_invalid());
        self.h
    }
}

impl Drop for ManualResetEvent {
    fn drop(&mut self) {
        if self.h.is_invalid() {
            return;
        }
        // Only inspect the thread error when the call actually failed. Reading
        // it after success returns whatever the thread last recorded, which is
        // unrelated to this handle.
        if let Err(e) = unsafe { CloseHandle(self.h) } {
            debug_assert!(false, "failed to close event handle: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ManualResetEvent;

    #[test]
    fn manual_reset_event_test() {
        {
            let e = ManualResetEvent::new();
            e.set().unwrap();
            e.reset().unwrap();
        }
        {
            let mut e1 = ManualResetEvent::default();
            let mut e2 = ManualResetEvent::new();

            let h = e2.release();
            e1.assign(h);
            e1.set().unwrap();
        }
    }
}
