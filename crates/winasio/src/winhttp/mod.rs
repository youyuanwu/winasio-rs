// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! An asynchronous HTTP and HTTPS client, built on WinHTTP.
//!
//! [`Session`] holds process-scoped configuration, [`Session::connect`] names a
//! server, and [`Connection::open_request`] produces a [`Request`] — the handle
//! every transfer happens on. A request is always asynchronous: there is no
//! synchronous surface to fall back to, and no way to ask for one.
//!
//! # Invariants and obligations
//!
//! * **Async always.** Every session is opened with `WINHTTP_FLAG_ASYNC`.
//!   The flag is not a parameter and cannot be cleared, because this module's
//!   soundness argument rests on completions arriving through the status
//!   callback.
//! * **One transfer at a time.** WinHTTP permits at most one outstanding
//!   asynchronous operation per request. Taking `&mut self` makes the ordinary
//!   mistake — two live futures — a compile error. The case `&mut` cannot
//!   catch is abandoning a future and starting another; that is refused with
//!   [`WinHttpError::OperationInProgress`] rather than relayed to WinHTTP.
//! * **Operations own their buffers.** Every transfer takes its buffer by
//!   value and returns it in an [`OpResult`](crate::iocp::OpResult), on success
//!   and on failure alike. A borrowed buffer cannot be made sound here; see
//!   the section on cancellation below.
//! * **Abandoning a transfer costs you the buffer, and parks the request.**
//!   Dropping a pending future is safe but not free. See below.
//! * **Handles may be dropped in any order — the crate makes that true.** The
//!   platform does not. Closing a *connection* handle is genuinely harmless: a
//!   request derived from it keeps working. Closing a *session* handle is not:
//!   every subsequent operation on a derived request fails immediately with
//!   [`WinHttpError::Cancelled`], reported inline before the submitting call
//!   returns. Rather than expose that asymmetry as a lifetime parameter or a
//!   documented footgun, a [`Connection`] and a [`Request`] each hold a
//!   reference to the session handle and keep it open. Dropping a [`Session`]
//!   value is therefore always safe; the handle behind it closes when the last
//!   thing derived from it is gone. The connection handle is *not* held,
//!   because it was measured not to matter.
//! * **Closing never blocks.** Dropping a request with a transfer in flight
//!   returns promptly. The transfer then fails with
//!   [`WinHttpError::Cancelled`] — as a failure, never as an empty successful
//!   body.
//! * **Header queries are synchronous.** [`Request::status_code`],
//!   [`Request::header`] and [`Request::raw_headers`] are ordinary methods, not
//!   futures. `WinHttpQueryHeaders` answers on the calling thread even on an
//!   async handle. Making them `async` would be a lie about what they do.
//! * **An empty header block means no headers.** `Some(Vec::new())` passed to
//!   [`Request::send`] is normalised to "no additional headers" before it
//!   reaches the platform. Forwarding it verbatim faults the process: the
//!   empty `Vec`'s dangling pointer is handed to `WinHttpSendRequest`, which
//!   dereferences it whatever the stated length says. A caller assembling a
//!   header block from a loop that happened to produce nothing should not have
//!   to know that, so the module absorbs it.
//! * **Redirects are followed by default, and that is rarely what you want.**
//!   The platform default rewrites a `POST` into a `GET` across a `301` without
//!   telling the caller, and fails outright on any request whose body was
//!   streamed. [`Session::set_redirect_policy`] turns it off.
//!
//! # Why this module is not built on the IOCP core
//!
//! Every other I/O module in this crate — [`fs`](crate::fs),
//! [`pipe`](crate::pipe), [`net`](crate::net), [`httpsys`](crate::httpsys) — is
//! generic over a [`Submitter`](crate::iocp::Submitter), so the same code runs
//! on a caller-driven [`Proactor`](crate::iocp::Proactor) or on the system
//! thread pool. This module is generic over nothing, and that is not an
//! oversight or unfinished work.
//!
//! The IOCP core exists to make *overlapped* APIs awaitable: you hand Windows
//! an `OVERLAPPED`, Windows posts a completion packet to a port you own, and
//! the crate matches the packet back to the operation that produced it.
//! WinHTTP's asynchronous mode is not built that way. It exposes no
//! `OVERLAPPED`, offers no way to associate a handle with a completion port,
//! and instead runs its own internal thread pool that invokes a
//! `WINHTTP_STATUS_CALLBACK` when an operation finishes. There is nothing to
//! give a [`Proactor`](crate::iocp::Proactor), so a `Submitter` bound would be
//! a parameter that changed nothing.
//!
//! The consequence is not a limitation. The module is a self-contained
//! [`Waker`](std::task::Waker)-driven state machine that WinHTTP's callback
//! wakes, so it runs under **any** executor, including a bare
//! `futures::executor::block_on` with no reactor and no worker threads. It does
//! borrow the crate's buffer vocabulary — [`IoBuf`],
//! [`IoBufMut`] and
//! [`OpResult`](crate::iocp::OpResult) — so that a caller mixing this module
//! with the others learns one ownership discipline rather than two.
//!
//! One further asymmetry follows from the same root: WinHTTP's callback can run
//! **inline**, on the thread that submitted the operation, re-entering before
//! the submitting call has returned. An IOCP completion never does that. Every
//! submission here therefore releases its lock before calling WinHTTP, and
//! stores its waker before rather than after the call.
//!
//! # Buffers, cancellation, and what abandoning an operation costs
//!
//! A pending future may be dropped at any time — by `select!`, by a timeout, by
//! task cancellation, or by a plain `drop`. WinHTTP does not know that
//! happened, and will still write into the buffer it was given. So:
//!
//! * **The buffer is retired, not freed.** A dropped future moves its buffer
//!   into the request's completion context, where it stays until WinHTTP
//!   delivers the completion for the abandoned operation — or until the handle
//!   closes, whichever comes first. Nothing is freed while the platform can
//!   still write to it.
//! * **You do not get the buffer back.** This is the same trade the IOCP core
//!   already makes and states: await the future if you need the memory
//!   returned.
//! * **The request is parked until the abandoned completion lands.** WinHTTP
//!   allows one transfer at a time, so until the abandoned one finishes, the
//!   next call resolves to [`WinHttpError::OperationInProgress`]. The supported
//!   recovery is to drop the request; retrying is possible but the timing is
//!   the platform's to decide, not yours.
//!
//! **The send body is a deliberate exception.** [`Request::send`] takes
//! ownership of the request body and never gives it back, which is why it
//! resolves to `Result<(), Error>` rather than the `OpResult<_, B>` every other
//! operation here returns. WinHTTP is documented as reading that body until the
//! *response* has been received, not merely until the send completes, because
//! it may re-send it unprompted to follow a redirect or answer an
//! authentication challenge. Returning the buffer at send-complete would
//! therefore hand the caller memory the platform is still reading. The body is
//! held in the request's context instead, and released when
//! [`Request::receive_response`] completes or the handle closes.
//!
//! **The alternative that was rejected.** The obvious other answer is to make
//! operations non-cancel-safe by construction: have the future's `Drop` block
//! until WinHTTP signals completion, so the buffer can be handed back every
//! time. It was rejected for three reasons, the first of which is fatal on its
//! own.
//!
//! 1. WinHTTP delivers completion callbacks — and the final `HANDLE_CLOSING` —
//!    *on the dropping thread*, inline. A `Drop` that blocks waiting for a
//!    callback that is waiting for the same thread deadlocks.
//! 2. `futures::executor::block_on` on a single thread with no reactor is an
//!    executor this module is required to support. A blocking `Drop` there
//!    would hang outright.
//! 3. The crate has already rejected blocking in `Drop` once, for graceful
//!    socket close, on the grounds that "a graceful close can block and a
//!    `Drop` that blocks is worse than one that is abrupt". The same reasoning
//!    applies unchanged.
//!
//! # Closing handles
//!
//! Dropping any of these types closes its handle and returns; it does not wait
//! for outstanding work. Completions still in flight then fail with
//! [`WinHttpError::Cancelled`], and WinHTTP delivers a final `HANDLE_CLOSING`
//! notification. That notification is the *only* signal that says the platform
//! will never call back again, and it is where this module releases the
//! request's completion context.
//!
//! It is worth knowing why there is no "unregister the callback first" step,
//! since that is the obvious defensive move: `WinHttpSetStatusCallback` with a
//! null callback **fails** while an operation is in flight, and notifications
//! continue to arrive afterwards. Unregistering is not available as a teardown
//! step, so the design does not attempt it.
//!
//! # Example
//!
//! A complete GET. The executor is `futures::executor::block_on`; nothing here
//! needs a runtime.
//!
//! ```no_run
//! # use windows::core::HSTRING;
//! # use winasio::iocp::OpResult;
//! # use winasio::winhttp::Session;
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let session = Session::new(&HSTRING::from("winasio-example"))?;
//! let connection = session.connect(&HSTRING::from("example.com"), 80)?;
//! let mut request =
//!     connection.open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)?;
//!
//! futures::executor::block_on(async {
//!     request.send(None, Vec::new(), 0).await?;
//!     request.receive_response().await?;
//!
//!     let status = request.status_code()?;
//!     println!("status {status}");
//!
//!     let mut body = Vec::new();
//!     loop {
//!         let available = request.query_data_available().await?;
//!         if available == 0 {
//!             break;
//!         }
//!         let OpResult(read, chunk) =
//!             request.read_data(Vec::with_capacity(available as usize)).await;
//!         body.extend_from_slice(&chunk[..read?]);
//!     }
//!     println!("{}", String::from_utf8_lossy(&body));
//!     Ok::<_, windows::core::Error>(())
//! })?;
//! # Ok(())
//! # }
//! ```

