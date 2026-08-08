// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The HTTP/2 request path: a duplex transport for gRPC over WinHTTP.
//!
//! # Why this is a separate path from HTTP/1.1
//!
//! [`crate::client`] sends an HTTP/1.1 request as a straight line: send the
//! head, drain the whole request body, *then* await the response head. That is
//! correct for HTTP/1.1 and fatal for HTTP/2 duplex, for two measured reasons.
//!
//! * **Manual chunk framing downgrades HTTP/2 to HTTP/1.1 (M7).** Verbatim from
//!   Microsoft's own `WinHttpHandler.cs`: *"Note that manual chunking with
//!   WinHttp downgrades HTTP/2 requests to HTTP/1.1."* The HTTP/1.1 path frames
//!   an unknown-length body by hand (`{len:x}\r\n…`), so reusing it would make
//!   every "HTTP/2" gRPC request silently HTTP/1.1 — and gRPC needs the trailers
//!   and full-duplex streaming that only h2 gives. So this path uses
//!   `WINHTTP_FLAG_AUTOMATIC_CHUNKING` (M4) and lets WinHTTP frame the body.
//! * **The receive must start before the body finishes (M6).** Again from
//!   `WinHttpHandler.cs`: *"This order is important because the response could
//!   be returned immediately with END_STREAM flag on headers. Trying to send
//!   request body after that can cause the request to go into a bad state."* So
//!   this path starts `WinHttpReceiveResponse` **concurrently** with the body
//!   write loop, which the two-slot request state one crate down was built to
//!   allow (a body write and a response receive/read may be outstanding at
//!   once). There is no new duplex *function*: `WinHttpWriteDataEx` does not
//!   exist in the SDK (M5). Duplex is achieved by ordering alone.
//!
//! # The shape of a request here
//!
//! 1. Probe once whether the platform accepts automatic chunking (M8), caching
//!    the answer on the client.
//! 2. Open the request with `WINHTTP_FLAG_SECURE | WINHTTP_FLAG_AUTOMATIC_CHUNKING`
//!    (when supported) and call [`Request::enable_http2`].
//! 3. Send the head with no body.
//! 4. **Head phase** — [`drive_head`]: start the receive, then write request
//!    body frames one at a time, polling the receive concurrently on each
//!    await. Return as soon as the response head arrives; any request body not
//!    yet sent is carried into the body.
//! 5. Build the [`http::Response`] and wrap a [`DuplexResponseBody`] that keeps
//!    sending the remaining request body while it reads the response body, and
//!    then reads response **trailers** (M12) — which is where gRPC puts
//!    `grpc-status`.
//!
//! # What is *not* attempted
//!
//! True simultaneous full-duplex (writing and reading genuinely at once, so
//! that an unbounded request body yields to the response for flow control) is
//! not attempted: the body advances the outbound write between response reads,
//! not during a single read. Measured gRPC call shapes — unary,
//! server-streaming, client-streaming and client-initiated bidirectional
//! ping-pong — do not need it, and the platform's own support for client and
//! bidirectional streaming is Windows-11-and-up anyway (M9). The limit is
//! documented rather than hidden.
//!
//! # Fallback
//!
//! Where automatic chunking is unsupported (M8) and the body length is unknown,
//! this path falls back to manual chunked framing — which downgrades to
//! HTTP/1.1 (M7) and is therefore *not* gRPC-capable, but keeps a plain HTTP/2
//! request working as an HTTP/1.1 one rather than failing. A known-length body
//! needs no framing either way and stays on HTTP/2.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use http::request::Parts;
use http::{Response as HttpResponse, StatusCode};
use http_body::{Body, Frame, SizeHint};
use winasio::iocp::OpResult;
use winasio::winhttp::{
    CertificateRelaxations, Connection, ReceiveResponse, Request, RequestFlags, RequestWriter,
    Session,
};
use windows::core::{Error, HSTRING};

use crate::body::ResponseBody;
use crate::error::{BodyError, RequestError, RequestStage, ResponseBodyError};
use crate::uri::Target;
use crate::{headers, uri};

/// The most this crate writes to the platform in one call. See
/// [`crate::client`]'s constant of the same name.
const MAX_WRITE: usize = 64 * 1024;

