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

let queue = Arc::new(RequestQueue::new()?);
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

# Winhttp
Winhttp in async mode with rust async await wrapper.
Example snippit:
```rs
  let req = conn
    .open_request(
        HSTRING::from("GET"),
        HSTRING::from("hello/world"),
        HSTRING::from("HTTP/1.1"),
        HSTRING::new(),
        Some(vec![HSTRING::from("application/json")]),
        WINHTTP_OPEN_REQUEST_FLAGS(0), // not use WINHTTP_FLAG_SECURE
    )
    .unwrap();

  let mut async_req: HRequestAsync = HRequestAsync::new(req);

  async_req.async_send(HSTRING::new(), &[], 0).await.unwrap();

  async_req.async_receive_response().await.unwrap();

  loop {
      let len = async_req.async_query_data_available().await.unwrap();
      if len == 0 {
          break;
      }
      let mut buffer: Vec<u8> = vec![0; len as usize];
      let len_read = async_req
          .async_read_data(buffer.as_mut_slice(), len)
          .await
          .unwrap();
      assert!(len == len_read);
      let s = String::from_utf8_lossy(&buffer);
      print!("{}", s);
  }
```
See full working code in [example test](./crates/winasio-tests/tests/winhttp.rs)

# Layout
This repo is a cargo workspace:
- `crates/winasio`: the library crate.
- `crates/winasio-tests`: test only crate holding the integration tests.

# MISC
C++ counterpart of this lib: [winasio](https://github.com/youyuanwu/winasio)

# License
MIT License