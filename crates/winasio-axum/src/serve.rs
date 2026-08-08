// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The concurrent serve loop: an [`axum::serve`]-shaped driver over
//! [`winasio_util::Server`].
//!
//! [`serve`] accepts requests in a loop and dispatches each to a caller-supplied
//! [`Executor`], so many requests are in flight at once. It is the concurrent
//! counterpart to [`winasio_util::Server::serve`], which is deliberately
//! sequential.
//!
//! # D1. Why the server is borrowed, not consumed
//!
//! [`serve`] takes `&Server`, mirroring `winasio-util`'s own
//! [`accept`](winasio_util::Server::accept)/[`serve_one`](winasio_util::Server::serve_one)/[`serve`](winasio_util::Server::serve)
//! (all `&self`), rather than `axum::serve`'s by-value listener. Borrowing keeps
//! the `Server` usable by the caller for a
//! [`ShutdownHandle`](winasio_util::ShutdownHandle) while the loop runs, and it
//! is what lets a test drop the returned future and then call `serve_one` on the
//! same server to observe that no request was consumed while unready.
//!
//! **Rejected alternative**: consuming the server (axum's shape) would forbid
//! that same-server backpressure proof and force a caller who wants a shutdown
//! handle to obtain it before calling `serve`.
//!
//! # D2. The loop: cancel-safe, backpressure-correct, starvation-free
//!
//! Each iteration:
//!
//! 1. **Backpressure gate.** Clone the service, then await *that clone's*
//!    [`poll_ready`](tower_service::Service::poll_ready) before accepting. A
//!    service that is not ready therefore stops the loop pulling a request out
//!    of the kernel queue it could not place — the same observable backpressure
//!    `winasio-util` gives (`winasio-util` D4). The **same** readied clone is the
//!    one dispatched, so the reservation Tower ties to the instance that
//!    acquired it is spent, not leaked.
//! 2. **Accept.** Await one pinned `accept` future, recreated only after it
//!    resolves (so it is never dropped mid-flight — cancel-safe).
//! 3. **Dispatch or classify.** A good request becomes a panic-contained
//!    [`RequestTask`] handed to the executor; an error is [`classify`]-ed into
//!    continue / clean-stop / fatal.
//!
//! Both the readiness gate and the accept await also drive the executor's
//! [`poll_progress`](Executor::poll_progress) every poll, so a
//! [`CurrentThread`](crate::CurrentThread) executor makes progress on in-flight
//! requests while the loop waits. A slow handler cannot starve accepts (both are
//! polled on every wake) and a flood of accepts cannot starve handlers
//! (`poll_progress` runs inside both gates).
//!
//! # D3. Documented backpressure contract
//!
//! The loop assumes the supplied service honours the Tower readiness contract:
//! once [`poll_ready`](tower_service::Service::poll_ready) reports ready it stays
//! ready, and re-polling a ready instance does not acquire a *second*
//! reservation. Under that contract exactly one reservation is consumed per
//! request across the gate poll and [`Accepted::serve`](winasio_util::Accepted::serve)'s
//! idempotent re-poll. For [`axum::Router`] (immediately ready) the gate is a
//! no-op; for a stateful or rate-limited service it is the mechanism that keeps
//! the observable backpressure.
//!
//! # D4. Error policy without a logging dependency
//!
//! Per-request failures and caught panics are routed to a caller-supplied
//! observer ([`Serve::on_error`], default no-op), so the crate reports errors
//! without depending on any logging framework. Accept-time errors are triaged by
//! the pure [`classify`] helper:
//!
//! - a **closed queue** ([`AcceptError::is_queue_closed`](winasio_util::AcceptError::is_queue_closed))
//!   stops the loop cleanly (`Ok(())`);
//! - [`RequestTooLarge`](winasio_util::AcceptError::RequestTooLarge) and
//!   [`MalformedRequest`](winasio_util::AcceptError::MalformedRequest) are
//!   **recoverable** — the offending request is already gone below, so the
//!   observer is told and the loop continues (it does not spin or abort);
//! - any other receive failure is **fatal** and ends the loop with the error.
//!
//! # D5. Panic containment (uniform for both executors)
//!
//! Every request task is wrapped in
//! [`AssertUnwindSafe`](std::panic::AssertUnwindSafe) +
//! [`catch_unwind`](futures_util::future::FutureExt::catch_unwind), so a handler
//! panic is caught and reported to the observer as a [`HandlerPanic`] inside
//! [`ServeError::Service`](winasio_util::ServeError::Service) rather than
//! unwinding. This matters most for [`CurrentThread`](crate::CurrentThread),
//! whose tasks share the loop's own thread: without the guard a single panicking
//! handler would tear down the serve loop. A dropped
//! [`Responder`](winasio_util::Responder) answers nothing, so the panicking
//! request's peer times out while the queue and every other request stay usable
//! (`winasio-util`'s measured abrupt-drop semantics).
//!
//! # D6. Shutdown: abrupt, one rule for both executors (R3)
//!
//! When the loop stops (queue closed or fatal) [`serve`] returns **immediately,
//! without draining in-flight work**. This is uniform: [`CurrentThread`](crate::CurrentThread)
//! tasks still in its set are dropped; [`ThreadPerRequest`](crate::ThreadPerRequest)
//! threads keep running untracked and may finish (and fire their observer
//! callback) *after* `serve` has returned. There is deliberately no post-loop
//! drain — draining `ThreadPerRequest` would mean joining threads the loop never
//! owned — matching `winasio-util`'s documented abrupt-shutdown semantics. A
//! caller that needs "all work finished" must coordinate that itself.
//!
//! **Rejected alternative**: draining only the [`CurrentThread`](crate::CurrentThread)
//! executor would make shutdown depend on which executor was chosen; a single
//! abrupt rule keeps the contract uniform.

