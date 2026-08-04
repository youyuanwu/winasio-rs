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

mod common;

use std::rc::Rc;
use std::time::Duration;

use winasio::iocp::{
    live_operations, OpResult, Proactor, ReadAt, Registrar, RegistrationError, ThreadPool,
    ThreadPoolIo, WriteAt,
};
use winasio::pipe::{ClientOptions, NamedPipe, ReadOutcome, ServerOptions};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_OPERATION_ABORTED, GENERIC_WRITE, HANDLE};
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
    w.0.unwrap();

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
    w.0.unwrap();

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
    /// `None`: this never starts, so no completion packet can follow.
    fn handle(&self) -> Option<HANDLE> {
        None
    }

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

// --- synchronous completion on both backends (FR-010, SC-011) -----------

/// Completes inline without touching Windows, so the synchronous path is
/// deterministic. Real file I/O essentially never completes inline.
struct InlineOp {
    outcome: std::result::Result<usize, windows::core::Error>,
    completed_with: Option<usize>,
    buffer: Vec<u8>,
}

unsafe impl winasio::iocp::OpCode for InlineOp {
    /// `None`: this operation is not backed by a handle, so no completion
    /// packet can follow its inline result.
    fn handle(&self) -> Option<HANDLE> {
        None
    }

    unsafe fn operate(
        &mut self,
        _optr: *mut windows::Win32::System::IO::OVERLAPPED,
    ) -> std::task::Poll<windows::core::Result<usize>> {
        std::task::Poll::Ready(self.outcome.clone())
    }

    unsafe fn on_complete(&mut self, result: &windows::core::Result<usize>) {
        self.completed_with = result.as_ref().ok().copied();
    }
}

impl winasio::iocp::IntoInner for InlineOp {
    type Inner = (Vec<u8>, Option<usize>);
    fn into_inner(self) -> Self::Inner {
        (self.buffer, self.completed_with)
    }
}

fn assert_inline_success(result: windows::core::Result<usize>, buf: Vec<u8>, seen: Option<usize>) {
    assert_eq!(result.unwrap(), 17, "the transferred count survives");
    assert_eq!(buf, vec![1, 2, 3], "the buffer comes back");
    assert_eq!(seen, Some(17), "on_complete runs even with no packet");
}

#[test]
fn inline_success_own_port() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bpf"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    let submitted = proactor.submit(InlineOp {
        outcome: Ok(17),
        completed_with: None,
        buffer: vec![1, 2, 3],
    });
    assert!(
        submitted.is_ready(),
        "an inline op resolves without polling"
    );

    let (result, (buf, seen)) = drive_own_port(&proactor, submitted).into_inner_parts();
    assert_inline_success(result, buf, seen);

    assert_eq!(
        proactor.poll(Some(Duration::from_millis(20))).unwrap(),
        0,
        "an inline completion must not also queue a packet"
    );
}

#[test]
fn inline_success_thread_pool() {
    let _guard = counter_guard();
    let baseline = live_operations();
    let file = TempFile::create(w!("bpg"));
    let pool = ThreadPoolIo::new(file.handle).unwrap();

    let submitted = pool.submit(InlineOp {
        outcome: Ok(17),
        completed_with: None,
        buffer: vec![1, 2, 3],
    });
    assert!(
        submitted.is_ready(),
        "an inline op resolves without polling"
    );

    let (result, (buf, seen)) = drive_thread_pool(submitted).into_inner_parts();
    assert_inline_success(result, buf, seen);

    // If StartThreadpoolIo were left unbalanced here, this drop would hang.
    let started = std::time::Instant::now();
    drop(pool);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the inline path must balance the thread-pool count"
    );
    assert_eq!(live_operations(), baseline);
}

// --- both backends active at once (FR-013) ------------------------------

