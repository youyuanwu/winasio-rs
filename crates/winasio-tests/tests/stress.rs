// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Cancellation soak.
//!
//! The redesign's central safety claim is that abandoning an in-flight
//! operation is safe: the allocation survives until Windows delivers the
//! completion, then is released exactly once. This exercises that under
//! sustained load with randomised abandonment, against both backends, and
//! asserts exact accounting rather than the absence of a crash.
//!
//! The full soak is `#[ignore]`d so `cargo test` stays fast; CI runs it with
//! `-- --ignored`. A reduced smoke variant runs by default.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use winasio::iocp::{live_operations, IntoInner, OpCode, Proactor, ReadAt, ThreadPoolIo, WriteAt};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, GetTempFileNameW, GetTempPathW, CREATE_ALWAYS, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_SHARE_NONE,
};

/// `live_operations` is process-global, so soak tests serialise.
static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn counter_guard() -> std::sync::MutexGuard<'static, ()> {
    COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every operation reaches exactly one terminal outcome; these count them.
static RESOLVED: AtomicUsize = AtomicUsize::new(0);
static ABANDONED: AtomicUsize = AtomicUsize::new(0);

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

/// Fails to start, so the soak also covers the unwind path.
struct FailingOp {
    buffer: Vec<u8>,
}