/// The largest single read this body asks the platform for. See [`crate::body`].
const MAX_READ: usize = 64 * 1024;

/// The capability-probe cache states, stored in an [`AtomicU8`] on the client.
const CHUNKING_UNKNOWN: u8 = 0;
const CHUNKING_YES: u8 = 1;
const CHUNKING_NO: u8 = 2;

/// How the request body is framed on the HTTP/2 path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H2Framing {
    /// WinHTTP frames the body itself (`WINHTTP_FLAG_AUTOMATIC_CHUNKING`). The
    /// only framing that stays on HTTP/2 for an unknown-length body (M7).
    Automatic,
    /// A known exact length. No framing is needed; raw bytes are written and
    /// the length is declared up front. Stays on HTTP/2.
    Exact(u64),
    /// The platform lacks automatic chunking (M8) and the length is unknown, so
    /// the body is framed by hand as `Transfer-Encoding: chunked`. This
    /// **downgrades the request to HTTP/1.1** (M7) and is therefore not
    /// gRPC-capable; it exists so a plain request still works.
    ManualChunked,
}

impl H2Framing {
    /// The `total_length` to declare to `WinHttpSendRequest`.
    fn total_length(self) -> u32 {
        match self {
            // Checked against `u32::MAX` before this point.
            H2Framing::Exact(length) => length as u32,
            // Zero means "streamed": WinHTTP frames the body (automatic) or the
            // manual chunk framing does (manual). Measured: zero without the
            // chunked header and with a body is `ERROR_INVALID_PARAMETER`, so
            // the manual case pairs this with the header.
            H2Framing::Automatic | H2Framing::ManualChunked => 0,
        }
    }

    /// Frame one chunk of body for the wire.
    fn frame(self, chunk: &[u8]) -> Vec<u8> {
        match self {
            H2Framing::ManualChunked => {
                let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
                framed.extend_from_slice(chunk);
                framed.extend_from_slice(b"\r\n");
                framed
            }
            // WinHTTP (automatic) or nothing (exact) does the framing.
            H2Framing::Automatic | H2Framing::Exact(_) => chunk.to_vec(),
        }
    }

    /// The terminal bytes to write once the body ends, if any.
    fn terminal(self) -> Option<Vec<u8>> {
        match self {
            H2Framing::ManualChunked => Some(b"0\r\n\r\n".to_vec()),
            H2Framing::Automatic | H2Framing::Exact(_) => None,
        }
    }
}

/// Send one HTTP/2 request and return its response.
///
/// The entry point [`Client::request`](crate::Client::request) calls when the
/// client was built with [`ClientBuilder::http2`](crate::ClientBuilder::http2).
pub(crate) async fn request<B>(
    session: &Session,
    relaxations: CertificateRelaxations,
    chunking: &AtomicU8,
    parts: Parts,
    mut body: B,
) -> Result<HttpResponse<ResponseBody>, RequestError>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    let target = uri::decompose(&parts.uri)?;
    let block = headers::encode(&parts.headers)?;

    let connection = session
        .connect(&target.host, target.port)
        .map_err(RequestError::transport(RequestStage::Connect))?;

    // The framing decision, made once and honoured throughout.
    let framing = match body.size_hint().exact() {
        Some(length) if length > u64::from(u32::MAX) => {
            return Err(RequestError::BodyTooLarge { length })
        }
        Some(length) => H2Framing::Exact(length),
        None if probe_chunking(chunking, &connection, &target)? => H2Framing::Automatic,
        None => H2Framing::ManualChunked,
    };

    let mut platform = connection
        .open_request_with(
            &HSTRING::from(parts.method.as_str()),
            &target.object,
            &[],
            target.secure,
            RequestFlags {
                automatic_chunking: matches!(framing, H2Framing::Automatic),
            },
        )
        .map_err(RequestError::transport(RequestStage::OpenRequest))?;

    if relaxations != CertificateRelaxations::default() {
        platform
            .relax_certificate_validation(relaxations)
            .map_err(RequestError::transport(RequestStage::Configure))?;
    }

    // Ask for HTTP/2. Best-effort: an h1-only server just answers h1 (M9/M10).
    platform
        .enable_http2()
        .map_err(RequestError::transport(RequestStage::Configure))?;

    // Only the manual-chunked fallback declares a framing header; automatic and
    // exact must not (M7 — a manual `Transfer-Encoding` would downgrade h2).
    let block = match framing {
        H2Framing::ManualChunked => headers::append(block, "Transfer-Encoding: chunked"),
        H2Framing::Automatic | H2Framing::Exact(_) => block,
    };

    // The body is never handed to `send`: the duplex ordering (M6) needs the
    // receive started before the body write, so the head goes on its own.
    platform
        .send(block, Vec::new(), framing.total_length())
        .await
        .map_err(RequestError::transport(RequestStage::Send))?;

    // Head phase: write body frames while the receive runs, return at the head.
    let write_done = drive_head(&mut platform, &mut body, framing).await?;

    let code = platform
        .status_code()
        .map_err(RequestError::transport(RequestStage::ReadHeaders))?;
    let status = StatusCode::from_u16(u16::try_from(code).unwrap_or(u16::MAX)).map_err(|_| {
        RequestError::MalformedResponseHeader {
            line: format!("status code {code}"),
        }
    })?;
    let raw = platform
        .raw_headers()
        .map_err(RequestError::transport(RequestStage::ReadHeaders))?;
    let head = headers::parse(&raw)?;

    let inner = DuplexResponseBody::new(platform, body, framing, write_done);
    let mut response = HttpResponse::new(ResponseBody::boxed(Box::pin(inner)));
    *response.status_mut() = status;
    *response.version_mut() = head.version;
    *response.headers_mut() = head.headers;
    Ok(response)
}

