// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! One operation suite run against both completion backends.
//!
//! The point is that an `OpCode` is written once and behaves identically
//! whichever backend delivers its completion. Every case below therefore has a
//! `_own_port` and a `_thread_pool` variant driven by the same body.

use std::time::Duration;

use winasio::iocp::{live_operations, Proactor, ReadAt, RegistrationError, ThreadPoolIo, WriteAt};
use windows::core::{w, HSTRING, PCWSTR};
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
    fn create(tag: PCWSTR) -> Self {
        let mut dir = vec![0u16; 260];
        let len = unsafe { GetTempPathW(Some(dir.as_mut_slice())) };
        assert_ne!(len, 0);
        dir.truncate(len as usize);
        let dir = HSTRING::from_wide(&dir);

        let mut name = [0u16; 260];
        let n = unsafe { GetTempFileNameW(&dir, tag, 0, &mut name) };
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

/// Drive a caller-driven proactor while awaiting one operation.
fn drive_own_port<F: std::future::Future>(proactor: &Proactor, fut: F) -> F::Output {
    let mut fut = Box::pin(fut);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(out) = poll_once(&mut fut) {
            return out;
        }
        proactor.poll(Some(Duration::from_millis(5))).unwrap();
        assert!(
            std::time::Instant::now() < deadline,
            "operation did not complete within the deadline"
        );
    }
}

/// Await an operation on the thread pool; no driver is needed.
fn drive_thread_pool<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = Box::pin(fut);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(out) = poll_once(&mut fut) {
            return out;
        }
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            std::time::Instant::now() < deadline,
            "operation did not complete within the deadline"
        );
    }
}

fn poll_once<F: std::future::Future>(fut: &mut std::pin::Pin<Box<F>>) -> Option<F::Output> {
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

// --- round trip ---------------------------------------------------------

fn round_trip_body(
    submit_write: impl FnOnce(WriteAt<Vec<u8>>) -> Vec<u8>,
    submit_read: impl FnOnce(ReadAt<Vec<u8>>) -> Vec<u8>,
    handle: HANDLE,
) {
    let data: Vec<u8> = "HelloWorld".repeat(200).into_bytes();
    let expected = data.clone();

    let returned = submit_write(WriteAt::new(handle, 0, data));
    assert_eq!(returned, expected, "the write buffer comes back intact");

    let got = submit_read(ReadAt::new(handle, 0, Vec::with_capacity(expected.len())));
    assert_eq!(got, expected, "contents round-trip");
}

#[test]
fn round_trip_own_port() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bp1"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    round_trip_body(
        |op| {
            let r = drive_own_port(&proactor, proactor.submit(op));
            let (res, buf) = r.into_inner_parts();
            res.unwrap();
            buf
        },
        |op| {
            let r = drive_own_port(&proactor, proactor.submit(op));
            let (res, buf) = r.into_inner_parts();
            res.unwrap();
            buf
        },
        file.handle,
    );
}

#[test]
fn round_trip_thread_pool() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bp2"));
    let pool = ThreadPoolIo::new(file.handle).unwrap();

    round_trip_body(
        |op| {
            let r = drive_thread_pool(pool.submit(op));
            let (res, buf) = r.into_inner_parts();
            res.unwrap();
            buf
        },
        |op| {
            let r = drive_thread_pool(pool.submit(op));
            let (res, buf) = r.into_inner_parts();
            res.unwrap();
            buf
        },
        file.handle,
    );
}

// --- zero-byte read at end of file --------------------------------------

fn assert_eof_outcome(result: windows::core::Result<usize>, buf: Vec<u8>) {
    match result {
        Ok(0) => {}
        Ok(other) => panic!("expected zero bytes at EOF, got {other}"),
        // Some handle types report EOF as an error instead; either way it is a
        // terminal outcome and the buffer comes back.
        Err(e) => assert_eq!(
            e.code(),
            windows::Win32::Foundation::ERROR_HANDLE_EOF.to_hresult(),
            "EOF must be reported as EOF"
        ),
    }
    assert_eq!(buf.len(), 0, "nothing was read");
}