mod consts;
mod context;
mod error;
mod handle;
mod ops;

use std::ffi::c_void;
use std::sync::Arc;

use windows::core::{Error, HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpSetOption,
    WinHttpSetStatusCallback, WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_FLAG_ASYNC,
    WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_OPTION_CONTEXT_VALUE,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_ALWAYS,
    WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    WINHTTP_OPTION_SECURITY_FLAGS, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_RAW_HEADERS_CRLF,
    WINHTTP_QUERY_STATUS_CODE,
};

use crate::iocp::{IoBuf, IoBufMut};

use consts::{
    SECURITY_FLAG_IGNORE_CERT_CN_INVALID, SECURITY_FLAG_IGNORE_CERT_DATE_INVALID,
    SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE, SECURITY_FLAG_IGNORE_UNKNOWN_CA,
    WINHTTP_CALLBACK_FLAG_ALL_NOTIFICATIONS, WINHTTP_INVALID_STATUS_CALLBACK,
    WINHTTP_NO_HEADER_INDEX,
};
use context::RequestContext;
use handle::Handle;

pub use context::live_context_count;
pub use error::WinHttpError;
pub use ops::{QueryDataAvailable, ReadData, ReceiveResponse, SendRequest, WriteData};

/// How a session reaches the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessType {
    /// Let WinHTTP discover the proxy configuration.
    AutomaticProxy,
    /// Use the proxy named when the session was created.
    NamedProxy,
}