#[test]
fn both_backends_can_be_active_simultaneously() {
    let _guard = counter_guard();
    let a = TempFile::create(w!("bph"));
    let b = TempFile::create(w!("bpi"));

    let proactor = Proactor::new().unwrap();
    proactor.attach(a.handle).unwrap();
    let pool = ThreadPoolIo::new(b.handle).unwrap();

    let payload = b"coexisting backends".to_vec();
    let expected = payload.len();

    // Interleave work on both, with each registration live throughout.
    let w1 = drive_own_port(
        &proactor,
        proactor.submit(WriteAt::new(a.handle, 0, payload.clone())),
    );
    let w2 = drive_thread_pool(pool.submit(WriteAt::new(b.handle, 0, payload)));
    assert_eq!(w1.0.unwrap(), expected);
    assert_eq!(w2.0.unwrap(), expected);

    let r1 = drive_own_port(
        &proactor,
        proactor.submit(ReadAt::new(a.handle, 0, Vec::with_capacity(expected))),
    );
    let r2 = drive_thread_pool(pool.submit(ReadAt::new(b.handle, 0, Vec::with_capacity(expected))));
    let (_, buf1) = r1.into_inner_parts();
    let (_, buf2) = r2.into_inner_parts();
    assert_eq!(buf1, buf2, "both backends produced the same bytes");
    assert_eq!(buf1.len(), expected);
}

// --- foreign completion packets (safety regression) ---------------------

/// A completion packet on an attached handle is not necessarily ours: the
/// completion key is set per handle, so any overlapped call the caller makes on
/// that handle produces a packet carrying it.
///
/// If ownership were decided by reading a header out of the packet, this would
/// read past the caller's `OVERLAPPED` — an out-of-bounds read that access-
/// violates when the allocation sits at the end of a page.
#[test]
fn a_foreign_overlapped_on_an_attached_handle_is_ignored() {
    let _guard = counter_guard();
    let file = TempFile::create(w!("bpj"));
    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    // Give the file some content to read.
    let w = drive_own_port(
        &proactor,
        proactor.submit(WriteAt::new(file.handle, 0, vec![7u8; 256])),
    );
    w.0.unwrap();

    // Issue an overlapped read the crate knows nothing about. Its OVERLAPPED is
    // the last field of the allocation, so any read past it is out of bounds.
    #[repr(C)]
    struct Foreign {
        pad: [u8; 64],
        overlapped: windows::Win32::System::IO::OVERLAPPED,
    }
    let mut foreign = Box::new(Foreign {
        pad: [0xCD; 64],
        overlapped: Default::default(),
    });
    let mut buf = vec![0u8; 128];

    let optr = std::ptr::addr_of_mut!(foreign.overlapped);
    let started = unsafe {
        windows::Win32::Storage::FileSystem::ReadFile(
            file.handle,
            Some(buf.as_mut_slice()),
            None,
            Some(optr),
        )
    };
    let pending = started.is_err()
        && windows::core::Error::from_thread().code()
            == windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult();
    assert!(
        started.is_ok() || pending,
        "the foreign read should start or complete"
    );

    // The proactor must ignore this packet rather than read through it.
    let delivered = proactor.poll(Some(Duration::from_millis(200))).unwrap();
    assert_eq!(
        delivered, 0,
        "a foreign completion must not be dispatched as one of ours"
    );

    // And the proactor must still work afterwards.
    let r = drive_own_port(
        &proactor,
        proactor.submit(ReadAt::new(file.handle, 0, Vec::with_capacity(256))),
    );
    let (result, got) = r.into_inner_parts();
    result.unwrap();
    assert_eq!(
        got.len(),
        256,
        "the proactor still serves its own operations"
    );
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
        assert_eq!(written.0.unwrap(), expected.len());

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

// ---------------------------------------------------------------------------
// The registrar/submitter abstraction.
//
// Everything above names a concrete backend. These name none: the body is
// written against the traits, and only the registrar differs at the call site.
// ---------------------------------------------------------------------------

/// One round trip, expressed without naming a backend.
fn trait_round_trip<S: winasio::iocp::Submitter>(
    io: &S,
    handle: HANDLE,
    drive: impl Fn(&mut dyn FnMut() -> bool),
) {
    let payload = b"through the trait".to_vec();
    let expected = payload.clone();

    let mut write = Box::pin(winasio::iocp::Submitter::submit(
        io,
        WriteAt::new(handle, 0, payload),
    ));
    let mut done = None;
    drive(&mut || match poll_once(&mut write) {
        Some(r) => {
            done = Some(r);
            true
        }
        None => false,
    });
    assert_eq!(done.expect("write completed").0.unwrap(), expected.len());

    let mut read = Box::pin(winasio::iocp::Submitter::submit(
        io,
        ReadAt::new(handle, 0, Vec::with_capacity(expected.len())),
    ));
    let mut done = None;
    drive(&mut || match poll_once(&mut read) {
        Some(r) => {
            done = Some(r);
            true
        }
        None => false,
    });
    let (result, buf) = done.expect("read completed").into_inner_parts();
    result.unwrap();
    assert_eq!(buf, expected, "same body, either backend");
}

/// Poll a future once with a no-op waker, returning its output if ready.
// (reuses the `poll_once` defined above)

#[test]
fn trait_round_trip_own_port() {
    use std::rc::Rc;
    use winasio::iocp::Registrar;

    // Creating operations perturbs the process-global counter other tests in
    // this binary assert on, so serialise with them.
    let _guard = counter_guard();
    let file = TempFile::create(w!("trt"));
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    let io = proactor.register(file.handle).expect("register");

    trait_round_trip(&io, file.handle, |ready| {
        while !ready() {
            proactor
                .poll(Some(Duration::from_millis(100)))
                .expect("poll");
        }
    });
}

#[test]
fn trait_round_trip_thread_pool() {
    use winasio::iocp::{Registrar, ThreadPool};

    // See `trait_round_trip_own_port`: this creates operations too.
    let _guard = counter_guard();
    let file = TempFile::create(w!("trtp"));
    let io = ThreadPool.register(file.handle).expect("register");

    trait_round_trip(&io, file.handle, |ready| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "completion never arrived"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    });
}