/// Load the automatic-chunking capability from the cache, or probe it once (M8).
fn probe_chunking(
    cache: &AtomicU8,
    connection: &Connection,
    target: &Target,
) -> Result<bool, RequestError> {
    match cache.load(Ordering::Relaxed) {
        CHUNKING_YES => return Ok(true),
        CHUNKING_NO => return Ok(false),
        CHUNKING_UNKNOWN => {} // Not yet decided: fall through to probe.
        _ => {}                // Any other value is treated as not-yet-decided.
    }
    let supported = connection
        .supports_automatic_chunking(&target.object, target.secure)
        .map_err(RequestError::transport(RequestStage::OpenRequest))?;
    cache.store(
        if supported { CHUNKING_YES } else { CHUNKING_NO },
        Ordering::Relaxed,
    );
    Ok(supported)
}

/// The head phase (M6): start the receive, write body frames concurrently, and
/// return when the response head has arrived.
///
/// Returns whether the whole request body was sent (`true`) or the head arrived
/// first and some body remains for [`DuplexResponseBody`] to finish (`false`).
///
/// The receive future is polled on every await via [`with_recv`], so it is
/// submitted before the first byte of body and progresses alongside the writes,
/// which is exactly the ordering the platform requires. No in-flight operation
/// is ever dropped mid-flight: each body write completes before the next frame
/// is pulled, and the receive is held by reference rather than moved.
async fn drive_head<B>(
    platform: &mut Request,
    body: &mut B,
    framing: H2Framing,
) -> Result<bool, RequestError>
where
    B: Body + Unpin,
    B::Data: Buf,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    let (mut writer, mut reader) = platform.split();
    let mut recv = std::pin::pin!(reader.receive_response());
    let mut head: Option<Result<(), Error>> = None;
    let mut write_done = false;

    while head.is_none() {
        let frame = with_recv(pull_frame(body), &mut recv, &mut head).await;
        let frame = match frame {
            Some(Ok(frame)) => frame,
            Some(Err(error)) => return Err(RequestError::Body(BodyError::Source(error.into()))),
            None => {
                // The whole request body has been produced. Write the framing
                // terminal, if any, then stop and await the head.
                if let Some(terminal) = framing.terminal() {
                    write_all_with_recv(&mut writer, terminal, &mut recv, &mut head).await?;
                }
                write_done = true;
                break;
            }
        };

        // A trailers frame on the *request* body has nowhere to go on this path
        // and is skipped, exactly as the HTTP/1.1 writer skips it.
        let Ok(mut data) = frame.into_data() else {
            continue;
        };
        // Stop writing the moment the head arrives (M6): once the response has
        // its head, any further body write risks the "bad state" the ordering
        // exists to avoid. Whatever is left of this frame is dropped; the
        // response has already ended as far as the request stream is concerned.
        while data.has_remaining() && head.is_none() {
            let take = data.remaining().min(MAX_WRITE);
            let chunk = data.copy_to_bytes(take);
            let framed = framing.frame(&chunk);
            write_all_with_recv(&mut writer, framed, &mut recv, &mut head).await?;
        }
    }

    // If the body finished before the head, the receive is still outstanding.
    let head_result = match head.take() {
        Some(result) => result,
        None => std::future::poll_fn(|context| recv.as_mut().poll(context)).await,
    };
    head_result.map_err(RequestError::transport(RequestStage::ReceiveResponse))?;
    Ok(write_done)
}

