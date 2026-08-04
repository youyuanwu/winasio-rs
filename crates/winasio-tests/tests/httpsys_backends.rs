// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! HTTP.sys request queues with non-default completion backends.

mod common;

use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use windows::core::HSTRING;

use winasio::httpsys::{
    HttpInitializer, ReceiveConfig, ReceiveError, RequestQueue, Response, ResponseHeader,
    ServerSession, UrlGroup,
};
use winasio::iocp::{Proactor, ThreadPoolIo};

const PORT: u16 = 12366;

struct Binding {
    group: Option<UrlGroup<'static>>,
    session: Option<Box<ServerSession>>,
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.group = None;
        self.session = None;
    }
}

struct ProactorServer {
    _binding: Binding,
    queue: RequestQueue<Rc<Proactor>>,
    proactor: Rc<Proactor>,
    _http: HttpInitializer,
    port: u16,
}

impl ProactorServer {
    fn start(port: u16, path: &str, config: ReceiveConfig) -> Option<Self> {
        let http = HttpInitializer::new().expect("HttpInitializer::new");
        let session = Box::new(ServerSession::new().expect("ServerSession::new"));

        // SAFETY: the session is boxed, so its address is stable, and `Binding`
        // drops the group before the session.
        let session_ref: &'static ServerSession = unsafe { &*(&*session as *const ServerSession) };
        let group = UrlGroup::new(session_ref).expect("UrlGroup::new");

        let proactor = Rc::new(Proactor::new().expect("Proactor::new"));
        let queue =
            RequestQueue::with_config(&proactor, config).expect("RequestQueue::with_config");
        queue.bind_url_group(&group).expect("bind_url_group");

        let url = HSTRING::from(format!("http://localhost:{port}/{path}/"));
        if let Err(e) = group.add_url(&url) {
            eprintln!("skipping: cannot bind {url}: {e} (a URL reservation may be needed)");
            return None;
        }

        Some(ProactorServer {
            _binding: Binding {
                group: Some(group),
                session: Some(session),
            },
            queue,
            proactor,
            _http: http,
            port,
        })
    }

    fn request(&self, target: &str) -> std::thread::JoinHandle<Option<Vec<u8>>> {
        let port = self.port;
        let target = target.to_string();
        std::thread::spawn(move || common::send_raw(port, "GET", &target, &[], &[]))
    }
}

fn poll_once<F: Future>(fut: &mut std::pin::Pin<Box<F>>) -> Poll<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    fut.as_mut().poll(&mut cx)
}

fn assert_pending<F: Future>(fut: &mut std::pin::Pin<Box<F>>, message: &str) {
    assert!(matches!(poll_once(fut), Poll::Pending), "{message}");
}

#[test]
fn proactor_queue_serves_a_real_request_end_to_end() {
    let _guard = common::serial();
    let Some(server) = ProactorServer::start(PORT, "backend-e2e", ReceiveConfig::default()) else {
        return;
    };

    let mut receive = Box::pin(server.queue.receive());
    assert_pending(
        &mut receive,
        "the receive must be genuinely outstanding before the client connects",
    );

    let client = server.request("backend-e2e/hello");
    let request = common::drive_proactor(server.proactor.as_ref(), receive).expect("receive");
    assert_eq!(request.raw_target(), b"/backend-e2e/hello");

    let mut reply = Response::new(202);
    reply
        .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
        .add_body(&b"served by proactor"[..]);
    common::drive_proactor(
        server.proactor.as_ref(),
        server.queue.send(request.id(), reply),
    )
    .0
    .expect("send");

    let raw = client.join().expect("client thread").expect("client reply");
    let (status, headers, body) = common::parse_response(&raw);
    assert!(
        status.starts_with("HTTP/1.1 202"),
        "client observed status line {status:?}"
    );
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("content-type") && value == "text/plain"),
        "client observed response headers {headers:?}"
    );
    assert_eq!(body, b"served by proactor");
}

#[test]
fn closing_proactor_queue_resolves_an_outstanding_receive() {
    let _guard = common::serial();
    let _http = HttpInitializer::new().expect("HttpInitializer::new");
    let proactor = Rc::new(Proactor::new().expect("Proactor::new"));
    let queue = RequestQueue::new(&proactor).expect("RequestQueue::new");

    let mut receive = Box::pin(queue.receive());
    assert_pending(
        &mut receive,
        "the receive must be pending before close() cancels it",
    );

    queue.close().expect("close");
    let result = common::drive_proactor(proactor.as_ref(), receive);
    match result {
        Err(ReceiveError::Failed(_)) => {}
        Err(e) => panic!("a cancelled receive must be a plain failure, got {e}"),
        Ok(_) => panic!("a cancelled receive must not produce a request"),
    }
}

#[test]
fn thread_pool_request_queue_is_send_and_sync() {
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<RequestQueue<ThreadPoolIo>>();
}
