// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! The client transport: a WinHTTP-backed `tower` [`Service`] a tonic client
//! accepts as its channel.
//!
//! # What tonic asks for
//!
//! A tonic-generated client (`EchoClient<T>`) is generic over a transport `T`
//! that satisfies [`tonic::client::GrpcService<ReqBody>`]. That trait has a
//! blanket impl for any `tower_service::Service<http::Request<ReqBody>,
//! Response = http::Response<B>>` whose bodies and errors fit gRPC's shape, so
//! the way to be a tonic transport is to be that tower service. [`WinHttpChannel`]
//! is exactly that, over [`winasio_util::Client`] in HTTP/2 mode.
//!
//! # Why this instead of tonic's own `Channel`
//!
//! tonic's built-in `Channel` is hyper-based: enabling it drags in `hyper`,
//! `hyper-util`, `h2`, `socket2` and a tokio reactor (`net`/`time`). This whole
//! workspace exists to speak HTTP without any of that — the wire work is
//! WinHTTP's. So `winasio-tonic` builds tonic with `default-features = false`
//! and its own transport (D2, M13). The generated `connect()` convenience that
//! references `tonic::transport::Channel` is suppressed in `build.rs`
//! (`build_transport(false)`); a caller constructs a [`WinHttpChannel`] instead.
//!
//! # The four call types and duplex
//!
//! Unary and server-streaming need only a request that is sent then a response
//! that is read. Client-streaming and bidirectional need the request body to
//! keep being written *after* the response has begun — duplex. That is precisely
//! the WinHTTP ordering [`winasio_util::Client`] gained in Phase 1 (send
//! headers, start `WinHttpReceiveResponse` before the body finishes, M6). This
//! transport does nothing special for duplex: it hands tonic's streaming request
//! body to the client and the client's own duplex loop does the rest. Whether
//! duplex actually works is an OS property (M9/M11), surfaced by the client's
//! auto-chunking capability probe, not decided here.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http::{Request, Response, Uri};
use winasio_util::{CertificateRelaxations, Client, ClientConfigError, RequestError, ResponseBody};

/// A `tower` service that carries gRPC requests over WinHTTP HTTP/2.
///
/// Hand one to a tonic-generated client:
///
/// ```no_run
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use winasio_tonic::WinHttpChannel;
/// let channel = WinHttpChannel::new(
///     "https://localhost:12495".parse()?,
///     "winasio-tonic/0.1",
/// )?;
/// // let mut client = echo_client::EchoClient::new(channel);
/// # let _ = channel;
/// # Ok(())
/// # }
/// ```
///
/// It is [`Clone`] (cheaply — the underlying [`Client`] is an `Arc` over a
/// WinHTTP session), so a tonic client that clones its transport per call costs
/// nothing extra.
#[derive(Clone)]
pub struct WinHttpChannel {
    client: Client,
    /// Scheme + authority only (e.g. `https://localhost:12495`). tonic sends
    /// requests with a path-only URI and expects the transport to supply the
    /// origin — its own `Channel` does this with an `AddOrigin` tower layer.
    /// We fold it in per request in [`with_origin`].
    origin: Uri,
}

impl std::fmt::Debug for WinHttpChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinHttpChannel")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl WinHttpChannel {
    /// Build a channel to `origin` (scheme + authority, e.g.
    /// `https://localhost:12495`) with a default HTTP/2 client.
    ///
    /// The client is created with HTTP/2 enabled ([`ClientBuilder::http2`]),
    /// which is mandatory: gRPC is HTTP/2-only (M9/M10). For anything beyond the
    /// defaults (certificate relaxations, timeouts) build the [`Client`]
    /// yourself and use [`from_client`](Self::from_client).
    ///
    /// [`ClientBuilder::http2`]: winasio_util::ClientBuilder::http2
    pub fn new(origin: Uri, agent: &str) -> Result<WinHttpChannel, ClientConfigError> {
        let client = Client::builder(agent).http2(true).build()?;
        Ok(WinHttpChannel::from_client(client, origin))
    }

    /// Build a channel to `origin` with a client that trusts a self-signed /
    /// otherwise-unverifiable server certificate per `relaxations`.
    ///
    /// A convenience for the common test/dev shape; production callers should
    /// prefer a properly chained certificate and [`new`](Self::new).
    pub fn with_relaxations(
        origin: Uri,
        agent: &str,
        relaxations: CertificateRelaxations,
    ) -> Result<WinHttpChannel, ClientConfigError> {
        let client = Client::builder(agent)
            .http2(true)
            .certificate_relaxations(relaxations)
            .build()?;
        Ok(WinHttpChannel::from_client(client, origin))
    }

    /// Build a channel from a [`Client`] the caller has already configured.
    ///
    /// **The client must have HTTP/2 enabled** ([`ClientBuilder::http2(true)`]);
    /// a plain HTTP/1.1 client cannot carry gRPC (it would downgrade the wire,
    /// M7). This is not checked here because the client does not expose its mode;
    /// it is the caller's contract.
    ///
    /// Only the scheme and authority of `origin` are used; any path is dropped
    /// and replaced per request by tonic's `:path`.
    ///
    /// [`ClientBuilder::http2(true)`]: winasio_util::ClientBuilder::http2
    pub fn from_client(client: Client, origin: Uri) -> WinHttpChannel {
        WinHttpChannel {
            client,
            origin: origin_of(&origin),
        }
    }

    /// The origin (scheme + authority) this channel targets.
    pub fn origin(&self) -> &Uri {
        &self.origin
    }
}