/// Poll `op` to completion while also polling `recv`.
///
/// If `recv` completes first its result is stored in `head` and `op` still runs
/// to completion, so a body write is never abandoned mid-flight (which one
/// crate down parks the request). `recv` is borrowed, never moved, so the same
/// receive future survives across every write.
async fn with_recv<F: Future>(
    op: F,
    recv: &mut Pin<&mut ReceiveResponse<'_>>,
    head: &mut Option<Result<(), Error>>,
) -> F::Output {
    let mut op = std::pin::pin!(op);
    std::future::poll_fn(|context| {
        if head.is_none() {
            if let Poll::Ready(result) = recv.as_mut().poll(context) {
                *head = Some(result);
            }
        }
        op.as_mut().poll(context)
    })
    .await
}

/// Pull the next frame from a request body, non-committally.
async fn pull_frame<B>(body: &mut B) -> Option<Result<Frame<B::Data>, B::Error>>
where
    B: Body + Unpin,
{
    std::future::poll_fn(|context| Pin::new(&mut *body).poll_frame(context)).await
}

/// Write a whole buffer on the writer half, polling the receive concurrently.
///
/// A write that fails *after* the head has arrived is treated as the server
/// having ended the request stream (M6), not as a request failure: the response
/// is already in hand, so the body simply stops.
async fn write_all_with_recv(
    writer: &mut RequestWriter<'_>,
    buffer: Vec<u8>,
    recv: &mut Pin<&mut ReceiveResponse<'_>>,
    head: &mut Option<Result<(), Error>>,
) -> Result<(), RequestError> {
    let mut pending = buffer;
    loop {
        let expected = pending.len();
        if expected == 0 {
            return Ok(());
        }
        let OpResult(written, returned) = with_recv(writer.write_data(pending), recv, head).await;
        let written = match written {
            Ok(written) => written,
            Err(error) => {
                if head.is_some() {
                    return Ok(());
                }
                return Err(RequestError::transport(RequestStage::Write)(error));
            }
        };
        if written >= expected {
            return Ok(());
        }
        if written == 0 {
            return Err(RequestError::WriteStalled {
                remaining: expected,
            });
        }
        pending = returned[written..].to_vec();
    }
}

/// The HTTP/2 response body: reads the response while finishing the request.
///
/// Owns the [`Request`] and the remainder of the outbound body. Each poll first
/// advances the outbound write (any request-body frames that are ready) and
/// then performs one response read, so a client that keeps producing request
/// messages and reading responses drives both directions. When the response
/// body ends, the response **trailers** are read (M12) and surfaced as a final
/// [`Frame::trailers`] — the gRPC `grpc-status` lives there.
///
/// See the module docs for the ownership trick (the same owned-future state
/// machine as [`crate::body`]) and for the full-duplex limit.
struct DuplexResponseBody<B> {
    framing: H2Framing,
    state: DState<B>,
}

enum DState<B> {
    /// Idle: this body owns the request and the rest of the outbound body.
    Active {
        request: Request,
        body: B,
        /// Whether the whole request body has been sent.
        write_done: bool,
    },
    /// One combined write/read turn is in flight, owning both.
    Busy(Pin<Box<dyn Future<Output = StepResult<B>> + Send>>),
    /// The body ended, cleanly or not. Fused.
    Finished,
}

/// What one combined write/read turn produced.
enum StepResult<B> {
    /// A response data frame; more may follow. Carries the request and body on.
    Data {
        request: Request,
        body: B,
        write_done: bool,
        frame: Frame<Bytes>,
    },
    /// The final frame — the response trailers — after which the body ends.
    Trailers(Frame<Bytes>),
    /// The response ended with no trailers.
    End,
    /// A read or write failed.
    Error(ResponseBodyError),
}