impl From<AccessType> for WINHTTP_ACCESS_TYPE {
    fn from(access: AccessType) -> Self {
        match access {
            AccessType::AutomaticProxy => WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            AccessType::NamedProxy => WINHTTP_ACCESS_TYPE_NAMED_PROXY,
        }
    }
}

/// Whether WinHTTP follows redirects on the caller's behalf.
///
/// The platform default is [`RedirectPolicy::Always`], and that default is
/// surprising enough to be worth stating plainly. Measured behaviour:
///
/// - A `301` answering a `POST` is replayed as a `GET` with the body dropped.
///   The caller never sees the `301`; it sees the final response and has no way
///   to tell that the method it asked for is not the method that was used.
/// - A redirect arriving for a request whose body was written with
///   [`Request::write_data`] — or which declared `Transfer-Encoding: chunked` —
///   fails the transfer outright with `ERROR_WINHTTP_RESEND_REQUEST` (12032),
///   because the platform cannot replay a body it never held.
///
/// A caller that wants to implement its own redirect policy, or that streams
/// request bodies, wants [`RedirectPolicy::Never`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectPolicy {
    /// Return the redirect response to the caller; follow nothing.
    Never,
    /// Follow redirects, except from `https` to `http`.
    DisallowHttpsToHttp,
    /// Follow every redirect, including a downgrade to `http`. The platform
    /// default.
    Always,
}

impl RedirectPolicy {
    fn value(self) -> u32 {
        match self {
            RedirectPolicy::Never => WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
            RedirectPolicy::DisallowHttpsToHttp => {
                WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP
            }
            RedirectPolicy::Always => WINHTTP_OPTION_REDIRECT_POLICY_ALWAYS,
        }
    }
}

