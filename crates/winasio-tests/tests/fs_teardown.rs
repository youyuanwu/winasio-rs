// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

mod common;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use winasio::fs::test_util::{pending_read_file, DropProbeBuf};
use winasio::fs::{OpenOptions, ReadOutcome};
use winasio::iocp::{live_operations, OpResult, Proactor, ThreadPool};
use windows::Win32::Foundation::{ERROR_INVALID_HANDLE, ERROR_OPERATION_ABORTED};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn temp_path(name: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "winasio-fs-teardown-{}-{name}-{n}.tmp",
        std::process::id()
    ))
}

fn wait_for_baseline(proactor: Option<&Proactor>, baseline: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while live_operations() > baseline && Instant::now() < deadline {
        if let Some(proactor) = proactor {
            let _ = proactor.poll(Some(Duration::from_millis(5)));
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    assert_eq!(live_operations(), baseline);
}

fn assert_pending<F: Future>(future: &mut Pin<Box<F>>) {
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(future.as_mut().poll(&mut cx), Poll::Pending),
        "operation must be genuinely pending before teardown is exercised"
    );
}

fn assert_not_invalid_handle<T>(result: &windows::core::Result<T>) {
    if let Err(e) = result {
        assert_ne!(
            e.code(),
            ERROR_INVALID_HANDLE.to_hresult(),
            "owner drop must not turn a pending operation into a stale-handle failure"
        );
    }
}

#[test]
fn thread_pool_drop_with_operation_future_held_reclaims() {
    let _guard = common::serial();
    let baseline = live_operations();

    let (file, _peer) = pending_read_file(&ThreadPool).unwrap();
    let mut read = Box::pin(file.read_at(0, Vec::with_capacity(64)));
    assert_pending(&mut read);
    assert!(
        live_operations() > baseline,
        "the pending read must have an operation record in flight"
    );

    let started = Instant::now();
    drop(file);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "thread-pool owner drop must be bounded"
    );

    let OpResult(result, buffer) = common::block_on(read);
    assert_not_invalid_handle(&result);
    match result {
        Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => {}
        other => panic!("expected cancellation after owner drop, got {other:?}"),
    }
    assert!(buffer.capacity() >= 64);
    wait_for_baseline(None, baseline);
}

#[test]
fn caller_driven_drop_returns_then_drive_reclaims_held_future() {
    let _guard = common::serial();
    let baseline = live_operations();

    let proactor = Rc::new(Proactor::new().unwrap());
    let (file, _peer) = pending_read_file(&proactor).unwrap();
    let mut read = Box::pin(file.read_at(0, Vec::with_capacity(64)));
    assert_pending(&mut read);
    assert!(
        live_operations() > baseline,
        "the pending read must have an operation record in flight"
    );

    let started = Instant::now();
    drop(file);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "caller-driven owner drop must not wait for completions"
    );
    assert!(
        live_operations() > baseline,
        "without driving the proactor, the operation record must remain live"
    );

    let OpResult(result, buffer) = proactor.block_on(read);
    assert_not_invalid_handle(&result);
    match result {
        Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => {}
        other => panic!("expected cancellation after owner drop, got {other:?}"),
    }
    assert!(buffer.capacity() >= 64);
    wait_for_baseline(Some(&proactor), baseline);
}

#[test]
fn caller_driven_late_future_drop_does_not_poison_next_handle() {
    let _guard = common::serial();
    let baseline = live_operations();

    let proactor = Rc::new(Proactor::new().unwrap());
    let (file, _peer) = pending_read_file(&proactor).unwrap();
    let mut read = Box::pin(file.read_at(0, Vec::with_capacity(64)));
    assert_pending(&mut read);
    assert!(live_operations() > baseline);

    drop(file);
    drop(read);
    assert!(
        live_operations() > baseline,
        "dropping the future before driving should abandon, not reclaim inline"
    );
    wait_for_baseline(Some(&proactor), baseline);

    let path = temp_path("after-late-drop");
    std::fs::write(&path, b"abc").unwrap();
    let mut options = OpenOptions::new();
    options.read(true);
    let reopened = options.open(&proactor, &path).unwrap();
    let OpResult(result, buf) = proactor.block_on(reopened.read_at(0, Vec::with_capacity(8)));
    assert_eq!(result.unwrap(), ReadOutcome::Bytes(3));
    assert_eq!(buf, b"abc");
    drop(reopened);
    let _ = std::fs::remove_file(path);
}

#[test]
fn dropping_in_flight_future_does_not_return_buffer_and_reclaims() {
    let _guard = common::serial();
    let baseline = live_operations();

    let (file, _peer) = pending_read_file(&ThreadPool).unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let buffer = DropProbeBuf::with_capacity(64, Arc::clone(&drops));
    let mut read = Box::pin(file.read_at(0, buffer));
    assert_pending(&mut read);
    assert!(live_operations() > baseline);

    drop(read);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "dropping the future must not return or drop the buffer synchronously"
    );
    wait_for_baseline(None, baseline);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "the abandoned buffer is dropped only after the cancellation completion"
    );
    drop(file);
}