use std::future::{Future, IntoFuture};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Buf;
use futures_util::future::FutureExt;
use http::{Request, Response};
use http_body::Body;
use tower_service::Service;
use winasio_util::{AcceptError, Accepted, Backend, IncomingBody, ServeError, Server};

use crate::executor::{Executor, RequestTask};

/// The boxed error type `winasio-util`'s service bounds are written against.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The error handed to the [`Serve::on_error`] observer when a request handler
/// panics.
///
/// Carries the panic message (when it was a `&str` or `String`). An observer can
/// [`downcast`](std::error::Error) the [`ServeError::Service`](winasio_util::ServeError::Service)
/// payload to this type to tell a caught panic apart from an ordinary service
/// error.
#[derive(Debug)]
pub struct HandlerPanic {
    message: String,
}

impl HandlerPanic {
    /// The panic message, or a fallback when the payload was not a string.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for HandlerPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the request handler panicked: {}", self.message)
    }
}

impl std::error::Error for HandlerPanic {}

/// What the loop should do about an [`AcceptError`].
///
/// Kept separate from the loop so the triage is a pure function, unit-testable
/// without the kernel having to emit each error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    /// Recoverable: tell the observer and keep accepting.
    Continue,
    /// The queue closed: stop the loop cleanly.
    CleanStop,
    /// Unrecoverable: stop the loop with the error.
    Fatal,
}

/// Triage an accept-time error. Pure — see [`Disposition`].
fn classify(error: &AcceptError) -> Disposition {
    if error.is_queue_closed() {
        return Disposition::CleanStop;
    }
    match error {
        AcceptError::RequestTooLarge { .. } | AcceptError::MalformedRequest { .. } => {
            Disposition::Continue
        }
        AcceptError::Receive(_) => Disposition::Fatal,
    }
}

/// Best-effort message from a caught panic payload.
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "handler panicked".to_string()
    }
}

type ErrorObserver = Arc<dyn Fn(ServeError) + Send + Sync + 'static>;