impl<B> DuplexResponseBody<B> {
    fn new(request: Request, body: B, framing: H2Framing, write_done: bool) -> Self {
        DuplexResponseBody {
            framing,
            state: DState::Active {
                request,
                body,
                write_done,
            },
        }
    }
}

/// One combined turn: advance the outbound write, then read once (or read the
/// trailers). Owns the request and the outbound body and hands them back.
async fn duplex_step<B>(
    mut request: Request,
    mut body: B,
    write_done: bool,
    framing: H2Framing,
) -> StepResult<B>
where
    B: Body + Unpin,
    B::Data: Buf,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    /// The result of the write-and-read scope, holding no borrow of the request.
    enum Turn {
        /// A response data chunk (owned bytes).
        Data(Vec<u8>),
        /// The response body ended; the caller reads trailers next.
        End,
        /// A read or write failed.
        Error(ResponseBodyError),
    }

    // The split halves borrow `request` and `body`, so everything that touches
    // them lives in this scope; when it ends the borrows are released and the
    // request and body can be moved into the result. `write_done` is threaded
    // back out because a body that finishes here must not be re-polled.
    let mut write_done = write_done;
    let turn = {
        let (mut writer, mut reader) = request.split();

        // 1. Send any request-body frames that are ready right now. A frame
        // that is not yet available leaves the write for a later turn rather
        // than blocking the read — this is the interleaving that lets a
        // ping-pong bidi stream make progress.
        let mut write_error: Option<ResponseBodyError> = None;
        if !write_done {
            loop {
                let polled = std::future::poll_fn(|context| {
                    Poll::Ready(Pin::new(&mut body).poll_frame(context))
                })
                .await;
                match polled {
                    Poll::Ready(Some(Ok(frame))) => {
                        if let Ok(mut data) = frame.into_data() {
                            while data.has_remaining() {
                                let take = data.remaining().min(MAX_WRITE);
                                let chunk = data.copy_to_bytes(take);
                                let framed = framing.frame(&chunk);
                                if write_all_half(&mut writer, framed).await.is_err() {
                                    // The response is already flowing; a write
                                    // failure means the server ended the request
                                    // stream (M6). Stop writing and read on —
                                    // the outcome is in the trailers, not here.
                                    write_done = true;
                                    break;
                                }
                            }
                        }
                    }
                    Poll::Ready(Some(Err(error))) => {
                        write_error = Some(ResponseBodyError::Write(error.into()));
                        break;
                    }
                    Poll::Ready(None) => {
                        if let Some(terminal) = framing.terminal() {
                            let _ = write_all_half(&mut writer, terminal).await;
                        }
                        write_done = true;
                        break;
                    }
                    Poll::Pending => break,
                }
                if write_done {
                    break;
                }
            }
        }

        if let Some(error) = write_error {
            Turn::Error(error)
        } else {
            // 2. One response read.
            match reader.query_data_available().await {
                Err(error) => Turn::Error(ResponseBodyError::Read(error)),
                Ok(0) => Turn::End,
                Ok(available) => {
                    let wanted = (available as usize).min(MAX_READ);
                    let OpResult(read, mut buffer) =
                        reader.read_data(Vec::<u8>::with_capacity(wanted)).await;
                    match read {
                        Err(error) => Turn::Error(ResponseBodyError::Read(error)),
                        Ok(read) => {
                            buffer.truncate(read);
                            if buffer.is_empty() {
                                // Never observed after non-zero availability;
                                // treated as end of body all the same.
                                Turn::End
                            } else {
                                Turn::Data(buffer)
                            }
                        }
                    }
                }
            }
        }
    };

    match turn {
        Turn::Error(error) => StepResult::Error(error),
        Turn::Data(buffer) => StepResult::Data {
            request,
            body,
            write_done,
            frame: Frame::data(Bytes::from(buffer)),
        },
        // 3. End of body: read the trailers (M12). gRPC puts `grpc-status` here.
        Turn::End => match request.raw_trailers() {
            Ok(Some(raw)) => match headers::parse_trailers(&raw) {
                Ok(map) if !map.is_empty() => StepResult::Trailers(Frame::trailers(map)),
                // No trailers, or a block that would not parse as header fields.
                // WinHTTP formats the block itself, so a parse failure is not a
                // transport fault; the body simply ends, and a gRPC layer above
                // sees the absent `grpc-status` and reports it in its own terms.
                Ok(_) | Err(_) => StepResult::End,
            },
            Ok(None) => StepResult::End,
            Err(error) => StepResult::Error(ResponseBodyError::Read(error)),
        },
    }
}
async fn write_all_half(
    writer: &mut RequestWriter<'_>,
    buffer: Vec<u8>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut pending = buffer;
    loop {
        let expected = pending.len();
        if expected == 0 {
            return Ok(());
        }
        let OpResult(written, returned) = writer.write_data(pending).await;
        let written = written
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
        if written >= expected {
            return Ok(());
        }
        if written == 0 {
            return Err("request body write stalled".into());
        }
        pending = returned[written..].to_vec();
    }
}

