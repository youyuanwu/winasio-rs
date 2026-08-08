# winasio-rs
![ci](https://github.com/youyuanwu/winasio-rs/actions/workflows/build.yaml/badge.svg)
[![codecov](https://codecov.io/github/youyuanwu/winasio-rs/branch/main/graph/badge.svg?token=B5HPWPDJCI)](https://codecov.io/github/youyuanwu/winasio-rs)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://raw.githubusercontent.com/youyuanwu/winasio-rs/main/LICENSE)

Windows Async I/O and Networking lib for Rust.

# IOCP
Runtime-agnostic infrastructure for making any Windows overlapped API awaitable.

Define an operation once by implementing `OpCode` — in your own crate, with no
change to `winasio` — and it works with either completion backend:

| | `Proactor` (own port) | `ThreadPoolIo` (system-managed) |
|---|---|---|
| Who drives completions | you, via `poll()` | the Win32 thread pool |
| Thread affinity | `!Send`, one thread | `Send + Sync` |
| Suits | single-threaded loops | multi-threaded runtimes |

A handle belongs to exactly one backend, permanently; registering twice fails
with a distinguishable error.

The operation owns whatever buffers or structures Windows writes into, and gets
them back on completion. That ownership transfer is what makes cancellation
safe: dropping a pending future requests cancellation but keeps the memory alive
until the kernel delivers the completion, so an abandoned read cannot corrupt
memory. The cost is that the buffer is not returned when you abandon an
operation.

`ReadAt`/`WriteAt` (byte buffers) and the HTTP Server API operations
(kernel-filled structures) ship as worked examples of both shapes.

# Httpsys
A safe, allocation-frugal wrapper over the Windows HTTP Server API, built on the
IOCP layer above. It is a building block rather than a framework: it covers the
request/response cycle and leaves the accept loop to you.

Serving one request end to end costs three allocations — the two operation
records and the request's metadata buffer — and does not grow with the number of
headers read or set. Reading a request allocates nothing at all: every accessor
borrows from the request's own buffer.

```rs
let _http = HttpInitializer::new()?;
let session = ServerSession::new()?;
let group = UrlGroup::new(&session)?;

let queue = Arc::new(RequestQueue::new(&ThreadPool)?);
queue.bind_url_group(&group)?;
group.add_url(&HSTRING::from("http://localhost:8080/demo/"))?;

while let Ok(request) = queue.receive().await {
    let target = request.target().unwrap_or_default().to_owned();

    let mut reply = Response::new(200);
    reply
        .set_header(ResponseHeader::CONTENT_TYPE, &b"text/plain"[..])
        .add_body(format!("you asked for {target}").into_bytes());

    queue.send(request.id(), reply).await.0?;
}
```

Three things are worth knowing:

- **A received request is an ordinary movable value.** HTTP.sys stores pointers
  into the tail of the buffer it fills, but that buffer is its own heap
  allocation, so moving a `Request` moves a pointer rather than the bytes. No
  `Pin` appears in the API.
- **Request and reply header names are separate types.** HTTP.sys numbers the
  two sets differently, and every id from 20 to 29 means a *different* header on
  each side — `Cookie` and `Retry-After` are both 25. `RequestHeader` and
  `ResponseHeader` cannot be interchanged, and the compiler enforces it.
- **An over-large request must be rejected, not just logged.** Requests are
  retried at a larger buffer automatically; past the retry bound the library
  discards the request itself and reports `ReceiveError::TooLarge`. Discarding is
  not left to the caller, because a queued request that cannot be delivered would
  be returned by every subsequent receive — so a loop that only logged the error
  would spin on it forever.

See the [example server](./crates/winasio-tests/examples/httpsys_server.rs) for
complete, runnable code.

## Server-side TLS

`bind_ssl_certificate` binds a certificate — named by its SHA-1 thumbprint in a
system store — to an `ip:port`, wrapping `HttpSetServiceConfiguration` for
`HttpServiceConfigSSLCertInfo`. It returns an `SslCertBinding` guard that unbinds
on drop, because a leaked binding is machine-wide global state, not a
process-local handle; `query_ssl_binding` reads a binding back. Writing the
binding table requires an elevated process, and that one failure is modelled
distinctly as `SslBindError::RequiresElevation` so a caller can say "needs
elevation" rather than reporting a generic error. This is the piece HTTP.sys
otherwise leaves to `netsh http add sslcert`.

### Running the HTTPS integration tests

Because binding a certificate is a machine-wide, administrator-only operation,
the end-to-end HTTPS tests do **not** bind anything themselves. Provisioning is a
one-time, out-of-process step: run
[`scripts/setup-https-test.ps1`](./scripts/setup-https-test.ps1) once per
machine. It generates a self-signed `localhost` certificate (with a
`DNS:localhost` SAN) into `LocalMachine\My` and binds it to a fixed port:

```powershell
# once; run from an ordinary PowerShell -- the script self-elevates and Windows
# shows a UAC prompt to approve. (Already elevated? It just runs, no prompt.)
pwsh -File scripts/setup-https-test.ps1
```

The tests then run **unelevated** (`cargo test`): they detect the binding and, if
it is absent, skip with a greppable `HTTPS_TLS_TEST: SKIPPED` line rather than
failing. Tear the machine state down completely — binding, certificate and CNG
key container — with (this also self-elevates):

```powershell
pwsh -File scripts/setup-https-test.ps1 -Uninstall
```

The port, AppId and certificate subject live in a single source of truth,
[`scripts/https-test-config.ps1`](./scripts/https-test-config.ps1), which both
the script and the tests read, so they cannot drift. See
[`httpsys_tls.rs`](./crates/winasio-tests/tests/httpsys_tls.rs) for the
end-to-end WinHTTP-over-HTTPS test, paired with a negative control that confirms
an unrelaxed client rejects the self-signed certificate. CI provisions the
binding on its elevated runner before the test step, so the TLS tests execute
there too (grep the CI log for `HTTPS_TLS_TEST:` to see `RAN` vs `SKIPPED`).

Where the tests are *expected* to run — CI, or any run after you have
provisioned the binding — set `WINASIO_REQUIRE_TLS_TESTS=1`. With it set, a
missing or unreadable binding is a hard **failure** instead of a skip, so a
broken provisioning step or a `query_ssl_binding` regression cannot silently
drop all HTTPS coverage while the run stays green. The CI workflow sets it; an
unelevated local run without it still skips cleanly.

# Fs
Safe asynchronous file I/O on top of the IOCP layer. Files are opened for
overlapped I/O, registered immediately, and expose positional reads, writes, and
whole-buffer helpers while returning the caller's buffer on resolved operations.

```rs
let mut options = OpenOptions::new();
options.read(true).write(true).create(true).truncate(true);

let file = options.open(&ThreadPool, "target/winasio-readme-fs.bin")?;
let data = b"hello file".to_vec();
let OpResult(written, data) = file.write_at(0, data).await;
written?;

let OpResult(read, buf) = file.read_at(0, Vec::with_capacity(data.len())).await;
assert_eq!(read?, ReadOutcome::Bytes(data.len()));
assert_eq!(&buf, b"hello file");
```

# Pipe
Safe local named-pipe server and client APIs with byte mode by default and
message mode available when framing matters. A server instance connects into a
typed connected pipe; disconnect consumes it and returns a reusable server
instance.

```rs
let name = "winasio_readme_pipe";
let server = ServerOptions::new(name).create(&ThreadPool)?;
let client = ClientOptions::new(name).connect(&ThreadPool)?;
let server = server.connect().await?;

let OpResult(written, payload) = client.write(b"ping".to_vec()).await;
written?;

let OpResult(read, buf) = server.read(Vec::with_capacity(payload.len())).await;
assert_eq!(read?, ReadOutcome::Bytes(payload.len()));
assert_eq!(&buf, b"ping");
```

# Net
Asynchronous TCP on top of the same completion machinery: `TcpListener` accepts
with `AcceptEx`, `TcpStream` connects with `ConnectEx` and transfers with
`WSARecv`/`WSASend`. Both are generic over the backend, so the same code runs on
an owned `Proactor` or on the system thread pool.

Closing is abrupt, unlike a file or a pipe, because a socket is the only one of
the three with a half-open state worth distinguishing. Call `shutdown` if the
peer needs to tell "done sending" from "gone".

```rs
let listener = TcpListener::bind(&ThreadPool, "127.0.0.1:0".parse()?)?;
let addr = listener.local_addr();

let client = TcpStream::connect(&ThreadPool, addr).await?;
let (server, _peer) = listener.accept().await?;

let OpResult(written, _) = client.write(b"ping".to_vec()).await;
written?;

let OpResult(read, buf) = server.read(vec![0u8; 4]).await;
assert_eq!(read?, ReadOutcome::Bytes(4));
assert_eq!(&buf, b"ping");
```

UDP, `WSARecvFrom`/`WSASendTo`, `WSARecvMsg`, `TransmitFile` and vectored I/O
are out of scope.

# Winhttp
An asynchronous HTTP and HTTPS client on top of WinHTTP. `Session` holds
process-scoped configuration and timeouts, `Connection` names a server, and
`Request` is the handle every transfer happens on.

Async is the only mode. Every session is opened with `WINHTTP_FLAG_ASYNC`, the
flag is not a parameter, and there is no synchronous surface to fall back to.

Unlike every other I/O module here, this one is **not** generic over a
`Submitter`. WinHTTP exposes no `OVERLAPPED` and cannot be associated with a
completion port -- it runs its own thread pool and delivers completions through a
status callback -- so there is nothing to hand a `Proactor`. What falls out of
that is a self-contained `Waker`-driven state machine, which means the client
runs under **any** executor, including a bare `futures::executor::block_on`
with no reactor and no worker threads. The module docs explain the asymmetry in
full.

Transfers take their buffers **by value** and hand them back in an `OpResult`,
the same discipline the IOCP modules use. A borrowed buffer cannot be made sound
here: dropping a pending future ends the borrow, but WinHTTP still owns the
pointer and will write through it. Abandoning a transfer therefore costs you the
buffer, and parks the request until the abandoned completion lands.

`send` is the one exception: it consumes the request body and never returns it,
because WinHTTP may re-read the body after the send completes in order to follow
a redirect or answer an authentication challenge. The body is released once the
response has been received.

Header queries are ordinary synchronous methods, not futures, because
`WinHttpQueryHeaders` answers on the calling thread even on an async handle.

WinHTTP WebSockets, a higher-level request builder and URL parsing are out of
scope.

```rs
let session = Session::new(&HSTRING::from("winasio-example"))?;
    session.set_timeouts(5_000, 5_000, 5_000, 5_000)?;
    let connection = session.connect(&host, port)?;
    let mut request =
        connection.open_request(&HSTRING::from("GET"), &HSTRING::from("/"), &[], false)?;

    let body = futures::executor::block_on(async {
        request.send(None, Vec::new(), 0).await?;
        request.receive_response().await?;
        let status = request.status_code()?;
        assert_eq!(status, 200);

        let mut body = Vec::new();
        loop {
            let available = request.query_data_available().await?;
            if available == 0 {
                break;
            }
            let OpResult(read, chunk) = request
                .read_data(Vec::with_capacity(available as usize))
                .await;
            body.extend_from_slice(&chunk[..read?]);
        }
        Ok::<_, windows::core::Error>(body)
    })?;
```
The snippet above is a literal, line-for-line copy of a test that runs in CI; see
the [example test](./crates/winasio-tests/tests/winhttp.rs).

Redirects are followed by the platform unless told otherwise, and the default is
worth knowing about: a `301` answering a `POST` is silently replayed as a `GET`
with the body dropped, and a redirect arriving for a request whose body was
streamed fails the transfer outright. `Session::set_redirect_policy` turns it
off.

# Winasio-util
Higher-level HTTP over `winasio`, shaped around the `http` crate: a **client**
over `winasio::winhttp` and a **server** over `winasio::httpsys`. Both speak
`http::Request`/`http::Response` with bodies that implement `http_body::Body`
over `bytes::Bytes` -- the same types hyper uses, so a hyper or axum user should
find nothing surprising in the shape of the API, only in what it deliberately
does not have.

## Client

```rs
let client = Client::new("winasio-util/0.1")?;
let request = http::Request::get("http://example.com/").body(Empty::<Bytes>::new())?;

// No runtime anywhere: `block_on` is a bare single-threaded executor.
let response = futures::executor::block_on(client.request(request))?;
assert!(response.status().is_success());

let body = futures::executor::block_on(response.into_body().collect())?.to_bytes();
```

The crate owns message framing: `Content-Length` and `Transfer-Encoding` are
derived from the request body's `size_hint`, and a caller that sets either is
refused. A known length is declared up front; an unknown one is streamed with
chunked framing rather than buffered, so a body larger than memory still works.

A request header whose value is not printable ASCII is **rejected**, naming the
header, rather than lossily converted -- `http::HeaderValue` holds arbitrary
bytes and WinHTTP wants UTF-16, and sending something the caller did not write
would be worse than failing. Response headers are parsed from the raw header
block rather than queried by name, so repeated headers such as `Set-Cookie`
survive as multiple `HeaderMap` entries.

**A body that was cut off is never a body that ended.** WinHTTP reports a
connection closed gracefully mid-body as a clean end of body, which is exactly
the trap `crates/winasio/src/net/outcome.rs` was written to avoid one level down.
This crate counts delivered bytes against the declared `Content-Length` and
reports `ResponseBodyError::Truncated`. A chunked or close-delimited response has no
declared length and cannot be checked; that is a property of HTTP and is
documented rather than guessed around.

Redirect following is off, because the platform's rewrites a `POST` into a `GET`
without saying so and breaks every streamed body. Cookie jars and retry policy
are out of scope, as are implementations of hyper's own client/server traits.

This crate implements no connection pool -- but WinHTTP keeps one anyway, and it
was measured to be **process-wide**, not per-session: a brand new `Client` per
request reuses sockets exactly as often as a shared one does. WinHTTP does not
retry a pooled connection the server has since closed, in synchronous or
asynchronous mode, so a healthy server reaping an idle keep-alive connection can
occasionally surface a transport error. That is reported rather than papered
over: a retry would be out of scope, unsafe for a non-idempotent request, and
impossible anyway once `WinHttpSendRequest` has consumed the body. The failure is
always a visible `RequestError::Transport` at `RequestStage::ReceiveResponse` carrying
`WinHttpError::ConnectionError`, never a silent truncation, so a caller who knows
its request is idempotent can retry on exactly that.

The response body is an explicit state machine that **owns** its `Request` and
holds one boxed operation future across polls. Every operation in the lower crate
borrows the request mutably, so a `Body` holding both would be self-referential;
inverting the ownership solves it in safe Rust. Recreating the future on each
poll -- the obvious alternative -- was measured to retire a buffer per poll and
then park the request with `ERROR_BUSY`. The module docs record the rejected
alternatives in full.

## Server

The server half takes a **`tower_service::Service`** and drives it over HTTP.sys.
`tower-service` is trait-only, has no runtime and no dependencies, and is the
trait axum's `Router` and every `tower-http` layer already implement -- so an
axum router is servable here directly, and a hyper service bridges through
`hyper-util`'s existing adapter.

```rs
// The session owns the subsystem initialisation: HTTP.sys will not create a
// session before `HttpInitialize` has run.
let session = ServerSession::new()?;
let server = Server::builder(&session)
    .url("http://localhost:8080/demo/")
    .build(&ThreadPool)?;

let mut service = tower::service_fn(|_req: http::Request<IncomingBody>| async {
    Ok::<_, Infallible>(http::Response::new(Full::new(Bytes::from_static(b"hi"))))
});

// No runtime anywhere: `block_on` is a bare single-threaded executor.
futures::executor::block_on(server.serve_one(&mut service))?;
```

**Nothing here spawns.** No task, no thread, no reactor, no runtime. Concurrency
is the caller's, and both shapes work: `serve`/`serve_one` take `&mut S` and
drive requests one at a time, which is all a single-threaded `block_on` loop
needs; or `accept` hands back an `Accepted` that owns everything it needs, is
`Send + 'static` on the thread-pool backend, and can be moved onto whatever
executor the caller likes together with a clone of the service. The type is
generic over the I/O backend, so a `!Send` single-threaded `Proactor` loop works
as well as the thread pool.

`poll_ready` is honoured rather than skipped. The sequential driver awaits
readiness **before** it accepts, so a service that is not ready stops requests
being pulled out of the kernel queue -- backpressure that does something. The
concurrent driver awaits readiness on the clone that will handle the request,
because a tower reservation belongs to the clone that made it.

Framing is the crate's, and it is measured rather than assumed. HTTP.sys computes
`Content-Length` for a fully buffered reply but does **no** framing at all for a
streamed one -- not even on a keep-alive connection, where the result is an
undelimited body running into the next response -- so this crate declares a
length when the body's `size_hint` gives one and writes chunked framing when it
does not. A caller's `Content-Length` is allowed, because an `axum::Router` sets
one, but it is checked against the body rather than trusted; a caller's
`Transfer-Encoding` is refused. A reply that under-delivers its declared length
is an error, not a silently truncated message, which is what HTTP.sys would
otherwise put on the wire. A `HEAD` reply and a `204` never send a body -- HTTP.sys
sends both if given one.

The two header numbering tables are the sharpest edge below: every id from 20 to
29 means a *different* header on a request than on a reply, so `25` is `Cookie`
inbound and `Retry-After` outbound. The conversion reads each side through its
own table, and a test asserts it end to end rather than by inspection.

Out of scope here: TLS certificate configuration, which lives one layer down in
`winasio::httpsys` (see [Server-side TLS](#server-side-tls)) rather than in this
`tower`-facing wrapper; HTTP/2 specifics, WebSockets, server push,
authentication; and routing, which is what a `tower::Service` is for. Free from
the platform and therefore not built: `Expect: 100-continue`, request
de-chunking, and truncated-body detection -- unlike WinHTTP, HTTP.sys reports a
cut-off request body as an error.

`crates/winasio-tests/examples/util_server.rs` is a complete server in safe code
on a bare executor; the test suite compiles it, runs it, and asserts textually
that it contains no `unsafe`.

# Winasio-axum
A concurrent driver that serves an `axum::Router` over HTTP.sys. An axum router
already runs on `winasio-util`'s server -- a `Router` is a `tower::Service` and
that is all `serve_one` needs -- so this crate adds not "axum support" but the
**driver**: a concurrent accept-and-dispatch loop shaped like `axum::serve`, an
`Executor` seam so concurrency is the caller's choice of runtime (or none), and
axum-shaped ergonomics. Request decoding, response framing, body handling,
`poll_ready` backpressure, and connection info stay `winasio-util`'s.

```rs
let session = ServerSession::new()?;
let server = winasio_util::Server::builder(&session)
    .url("http://localhost:8080/axum/")
    .build(&ThreadPool)?;

// HTTP.sys delivers the registered prefix, so routes carry it: `/axum/greet`.
let app = axum::Router::new().route("/axum/greet", axum::routing::get(|| async { "hi" }));

// No runtime anywhere: `block_on` is a bare single-threaded executor and
// `CurrentThread` drives many in-flight requests on it, spawning nothing.
// `serve` returns a `Serve`, which is `IntoFuture` (not `Future`), so it is
// awaited inside the `block_on`ed async block.
futures::executor::block_on(async {
    serve(&server, app, CurrentThread::new()).await
})?;
```

**Concurrency is a trait the caller supplies.** `Executor<Fut>` has the shape of
`hyper::rt::Executor` -- one `execute(&self, fut)` method -- plus a defaulted
`poll_progress` hook a current-thread executor needs and a spawning one ignores,
so plugging in tokio is a few lines and pulls no tokio into this crate. Two
executors ship built in: `CurrentThread` interleaves many in-flight requests on
the caller's own thread through a `FuturesUnordered`, spawning nothing and
keeping the runtime-free story; `ThreadPerRequest` runs each request on a fresh
`std::thread` for real parallelism. The distinction is proved by tests that only
complete when several requests are in flight at once -- served one at a time they
would deadlock -- so the concurrency is measured, not asserted.

**Shutdown is abrupt, and uniform across both executors.** When the queue closes
`serve` returns immediately without draining in-flight work; a `ThreadPerRequest`
task (and its error-observer callback) may still run after `serve` has returned.
Draining only `CurrentThread` would make the contract depend on the executor, so
a single abrupt rule was chosen instead. A handler panic is caught per task and
reported to a caller-supplied `on_error` observer as a `HandlerPanic` rather than
unwinding the loop -- which matters most for `CurrentThread`, whose tasks share
the loop's thread -- and a recoverable accept error (`RequestTooLarge`) is
reported and stepped over rather than allowed to spin or abort.

**No `ConnectInfo`, by choice.** axum's `ConnectInfo<SocketAddr>` extractor is
gated behind axum's `tokio` feature, and enabling it pulls tokio + mio + hyper +
hyper-util into the normal dependency graph (measured) -- the cost that would
contradict this workspace's runtime-agnostic stance. This crate depends on `axum`
with `default-features = false` and skips it; the peer address remains reachable
through the `winasio_util::ConnectionInfo` extension on the request. A caller who
specifically wants the extractor can enable axum's `tokio` feature in their own
crate and insert `ConnectInfo(addr)` into the request extensions with a tiny
layer (measured to satisfy it). The runtime-free guarantee is defined on this
crate's own normal dependency tree -- a sibling crate enabling `axum/tokio`
unifies that feature into a `--workspace` build -- and is checked by
`cargo tree -e normal -p winasio-axum`.

`crates/winasio-tests/examples/axum_server.rs` is a complete concurrent axum
server in safe code on a bare executor; the test suite compiles it, runs it, and
asserts textually that it contains no `unsafe`.

# Winasio-tonic
gRPC (tonic) over WinHTTP (client) and HTTP.sys (server). tonic's generated
**server** is an `axum::Router`, so it rides directly on `winasio-axum`'s driver
via `winasio_tonic::serve_grpc`, which selects the raw HTTP/2 DATA-frame +
trailers response path so a service's terminating `grpc-status` trailer reaches
the peer. The **client** side is `winasio_tonic::WinHttpChannel`, a `Channel`-like
`tower::Service` that satisfies `tonic::client::GrpcService<BoxBody>`, so a
generated tonic client speaks to it directly.

```rs
// Server: mount a tonic service (an axum Router under the hood) and serve it.
let router = axum::Router::new().fallback_service(EchoServer::new(MyService));
winasio_tonic::serve_grpc(&server, router, CurrentThread::new()).await?;

// Client: build a channel over WinHTTP and hand it to the generated client.
let channel = winasio_tonic::WinHttpChannel::new(
    "https://localhost:12495".parse()?,
    "winasio-tonic/0.1",
)?;
let mut client = EchoClient::new(channel);
```

**All four call types are supported** — unary, server-streaming, client-streaming,
and bidirectional — built on the WinHTTP HTTP/2 duplex path (start the response
receive before finishing the request body; end the request body with an explicit
empty DATA frame carrying END_STREAM) and HTTP.sys response trailers.

**TLS is mandatory.** HTTP.sys speaks HTTP/2 only over a TLS binding (there is no
h2c), and Microsoft supports gRPC over `WinHttpHandler` only over TLS. Every e2e
gRPC test therefore depends on the provisioned certificate (see *Server-side TLS*)
and uses the same require/skip idiom as the other TLS tests.

**Duplex streaming is platform-dependent (M9/M11).** On Windows 11 / Server 2022+
all four call types work. On Windows Server 2019/2022 without the automatic-chunking
backport, only unary and server-streaming are guaranteed; client-streaming and
bidirectional need the duplex request path. The client probes the
`WINHTTP_FLAG_AUTOMATIC_CHUNKING` capability at request time and falls back to
manual chunking (which downgrades to HTTP/1.1 and cannot carry gRPC) when it is
absent. The e2e tests log greppable `GRPC_TLS_TEST:` and `GRPC_DUPLEX:` tokens
recording exactly which call types were exercised, so a platform that lacks duplex
produces a **visible, narrowly-scoped** skip rather than a silent green.

**`tokio`, but no runtime.** `winasio-tonic` allows bare `tokio` in its graph
(tonic pulls it via `tokio-stream`, features `default` + `sync` only) but no
async *runtime*: `rt`, `net`, `mio`, `hyper`, and `hyper-util` must not appear.
This is enforced by `winasio_tonic_pulls_in_no_async_runtime_beyond_tokio` in
`crates/winasio-tests/tests/dependencies.rs`.

Codegen is `build.rs` + `tonic-prost-build` from a checked-in `.proto`, so a
`protoc` install is required to build the tests (CI installs it via
`arduino/setup-protoc`).

# Layout
This repo is a cargo workspace:
- `crates/winasio`: the library crate.
- `crates/winasio-util`: higher-level HTTP client over `winasio::winhttp` and
  HTTP server over `winasio::httpsys`.
- `crates/winasio-axum`: concurrent driver that serves an `axum::Router` over
  HTTP.sys through a caller-supplied executor.
- `crates/winasio-tonic`: gRPC (tonic) client transport and server glue over
  `winasio-util`/`winasio-axum`.
- `crates/winasio-tests`: test only crate holding the integration tests.

# MISC
C++ counterpart of this lib: [winasio](https://github.com/youyuanwu/winasio)

# License
MIT License