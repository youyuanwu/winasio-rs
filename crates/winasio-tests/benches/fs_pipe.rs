// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! File and named-pipe round-trip benchmarks.
//!
//! `harness = false` means `cargo test --all-targets` executes this binary in
//! test mode. Criterion handles that path; setup failures print and skip rather
//! than hanging the test run.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use winasio::fs::{File, OpenOptions, ReadOutcome};
use winasio::iocp::{OpResult, ThreadPool, ThreadPoolIo};
use winasio::pipe::{ClientOptions, NamedPipe, ServerOptions};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn block_on<F: Future>(fut: F) -> F::Output {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "benchmark operation did not complete"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn artifact_path(name: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        panic!("create benchmark target dir: {e}");
    }
    dir.join(format!(
        "winasio-fs-pipe-bench-{}-{name}-{n}.tmp",
        std::process::id()
    ))
}

fn unique_pipe_name(name: &str) -> String {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    format!("winasio_bench_{}_{}_{}", std::process::id(), name, n)
}

fn open_bench_file(path: &PathBuf) -> File<ThreadPoolIo> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    options.open(&ThreadPool, path).expect("open bench file")
}

fn connected_pipe(name: &str) -> (NamedPipe<ThreadPoolIo>, NamedPipe<ThreadPoolIo>) {
    let server = ServerOptions::new(name)
        .create(&ThreadPool)
        .expect("create pipe server");
    let accept = server.connect();
    let client = ClientOptions::new(name)
        .connect(&ThreadPool)
        .expect("connect pipe client");
    let server = block_on(accept).expect("accept pipe client");
    (server, client)
}

fn bench_file_round_trip(c: &mut Criterion) {
    let path = artifact_path("file");
    let file = open_bench_file(&path);
    let payload = vec![0x5a; 4096];

    let mut group = c.benchmark_group("fs_pipe");
    group.sample_size(50);
    group.bench_function("file_read_write_round_trip", |b| {
        b.iter(|| {
            let OpResult(written, returned) = block_on(file.write_at(0, payload.clone()));
            assert_eq!(written.unwrap(), returned.len());
            let OpResult(read, got) = block_on(file.read_at(0, Vec::with_capacity(returned.len())));
            assert_eq!(read.unwrap(), ReadOutcome::Bytes(returned.len()));
            assert_eq!(got, returned);
        })
    });
    group.finish();

    drop(file);
    let _ = std::fs::remove_file(path);
}

fn bench_pipe_round_trip(c: &mut Criterion) {
    let name = unique_pipe_name("pipe");
    let (server, client) = connected_pipe(&name);
    let request = b"ping".to_vec();
    let response = b"pong".to_vec();

    let mut group = c.benchmark_group("fs_pipe");
    group.sample_size(50);
    group.bench_function("pipe_request_response_round_trip", |b| {
        b.iter(|| {
            let OpResult(written, returned) = block_on(client.write(request.clone()));
            assert_eq!(written.unwrap(), returned.len());
            let OpResult(read, got) = block_on(server.read(Vec::with_capacity(16)));
            assert_eq!(read.unwrap(), ReadOutcome::Bytes(returned.len()));
            assert_eq!(got, returned);

            let OpResult(written, returned) = block_on(server.write(response.clone()));
            assert_eq!(written.unwrap(), returned.len());
            let OpResult(read, got) = block_on(client.read(Vec::with_capacity(16)));
            assert_eq!(read.unwrap(), ReadOutcome::Bytes(returned.len()));
            assert_eq!(got, returned);
        })
    });
    group.finish();
}

criterion_group!(benches, bench_file_round_trip, bench_pipe_round_trip);
criterion_main!(benches);
