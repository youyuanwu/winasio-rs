// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Proof that arbitrary Windows overlapped APIs can be wrapped from outside the
//! crate.
//!
//! `winasio` contains no reference to named pipes anywhere. Everything below is
//! written against the crate's public API only, with no change to `winasio`
//! required — which is the whole claim of the redesign.
//!
//! Two operation shapes are covered:
//!
//! * `ConnectPipe` owns no buffer at all. Its "result" is the connection itself.
//! * `PipeRead` fills a caller-allocated structure rather than a byte slice,
//!   the shape that a buffer trait could not express.

use std::task::Poll;
use std::time::{Duration, Instant};

use winasio::iocp::{win32_result, IntoInner, OpCode, Proactor, RegistrationError};
use windows::core::{Result, HSTRING};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};

/// A handle usable from any thread. `HANDLE` is a raw pointer and so not `Send`.
#[derive(Clone, Copy)]
struct Handle(HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

// --- operation 1: no buffer at all --------------------------------------

/// Waits for a client to connect to a named pipe.
///
/// Owns no buffer: the operation's meaningful result is that the connection
/// happened. An `OpCode` that required a buffer trait could not express this.
struct ConnectPipe {
    pipe: Handle,
}

unsafe impl OpCode for ConnectPipe {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.pipe.0)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        let res = unsafe { ConnectNamedPipe(self.pipe.0, Some(optr)) };
        match res {
            Ok(()) => unsafe { win32_result(true, optr) },
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                // A client that connected before we asked is still a success.
                Poll::Ready(Ok(0))
            }
            Err(_) => unsafe { win32_result(false, optr) },
        }
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.pipe.0, Some(optr)) }
    }
}

impl IntoInner for ConnectPipe {
    type Inner = ();
    fn into_inner(self) {}
}

// --- operation 2: fills a caller-allocated structure ---------------------

/// A message frame the kernel writes into directly.
///
/// This is the struct-filling shape: a fixed header plus an inline payload, all
/// owned by the operation and handed back intact. Nothing about it is a byte
/// slice, so `IoBufMut` would not apply.
#[repr(C)]
struct Frame {
    length: u32,
    payload: [u8; 256],
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            length: 0,
            payload: [0u8; 256],
        }
    }
}

/// Reads a frame from a pipe, letting the kernel fill the structure.
struct PipeRead {
    pipe: Handle,
    frame: Box<Frame>,
}

unsafe impl OpCode for PipeRead {
    fn handle(&self) -> Option<HANDLE> {
        Some(self.pipe.0)
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<Result<usize>> {
        // The kernel writes straight into the boxed structure. The pointer is
        // derived from `&mut self`, which lives at a stable heap address for the
        // operation's whole lifetime.
        let dst = std::ptr::addr_of_mut!(self.frame.payload) as *mut u8;
        let slice = unsafe { std::slice::from_raw_parts_mut(dst, 256) };
        let ok = unsafe { ReadFile(self.pipe.0, Some(slice), None, Some(optr)) }.is_ok();
        unsafe { win32_result(ok, optr) }
    }

    unsafe fn cancel(&mut self, optr: *mut OVERLAPPED) -> Result<()> {
        unsafe { CancelIoEx(self.pipe.0, Some(optr)) }
    }

    unsafe fn on_complete(&mut self, result: &Result<usize>) {
        // Record the length into the structure, as a real protocol would.
        if let Ok(n) = result {
            self.frame.length = *n as u32;
        }
    }
}

impl IntoInner for PipeRead {
    type Inner = Box<Frame>;
    fn into_inner(self) -> Box<Frame> {
        self.frame
    }
}

// --- harness ------------------------------------------------------------

fn poll_once<F: std::future::Future>(fut: &mut std::pin::Pin<Box<F>>) -> Option<F::Output> {
    use std::task::{Context, RawWaker, RawWakerVTable, Waker};
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

fn drive<F: std::future::Future>(proactor: &Proactor, fut: F) -> F::Output {
    let mut fut = Box::pin(fut);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(out) = poll_once(&mut fut) {
            return out;
        }
        proactor.poll(Some(Duration::from_millis(5))).unwrap();
        assert!(Instant::now() < deadline, "operation did not complete");
    }
}

struct Pipe(HANDLE);

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn unique_pipe_name() -> HSTRING {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    HSTRING::from(format!(r"\\.\pipe\winasio_ext_{pid:x}_{n:x}"))
}

#[test]
fn an_operation_defined_outside_the_crate_works() {
    let name = unique_pipe_name();

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
    assert!(!server.is_invalid(), "create named pipe");
    let server = Pipe(server);

    let proactor = Proactor::new().unwrap();
    proactor.attach(server.0).unwrap();

    // Start awaiting a connection, then connect from another thread.
    let mut connect = Box::pin(proactor.submit(ConnectPipe {
        pipe: Handle(server.0),
    }));

    let client_name = name.clone();
    let client = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        let h = unsafe {
            CreateFileW(
                &client_name,
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .expect("connect to pipe");
        // Send a frame's worth of bytes.
        let payload = b"defined outside winasio".to_vec();
        let mut written = 0u32;
        unsafe { WriteFile(h, Some(&payload), Some(&mut written), None) }.unwrap();
        std::thread::sleep(Duration::from_millis(50));
        unsafe {
            let _ = CloseHandle(h);
        }
        payload.len()
    });

    // Await the connection through the crate's infrastructure.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(out) = poll_once(&mut connect) {
            out.0.expect("connection completes");
            break;
        }
        proactor.poll(Some(Duration::from_millis(5))).unwrap();
        assert!(Instant::now() < deadline, "connect stalled");
    }

    // Now read into a caller-allocated structure.
    let read = drive(
        &proactor,
        proactor.submit(PipeRead {
            pipe: Handle(server.0),
            frame: Box::default(),
        }),
    );
    let (result, frame) = read.into_inner_parts();
    let n = result.expect("read completes");

    let expected = client.join().unwrap();
    assert_eq!(n, expected, "the transferred count is reported");
    assert_eq!(
        frame.length as usize, n,
        "the operation filled its own structure via on_complete"
    );
    assert_eq!(
        &frame.payload[..n],
        b"defined outside winasio",
        "the kernel wrote into the caller-allocated structure"
    );
}

#[test]
fn a_foreign_operation_can_be_abandoned_safely() {
    let name = unique_pipe_name();
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
    assert!(!server.is_invalid());
    let server = Pipe(server);

    let proactor = Proactor::new().unwrap();
    proactor.attach(server.0).unwrap();

    // Nobody will ever connect; drop the future while it is pending.
    {
        let pending = proactor.submit(ConnectPipe {
            pipe: Handle(server.0),
        });
        drop(pending);
    }

    // Cancellation must resolve promptly rather than hanging.
    let started = Instant::now();
    while proactor.pending_count() > 0 && started.elapsed() < Duration::from_secs(5) {
        proactor.poll(Some(Duration::from_millis(10))).unwrap();
    }
    assert_eq!(
        proactor.pending_count(),
        0,
        "an abandoned foreign operation is cancelled and drained"
    );
}

#[test]
fn duplicate_registration_is_reported_for_foreign_handles_too() {
    let name = unique_pipe_name();
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
    assert!(!server.is_invalid());
    let server = Pipe(server);

    let proactor = Proactor::new().unwrap();
    proactor.attach(server.0).unwrap();
    assert!(matches!(
        proactor.attach(server.0),
        Err(RegistrationError::AlreadyRegistered(_))
    ));
}
