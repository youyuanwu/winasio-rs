// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! TEMPORARY: Phase 0 retry-mechanics spike.
//!
//! Answers the six questions in `ImplementationPlan.md` Phase 0 about what is
//! observable when `HttpReceiveHttpRequest` is given an undersized buffer on an
//! *overlapped* handle. Deleted in Phase 4 once its findings are recorded.
//!
//! Deliberately builds its own queue, thread-pool registration and `OpCode`
//! from the raw bindings: the existing `RequestQueue` exposes no handle and its
//! request buffer is a fixed size, so it cannot be undersized.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, WIN32_ERROR};
use windows::Win32::Networking::HttpServer::{
    HttpAddUrlToUrlGroup, HttpCloseRequestQueue, HttpCloseServerSession, HttpCloseUrlGroup,
    HttpCreateRequestQueue, HttpCreateServerSession, HttpCreateUrlGroup, HttpInitialize,
    HttpReceiveHttpRequest, HttpSetUrlGroupProperty, HttpTerminate, HttpServerBindingProperty,
    HTTPAPI_VERSION, HTTP_BINDING_INFO, HTTP_INITIALIZE_CONFIG, HTTP_INITIALIZE_SERVER,
    HTTP_PROPERTY_FLAGS, HTTP_RECEIVE_HTTP_REQUEST_FLAGS, HTTP_REQUEST_V2,
};
use windows::Win32::System::IO::OVERLAPPED;

use winasio::iocp::{OpCode, ThreadPoolIo};

const PORT: u16 = 12359;
const VERSION: HTTPAPI_VERSION = HTTPAPI_VERSION {
    HttpApiMajorVersion: 2,
    HttpApiMinorVersion: 0,
};

/// Records everything the spike can observe about one receive attempt.
#[derive(Default)]
struct Findings {
    /// Raw return code from `HttpReceiveHttpRequest` inside `operate()`.
    operate_code: AtomicU32,
    /// `InternalHigh` read inside `operate()` on the non-pending path, or -1.
    internal_high_in_operate: AtomicI64,
    /// `InternalHigh` read inside `on_complete`, or -1 if `on_complete` never ran.
    internal_high_in_complete: AtomicI64,
    /// Whether the operation went pending rather than resolving inline.
    went_pending: AtomicU32,
    /// `RequestId` recovered from the partially filled buffer.
    request_id: AtomicUsize,
    /// Completion result code, or -1 if it completed inline.
    completion_code: AtomicI64,
}

struct SpikeReceive {
    queue: usize,
    buffer: Box<[u64]>,
    capacity_bytes: u32,
    request_id: u64,
    findings: Arc<Findings>,
    /// Stashed only to read `InternalHigh` in `on_complete`. The `OpCode`
    /// contract forbids retaining `optr` beyond `operate()`; this is the
    /// contract-violating probe the plan predicted, kept solely to *measure*
    /// whether it would even yield anything, and deleted with this file.
    optr_probe: usize,
}

impl SpikeReceive {
    fn new(queue: HANDLE, capacity_bytes: u32, request_id: u64, findings: Arc<Findings>) -> Self {
        let elems = (capacity_bytes as usize).div_ceil(8);
        SpikeReceive {
            queue: queue.0 as usize,
            buffer: vec![0u64; elems].into_boxed_slice(),
            capacity_bytes,
            request_id,
            findings,
            optr_probe: 0,
        }
    }

    fn raw(&mut self) -> *mut HTTP_REQUEST_V2 {
        self.buffer.as_mut_ptr() as *mut HTTP_REQUEST_V2
    }
}

// SAFETY: the spike owns its buffer; the handle is thread-agnostic.
unsafe impl Send for SpikeReceive {}

unsafe impl OpCode for SpikeReceive {
    fn handle(&self) -> Option<HANDLE> {
        Some(HANDLE(self.queue as *mut std::ffi::c_void))
    }

    unsafe fn operate(&mut self, optr: *mut OVERLAPPED) -> Poll<windows::core::Result<usize>> {
        self.optr_probe = optr as usize;
        let queue = HANDLE(self.queue as *mut std::ffi::c_void);
        let cap = self.capacity_bytes;
        let id = self.request_id;
        let raw = self.raw();

        let code = unsafe {
            HttpReceiveHttpRequest(
                queue,
                id,
                HTTP_RECEIVE_HTTP_REQUEST_FLAGS(0),
                raw,
                cap,
                None, // must be NULL for overlapped calls
                Some(optr),
            )
        };
        self.findings.operate_code.store(code, Ordering::SeqCst);

        const ERROR_IO_PENDING: u32 = 997;
        if code == ERROR_IO_PENDING {
            self.findings.went_pending.store(1, Ordering::SeqCst);
            return Poll::Pending;
        }

        // Non-pending: read InternalHigh right here, which IS contract-clean
        // because `optr` is live for the duration of this call.
        let ih = unsafe { (*optr).InternalHigh } as i64;
        self.findings
            .internal_high_in_operate
            .store(ih, Ordering::SeqCst);
        self.record_request_id();

        if code == 0 {
            Poll::Ready(Ok(unsafe { (*optr).InternalHigh }))
        } else {
            Poll::Ready(Err(windows::core::Error::from_hresult(
                WIN32_ERROR(code).to_hresult(),
            )))
        }
    }

