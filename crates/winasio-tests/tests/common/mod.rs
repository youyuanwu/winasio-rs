// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Shared harness for the HTTP.sys integration tests.
//!
//! Each test binary owns a distinct port (see `ImplementationPlan.md`), because
//! cargo runs integration binaries in parallel and two processes cannot bind the
//! same HTTP.sys prefix.

#![allow(dead_code)]

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use windows::core::HSTRING;

use winasio::httpsys::{HttpInitializer, ReceiveConfig, RequestQueue, ServerSession, UrlGroup};

/// Drives a future to completion on this thread.
///
/// The crate's own `block_on` is tied to `Proactor`; a request queue uses the
/// thread-pool backend, whose completions arrive on pool threads, so a plain
/// park-and-retry loop is all that is needed here.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |d| RawWaker::new(d, &VTABLE),
        |d| unsafe { (*(d as *const AtomicBool)).store(true, Ordering::SeqCst) },
        |d| unsafe { (*(d as *const AtomicBool)).store(true, Ordering::SeqCst) },
        |_| {},
    );

    let flag = AtomicBool::new(true);
    let raw = RawWaker::new(&flag as *const AtomicBool as *const (), &VTABLE);
    // SAFETY: the vtable only ever touches the `AtomicBool`, which outlives the
    // waker because the future is driven to completion before this returns.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = fut;
    // SAFETY: `fut` lives on this stack frame and is never moved again.
    let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if flag.swap(false, Ordering::SeqCst) {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for an HTTP.sys operation"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// A URL group together with the session it borrows.
///
/// `UrlGroup<'a>` borrows its `ServerSession`, which a plain struct cannot
/// express without self-reference. The session is boxed so its address is
/// stable, and the group is dropped explicitly before it.
struct Binding {
    group: Option<UrlGroup<'static>>,
    session: Option<Box<ServerSession>>,
}

impl Drop for Binding {
    fn drop(&mut self) {
        // The group refers to the session, so it must be closed first.
        self.group = None;
        self.session = None;
    }
}

/// A bound listener, torn down in the right order on drop.
pub struct Server {
    // Field order is drop order. The binding goes before the queue, and both
    // before the initializer.
    _binding: Binding,
    queue: Arc<RequestQueue>,
    _http: HttpInitializer,
    port: u16,
    path: String,
}

impl Server {
    /// Bind `http://localhost:<port>/<path>/` and return a ready listener.
    ///
    /// Returns `None` when the URL cannot be bound -- typically because the
    /// caller lacks a URL reservation -- so tests skip rather than fail on a
    /// machine that cannot host a listener.
    pub fn start(port: u16, path: &str, config: ReceiveConfig) -> Option<Server> {
        let http = HttpInitializer::new().ok()?;
        let session = Box::new(ServerSession::new().ok()?);

        // SAFETY: the session is boxed, so its address is stable, and `Binding`
        // drops the group before the session. The lifetime is erased only so the
        // two can be stored together.
        let session_ref: &'static ServerSession = unsafe { &*(&*session as *const ServerSession) };
        let group = UrlGroup::new(session_ref).ok()?;

        let queue = RequestQueue::with_config(config).ok()?;
        queue.bind_url_group(&group).ok()?;

        let url = HSTRING::from(format!("http://localhost:{port}/{path}/"));
        if let Err(e) = group.add_url(&url) {
            eprintln!("skipping: cannot bind {url}: {e}");
            return None;
        }

        Some(Server {
            _binding: Binding {
                group: Some(group),
                session: Some(session),
            },
            queue: Arc::new(queue),
            _http: http,
            port,
            path: path.to_string(),
        })
    }

    pub fn queue(&self) -> &Arc<RequestQueue> {
        &self.queue
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Fire a request at this listener from a background thread.
    ///
    /// Deliberately does not wait for a reply: most receive-side tests never
    /// answer their requests.
    pub fn client_request(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) {
        let handle = self.request(method, target, headers, body);
        // Give the request time to reach the queue before the test awaits it.
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(handle);
    }

    /// Fire a request and hand back a join handle carrying the raw reply.
    pub fn request(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> std::thread::JoinHandle<Option<Vec<u8>>> {
        let port = self.port;
        let method = method.to_string();
        let target = target.to_string();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let body = body.to_vec();
        std::thread::spawn(move || send_raw(port, &method, &target, &headers, &body))
    }
}

/// A minimal HTTP/1.1 client over a raw socket.
///
/// WinHTTP normalises and rejects too much for these tests -- unrecognised
/// methods, repeated headers, deliberately oversized headers -- so requests are
/// written by hand.
pub fn send_raw(
    port: u16,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Option<Vec<u8>> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok()?;

    let mut req = format!("{method} /{target} HTTP/1.1\r\nHost: localhost:{port}\r\n");
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    // Without this the connection is kept alive and `read_to_end` below blocks
    // until the read timeout, which makes every test wall-clock bound.
    req.push_str("Connection: close\r\n");
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    req.push_str("\r\n");

    stream.write_all(req.as_bytes()).ok()?;
    if !body.is_empty() {
        stream.write_all(body).ok()?;
    }
    stream.flush().ok()?;

    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    Some(response)
}

/// Serialises tests that observe process-global state.
///
/// `live_operations()` counts every operation in the process, and cargo runs the
/// tests in a binary concurrently, so a test asserting on it must hold this.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Split a raw HTTP reply into its status line, headers and body.
pub fn parse_response(raw: &[u8]) -> (String, Vec<(String, String)>, Vec<u8>) {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = if split + 4 <= raw.len() {
        raw[split + 4..].to_vec()
    } else {
        Vec::new()
    };

    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("").to_string();
    let headers = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    (status, headers, body)
}