/// A concurrent [`axum::serve`]-shaped driver, awaitable via [`IntoFuture`].
///
/// Build one with [`serve`], optionally attach an error observer with
/// [`on_error`](Serve::on_error), then `.await` it.
#[must_use = "a Serve does nothing until you .await it"]
pub struct Serve<'srv, 'sess, S: Backend, Svc, E> {
    server: &'srv Server<'sess, S>,
    service: Svc,
    executor: E,
    on_error: ErrorObserver,
}

impl<'srv, 'sess, S: Backend, Svc, E> Serve<'srv, 'sess, S, Svc, E> {
    /// Observe per-request failures and caught handler panics.
    ///
    /// Called for every [`ServeError`] the loop would otherwise discard: a
    /// recoverable accept error, a service error, or a [`HandlerPanic`]. The
    /// default is a no-op. This is how the crate surfaces errors without taking
    /// a logging dependency (D4).
    pub fn on_error<F>(mut self, observer: F) -> Self
    where
        F: Fn(ServeError) + Send + Sync + 'static,
    {
        self.on_error = Arc::new(observer);
        self
    }
}

/// Serve an [`axum::Router`] (or any equivalent [`tower_service::Service`]) over
/// HTTP.sys concurrently, dispatching each request through `executor`.
///
/// The server is borrowed, not consumed (D1). The returned [`Serve`] is
/// awaitable and offers [`on_error`](Serve::on_error); it resolves to `Ok(())`
/// when the queue closes and `Err` on a fatal accept failure (D4, D6).
///
/// ```no_run
/// # async fn example(server: &winasio_util::Server<'_>) -> Result<(), winasio_util::ServeError> {
/// use axum::{routing::get, Router};
/// use winasio_axum::{serve, CurrentThread};
///
/// // Routes carry HTTP.sys's registered prefix (see the crate docs on M8).
/// let app: Router = Router::new().route("/demo/greet", get(|| async { "hi" }));
///
/// // Runs concurrently on the caller's own thread — no runtime required.
/// serve(server, app, CurrentThread::new())
///     .on_error(|error| eprintln!("request failed: {error}"))
///     .await
/// # }
/// ```
pub fn serve<'srv, 'sess, S, Svc, E>(
    server: &'srv Server<'sess, S>,
    service: Svc,
    executor: E,
) -> Serve<'srv, 'sess, S, Svc, E>
where
    S: Backend,
{
    Serve {
        server,
        service,
        executor,
        on_error: Arc::new(|_| {}),
    }
}

impl<'srv, 'sess, S, Svc, B, E> IntoFuture for Serve<'srv, 'sess, S, Svc, E>
where
    'sess: 'srv,
    S: Backend + Send + Sync,
    S::Read: Send,
    Accepted<S>: Send + 'static,
    Svc: Service<Request<IncomingBody<S>>, Response = Response<B>> + Clone + Send + 'static,
    Svc::Future: Send + 'static,
    Svc::Error: Into<BoxError> + Send + 'static,
    B: Body + Send + 'static,
    B::Data: Buf + Send,
    B::Error: Into<BoxError>,
    E: Executor<RequestTask> + 'srv,
{
    type Output = Result<(), ServeError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Result<(), ServeError>> + 'srv>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(run(self.server, self.service, self.executor, self.on_error))
    }
}