    unsafe fn on_complete(&mut self, result: &windows::core::Result<usize>) {
        if self.optr_probe != 0 {
            let optr = self.optr_probe as *mut OVERLAPPED;
            let ih = unsafe { (*optr).InternalHigh } as i64;
            self.findings
                .internal_high_in_complete
                .store(ih, Ordering::SeqCst);
        }
        let code = match result {
            Ok(_) => 0i64,
            Err(e) => (e.code().0 & 0xFFFF) as i64,
        };
        self.findings.completion_code.store(code, Ordering::SeqCst);
        self.record_request_id();
    }
}

impl SpikeReceive {
    fn record_request_id(&mut self) {
        // The base structure is only meaningful if the buffer could hold it.
        if (self.capacity_bytes as usize) < std::mem::size_of::<HTTP_REQUEST_V2>() {
            return;
        }
        let raw = self.buffer.as_ptr() as *const HTTP_REQUEST_V2;
        let id = unsafe { (*raw).Base.RequestId };
        self.findings.request_id.store(id as usize, Ordering::SeqCst);
    }
}

/// Minimal RAII listener built straight from the bindings.
struct Listener {
    session: u64,
    group: u64,
    queue: HANDLE,
}

impl Listener {
    fn new(url: &str) -> Option<Listener> {
        unsafe {
            let ec = HttpInitialize(
                VERSION,
                HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG,
                None,
            );
            if ec != 0 {
                return None;
            }
            let mut session = 0u64;
            if HttpCreateServerSession(VERSION, &mut session, None) != 0 {
                return None;
            }
            let mut group = 0u64;
            if HttpCreateUrlGroup(session, &mut group, None) != 0 {
                return None;
            }
            let mut queue = HANDLE::default();
            if HttpCreateRequestQueue(VERSION, PCWSTR::null(), None, None, &mut queue) != 0 {
                return None;
            }
            let info = HTTP_BINDING_INFO {
                Flags: HTTP_PROPERTY_FLAGS { _bitfield: 1 },
                RequestQueueHandle: queue,
            };
            let ec = HttpSetUrlGroupProperty(
                group,
                HttpServerBindingProperty,
                std::ptr::addr_of!(info) as *const std::ffi::c_void,
                std::mem::size_of::<HTTP_BINDING_INFO>() as u32,
            );
            if ec != 0 {
                return None;
            }
            let ec = HttpAddUrlToUrlGroup(group, &HSTRING::from(url), 0, None);
            if ec != 0 {
                eprintln!("SPIKE: could not bind {url}: win32 {ec} (needs a URL ACL or elevation)");
                return None;
            }
            Some(Listener {
                session,
                group,
                queue,
            })
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        unsafe {
            let _ = HttpCloseUrlGroup(self.group);
            let _ = HttpCloseRequestQueue(self.queue);
            let _ = HttpCloseServerSession(self.session);
            let _ = HttpTerminate(HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG, None);
        }
    }
}

fn send_request(path: &str, header_padding: usize) {
    use windows::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_OPEN_REQUEST_FLAGS,
    };
    use winasio::winhttp::HSession;

    let session = HSession::new(
        HSTRING::from("spike"),
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        HSTRING::new(),
        HSTRING::new(),
        0,
    )
    .unwrap();
    let conn = session.connect(HSTRING::from("localhost"), PORT).unwrap();
    let big = "x".repeat(header_padding);
    let req = conn
        .open_request(
            HSTRING::from("GET"),
            HSTRING::from(path),
            HSTRING::from("HTTP/1.1"),
            HSTRING::new(),
            None,
            WINHTTP_OPEN_REQUEST_FLAGS(0),
        )
        .unwrap();
    let headers = HSTRING::from(format!("X-Pad: {big}"));
    let _ = req.send(headers, &[], 0, 0);
}