/// Certificate checks to waive on a request.
///
/// Every field defaults to `false`, and each one names exactly what it turns
/// off. There is deliberately no single "insecure" switch: relaxing a specific
/// check for a specific reason is a defensible decision, and turning off
/// certificate validation wholesale by accident should not be one keystroke
/// away.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CertificateRelaxations {
    /// Accept a certificate signed by an authority this machine does not trust.
    pub unknown_certificate_authority: bool,
    /// Accept a certificate whose subject does not match the host requested.
    pub wrong_host_name: bool,
    /// Accept a certificate that has expired or is not yet valid.
    pub certificate_date_invalid: bool,
    /// Accept a certificate whose key usage does not permit server
    /// authentication.
    pub wrong_usage: bool,
}

impl CertificateRelaxations {
    fn bits(self) -> u32 {
        let mut bits = 0;
        if self.unknown_certificate_authority {
            bits |= SECURITY_FLAG_IGNORE_UNKNOWN_CA;
        }
        if self.wrong_host_name {
            bits |= SECURITY_FLAG_IGNORE_CERT_CN_INVALID;
        }
        if self.certificate_date_invalid {
            bits |= SECURITY_FLAG_IGNORE_CERT_DATE_INVALID;
        }
        if self.wrong_usage {
            bits |= SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE;
        }
        bits
    }
}

/// Process-scoped WinHTTP configuration.
///
/// The handle is held behind an [`Arc`] so that every [`Connection`] and
/// [`Request`] derived from it keeps it open. That is not tidiness: a request
/// whose *session* handle has been closed fails every subsequent operation with
/// [`WinHttpError::Cancelled`], reported inline before the submitting call even
/// returns. Closing the intervening *connection* handle, by contrast, was
/// measured to be harmless — a request keeps working — so only the session is
/// held.
pub struct Session {
    handle: Arc<Handle>,
}

impl Session {
    /// Open a session that discovers its proxy configuration automatically.
    pub fn new(agent: &HSTRING) -> Result<Session, Error> {
        Session::with_proxy(agent, AccessType::AutomaticProxy, None, None)
    }

    /// Open a session with an explicit access type and optional proxy.
    pub fn with_proxy(
        agent: &HSTRING,
        access_type: AccessType,
        proxy: Option<&HSTRING>,
        proxy_bypass: Option<&HSTRING>,
    ) -> Result<Session, Error> {
        let proxy = proxy.map(wide).unwrap_or_else(PCWSTR::null);
        let bypass = proxy_bypass.map(wide).unwrap_or_else(PCWSTR::null);
        // `WINHTTP_FLAG_ASYNC` is unconditional. It is not a parameter, and
        // there is no code path that clears it: the whole module assumes
        // completions arrive through the status callback.
        //
        // SAFETY: all four strings outlive the call.
        let raw = unsafe {
            WinHttpOpen(
                wide(agent),
                access_type.into(),
                proxy,
                bypass,
                WINHTTP_FLAG_ASYNC,
            )
        };
        if raw.is_null() {
            return Err(Error::from_thread());
        }
        // SAFETY: a fresh handle that nothing else owns.
        Ok(Session {
            handle: Arc::new(unsafe { Handle::from_raw(raw) }),
        })
    }

    /// Set the resolve, connect, send and receive deadlines, in milliseconds.
    ///
    /// Zero means "no timeout". Timeouts set here are inherited by every
    /// request derived from this session, which is why there is no per-request
    /// equivalent.
    ///
    /// A deadline that elapses surfaces as [`WinHttpError::Timeout`] on the
    /// operation that was waiting — as a failure, not as an empty response.
    pub fn set_timeouts(
        &self,
        resolve_ms: i32,
        connect_ms: i32,
        send_ms: i32,
        receive_ms: i32,
    ) -> Result<(), Error> {
        // SAFETY: the handle is owned by `self` and live for the call.
        unsafe {
            WinHttpSetTimeouts(
                self.handle.as_raw(),
                resolve_ms,
                connect_ms,
                send_ms,
                receive_ms,
            )
        }
    }