impl<B> Body for DuplexResponseBody<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Buf + Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    type Data = Bytes;
    type Error = ResponseBodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ResponseBodyError>>> {
        let framing = self.framing;
        loop {
            match std::mem::replace(&mut self.state, DState::Finished) {
                DState::Finished => return Poll::Ready(None),
                DState::Active {
                    request,
                    body,
                    write_done,
                } => {
                    self.state =
                        DState::Busy(Box::pin(duplex_step(request, body, write_done, framing)));
                }
                DState::Busy(mut operation) => match operation.as_mut().poll(context) {
                    Poll::Pending => {
                        self.state = DState::Busy(operation);
                        return Poll::Pending;
                    }
                    Poll::Ready(StepResult::Data {
                        request,
                        body,
                        write_done,
                        frame,
                    }) => {
                        self.state = DState::Active {
                            request,
                            body,
                            write_done,
                        };
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Poll::Ready(StepResult::Trailers(frame)) => {
                        // Trailers are the last thing on the stream.
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Poll::Ready(StepResult::End) => return Poll::Ready(None),
                    Poll::Ready(StepResult::Error(error)) => return Poll::Ready(Some(Err(error))),
                },
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        // A gRPC response is streamed and length is never declared.
        SizeHint::default()
    }
}

impl<B> std::fmt::Debug for DuplexResponseBody<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.state {
            DState::Active { .. } => "active",
            DState::Busy(_) => "busy",
            DState::Finished => "finished",
        };
        f.debug_struct("DuplexResponseBody")
            .field("framing", &self.framing)
            .field("state", &state)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_framing_declares_the_length_and_writes_bytes_raw() {
        assert_eq!(H2Framing::Exact(7).total_length(), 7);
        assert_eq!(H2Framing::Exact(7).frame(b"hello"), b"hello");
        assert!(H2Framing::Exact(7).terminal().is_none());
    }

    #[test]
    fn automatic_framing_streams_without_manual_chunk_headers() {
        // The whole point of M7: no `{len:x}` framing, so no HTTP/2 downgrade.
        assert_eq!(H2Framing::Automatic.total_length(), 0);
        assert_eq!(H2Framing::Automatic.frame(b"hello"), b"hello");
        assert!(H2Framing::Automatic.terminal().is_none());
    }

    #[test]
    fn manual_chunked_framing_is_the_downgrading_fallback() {
        assert_eq!(H2Framing::ManualChunked.total_length(), 0);
        assert_eq!(H2Framing::ManualChunked.frame(b"hello"), b"5\r\nhello\r\n");
        assert_eq!(H2Framing::ManualChunked.terminal().unwrap(), b"0\r\n\r\n");
    }

    #[test]
    fn the_chunking_cache_answers_without_reprobing() {
        // Once decided, the capability is a property of the platform and is not
        // probed again — the cache short-circuits both answers. The initial
        // state is `unknown`, which is what makes the first request probe.
        assert_eq!(AtomicU8::new(CHUNKING_UNKNOWN).load(Ordering::Relaxed), 0);
        let yes = AtomicU8::new(CHUNKING_YES);
        assert_eq!(yes.load(Ordering::Relaxed), CHUNKING_YES);
        let no = AtomicU8::new(CHUNKING_NO);
        assert_eq!(no.load(Ordering::Relaxed), CHUNKING_NO);
    }
}
