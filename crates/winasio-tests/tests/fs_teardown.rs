// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

mod common;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use winasio::fs::{OpenOptions, ReadOutcome};
use winasio::iocp::{live_operations, OpResult, Proactor, ThreadPool};

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

#[test]
fn thread_pool_drop_with_operation_future_held_reclaims() {
    let _guard = common::serial();
    let baseline = live_operations();
    let path = temp_path("pool-held");
    std::fs::write(&path, b"abc").unwrap();

    let mut options = OpenOptions::new();
    options.read(true);
    let file = options.open(&ThreadPool, &path).unwrap();
    let read = file.read_at(1024 * 1024, Vec::with_capacity(64));
    drop(file);

    let OpResult(result, buffer) = common::block_on(read);
    match result {
        Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Bytes(_)) => {}
        Err(e) if e.code() == windows::Win32::Foundation::ERROR_OPERATION_ABORTED.to_hresult() => {}
        other => panic!("unexpected result after owner drop: {other:?}"),
    }
    assert!(buffer.capacity() >= 64);
    wait_for_baseline(None, baseline);
    let _ = std::fs::remove_file(path);
}

#[test]
fn caller_driven_drop_returns_then_drive_reclaims() {
    let _guard = common::serial();
    let baseline = live_operations();
    let path = temp_path("proactor-held");
    std::fs::write(&path, b"abc").unwrap();

    let proactor = Rc::new(Proactor::new().unwrap());
    let mut options = OpenOptions::new();
    options.read(true);
    let file = options.open(&proactor, &path).unwrap();
    let read = file.read_at(1024 * 1024, Vec::with_capacity(64));

    let started = Instant::now();
    drop(file);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "caller-driven owner drop must not wait for completions"
    );

    let OpResult(result, buffer) = proactor.block_on(read);
    match result {
        Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Bytes(_)) => {}
        Err(e) if e.code() == windows::Win32::Foundation::ERROR_OPERATION_ABORTED.to_hresult() => {}
        other => panic!("unexpected result after owner drop: {other:?}"),
    }
    assert!(buffer.capacity() >= 64);
    wait_for_baseline(Some(&proactor), baseline);
    let _ = std::fs::remove_file(path);
}

#[test]
fn dropping_unresolved_future_after_owner_drop_does_not_poison_next_handle() {
    let _guard = common::serial();
    let baseline = live_operations();
    let path = temp_path("late-future-drop");
    std::fs::write(&path, b"abc").unwrap();

    let mut options = OpenOptions::new();
    options.read(true);
    let file = options.open(&ThreadPool, &path).unwrap();
    let read = file.read_at(1024 * 1024, Vec::with_capacity(64));
    drop(file);
    drop(read);
    wait_for_baseline(None, baseline);

    let reopened = options.open(&ThreadPool, &path).unwrap();
    let OpResult(result, buf) = common::block_on(reopened.read_at(0, Vec::with_capacity(8)));
    assert_eq!(result.unwrap(), ReadOutcome::Bytes(3));
    assert_eq!(buf, b"abc");
    drop(reopened);
    let _ = std::fs::remove_file(path);
}
