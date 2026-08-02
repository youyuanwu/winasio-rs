// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Waiting on signalable handles through the operation model.

use std::time::{Duration, Instant};

use winasio::iocp::{live_operations, Proactor, WaitForHandle};
use winasio::sys::event::ManualResetEvent;

/// `live_operations` is process-global, so tests asserting on it serialise.
static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn counter_guard() -> std::sync::MutexGuard<'static, ()> {
    COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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

#[test]
fn a_wait_completes_when_the_handle_is_signalled() {
    let _guard = counter_guard();
    let proactor = Proactor::new().unwrap();
    let event = ManualResetEvent::new();

    let mut waiting = Box::pin(proactor.submit(WaitForHandle::new(&proactor, event.get())));

    // Not yet signalled.
    assert!(
        poll_once(&mut waiting).is_none(),
        "the wait must not complete before the handle signals"
    );
    proactor.poll(Some(Duration::from_millis(20))).unwrap();
    assert!(poll_once(&mut waiting).is_none(), "still unsignalled");

    // Signal it; the wait callback posts the completion to the port.
    event.set().unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let done = loop {
        if let Some(out) = poll_once(&mut waiting) {
            break out;
        }
        proactor.poll(Some(Duration::from_millis(5))).unwrap();
        assert!(Instant::now() < deadline, "the wait never completed");
    };
    done.into_result().expect("the wait succeeds");
}

#[test]
fn a_signalled_handle_completes_promptly() {
    let _guard = counter_guard();
    let proactor = Proactor::new().unwrap();
    let event = ManualResetEvent::new();
    // Already signalled before the wait is registered.
    event.set().unwrap();

    let mut waiting = Box::pin(proactor.submit(WaitForHandle::new(&proactor, event.get())));

    let deadline = Instant::now() + Duration::from_secs(5);
    let done = loop {
        if let Some(out) = poll_once(&mut waiting) {
            break out;
        }
        proactor.poll(Some(Duration::from_millis(5))).unwrap();
        assert!(Instant::now() < deadline, "the wait never completed");
    };
    done.into_result().unwrap();
}

#[test]
fn abandoning_a_wait_does_not_block_and_releases_it() {
    let _guard = counter_guard();
    let baseline = live_operations();

    let proactor = Proactor::new().unwrap();
    let event = ManualResetEvent::new();

    let started = Instant::now();
    {
        // Never signalled; drop the future while the wait is registered.
        let waiting = proactor.submit(WaitForHandle::new(&proactor, event.get()));
        drop(waiting);
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "dropping a wait must not block on its callback (took {:?})",
        started.elapsed()
    );

    // Signal so the callback fires and the allocation is released.
    event.set().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while live_operations() > baseline && Instant::now() < deadline {
        proactor.poll(Some(Duration::from_millis(10))).unwrap();
    }
    assert_eq!(
        live_operations(),
        baseline,
        "an abandoned wait is released once its callback runs"
    );
}

#[test]
fn manual_reset_event_still_works_standalone() {
    let e = ManualResetEvent::new();
    e.set().unwrap();
    e.reset().unwrap();

    let mut e1 = ManualResetEvent::default();
    let mut e2 = ManualResetEvent::new();
    let h = e2.release();
    e1.assign(h);
    e1.set().unwrap();
}

#[test]
fn many_waits_can_be_outstanding_at_once() {
    let _guard = counter_guard();
    let proactor = Proactor::new().unwrap();
    let events: Vec<ManualResetEvent> = (0..8).map(|_| ManualResetEvent::new()).collect();

    let mut pending: Vec<_> = events
        .iter()
        .map(|e| Box::pin(proactor.submit(WaitForHandle::new(&proactor, e.get()))))
        .collect();

    for e in &events {
        e.set().unwrap();
    }

    let mut resolved = 0usize;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pending.is_empty() {
        pending.retain_mut(|w| {
            if let Some(out) = poll_once(w) {
                out.into_result().expect("each wait succeeds");
                resolved += 1;
                false
            } else {
                true
            }
        });
        if !pending.is_empty() {
            proactor.poll(Some(Duration::from_millis(5))).unwrap();
        }
        assert!(Instant::now() < deadline, "not all waits completed");
    }

    assert_eq!(resolved, 8, "every wait resolves exactly once");
}