#[test]
fn zero_byte_read_own_port() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bp3"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    let w = drive_own_port(
        &proactor,
        proactor.submit(WriteAt::new(file.handle, 0, b"abc".to_vec())),
    );
    w.into_result().unwrap();

    let r = drive_own_port(
        &proactor,
        proactor.submit(ReadAt::new(file.handle, 4096, Vec::with_capacity(16))),
    );
    let (result, buf) = r.into_inner_parts();
    assert_eof_outcome(result, buf);
}

#[test]
fn zero_byte_read_thread_pool() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bp4"));
    let pool = ThreadPoolIo::new(file.handle).unwrap();

    let w = drive_thread_pool(pool.submit(WriteAt::new(file.handle, 0, b"abc".to_vec())));
    w.into_result().unwrap();

    let r = drive_thread_pool(pool.submit(ReadAt::new(file.handle, 4096, Vec::with_capacity(16))));
    let (result, buf) = r.into_inner_parts();
    assert_eof_outcome(result, buf);
}

// --- duplicate and cross-backend registration ---------------------------

#[test]
fn duplicate_registration_same_backend_own_port() {
    let file = TempFile::create(w!("bp5"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    assert!(
        matches!(
            proactor.attach(file.handle),
            Err(RegistrationError::AlreadyRegistered(_))
        ),
        "a handle cannot be attached twice"
    );
}

#[test]
fn duplicate_registration_same_backend_thread_pool() {
    let file = TempFile::create(w!("bp6"));
    let _pool = ThreadPoolIo::new(file.handle).unwrap();

    assert!(
        matches!(
            ThreadPoolIo::new(file.handle),
            Err(RegistrationError::AlreadyRegistered(_))
        ),
        "a handle cannot be registered with the pool twice"
    );
}

#[test]
fn cross_backend_registration_port_then_pool_is_rejected() {
    let file = TempFile::create(w!("bp7"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    assert!(
        matches!(
            ThreadPoolIo::new(file.handle),
            Err(RegistrationError::AlreadyRegistered(_))
        ),
        "association is permanent: the pool must refuse a port-bound handle"
    );
}

#[test]
fn cross_backend_registration_pool_then_port_is_rejected() {
    let file = TempFile::create(w!("bp8"));
    let _pool = ThreadPoolIo::new(file.handle).unwrap();

    let proactor = Proactor::new().unwrap();
    assert!(
        matches!(
            proactor.attach(file.handle),
            Err(RegistrationError::AlreadyRegistered(_))
        ),
        "association is permanent: the port must refuse a pool-bound handle"
    );
}

// --- teardown with an operation outstanding -----------------------------

#[test]
fn teardown_with_outstanding_operation_own_port() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bp9"));

    let started = std::time::Instant::now();
    let submitted = {
        let proactor = Proactor::new().unwrap();
        proactor.attach(file.handle).unwrap();
        let op = proactor.submit(ReadAt::new(file.handle, 0, Vec::with_capacity(1 << 20)));
        drop(proactor);
        op
    };
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "shutdown must not hang"
    );
    drop(submitted);
}

#[test]
fn teardown_with_outstanding_operation_thread_pool() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bpa"));

    let started = std::time::Instant::now();
    let submitted = {
        let pool = ThreadPoolIo::new(file.handle).unwrap();
        let op = pool.submit(ReadAt::new(file.handle, 0, Vec::with_capacity(1 << 20)));
        // Dropping the registration must cancel and drain, not strand the
        // callback that owns the operation's reference.
        drop(pool);
        op
    };
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "thread-pool teardown must not hang"
    );
    drop(submitted);
}

// --- abandonment --------------------------------------------------------