#[test]
fn registering_the_same_handle_twice_fails_through_the_trait() {
    use winasio::iocp::{Registrar, ThreadPool};

    let file = TempFile::create(w!("trdup"));
    let _first = ThreadPool
        .register(file.handle)
        .expect("first registration");
    let second = ThreadPool.register(file.handle);
    assert!(
        matches!(second, Err(RegistrationError::AlreadyRegistered(_))),
        "a handle belongs to exactly one completion mechanism"
    );
}

#[test]
fn thread_pool_handles_skip_the_port_on_inline_success() {
    // `ConnectPipe` reports an already-connected client as an inline success and
    // relies on no completion packet following. That is only sound because this
    // flag is accepted; if it were ever false, teardown would block forever on a
    // pending-I/O count nothing clears.
    let file = TempFile::create(w!("trskip"));
    let io = ThreadPoolIo::new(file.handle).expect("register");
    assert!(
        io.skips_on_success(),
        "inline-success operations depend on this"
    );
}

trait PipeTestRegistrar: Registrar {
    const EXCHANGE_NAME: &'static str;
    const TEARDOWN_NAME: &'static str;

    fn drive<F: std::future::Future>(&self, future: F) -> F::Output;
    fn wait_for_baseline(&self, baseline: usize);
}

impl PipeTestRegistrar for ThreadPool {
    const EXCHANGE_NAME: &'static str = "backend_pipe_exchange_thread_pool";
    const TEARDOWN_NAME: &'static str = "backend_pipe_teardown_thread_pool";

    fn drive<F: std::future::Future>(&self, future: F) -> F::Output {
        drive_thread_pool(future)
    }

    fn wait_for_baseline(&self, baseline: usize) {
        wait_for_pipe_counter_baseline(None, baseline);
    }
}