    /// Choose whether WinHTTP follows redirects on its own.
    ///
    /// Set on the session, and inherited by every request derived from it —
    /// measured, not assumed: setting the option on the session handle before
    /// `connect` governs requests opened afterwards, so there is no need to
    /// touch each request. The equivalent per-request option exists and works
    /// too, but a session-wide setting is one call instead of one per transfer.
    ///
    /// See [`RedirectPolicy`] for what each choice means and why the platform
    /// default is worth overriding.
    pub fn set_redirect_policy(&self, policy: RedirectPolicy) -> Result<(), Error> {
        let bytes = policy.value().to_ne_bytes();
        // SAFETY: the handle is owned by `self` and the buffer outlives the
        // call.
        unsafe {
            WinHttpSetOption(
                Some(self.handle.as_raw().cast_const()),
                WINHTTP_OPTION_REDIRECT_POLICY,
                Some(&bytes),
            )
        }
    }

    /// Name a server and port to talk to.
    ///
    /// The returned [`Connection`] does not borrow the session: closing the
    /// session does not close or disturb it.
    pub fn connect(&self, server: &HSTRING, port: u16) -> Result<Connection, Error> {
        // SAFETY: the handle and the server name are live for the call.
        let raw = unsafe {
            WinHttpConnect(
                self.handle.as_raw(),
                wide(server),
                port,
                /* reserved */ 0,
            )
        };
        if raw.is_null() {
            return Err(Error::from_thread());
        }
        // SAFETY: a fresh handle that nothing else owns.
        Ok(Connection {
            handle: unsafe { Handle::from_raw(raw) },
            session: Arc::clone(&self.handle),
        })
    }
}

/// A named server and port.
///
/// Keeps its [`Session`] alive; see [`Session`] for why.
pub struct Connection {
    handle: Handle,
    session: Arc<Handle>,
}

impl Connection {
    /// Open a request.
    ///
    /// `accept_types` is the list of MIME types the client will accept; pass an
    /// empty slice for none. Set `secure` to require TLS.
    ///
    /// The returned [`Request`] does not borrow the connection.
    pub fn open_request(
        &self,
        verb: &HSTRING,
        object_name: &HSTRING,
        accept_types: &[HSTRING],
        secure: bool,
    ) -> Result<Request, Error> {
        // A null-terminated array of pointers, as the API expects. The `HSTRING`
        // values live in the caller's slice for the whole call.
        let mut accept: Vec<PCWSTR> = accept_types.iter().map(wide).collect();
        let accept_ptr = if accept.is_empty() {
            std::ptr::null_mut()
        } else {
            accept.push(PCWSTR::null());
            accept.as_mut_ptr()
        };

        let flags = if secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS::default()
        };

        // SAFETY: every string and the accept array outlive the call.
        let raw = unsafe {
            WinHttpOpenRequest(
                self.handle.as_raw(),
                wide(verb),
                wide(object_name),
                PCWSTR::null(),
                PCWSTR::null(),
                accept_ptr,
                flags,
            )
        };
        if raw.is_null() {
            return Err(Error::from_thread());
        }
        // SAFETY: a fresh handle that nothing else owns.
        let handle = unsafe { Handle::from_raw(raw) };

        Request::arm(handle, Arc::clone(&self.session))
    }
}

/// A request, and the only handle transfers happen on.
///
/// Dropping a `Request` closes its handle immediately, without waiting for any
/// transfer still in flight. See the module documentation for what that costs.
pub struct Request {
    // Declaration order is load-bearing: fields drop in order, so the request
    // handle is closed *before* this type's reference to the context is
    // released. The status callback can fire inline on this very thread during
    // that close, and it needs the context to still be alive when it does.
    //
    // The session reference is last for the same reason, one level up: a
    // request whose session handle has already been closed cannot complete any
    // operation, so the session must outlive the request.
    handle: Handle,
    context: Arc<RequestContext>,
    // Never read, and that is the point: its only job is to keep the session
    // handle open for as long as this request exists.
    #[allow(dead_code)]
    session: Arc<Handle>,
}

