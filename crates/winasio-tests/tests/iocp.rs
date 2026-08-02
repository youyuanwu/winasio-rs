// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Integration tests for the caller-driven (own-port) completion backend.

use std::time::Duration;

use winasio::iocp::{live_operations, Proactor, ReadAt, RegistrationError, WriteAt};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, GetTempFileNameW, GetTempPathW, CREATE_ALWAYS, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_SHARE_NONE,
};

/// `live_operations` is process-global, so tests asserting on it serialise.
static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn counter_guard() -> std::sync::MutexGuard<'static, ()> {
    COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct TempFile {
    handle: HANDLE,
    path: HSTRING,
}

impl TempFile {
    fn create(tag: &windows::core::PCWSTR) -> Self {
        let mut dir = vec![0u16; 260];
        let len = unsafe { GetTempPathW(Some(dir.as_mut_slice())) };
        assert_ne!(len, 0);
        dir.truncate(len as usize);
        let dir = HSTRING::from_wide(&dir);

        let mut name = [0u16; 260];
        let n = unsafe { GetTempFileNameW(&dir, *tag, 0, &mut name) };
        assert_ne!(n, 0);
        let path = HSTRING::from_wide(&name);

        let handle = unsafe {
            CreateFileW(
                &path,
                FILE_GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                CREATE_ALWAYS,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .expect("create temp file");

        TempFile { handle, path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
            let _ = DeleteFileW(&self.path);
        }
    }
}

/// Drive the proactor while awaiting an operation, on a single thread.
///
/// This is the canonical shape for the caller-driven backend: nothing completes
/// unless someone calls `poll`.
fn drive<F: std::future::Future>(proactor: &Proactor, fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        tokio::pin!(fut);
        loop {
            // Poll the operation first so an inline completion is observed.
            let polled = futures_poll_once(&mut fut);
            if let Some(out) = polled {
                return out;
            }
            proactor.poll(Some(Duration::from_millis(5))).unwrap();
            tokio::task::yield_now().await;
        }
    })
}

/// Poll a pinned future exactly once, returning its output if it is ready.
fn futures_poll_once<F: std::future::Future>(fut: &mut std::pin::Pin<&mut F>) -> Option<F::Output> {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}

#[test]
fn write_then_read_round_trips_and_returns_buffers() {
    let _guard = counter_guard();
    let file = TempFile::create(&w!("wa1"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    let data: Vec<u8> = "HelloWorld".repeat(200).into_bytes();
    let expected = data.clone();

    // Write, and get the buffer back.
    let written = drive(
        &proactor,
        proactor.submit(WriteAt::new(file.handle, 0, data)),
    );
    let (result, returned) = written.into_inner_parts();
    assert_eq!(result.unwrap(), expected.len());
    assert_eq!(returned, expected, "the write buffer comes back intact");

    // Read it back into a fresh buffer.
    let buf: Vec<u8> = Vec::with_capacity(expected.len());
    let read = drive(&proactor, proactor.submit(ReadAt::new(file.handle, 0, buf)));
    let (result, got) = read.into_inner_parts();
    assert_eq!(result.unwrap(), expected.len());
    assert_eq!(got, expected, "contents round-trip");
}

#[test]
fn zero_byte_read_at_eof_is_a_successful_terminal_outcome() {
    let _guard = counter_guard();
    let file = TempFile::create(&w!("wa2"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    // Write a little, then read past the end.
    let payload = b"abc".to_vec();
    let n = payload.len();
    let w = drive(
        &proactor,
        proactor.submit(WriteAt::new(file.handle, 0, payload)),
    );
    assert_eq!(w.into_parts().0.unwrap(), n);

    let buf: Vec<u8> = Vec::with_capacity(16);
    let r = drive(
        &proactor,
        proactor.submit(ReadAt::new(file.handle, 4096, buf)),
    );
    let (result, returned) = r.into_inner_parts();
    match result {
        Ok(0) => {}
        // Reading past EOF surfaces as ERROR_HANDLE_EOF on some handle types;
        // either way it is terminal and the buffer comes back.
        Ok(other) => panic!("expected zero bytes at EOF, got {other}"),
        Err(e) => assert_eq!(
            e.code(),
            windows::Win32::Foundation::ERROR_HANDLE_EOF.to_hresult(),
            "EOF must be reported as EOF"
        ),
    }
    assert_eq!(returned.len(), 0, "nothing was read");
}

#[test]
fn duplicate_attach_is_rejected_distinguishably() {
    let _guard = counter_guard();
    let file = TempFile::create(&w!("wa3"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    let again = proactor.attach(file.handle);
    match again {
        Err(RegistrationError::AlreadyRegistered(_)) => {}
        Err(other) => panic!("expected AlreadyRegistered, got {other:?}"),
        Ok(()) => panic!("a handle must not be registrable twice"),
    }

    // A different proactor must also refuse it: association is permanent.
    let other = Proactor::new().unwrap();
    assert!(
        matches!(
            other.attach(file.handle),
            Err(RegistrationError::AlreadyRegistered(_))
        ),
        "association is to one port for the handle's lifetime"
    );
}

#[test]
fn notify_wakes_a_blocked_poll_from_another_thread() {
    let _guard = counter_guard();
    let proactor = Proactor::new().unwrap();
    let notify = proactor.notify();

    let waker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        notify.wake().unwrap();
    });

    let started = std::time::Instant::now();
    proactor.poll(Some(Duration::from_secs(30))).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the sentinel must interrupt a long wait"
    );
    waker.join().unwrap();
}

#[test]
fn dropping_a_pending_operation_releases_it_on_completion() {
    let _guard = counter_guard();
    let baseline = live_operations();

    let file = TempFile::create(&w!("wa4"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    {
        // Submit a large read that cannot complete instantly, then abandon it.
        let buf: Vec<u8> = Vec::with_capacity(1 << 20);
        let submitted = proactor.submit(ReadAt::new(file.handle, 0, buf));
        drop(submitted);
    }

    // The allocation must survive the drop until Windows returns the operation.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while live_operations() > baseline && std::time::Instant::now() < deadline {
        proactor.poll(Some(Duration::from_millis(10))).unwrap();
    }

    assert_eq!(
        live_operations(),
        baseline,
        "an abandoned operation is released once its completion arrives"
    );
}

#[test]
fn proactor_drop_cancels_and_drains_in_flight_operations() {
    let _guard = counter_guard();

    let file = TempFile::create(&w!("wa5"));

    let started = std::time::Instant::now();
    let submitted = {
        let proactor = Proactor::new().unwrap();
        proactor.attach(file.handle).unwrap();

        let buf: Vec<u8> = Vec::with_capacity(1 << 20);
        let submitted = proactor.submit(ReadAt::new(file.handle, 0, buf));

        // Drop the proactor while the operation is still outstanding. Shutdown
        // must cancel it and drain its completion rather than closing the port
        // underneath the kernel.
        drop(proactor);
        submitted
    };
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown must not hang waiting on an outstanding operation (took {elapsed:?})"
    );

    // The future still holds its reference; dropping it releases the last one.
    drop(submitted);
}