/// Runs one receive against `capacity_bytes` and reports what was observable.
fn probe(label: &str, capacity_bytes: u32, padding: usize, submit_first: bool) {
    let url = format!("http://localhost:{PORT}/spike/");
    let Some(listener) = Listener::new(&url) else {
        eprintln!("SPIKE {label}: SKIPPED (no listener)");
        return;
    };
    let io = ThreadPoolIo::new(listener.queue).expect("register queue");
    let findings = Arc::new(Findings::default());
    findings.internal_high_in_operate.store(-1, Ordering::SeqCst);
    findings
        .internal_high_in_complete
        .store(-1, Ordering::SeqCst);
    findings.completion_code.store(-1, Ordering::SeqCst);

    let op = SpikeReceive::new(listener.queue, capacity_bytes, 0, findings.clone());

    let outcome = if submit_first {
        let fut = io.submit(op);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            send_request("spike", padding);
        });
        futures_block_on(fut)
    } else {
        let h = std::thread::spawn(move || send_request("spike", padding));
        std::thread::sleep(std::time::Duration::from_millis(500));
        let fut = io.submit(op);
        let r = futures_block_on(fut);
        let _ = h.join();
        r
    };

    let ordering = if submit_first {
        "submit-before-client (expect pending)"
    } else {
        "client-before-submit (expect inline)"
    };
    println!("--------------------------------------------------------------");
    println!("SPIKE {label} [{ordering}] capacity={capacity_bytes} bytes");
    println!("  Q1 operate() return code   : {}", findings.operate_code.load(Ordering::SeqCst));
    println!("  Q1 went pending            : {}", findings.went_pending.load(Ordering::SeqCst) == 1);
    println!("  Q1 completion result code  : {}", findings.completion_code.load(Ordering::SeqCst));
    println!("  Q2 RequestId recovered     : {:#x}", findings.request_id.load(Ordering::SeqCst));
    println!("  Q3 InternalHigh in operate : {}", findings.internal_high_in_operate.load(Ordering::SeqCst));
    println!("  Q3 InternalHigh in complete: {}", findings.internal_high_in_complete.load(Ordering::SeqCst));
    println!("  outcome                    : {:?}", outcome.0);
    drop(listener);
}

/// Minimal inline executor; the crate's `block_on` is `Proactor`-specific.
fn futures_block_on<F: std::future::Future>(mut fut: F) -> F::Output {
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, RawWaker, RawWakerVTable, Waker};

    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |d| RawWaker::new(d, &VTABLE),
        |d| unsafe { (*(d as *const AtomicBool)).store(true, Ordering::SeqCst) },
        |d| unsafe { (*(d as *const AtomicBool)).store(true, Ordering::SeqCst) },
        |_| {},
    );

    let flag = AtomicBool::new(true);
    let raw = RawWaker::new(&flag as *const AtomicBool as *const (), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if flag.swap(false, Ordering::SeqCst) {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("spike timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Q5: can `HttpCancelHttpRequest` remove a request left in the
/// `ERROR_MORE_DATA` state? Phase 4's rejection path depends on it.
fn probe_cancel() {
    use windows::Win32::Networking::HttpServer::HttpCancelHttpRequest;

    let url = format!("http://localhost:{PORT}/spike/");
    let Some(listener) = Listener::new(&url) else {
        eprintln!("SPIKE E: SKIPPED (no listener)");
        return;
    };
    let io = ThreadPoolIo::new(listener.queue).expect("register queue");
    let findings = Arc::new(Findings::default());
    let base = std::mem::size_of::<HTTP_REQUEST_V2>() as u32;

    let op = SpikeReceive::new(listener.queue, base + 16, 0, findings.clone());
    let fut = io.submit(op);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        send_request("spike", 4096);
    });
    let out = futures_block_on(fut);
    let id = findings.request_id.load(Ordering::SeqCst) as u64;

    println!("--------------------------------------------------------------");
    println!("SPIKE E cancel-after-ERROR_MORE_DATA");
    println!("  receive outcome            : {:?}", out.0);
    println!("  RequestId                  : {id:#x}");

    let cancel_code = unsafe { HttpCancelHttpRequest(listener.queue, id, None) };
    println!("  HttpCancelHttpRequest code : {cancel_code}");

    // If the cancel took effect, re-receiving *that specific* request must fail.
    let findings2 = Arc::new(Findings::default());
    let op2 = SpikeReceive::new(listener.queue, 65536, id, findings2.clone());
    let out2 = futures_block_on(io.submit(op2));
    println!("  re-receive that id         : {:?}", out2.0);
    println!("  => cancel usable for FR-016: {}", cancel_code == 0 && out2.0.is_err());
    drop(listener);
}

#[test]
#[ignore = "spike: binds a URL and requires the HTTP service; run explicitly"]
fn phase0_retry_mechanics_spike() {
    let base = std::mem::size_of::<HTTP_REQUEST_V2>() as u32;
    println!("size_of::<HTTP_REQUEST_V2>() = {base}");
    println!("align_of::<HTTP_REQUEST_V2>() = {}", std::mem::align_of::<HTTP_REQUEST_V2>());

    // Q1/Q2/Q3/Q6, pending ordering: undersized but >= base structure.
    probe("A undersized/pending", base + 16, 4096, true);
    // Q1/Q2/Q3/Q6, inline ordering.
    probe("B undersized/inline", base + 16, 4096, false);
    // Q4: buffer smaller than the base structure.
    probe("C below-base/inline", 64, 4096, false);
    // Control: ample buffer, should simply succeed.
    probe("D ample/inline", 65536, 64, false);
    // Q5: rejection path.
    probe_cancel();
}
