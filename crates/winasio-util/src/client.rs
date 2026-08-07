// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The client: an [`http::Request`] in, an [`http::Response`] out.
//!
//! # Invariants and obligations
//!
//! * **One request per call, but not one connection per call.** Each call to
//!   [`Client::request`] sends one request and returns one response. It does
//!   *not* follow that each call gets its own socket: WinHTTP keeps a keep-alive
//!   connection pool underneath, and that pool is outside this crate's control.
//!   See "Connection reuse belongs to the platform" below, because it has a
//!   consequence callers have to know about.
//! * **The client owns message framing.** `Content-Length` and
//!   `Transfer-Encoding` are derived from the body's
//!   [`size_hint`](http_body::Body::size_hint). A caller that sets either is
//!   refused with [`BodyError::FramingHeaderNotAllowed`], because a caller-supplied
//!   value replaces the computed one on the wire — measured — and would then
//!   describe a body that was never sent.
//! * **A header that cannot be sent is refused, not converted.** See
//!   [`crate::headers`] for the argument.
//! * **A body that was cut off is not a body that ended.** See [`crate::body`].
//! * **Redirects are not followed.** See below.
//! * **No runtime is required.** Nothing here spawns a task, blocks a thread or
//!   touches a reactor; the returned futures run on any executor, including
//!   `futures::executor::block_on`.
//!
//! # Connection reuse belongs to the platform
//!
//! This crate implements no connection pool, and an earlier draft of these docs
//! said flatly that there was "no pool and no reuse". That was wrong, and the
//! measurement that corrected it is worth recording, because the difference is
//! visible to callers.
//!
//! WinHTTP maintains its own keep-alive pool. Measured, against a server that
//! answers an HTTP/1.1 request without `Connection: close` and then closes:
//!
//! * The pool is **process-wide, not per-session**. Building a brand new
//!   [`Client`] for every request — a fresh `WinHttpOpen` session, closed again
//!   afterwards — reused sockets exactly as often as sharing one client did.
//!   Dropping the client does not drop the pooled connection.
//! * WinHTTP does **not retry** a pooled socket that turns out to be dead. The
//!   request fails with `ERROR_WINHTTP_CONNECTION_ERROR` at
//!   [`receive_response`](winasio::winhttp::Request::receive_response), which
//!   arrives here as [`RequestError::Transport`] with [`RequestStage::ReceiveResponse`].
//! * That is WinHTTP's own behaviour, not something this crate provokes: a
//!   hand-written *synchronous* WinHTTP client, sharing nothing with this crate
//!   but the platform, failed identically, and the failure is unaffected by
//!   this crate's redirect-policy setting.
//!
//! The consequence is that a perfectly healthy server which closes an idle
//! keep-alive connection can occasionally hand a caller a transport error that
//! a retry would have made disappear. This crate does not retry, for three
//! reasons. Retry policy is out of its scope. Replaying a request that is not
//! idempotent is unsafe, because the server may already have acted on it. And
//! structurally it could not: `WinHttpSendRequest` consumes the request body
//! and never returns it, so by the time the failure is visible there is nothing
//! left to replay.
//!
//! What a caller is owed instead is the ability to *see* it, and that is
//! guaranteed: the failure is a [`RequestStage::ReceiveResponse`] transport error
//! whose [`RequestError::win_http`] is [`WinHttpError::ConnectionError`], never a
//! silent truncation and never a hang. A caller that knows its own request is
//! idempotent can retry on exactly that.
//!
//! # Why redirects are off
//!
//! WinHTTP follows them by default, and the default is measurably wrong for a
//! client that hands its caller an `http::Response`:
//!
//! * A `301` answering a `POST` is replayed as a `GET` with the body dropped.
//!   The caller is shown the final `200` and cannot tell that the method and
//!   body it asked for are not what was sent.
//! * A redirect arriving for a request whose body was streamed fails the whole
//!   transfer with `ERROR_WINHTTP_RESEND_REQUEST`, because the platform cannot
//!   replay a body it never held. Since this client streams every request body,
//!   leaving redirects on would make them a latent failure on any redirecting
//!   endpoint.
//!
//! So a `3xx` is returned to the caller as the response it is. Redirect policy
//! is explicitly out of this crate's scope; a caller that wants the platform's
//! can ask for it with [`ClientBuilder::platform_redirects`], with the caveats
//! above.
//!
//! # Why a request body of unknown length is chunked rather than buffered
//!
//! `WinHttpSendRequest` takes the total length up front. When
//! [`size_hint`](http_body::Body::size_hint) gives an exact value that is what
//! gets declared. When it does not, the two options are to buffer the whole
//! body to learn its length, or to declare `Transfer-Encoding: chunked` and
//! frame the chunks.
//!
//! Buffering needs an arbitrary bound, turns a streaming body into a
//! non-streaming one, and rejects bodies that are legitimately larger than
//! memory. Chunking was measured to work: declaring the header, passing a total
//! length of zero and writing the chunk framing by hand puts a correct chunked
//! message on the wire, and WinHTTP adds no `Content-Length` of its own and
//! does no framing of its own. So chunking it is.
//!
//! (Measured and worth knowing: a total length of zero *without* the header,
//! with a non-empty body, is not treated as chunked — it fails with
//! `ERROR_INVALID_PARAMETER`.)