impl PipeTestRegistrar for Rc<Proactor> {
    const EXCHANGE_NAME: &'static str = "backend_pipe_exchange_own_port";
    const TEARDOWN_NAME: &'static str = "backend_pipe_teardown_own_port";

    fn drive<F: std::future::Future>(&self, future: F) -> F::Output {
        drive_own_port(self.as_ref(), future)
    }

    fn wait_for_baseline(&self, baseline: usize) {
        wait_for_pipe_counter_baseline(Some(self.as_ref()), baseline);
    }
}

fn pipe_pair<R: PipeTestRegistrar>(
    registrar: &R,
    test_name: &str,
) -> (NamedPipe<R::Io>, NamedPipe<R::Io>) {
    let name = common::unique_pipe_name(test_name);
    let server = ServerOptions::new(&name)
        .create(registrar)
        .expect("create server");
    let accept = server.connect();
    let client = ClientOptions::new(&name)
        .connect(registrar)
        .expect("connect client");
    let server = registrar.drive(accept).expect("accept client");
    (server, client)
}

fn wait_for_pipe_counter_baseline(proactor: Option<&Proactor>, baseline: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while live_operations() > baseline && std::time::Instant::now() < deadline {
        if let Some(proactor) = proactor {
            let _ = proactor.poll(Some(Duration::from_millis(5)));
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    assert_eq!(
        live_operations(),
        baseline,
        "pipe operation records must return to their pre-test baseline"
    );
}

fn assert_pipe_cancelled(result: windows::core::Result<ReadOutcome>) {
    match result {
        Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => {}
        other => panic!("expected cancelled in-flight pipe read, got {other:?}"),
    }
}

fn pipe_exchange_body<R: PipeTestRegistrar>(registrar: &R) {
    let (server, client) = pipe_pair(registrar, R::EXCHANGE_NAME);

    let OpResult(written, returned) = registrar.drive(client.write(b"request".to_vec()));
    assert_eq!(written.unwrap(), returned.len());
    let OpResult(read, got) = registrar.drive(server.read(Vec::with_capacity(16)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(returned.len()));
    assert_eq!(got, returned);

    let OpResult(written, returned) = registrar.drive(server.write(b"response".to_vec()));
    assert_eq!(written.unwrap(), returned.len());
    let OpResult(read, got) = registrar.drive(client.read(Vec::with_capacity(16)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(returned.len()));
    assert_eq!(got, returned);
}

#[test]
fn pipe_exchange_own_port_registrar() {
    let _guard = counter_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    pipe_exchange_body(&proactor);
}

#[test]
fn pipe_exchange_thread_pool_registrar() {
    let _guard = counter_guard();
    pipe_exchange_body(&ThreadPool);
}

fn pipe_teardown_body<R: PipeTestRegistrar>(registrar: &R) {
    let baseline = live_operations();
    let (server, _client) = pipe_pair(registrar, R::TEARDOWN_NAME);

    let mut read = Box::pin(server.read(Vec::with_capacity(64)));
    assert!(
        poll_once(&mut read).is_none(),
        "pipe read must be in flight before teardown"
    );
    assert!(live_operations() > baseline);

    let started = std::time::Instant::now();
    drop(server);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "dropping a pipe owner must be bounded"
    );

    let OpResult(result, buffer) = registrar.drive(read);
    assert_pipe_cancelled(result);
    assert!(buffer.capacity() >= 64);
    registrar.wait_for_baseline(baseline);
}

#[test]
fn pipe_teardown_with_in_flight_read_own_port_registrar() {
    let _guard = counter_guard();
    let proactor = Rc::new(Proactor::new().expect("proactor"));
    pipe_teardown_body(&proactor);
}

#[test]
fn pipe_teardown_with_in_flight_read_thread_pool_registrar() {
    let _guard = counter_guard();
    pipe_teardown_body(&ThreadPool);
}