impl Request {
    /// Install the completion context on a freshly opened request handle and
    /// register the status callback.
    ///
    /// # The ownership boundary
    ///
    /// This function is the only place in the module where a leak or a double
    /// free is possible, so the order of operations is fixed:
    ///
    /// 1. `Arc::into_raw` — infallible.
    /// 2. `WinHttpSetOption(WINHTTP_OPTION_CONTEXT_VALUE)` — fallible.
    /// 3. `WinHttpSetStatusCallback` — fallible.
    /// 4. **Ownership boundary.** Nothing fallible may follow.
    ///
    /// The order of 2 and 3 matters. Setting the option first means there is no
    /// interval in which the callback is live but `dwContext` is still zero.
    /// Reversed, a failure at step 2 would close a handle whose callback fires
    /// with a zero context — the callback would return early, and the raw
    /// reference would leak with nothing left to reclaim it.
    ///
    /// Before step 3 succeeds, no `HANDLE_CLOSING` carrying the pointer can
    /// ever be delivered, so the failure paths reclaim the reference here.
    /// After step 3 succeeds the pointer belongs to WinHTTP, and per-request
    /// options such as TLS relaxations are therefore applied by methods on the
    /// returned value rather than inside this constructor: adding a fallible
    /// step below the boundary would leave no correct way to unwind.
    fn arm(handle: Handle, session: Arc<Handle>) -> Result<Request, Error> {
        let context = RequestContext::new();
        let raw = Arc::into_raw(Arc::clone(&context));
        let value = raw as usize;

        // Step 2.
        let bytes = value.to_ne_bytes();
        // SAFETY: the handle is live, and the buffer outlives the call.
        let set = unsafe {
            WinHttpSetOption(
                Some(handle.as_raw().cast_const()),
                WINHTTP_OPTION_CONTEXT_VALUE,
                Some(&bytes),
            )
        };
        if let Err(error) = set {
            // SAFETY: nothing has been told about `raw` yet, so this is the
            // only reference to reclaim and doing so cannot race anything.
            drop(unsafe { Arc::from_raw(raw) });
            handle.close_now();
            return Err(error);
        }

        // Step 3.
        // SAFETY: the handle is live for the call.
        let previous = unsafe {
            WinHttpSetStatusCallback(
                handle.as_raw(),
                Some(context::status_callback),
                WINHTTP_CALLBACK_FLAG_ALL_NOTIFICATIONS,
                0,
            )
        };
        // The API reports failure by returning `WINHTTP_INVALID_STATUS_CALLBACK`
        // — all bits set — not by returning null, and not through a `Result`.
        // The previous implementation `unwrap`ped this comparison and panicked
        // on the caller's thread; a registration failure is an ordinary error.
        let previous: usize = previous.map_or(0, |f| f as usize);
        if previous == WINHTTP_INVALID_STATUS_CALLBACK {
            let error = Error::from_thread();
            // SAFETY: registration failed, so no callback will ever be invoked
            // with this pointer and no `HANDLE_CLOSING` can reclaim it.
            drop(unsafe { Arc::from_raw(raw) });
            handle.close_now();
            return Err(error);
        }

        // Step 4. The raw reference now belongs to WinHTTP and is released only
        // by the `HANDLE_CLOSING` arm of the callback. Nothing fallible below
        // this line.
        Ok(Request {
            handle,
            context,
            session,
        })
    }

    /// The context pointer, as WinHTTP knows it.
    ///
    /// Handed to `WinHttpSendRequest` as `dwContext`. It is deliberately the
    /// *same* pointer already installed with `WINHTTP_OPTION_CONTEXT_VALUE`: a
    /// non-zero `dwContext` overrides the option permanently, while zero leaves
    /// it alone, so passing the identical value makes the precedence question
    /// moot.
    pub(crate) fn context_value(&self) -> usize {
        Arc::as_ptr(&self.context) as usize
    }

    /// Waive specific certificate checks for this request.
    ///
    /// Applies only to a request opened with `secure = true`, and must be
    /// called before [`Request::send`].
    pub fn relax_certificate_validation(
        &mut self,
        relaxations: CertificateRelaxations,
    ) -> Result<(), Error> {
        let bytes = relaxations.bits().to_ne_bytes();
        // SAFETY: the handle is live and the buffer outlives the call.
        unsafe {
            WinHttpSetOption(
                Some(self.handle.as_raw().cast_const()),
                WINHTTP_OPTION_SECURITY_FLAGS,
                Some(&bytes),
            )
        }
    }