#[test]
fn abandoned_operation_is_released_thread_pool() {
    let _guard = counter_guard();
    let baseline = live_operations();

    let file = TempFile::create(w!("bpb"));
    let pool = ThreadPoolIo::new(file.handle).unwrap();

    {
        let op = pool.submit(ReadAt::new(file.handle, 0, Vec::with_capacity(1 << 20)));
        drop(op);
    }

    // The callback still has to run before the allocation is released.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while live_operations() > baseline && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        live_operations(),
        baseline,
        "an abandoned operation is released once its callback runs"
    );
}

// --- start/cancel balance (FR-023) --------------------------------------

/// Fails to start, without touching Windows. If the backend does not balance
/// `StartThreadpoolIo` with `CancelThreadpoolIo`, the pending-I/O count leaks
/// and dropping the registration blocks forever.
struct FailingOp {
    buffer: Vec<u8>,
}

unsafe impl winasio::iocp::OpCode for FailingOp {
    unsafe fn operate(
        &mut self,
        _optr: *mut windows::Win32::System::IO::OVERLAPPED,
    ) -> std::task::Poll<windows::core::Result<usize>> {
        std::task::Poll::Ready(Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult(),
        )))
    }
}

impl winasio::iocp::IntoInner for FailingOp {
    type Inner = Vec<u8>;
    fn into_inner(self) -> Vec<u8> {
        self.buffer
    }
}

#[test]
fn failed_start_balances_the_threadpool_count() {
    let _guard = counter_guard();
    let baseline = live_operations();
    let file = TempFile::create(w!("bpd"));

    let started = std::time::Instant::now();
    {
        let pool = ThreadPoolIo::new(file.handle).unwrap();

        for _ in 0..16 {
            let submitted = pool.submit(FailingOp {
                buffer: vec![0u8; 8],
            });
            assert!(submitted.is_ready(), "a failed start resolves immediately");
            let (result, buf) = drive_thread_pool(submitted).into_inner_parts();
            assert!(result.is_err(), "the failure surfaces");
            assert_eq!(buf.len(), 8, "state comes back on failure");
        }

        // If the count were unbalanced this drop would block until the
        // five-second guard in the test harness, or forever.
        drop(pool);
    }

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "an unbalanced StartThreadpoolIo would hang teardown"
    );
    assert_eq!(
        live_operations(),
        baseline,
        "every failed operation released its allocation"
    );
}

#[test]
fn failed_start_balances_the_own_port_backend() {
    let _guard = counter_guard();
    let baseline = live_operations();
    let file = TempFile::create(w!("bpe"));

    {
        let proactor = Proactor::new().unwrap();
        proactor.attach(file.handle).unwrap();

        for _ in 0..16 {
            let submitted = proactor.submit(FailingOp {
                buffer: vec![0u8; 8],
            });
            assert!(submitted.is_ready());
            let (result, buf) = drive_own_port(&proactor, submitted).into_inner_parts();
            assert!(result.is_err());
            assert_eq!(buf.len(), 8);
        }
        assert_eq!(
            proactor.pending_count(),
            0,
            "a failed start is not tracked as in-flight"
        );
    }

    assert_eq!(live_operations(), baseline);
}

// --- multi-threaded runtime ---------------------------------------------

#[test]
fn thread_pool_backend_works_under_a_multi_threaded_runtime() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bpc"));
    let pool = ThreadPoolIo::new(file.handle).unwrap();
    let pool = std::sync::Arc::new(pool);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let payload = b"across threads".to_vec();
        let expected = payload.clone();

        // The registration crosses a spawn boundary, and so does the future.
        let p = pool.clone();
        let written =
            tokio::spawn(async move { p.submit(WriteAt::new(p.handle(), 0, payload)).await })
                .await
                .unwrap();
        assert_eq!(written.into_result().unwrap(), expected.len());

        let p = pool.clone();
        let capacity = expected.len();
        let read = tokio::spawn(async move {
            p.submit(ReadAt::new(p.handle(), 0, Vec::with_capacity(capacity)))
                .await
        })
        .await
        .unwrap();
        let (result, buf) = read.into_inner_parts();
        result.unwrap();
        assert_eq!(buf, expected, "round-trip across worker threads");
    });
}