/// The serve loop (D2). Split out from [`IntoFuture`] so the bounds are written
/// once.
async fn run<S, Svc, B, E>(
    server: &Server<'_, S>,
    service: Svc,
    executor: E,
    on_error: ErrorObserver,
) -> Result<(), ServeError>
where
    S: Backend + Send + Sync,
    S::Read: Send,
    Accepted<S>: Send + 'static,
    Svc: Service<Request<IncomingBody<S>>, Response = Response<B>> + Clone + Send + 'static,
    Svc::Future: Send + 'static,
    Svc::Error: Into<BoxError> + Send + 'static,
    B: Body + Send + 'static,
    B::Data: Buf + Send,
    B::Error: Into<BoxError>,
    E: Executor<RequestTask>,
{
    // One pinned accept future, recreated only after it resolves (D2, cancel-safe).
    let mut accept_fut = Box::pin(server.accept());

    loop {
        // 1. Backpressure gate on the exact clone we will dispatch (D2/D3), while
        //    driving in-flight work so a slow handler cannot starve readiness.
        let mut svc = service.clone();
        std::future::poll_fn(|cx| {
            let _ = executor.poll_progress(cx);
            svc.poll_ready(cx)
        })
        .await
        .map_err(|error| ServeError::Service(error.into()))?;

        // 2. Accept, still driving in-flight work so accepts cannot be starved.
        let accepted = std::future::poll_fn(|cx| {
            let _ = executor.poll_progress(cx);
            accept_fut.as_mut().poll(cx)
        })
        .await;

        // 3. Dispatch or classify (D4).
        match accepted {
            Ok(accepted) => {
                // Resolved: it is now safe (required) to recreate the future.
                accept_fut.set(server.accept());

                let on_error_task = Arc::clone(&on_error);
                let task: RequestTask = Box::pin(async move {
                    // Panic containment (D5): a handler panic becomes an observer
                    // report, never an unwind through the loop.
                    match AssertUnwindSafe(accepted.serve(svc)).catch_unwind().await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => on_error_task(error),
                        Err(panic) => {
                            let message = panic_message(panic);
                            on_error_task(ServeError::Service(Box::new(HandlerPanic { message })));
                        }
                    }
                });
                executor.execute(task);
            }
            Err(error) => match classify(&error) {
                Disposition::CleanStop => return Ok(()),
                Disposition::Continue => {
                    on_error(ServeError::Accept(error));
                    accept_fut.set(server.accept());
                }
                Disposition::Fatal => return Err(ServeError::Accept(error)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winasio_util::RequestReason;

    /// A `windows::core::Error` whose HRESULT is the raw win32 code `code`.
    fn win_error(code: u32) -> windows::core::Error {
        windows::core::Error::from_hresult(windows::core::HRESULT(code as i32))
    }

    #[test]
    fn oversized_and_malformed_are_recoverable() {
        assert_eq!(
            classify(&AcceptError::RequestTooLarge { capacity: 4096 }),
            Disposition::Continue,
        );
        assert_eq!(
            classify(&AcceptError::MalformedRequest {
                reason: RequestReason::Target,
                value: "not a uri".to_string(),
            }),
            Disposition::Continue,
        );
    }

    #[test]
    fn a_closed_queue_is_a_clean_stop() {
        // 0x800703E3 == HRESULT_FROM_WIN32(ERROR_OPERATION_ABORTED), the code a
        // receive already waiting when the queue closed reports.
        assert_eq!(
            classify(&AcceptError::Receive(win_error(0x8007_03E3))),
            Disposition::CleanStop,
        );
        // 0x80070006 == HRESULT_FROM_WIN32(ERROR_INVALID_HANDLE), the code a
        // receive started after the close reports.
        assert_eq!(
            classify(&AcceptError::Receive(win_error(0x8007_0006))),
            Disposition::CleanStop,
        );
    }

    #[test]
    fn any_other_receive_failure_is_fatal() {
        // 0x80070005 == HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED): a genuine fault,
        // not shutdown.
        assert_eq!(
            classify(&AcceptError::Receive(win_error(0x8007_0005))),
            Disposition::Fatal,
        );
    }

    #[test]
    fn panic_message_prefers_the_payload_string() {
        assert_eq!(panic_message(Box::new("boom")), "boom");
        assert_eq!(panic_message(Box::new(String::from("kaboom"))), "kaboom");
        assert_eq!(panic_message(Box::new(42u8)), "handler panicked");
    }
}