    /// Send the request, with an optional header block and an optional body.
    ///
    /// `headers` is a raw `name: value` block encoded as UTF-16;
    /// [`encode_headers`] builds one. It is taken by value because WinHTTP
    /// reads it asynchronously for the duration of the send — the same reason
    /// the body is.
    ///
    /// `body` is **consumed, not returned** — unlike every other operation in
    /// this module. WinHTTP may re-read it after the send completes (to follow
    /// a redirect, or to answer an authentication challenge), so it is held in
    /// the request's context until [`Request::receive_response`] completes or
    /// the handle closes. Pass an empty `Vec<u8>` for a request with no body.
    ///
    /// `total_length` is the total size of the body across this call and any
    /// subsequent [`Request::write_data`] calls; pass the length of `body` when
    /// there will be no further writes.
    ///
    /// # Errors
    ///
    /// Fails with `ERROR_INVALID_PARAMETER` if `body` is larger than
    /// `u32::MAX`. A send is refused rather than truncated: a short send is not
    /// a partial success but a different request.
    pub fn send<B: IoBuf + Send>(
        &mut self,
        headers: Option<Vec<u16>>,
        body: B,
        total_length: u32,
    ) -> SendRequest<'_, B> {
        SendRequest::new(self, headers, body, total_length)
    }

    /// Write one chunk of the request body.
    ///
    /// The buffer is returned when the future resolves, along with the number
    /// of bytes WinHTTP accepted.
    pub fn write_data<B: IoBuf + Send>(&mut self, buffer: B) -> WriteData<'_, B> {
        WriteData::new(self, buffer)
    }

    /// Await the response headers.
    ///
    /// Must complete before [`Request::status_code`], [`Request::header`],
    /// [`Request::query_data_available`] or [`Request::read_data`] will produce
    /// anything.
    pub fn receive_response(&mut self) -> ReceiveResponse<'_> {
        ReceiveResponse::new(self)
    }

    /// Ask how many body bytes can be read without waiting.
    ///
    /// **Zero means the body has ended.** This, and not a zero-length
    /// [`Request::read_data`], is the end-of-body signal.
    pub fn query_data_available(&mut self) -> QueryDataAvailable<'_> {
        QueryDataAvailable::new(self)
    }

    /// Read body bytes into a caller-owned buffer.
    ///
    /// The number of bytes requested is the buffer's own spare capacity and
    /// nothing else — there is no separate length argument to get wrong. A
    /// buffer with no spare capacity is rejected rather than submitted, because
    /// a zero-length read would otherwise look exactly like the end of the
    /// body.
    pub fn read_data<B: IoBufMut + Send>(&mut self, buffer: B) -> ReadData<'_, B> {
        ReadData::new(self, buffer)
    }

    /// The response status code.
    ///
    /// Synchronous: `WinHttpQueryHeaders` answers on the calling thread even on
    /// an asynchronous handle, and produces no notification.
    pub fn status_code(&self) -> Result<u32, Error> {
        let mut value: u32 = 0;
        let mut length = size_of::<u32>() as u32;
        // SAFETY: the handle is live and the out-parameters outlive the call.
        unsafe {
            WinHttpQueryHeaders(
                self.handle.as_raw(),
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&raw mut value).cast::<c_void>()),
                &mut length,
                WINHTTP_NO_HEADER_INDEX,
            )
        }?;
        Ok(value)
    }

    /// A single response header, by name.
    ///
    /// Returns `Ok(None)` when the server did not send that header. An absent
    /// header is an ordinary answer, not a transport failure, and collapsing
    /// the two would leave a caller unable to tell "no such header" from "the
    /// query failed". A caller that wants the platform's own error can compare
    /// against [`WinHttpError::HeaderNotFound`].
    pub fn header(&self, name: &HSTRING) -> Result<Option<String>, Error> {
        match self.query_string(WINHTTP_QUERY_CUSTOM, wide(name)) {
            Ok(value) => Ok(Some(value)),
            Err(error) if WinHttpError::from_error(&error) == WinHttpError::HeaderNotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Every response header, as one CRLF-separated block including the status
    /// line.
    pub fn raw_headers(&self) -> Result<String, Error> {
        self.query_string(WINHTTP_QUERY_RAW_HEADERS_CRLF, PCWSTR::null())
    }

    /// The two-call sizing protocol, measured rather than assumed.
    ///
    /// The sizing call fails with `ERROR_INSUFFICIENT_BUFFER` and reports the
    /// required size **in bytes, including** the terminating NUL. The data call
    /// then reports the length **in bytes, excluding** it. Both are byte counts
    /// for a UTF-16 string, so the element count is half.
    fn query_string(&self, level: u32, name: PCWSTR) -> Result<String, Error> {
        let mut length: u32 = 0;
        // SAFETY: a null buffer with a zero length is the documented way to ask
        // for the size; the out-parameter outlives the call.
        let sized = unsafe {
            WinHttpQueryHeaders(
                self.handle.as_raw(),
                level,
                name,
                None,
                &mut length,
                WINHTTP_NO_HEADER_INDEX,
            )
        };
        match sized {
            // A zero-length answer, which some headers legitimately give.
            Ok(()) => return Ok(String::new()),
            Err(error) => {
                let insufficient = windows::core::HRESULT::from_win32(
                    windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER.0,
                );
                if error.code() != insufficient {
                    // Anything else — including `ERROR_WINHTTP_HEADER_NOT_FOUND`,
                    // which the caller above turns into `None` — is reported as
                    // it stands.
                    return Err(error);
                }
            }
        }

        // `length` is a byte count and includes the NUL.
        let elements = (length as usize).div_ceil(2);
        let mut buffer: Vec<u16> = vec![0; elements];
        let mut length = (elements * 2) as u32;
        // SAFETY: the buffer is at least `length` bytes and outlives the call.
        unsafe {
            WinHttpQueryHeaders(
                self.handle.as_raw(),
                level,
                name,
                Some(buffer.as_mut_ptr().cast::<c_void>()),
                &mut length,
                WINHTTP_NO_HEADER_INDEX,
            )
        }?;
        // This time the length excludes the NUL.
        let elements = (length as usize / 2).min(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..elements]))
    }
}