use std::pin::Pin;
use std::sync::Arc;

use bytes::Buf;
use http::{Request as HttpRequest, Response as HttpResponse, StatusCode};
use http_body::Body;
use winasio::winhttp::{CertificateRelaxations, RedirectPolicy, Session};
use windows::core::HSTRING;

use crate::body::ResponseBody;
use crate::error::{BodyError, ClientConfigError, ClientConfigStage, RequestError, RequestStage};
use crate::{headers, uri};

/// The most this crate will write to the platform in one call.
///
/// Only a ceiling on the buffer handed to `write_data`; a body producing larger
/// frames is written in several calls.
const MAX_WRITE: usize = 64 * 1024;

/// Build a [`Client`].
///
/// ```
/// # fn main() -> Result<(), winasio_util::ClientConfigError> {
/// use winasio_util::Client;
///
/// let client = Client::builder("my-agent/1.0")
///     .timeouts(5_000, 5_000, 5_000, 30_000)
///     .build()?;
/// # let _ = client;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ClientBuilder {
    agent: HSTRING,
    timeouts: Option<(i32, i32, i32, i32)>,
    relaxations: CertificateRelaxations,
    platform_redirects: bool,
}

impl ClientBuilder {
    /// Resolve, connect, send and receive deadlines, in milliseconds.
    ///
    /// Zero means no deadline. Left unset, the platform's own defaults apply.
    /// A deadline that elapses is reported as an error, never as an empty
    /// response.
    pub fn timeouts(mut self, resolve: i32, connect: i32, send: i32, receive: i32) -> Self {
        self.timeouts = Some((resolve, connect, send, receive));
        self
    }

    /// Certificate checks to waive on every request this client makes.
    ///
    /// Each field names exactly one check. There is deliberately no single
    /// "insecure" switch, for the reasons set out on
    /// [`CertificateRelaxations`].
    pub fn certificate_relaxations(mut self, relaxations: CertificateRelaxations) -> Self {
        self.relaxations = relaxations;
        self
    }

    /// Let WinHTTP follow redirects instead of returning them.
    ///
    /// Off by default. Read the module documentation before turning it on: the
    /// platform rewrites a `POST` into a `GET` across a `301` without saying
    /// so, and fails outright on any request body of unknown length, which this
    /// client sends chunked.
    pub fn platform_redirects(mut self, follow: bool) -> Self {
        self.platform_redirects = follow;
        self
    }

    /// Open the session.
    pub fn build(self) -> Result<Client, ClientConfigError> {
        let session = Session::new(&self.agent)
            .map_err(ClientConfigError::at(ClientConfigStage::OpenSession))?;
        if let Some((resolve, connect, send, receive)) = self.timeouts {
            session
                .set_timeouts(resolve, connect, send, receive)
                .map_err(ClientConfigError::at(ClientConfigStage::SetTimeouts))?;
        }
        // Set unconditionally, so that the platform default is never what
        // decides the behaviour.
        session
            .set_redirect_policy(if self.platform_redirects {
                RedirectPolicy::Always
            } else {
                RedirectPolicy::Never
            })
            .map_err(ClientConfigError::at(ClientConfigStage::SetRedirectPolicy))?;
        Ok(Client {
            session: Arc::new(session),
            relaxations: self.relaxations,
        })
    }
}