unsafe impl OpCode for FailingOp {
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

impl IntoInner for FailingOp {
    type Inner = Vec<u8>;
    fn into_inner(self) -> Vec<u8> {
        self.buffer
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

/// Prepare a file with content so reads have something to return.
fn seed_file(handle: HANDLE) {
    let proactor = Proactor::new().unwrap();
    proactor.attach(handle).unwrap();
    let payload = vec![0xA5u8; 64 * 1024];
    let mut fut = Box::pin(proactor.submit(WriteAt::new(handle, 0, payload)));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(r) = poll_once(&mut fut) {
            r.into_result().unwrap();
            break;
        }
        proactor.poll(Some(Duration::from_millis(5))).unwrap();
        assert!(Instant::now() < deadline, "seeding timed out");
    }
}

struct SoakConfig {
    operations: usize,
    min_duration: Option<Duration>,
    abandon_low: f64,
    abandon_high: f64,
    failure_rate: f64,
    seed: u64,
}

fn soak_own_port(cfg: &SoakConfig) -> (usize, usize) {
    let file = TempFile::create(w!("sk1"));
    let seeder = TempFile::create(w!("sk0"));
    seed_file(seeder.handle);

    let proactor = Proactor::new().unwrap();
    proactor.attach(file.handle).unwrap();

    let mut rng = StdRng::seed_from_u64(cfg.seed);
    let started = Instant::now();
    let mut submitted = 0usize;
    let mut abandoned = 0usize;

    // Keep going until both the operation count and the duration floor are
    // satisfied. The elapsed-time guard is generous so a fast machine still
    // sustains load rather than finishing early.
    while submitted < cfg.operations || cfg.min_duration.is_some_and(|d| started.elapsed() < d) {
        let abandon = rng.random_bool((cfg.abandon_low + cfg.abandon_high) / 2.0);
        let fail = rng.random_bool(cfg.failure_rate);

        if fail {
            let mut fut = Box::pin(proactor.submit(FailingOp {
                buffer: vec![0u8; 32],
            }));
            let out = poll_once(&mut fut).expect("a failed start resolves immediately");
            assert!(out.is_err());
            RESOLVED.fetch_add(1, Ordering::Relaxed);
        } else if abandon {
            // Submit and immediately drop, without awaiting.
            let fut = proactor.submit(ReadAt::new(file.handle, 0, Vec::with_capacity(4096)));
            drop(fut);
            abandoned += 1;
            ABANDONED.fetch_add(1, Ordering::Relaxed);
        } else {
            let mut fut =
                Box::pin(proactor.submit(ReadAt::new(file.handle, 0, Vec::with_capacity(4096))));
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if poll_once(&mut fut).is_some() {
                    RESOLVED.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                proactor.poll(Some(Duration::from_millis(1))).unwrap();
                assert!(Instant::now() < deadline, "operation stalled");
            }
        }
        submitted += 1;

        // Keep draining abandoned completions so they do not pile up.
        if submitted.is_multiple_of(64) {
            proactor.poll(Some(Duration::from_millis(1))).unwrap();
        }
    }

    // Drain everything still outstanding.
    let deadline = Instant::now() + Duration::from_secs(30);
    while proactor.pending_count() > 0 && Instant::now() < deadline {
        proactor.poll(Some(Duration::from_millis(10))).unwrap();
    }

    let ratio = abandoned as f64 / submitted as f64;
    assert!(
        ratio >= cfg.abandon_low && ratio <= cfg.abandon_high,
        "abandonment ratio {ratio:.3} outside [{}, {}] (seed {})",
        cfg.abandon_low,
        cfg.abandon_high,
        cfg.seed
    );
    assert_eq!(
        proactor.pending_count(),
        0,
        "every operation reached a terminal outcome (seed {})",
        cfg.seed
    );
    (submitted, abandoned)
}

fn soak_thread_pool(cfg: &SoakConfig) -> (usize, usize) {
    let file = TempFile::create(w!("sk2"));
    // A handle can only ever be registered once, so seed through the same
    // registration the soak will use.
    let pool = ThreadPoolIo::new(file.handle).unwrap();
    seed_via_pool(&pool, file.handle);

    let mut rng = StdRng::seed_from_u64(cfg.seed ^ 0x5555);
    let started = Instant::now();
    let mut submitted = 0usize;
    let mut abandoned = 0usize;

    // Keep going until both the operation count and the duration floor are
    // satisfied. The elapsed-time guard is generous so a fast machine still
    // sustains load rather than finishing early.
    while submitted < cfg.operations || cfg.min_duration.is_some_and(|d| started.elapsed() < d) {
        if rng.random_bool(cfg.failure_rate) {
            let mut fut = Box::pin(pool.submit(FailingOp {
                buffer: vec![0u8; 32],
            }));
            let out = poll_once(&mut fut).expect("a failed start resolves immediately");
            assert!(out.is_err());
            RESOLVED.fetch_add(1, Ordering::Relaxed);
        } else if rng.random_bool((cfg.abandon_low + cfg.abandon_high) / 2.0) {
            drop(pool.submit(ReadAt::new(file.handle, 0, Vec::with_capacity(4096))));
            abandoned += 1;
            ABANDONED.fetch_add(1, Ordering::Relaxed);
        } else {
            let mut fut =
                Box::pin(pool.submit(ReadAt::new(file.handle, 0, Vec::with_capacity(4096))));
            let deadline = Instant::now() + Duration::from_secs(5);
            while poll_once(&mut fut).is_none() {
                std::thread::sleep(Duration::from_micros(200));
                assert!(Instant::now() < deadline, "operation stalled");
            }
            RESOLVED.fetch_add(1, Ordering::Relaxed);
        }
        submitted += 1;
    }

    let ratio = abandoned as f64 / submitted as f64;
    assert!(
        ratio >= cfg.abandon_low && ratio <= cfg.abandon_high,
        "abandonment ratio {ratio:.3} outside [{}, {}] (seed {})",
        cfg.abandon_low,
        cfg.abandon_high,
        cfg.seed
    );

    // Dropping the registration cancels and drains everything outstanding.
    drop(pool);
    (submitted, abandoned)
}

fn seed_via_pool(pool: &ThreadPoolIo, handle: HANDLE) {
    let mut fut = Box::pin(pool.submit(WriteAt::new(handle, 0, vec![0x5Au8; 64 * 1024])));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(r) = poll_once(&mut fut) {
            r.into_result().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
        assert!(Instant::now() < deadline, "seeding timed out");
    }
}

#[test]
fn soak_smoke_own_port() {
    let _guard = counter_guard();
    let baseline = live_operations();

    soak_own_port(&SoakConfig {
        operations: 500,
        min_duration: None,
        abandon_low: 0.30,
        abandon_high: 0.70,
        failure_rate: 0.05,
        seed: 0xC0FFEE,
    });

    wait_for_baseline(baseline);
}

#[test]
fn soak_smoke_thread_pool() {
    let _guard = counter_guard();
    let baseline = live_operations();

    soak_thread_pool(&SoakConfig {
        operations: 500,
        min_duration: None,
        abandon_low: 0.30,
        abandon_high: 0.70,
        failure_rate: 0.05,
        seed: 0xBADF00D,
    });

    wait_for_baseline(baseline);
}

/// The full soak required by the specification: at least 10,000 operations over
/// at least 30 seconds with 40-60% abandonment, asserting exact accounting.
///
/// Ignored by default so `cargo test` stays fast; CI runs `-- --ignored`.
#[test]
#[ignore = "long-running soak; run with --ignored"]
fn soak_full_both_backends() {
    let _guard = counter_guard();
    let baseline = live_operations();
    let seed = 0x5EED_1234;
    println!("soak seed: {seed:#x}");

    let cfg = SoakConfig {
        operations: 10_000,
        min_duration: Some(Duration::from_secs(16)),
        abandon_low: 0.40,
        abandon_high: 0.60,
        failure_rate: 0.02,
        seed,
    };

    RESOLVED.store(0, Ordering::SeqCst);
    ABANDONED.store(0, Ordering::SeqCst);

    let started = Instant::now();
    let (sub_a, aband_a) = soak_own_port(&cfg);
    let (sub_b, aband_b) = soak_thread_pool(&cfg);
    let elapsed = started.elapsed();

    let submitted = sub_a + sub_b;
    let abandoned = aband_a + aband_b;
    let resolved = RESOLVED.load(Ordering::SeqCst);

    assert!(
        submitted >= 2 * cfg.operations,
        "each backend must run at least {} operations, got {submitted} total",
        cfg.operations
    );
    // Every operation reached exactly one terminal outcome: delivered to its
    // caller, or released after abandonment.
    assert_eq!(
        resolved + abandoned,
        submitted,
        "terminal outcomes ({resolved} resolved + {abandoned} abandoned) must equal \
         submissions ({submitted})"
    );
    assert_eq!(
        ABANDONED.load(Ordering::SeqCst),
        abandoned,
        "abandonment accounting must agree across backends"
    );

    assert!(
        elapsed >= Duration::from_secs(30),
        "the soak must sustain load for at least 30s (took {elapsed:?}); \
         raise the operation count if the machine is fast"
    );

    wait_for_baseline(baseline);
    println!(
        "soak complete: {submitted} submitted = {resolved} resolved + {abandoned} abandoned, {elapsed:?}"
    );
}

/// Abandoned operations are released only when their completion arrives, so the
/// counter returns to baseline slightly after the last submission.
fn wait_for_baseline(baseline: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while live_operations() > baseline && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        live_operations(),
        baseline,
        "every operation allocation must be released exactly once"
    );
}