/// Reduce a URI to just its scheme and authority, dropping any path/query.
fn origin_of(uri: &Uri) -> Uri {
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(http::uri::PathAndQuery::from_static("/"));
    // Rebuilding with a root path then reading scheme+authority back is the
    // simplest way to normalise; keep the root path so the value is a valid URI.
    Uri::from_parts(parts).unwrap_or_else(|_| uri.clone())
}

/// Replace a request's origin with this channel's, keeping tonic's `:path`.
///
/// tonic builds each request URI from the method's `PathAndQuery` alone; the
/// transport owns the scheme + authority. This mirrors what tonic's own
/// `AddOrigin` layer does for its hyper `Channel`.
fn with_origin<B>(request: Request<B>, origin: &Uri) -> Request<B> {
    let (mut parts, body) = request.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
    let mut origin_parts = origin.clone().into_parts();
    origin_parts.path_and_query = Some(path_and_query);
    // The origin came from a valid URI and the path/query from another, so the
    // reassembly is valid; fall back to the original URI rather than panicking
    // if some exotic combination is rejected.
    match Uri::from_parts(origin_parts) {
        Ok(uri) => parts.uri = uri,
        Err(_) => { /* keep the original URI */ }
    }
    Request::from_parts(parts, body)
}

impl<ReqBody> tower_service::Service<Request<ReqBody>> for WinHttpChannel
where
    ReqBody: http_body::Body + Send + Unpin + 'static,
    ReqBody::Data: Send,
    ReqBody::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    type Response = Response<ResponseBody>;
    type Error = RequestError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The WinHTTP session is always ready to open another request; WinHTTP
        // manages its own connection pool and concurrency. There is no
        // per-channel backpressure to report here.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        // Clone the (Arc-backed) client into the future so it owns everything it
        // needs; the returned future is Send and self-contained.
        let client = self.client.clone();
        let origin = self.origin.clone();
        Box::pin(async move {
            let request = with_origin(request, &origin);
            client.request(request).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing type check: a real tonic-generated client must accept a
    /// [`WinHttpChannel`] as its transport. `EchoClient::new` is bounded by
    /// `tonic::client::GrpcService<tonic::body::Body>` plus the response-body
    /// bounds, so if this compiles the channel satisfies tonic 0.14's transport
    /// contract with tonic's actual body type — not a hand-picked one.
    #[allow(dead_code)]
    fn winhttp_channel_is_a_tonic_transport(channel: WinHttpChannel) {
        let _client = crate::echo::echo_client::EchoClient::new(channel);
    }

    /// `with_origin` replaces scheme+authority but keeps tonic's `:path`.
    #[test]
    fn with_origin_keeps_the_path_and_swaps_the_authority() {
        let origin: Uri = "https://localhost:12495".parse().unwrap();
        let request = Request::builder()
            .uri("/winasio.echo.v1.Echo/Unary")
            .body(())
            .unwrap();
        let rewritten = with_origin(request, &origin);
        assert_eq!(
            rewritten.uri().to_string(),
            "https://localhost:12495/winasio.echo.v1.Echo/Unary"
        );
        assert_eq!(rewritten.uri().authority().unwrap(), "localhost:12495");
        assert_eq!(rewritten.uri().path(), "/winasio.echo.v1.Echo/Unary");
    }

    /// `origin_of` drops any path from the configured origin.
    #[test]
    fn origin_of_strips_the_path() {
        let uri: Uri = "https://localhost:12495/ignored/path".parse().unwrap();
        let origin = origin_of(&uri);
        assert_eq!(origin.authority().unwrap(), "localhost:12495");
        assert_eq!(origin.scheme_str(), Some("https"));
    }
}