/// An HTTP client that sends [`http::Request`] and returns [`http::Response`].
///
/// Cheap to clone — clones share one WinHTTP session — and `Send + Sync`, so
/// one client can serve a whole program.
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use http_body_util::{BodyExt, Empty};
/// use bytes::Bytes;
/// use winasio_util::Client;
///
/// let client = Client::new("my-agent/1.0")?;
/// let request = http::Request::get("http://example.com/")
///     .body(Empty::<Bytes>::new())?;
///
/// // No runtime: this is a bare single-threaded executor.
/// let response = futures::executor::block_on(client.request(request))?;
/// println!("{}", response.status());
///
/// let body = futures::executor::block_on(response.into_body().collect())?;
/// println!("{} bytes", body.to_bytes().len());
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Client {
    session: Arc<Session>,
    relaxations: CertificateRelaxations,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Session` is an opaque handle with no `Debug`, and printing a raw
        // handle value would be noise rather than information.
        f.debug_struct("Client")
            .field("relaxations", &self.relaxations)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client with the platform defaults, identifying itself as `agent`.
    pub fn new(agent: &str) -> Result<Client, ClientConfigError> {
        Client::builder(agent).build()
    }

    /// Start configuring a client.
    pub fn builder(agent: &str) -> ClientBuilder {
        ClientBuilder {
            agent: HSTRING::from(agent),
            timeouts: None,
            relaxations: CertificateRelaxations::default(),
            platform_redirects: false,
        }
    }

    /// Send one request and return its response.
    ///
    /// The response is returned as soon as its head has arrived; the body is
    /// read from the returned [`ResponseBody`] as it is polled. The request
    /// body is consumed before the response head is awaited, as HTTP requires.
    ///
    /// # Errors
    ///
    /// Every failure is a [`RequestError`] naming what went wrong, and the
    /// variants are closed, so a caller may match them exhaustively. Note what
    /// is *not* in that type: a response body that ends before its declared
    /// `Content-Length` is a [`ResponseBodyError::Truncated`] from the body's
    /// final poll — never a body that appeared to end — and it happens after
    /// this call has already returned `Ok`.
    ///
    /// [`ResponseBodyError::Truncated`]: crate::ResponseBodyError::Truncated
    pub async fn request<B>(
        &self,
        request: HttpRequest<B>,
    ) -> Result<HttpResponse<ResponseBody>, RequestError>
    where
        B: Body + Unpin,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (parts, body) = request.into_parts();
        let target = uri::decompose(&parts.uri)?;
        let block = headers::encode(&parts.headers)?;

        // The framing decision, made once and honoured by the writer below.
        let (block, declared) = match body.size_hint().exact() {
            Some(length) if length > u64::from(u32::MAX) => {
                return Err(RequestError::BodyTooLarge { length })
            }
            Some(length) => (block, Framing::Exact(length)),
            None => (
                headers::append(block, "Transfer-Encoding: chunked"),
                Framing::Chunked,
            ),
        };

        let connection = self
            .session
            .connect(&target.host, target.port)
            .map_err(RequestError::transport(RequestStage::Connect))?;
        let mut platform = connection
            .open_request(
                &HSTRING::from(parts.method.as_str()),
                &target.object,
                &[],
                target.secure,
            )
            .map_err(RequestError::transport(RequestStage::OpenRequest))?;

        if self.relaxations != CertificateRelaxations::default() {
            platform
                .relax_certificate_validation(self.relaxations)
                .map_err(RequestError::transport(RequestStage::Configure))?;
        }

        // The body is never handed to `send`. Passing it there would work only
        // for the exact-length case, and `write_data` has to exist anyway for
        // the chunked one, so there is one writer rather than two paths.
        platform
            .send(block, Vec::new(), declared.total_length())
            .await
            .map_err(RequestError::transport(RequestStage::Send))?;

        write_body(&mut platform, body, declared).await?;

        platform
            .receive_response()
            .await
            .map_err(RequestError::transport(RequestStage::ReceiveResponse))?;

        let code = platform
            .status_code()
            .map_err(RequestError::transport(RequestStage::ReadHeaders))?;
        let status =
            StatusCode::from_u16(u16::try_from(code).unwrap_or(u16::MAX)).map_err(|_| {
                RequestError::MalformedResponseHeader {
                    line: format!("status code {code}"),
                }
            })?;
        let raw = platform
            .raw_headers()
            .map_err(RequestError::transport(RequestStage::ReadHeaders))?;
        let head = headers::parse(&raw)?;

        // A response that cannot carry a body reports zero available whatever
        // its headers declared, so its declared length must not be believed.
        let body = if headers::may_have_body(&parts.method, status) {
            ResponseBody::new(platform, headers::content_length(&head.headers))
        } else {
            ResponseBody::empty(platform)
        };

        let mut response = HttpResponse::new(body);
        *response.status_mut() = status;
        *response.version_mut() = head.version;
        *response.headers_mut() = head.headers;
        Ok(response)
    }
}

/// How the request body is framed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// A known length, declared to the platform.
    Exact(u64),
    /// An unknown length, framed by this crate as `Transfer-Encoding: chunked`.
    Chunked,
}

impl Framing {
    fn total_length(self) -> u32 {
        match self {
            // Checked against `u32::MAX` before this point.
            Framing::Exact(length) => length as u32,
            // Measured: zero plus the chunked header is what makes WinHTTP
            // leave the framing alone. Zero *without* the header and with a
            // body is `ERROR_INVALID_PARAMETER`, not chunked.
            Framing::Chunked => 0,
        }
    }
}

/// Drain a request body onto a request handle.
async fn write_body<B>(
    platform: &mut winasio::winhttp::Request,
    mut body: B,
    framing: Framing,
) -> Result<(), RequestError>
where
    B: Body + Unpin,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut written: u64 = 0;
    loop {
        let frame = std::future::poll_fn(|context| Pin::new(&mut body).poll_frame(context)).await;
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|error| RequestError::Body(BodyError::Source(error.into())))?;
        // A trailers frame has nowhere to go: WinHTTP exposes no way to send
        // trailers, and a chunked message can only carry them after the
        // terminating chunk, which the platform does not let us reach. Skipped
        // rather than refused, so that a body which merely *can* produce them
        // still works.
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let mut data = data;
        while data.has_remaining() {
            let take = data.remaining().min(MAX_WRITE);
            let chunk = data.copy_to_bytes(take);
            written += take as u64;
            match framing {
                Framing::Exact(_) => write_all(platform, chunk.to_vec()).await?,
                Framing::Chunked => {
                    let mut framed = format!("{take:x}\r\n").into_bytes();
                    framed.extend_from_slice(&chunk);
                    framed.extend_from_slice(b"\r\n");
                    write_all(platform, framed).await?;
                }
            }
        }
    }

    match framing {
        Framing::Exact(declared) if declared != written => {
            // Caught here rather than left to the platform, which reports an
            // under-written body as a send timeout half a minute later —
            // measured — and an over-written one not at all.
            return Err(RequestError::Body(BodyError::LengthMismatch {
                declared,
                actual: written,
            }));
        }
        Framing::Exact(_) => {}
        Framing::Chunked => write_all(platform, b"0\r\n\r\n".to_vec()).await?,
    }
    Ok(())
}

/// Write a whole buffer, however many calls that takes.
///
/// A short write has never been observed — a single one-megabyte write was
/// measured to complete whole — but the API reports a count, so it is treated
/// as one that can be less than the whole. Five lines is a cheap price for not
/// depending on an unpromised behaviour.
async fn write_all(
    platform: &mut winasio::winhttp::Request,
    buffer: Vec<u8>,
) -> Result<(), RequestError> {
    let mut pending = buffer;
    loop {
        let expected = pending.len();
        if expected == 0 {
            return Ok(());
        }
        let winasio::iocp::OpResult(written, returned) = platform.write_data(pending).await;
        let written = written.map_err(RequestError::transport(RequestStage::Write))?;
        if written >= expected {
            return Ok(());
        }
        if written == 0 {
            // Making no progress and reporting no error. Never observed, and
            // the only alternative to reporting it is a loop that never ends.
            return Err(RequestError::WriteStalled {
                remaining: expected,
            });
        }
        pending = returned[written..].to_vec();
    }
}

/// A client is useless if it cannot be shared, and this is where that stops
/// being true silently.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Client>();
    assert_send_sync::<ClientBuilder>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_length_is_declared_and_an_unknown_one_is_chunked() {
        assert_eq!(Framing::Exact(0).total_length(), 0);
        assert_eq!(Framing::Exact(7).total_length(), 7);
        assert_eq!(Framing::Exact(u32::MAX as u64).total_length(), u32::MAX);
        // Zero, and only meaningful alongside the header the caller cannot set
        // and this crate always does.
        assert_eq!(Framing::Chunked.total_length(), 0);
    }

    #[test]
    fn a_body_larger_than_the_platform_can_declare_is_refused() {
        // Not a check that can be skipped: `total_length` casts, so without
        // the guard in `Client::request` a body of exactly 2^32 bytes would
        // declare zero and the platform would reject the first write.
        let oversize = u64::from(u32::MAX) + 1;
        assert_eq!(Framing::Exact(oversize).total_length(), 0);
    }
}
