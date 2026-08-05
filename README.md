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
        request.send(None, Vec::new(), 0).await.0?;
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
# Layout
This repo is a cargo workspace:
- `crates/winasio`: the library crate.
- `crates/winasio-tests`: test only crate holding the integration tests.

# MISC
C++ counterpart of this lib: [winasio](https://github.com/youyuanwu/winasio)

# License
MIT License