/// `WINHTTP_QUERY_CUSTOM` — query a header by name rather than by known index.
const WINHTTP_QUERY_CUSTOM: u32 = 65535;

/// Borrow an [`HSTRING`] as a `PCWSTR`.
///
/// The result borrows the string, so every use is written so the `HSTRING`
/// outlives the WinHTTP call it is passed to.
fn wide(text: &HSTRING) -> PCWSTR {
    PCWSTR(text.as_ptr())
}

/// Encode a header block for [`Request::send`].
///
/// Joins `name: value` pairs with CRLF, which is the form `WinHttpSendRequest`
/// expects.
pub fn encode_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<u16> {
    let mut text = String::new();
    for (name, value) in headers {
        text.push_str(name);
        text.push_str(": ");
        text.push_str(value);
        text.push_str("\r\n");
    }
    text.encode_utf16().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_redirect_policy_maps_to_its_own_platform_value() {
        // Three constants that differ by one, in a header where two of them
        // share a value with something else. A transposition here would be a
        // silent behaviour change, not a compile error.
        assert_eq!(RedirectPolicy::Never.value(), 0);
        assert_eq!(RedirectPolicy::DisallowHttpsToHttp.value(), 1);
        assert_eq!(RedirectPolicy::Always.value(), 2);
    }

    #[test]
    fn each_certificate_relaxation_sets_only_its_own_bit() {
        // A table, because a copy-paste error between four near-identical
        // fields would otherwise be invisible: the request would still work,
        // just with the wrong check waived.
        assert_eq!(CertificateRelaxations::default().bits(), 0);
        for (relaxations, expected) in [
            (
                CertificateRelaxations {
                    unknown_certificate_authority: true,
                    ..Default::default()
                },
                SECURITY_FLAG_IGNORE_UNKNOWN_CA,
            ),
            (
                CertificateRelaxations {
                    wrong_host_name: true,
                    ..Default::default()
                },
                SECURITY_FLAG_IGNORE_CERT_CN_INVALID,
            ),
            (
                CertificateRelaxations {
                    certificate_date_invalid: true,
                    ..Default::default()
                },
                SECURITY_FLAG_IGNORE_CERT_DATE_INVALID,
            ),
            (
                CertificateRelaxations {
                    wrong_usage: true,
                    ..Default::default()
                },
                SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE,
            ),
        ] {
            assert_eq!(relaxations.bits(), expected);
        }
    }

    #[test]
    fn all_four_relaxations_combine_without_colliding() {
        let all = CertificateRelaxations {
            unknown_certificate_authority: true,
            wrong_host_name: true,
            certificate_date_invalid: true,
            wrong_usage: true,
        };
        assert_eq!(all.bits().count_ones(), 4);
    }

    #[test]
    fn headers_are_encoded_as_crlf_separated_pairs() {
        let encoded = encode_headers([("Accept", "text/plain"), ("X-Trace", "1")]);
        assert_eq!(
            String::from_utf16_lossy(&encoded),
            "Accept: text/plain\r\nX-Trace: 1\r\n"
        );
    }

    #[test]
    fn an_empty_header_set_encodes_to_nothing() {
        assert!(encode_headers([]).is_empty());
    }
}
