// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

mod common;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use winasio::fs::{File, OpenOptions};
use winasio::io::{TransferError, TransferResult};
use winasio::iocp::{OpResult, ThreadPool, ThreadPoolIo};
use winasio::pipe::{ClientOptions, NamedPipe, ReadOutcome, ServerOptions};

static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy)]
enum Backend {
    File,
    Pipe,
}

fn assert_pending<F: Future>(future: &mut Pin<Box<F>>) {
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(future.as_mut().poll(&mut cx), Poll::Pending),
        "operation must be pending"
    );
}

fn artifact_path(name: &str) -> PathBuf {
    let n = NEXT_FILE.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::current_dir().unwrap().join("target");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!(
        "winasio-io-helpers-{}-{name}-{n}.tmp",
        std::process::id()
    ))
}

fn open_read_write(path: &PathBuf) -> File<ThreadPoolIo> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    options.open(&ThreadPool, path).unwrap()
}

fn open_read(path: &PathBuf) -> File<ThreadPoolIo> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.open(&ThreadPool, path).unwrap()
}

fn connected_pair(
    name: &str,
    server_options: &ServerOptions,
) -> (NamedPipe<ThreadPoolIo>, NamedPipe<ThreadPoolIo>) {
    let server = server_options.create(&ThreadPool).unwrap();
    let mut accept = Box::pin(server.connect());
    assert_pending(&mut accept);
    let client = ClientOptions::new(name).connect(&ThreadPool).unwrap();
    let server = common::block_on(accept).unwrap();
    (server, client)
}

fn patterned_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn assert_success<B>(result: &TransferResult<B>, expected: usize) {
    assert!(
        result.result.is_ok(),
        "helper should succeed: {:?}",
        result.result
    );
    assert_eq!(result.transferred, expected);
}

fn assert_unexpected_eof<B>(result: &TransferResult<B>, expected: usize) {
    assert!(
        matches!(&result.result, Err(TransferError::UnexpectedEof)),
        "expected unexpected EOF, got {:?}",
        result.result
    );
    assert_eq!(result.transferred, expected);
}

#[test]
fn write_all_large_payload_returns_buffer() {
    let payload = patterned_payload(16 * 1024 + 333);
    for backend in [Backend::File, Backend::Pipe] {
        match backend {
            Backend::File => {
                let path = artifact_path("write-all-large");
                let file = open_read_write(&path);
                let result = common::block_on(file.write_all(0, payload.clone()));
                assert_success(&result, payload.len());
                assert_eq!(result.buffer, payload);
                drop(file);
                assert_eq!(std::fs::read(&path).unwrap(), payload);
                std::fs::remove_file(path).unwrap();
            }
            Backend::Pipe => {
                let name = common::unique_pipe_name("write_all_large_payload_returns_buffer");
                let mut options = ServerOptions::new(&name);
                options.out_buffer_size(64).in_buffer_size(64);
                let (server, client) = connected_pair(&name, &options);
                let expected = payload.clone();
                let reader = std::thread::spawn(move || {
                    let mut got = Vec::new();
                    let mut reads = 0;
                    while got.len() < expected.len() {
                        let OpResult(read, chunk) =
                            common::block_on(client.read(Vec::with_capacity(127)));
                        match read.unwrap() {
                            ReadOutcome::Bytes(n) | ReadOutcome::MoreData(n) => {
                                assert_eq!(n, chunk.len());
                                assert!(n > 0, "reader must make progress");
                                got.extend_from_slice(&chunk);
                                reads += 1;
                            }
                            other => panic!("unexpected read outcome: {other:?}"),
                        }
                    }
                    (got, reads)
                });

                let result = common::block_on(server.write_all(payload.clone()));
                assert_success(&result, payload.len());
                assert_eq!(result.buffer, payload);
                drop(server);
                let (got, reads) = reader.join().unwrap();
                assert_eq!(got, payload);
                assert!(reads > 1, "small pipe buffer must require multiple reads");
            }
        }
    }
}

#[test]
fn read_to_end_returns_multi_operation_stream() {
    let payload = patterned_payload(18 * 1024 + 19);
    for backend in [Backend::File, Backend::Pipe] {
        match backend {
            Backend::File => {
                let path = artifact_path("read-to-end");
                std::fs::write(&path, &payload).unwrap();
                let file = open_read(&path);
                let result = common::block_on(file.read_to_end(0, Vec::new()));
                assert_success(&result, payload.len());
                assert_eq!(result.buffer, payload);
                drop(file);
                std::fs::remove_file(path).unwrap();
            }
            Backend::Pipe => {
                let name = common::unique_pipe_name("read_to_end_returns_multi_operation_stream");
                let options = ServerOptions::new(&name);
                let (server, client) = connected_pair(&name, &options);
                let expected = payload.clone();
                let writer = std::thread::spawn(move || {
                    let OpResult(written, returned) = common::block_on(server.write(expected));
                    assert_eq!(written.unwrap(), returned.len());
                    returned
                });

                let result = common::block_on(client.read_to_end(Vec::new()));
                assert_success(&result, payload.len());
                assert_eq!(result.buffer, payload);
                assert_eq!(writer.join().unwrap(), payload);
            }
        }
    }
}

#[test]
fn read_exact_short_stream_reports_unexpected_eof() {
    let short = b"short stream".to_vec();
    let requested = short.len() + 17;
    for backend in [Backend::File, Backend::Pipe] {
        match backend {
            Backend::File => {
                let path = artifact_path("read-exact-short");
                std::fs::write(&path, &short).unwrap();
                let file = open_read(&path);
                let result = common::block_on(file.read_exact(0, Vec::with_capacity(requested)));
                assert_unexpected_eof(&result, short.len());
                assert_eq!(result.buffer, short);
                drop(file);
                std::fs::remove_file(path).unwrap();
            }
            Backend::Pipe => {
                let name = common::unique_pipe_name("read_exact_short_stream_reports_eof");
                let options = ServerOptions::new(&name);
                let (server, client) = connected_pair(&name, &options);
                let OpResult(written, returned) = common::block_on(server.write(short.clone()));
                assert_eq!(written.unwrap(), short.len());
                assert_eq!(returned, short);
                drop(server);

                let result = common::block_on(client.read_exact(Vec::with_capacity(requested)));
                assert_unexpected_eof(&result, short.len());
                assert_eq!(result.buffer, short);
            }
        }
    }
}

#[test]
fn helper_failure_returns_buffer_category_and_nonzero_count() {
    let short = b"partial payload".to_vec();
    let requested = short.len() * 2;
    for backend in [Backend::File, Backend::Pipe] {
        match backend {
            Backend::File => {
                let path = artifact_path("failure-partway");
                std::fs::write(&path, &short).unwrap();
                let file = open_read(&path);
                let result = common::block_on(file.read_exact(0, Vec::with_capacity(requested)));
                assert_unexpected_eof(&result, short.len());
                assert!(result.transferred > 0);
                assert_eq!(result.buffer, short);
                drop(file);
                std::fs::remove_file(path).unwrap();
            }
            Backend::Pipe => {
                let name = common::unique_pipe_name("helper_failure_returns_nonzero_count");
                let options = ServerOptions::new(&name);
                let (server, client) = connected_pair(&name, &options);
                let OpResult(written, returned) = common::block_on(server.write(short.clone()));
                assert_eq!(written.unwrap(), short.len());
                assert_eq!(returned, short);
                drop(server);

                let result = common::block_on(client.read_exact(Vec::with_capacity(requested)));
                assert_unexpected_eof(&result, short.len());
                assert!(result.transferred > 0);
                assert_eq!(result.buffer, short);
            }
        }
    }
}

#[test]
fn write_all_closed_pipe_reports_closed_peer_category() {
    let name = common::unique_pipe_name("write_all_closed_pipe_reports_closed_peer");
    let options = ServerOptions::new(&name);
    let (server, client) = connected_pair(&name, &options);
    drop(client);

    let payload = b"peer is gone".to_vec();
    let result = common::block_on(server.write_all(payload.clone()));
    assert!(
        matches!(&result.result, Err(TransferError::ClosedPeer)),
        "expected closed peer, got {:?}",
        result.result
    );
    assert_eq!(result.transferred, 0);
    assert_eq!(result.buffer, payload);
}

#[test]
fn write_all_and_read_exact_return_same_allocation() {
    for backend in [Backend::File, Backend::Pipe] {
        match backend {
            Backend::File => {
                let path = artifact_path("allocation-identity");
                let file = open_read_write(&path);
                let payload = patterned_payload(1024);
                let write_ptr = payload.as_ptr() as usize;
                let write_cap = payload.capacity();
                let write = common::block_on(file.write_all(0, payload));
                assert_success(&write, 1024);
                assert_eq!(write.buffer.as_ptr() as usize, write_ptr);
                assert_eq!(write.buffer.capacity(), write_cap);

                let read_buf = Vec::with_capacity(1024);
                let read_ptr = read_buf.as_ptr() as usize;
                let read_cap = read_buf.capacity();
                let read = common::block_on(file.read_exact(0, read_buf));
                assert_success(&read, 1024);
                assert_eq!(read.buffer.as_ptr() as usize, read_ptr);
                assert_eq!(read.buffer.capacity(), read_cap);
                drop(file);
                std::fs::remove_file(path).unwrap();
            }
            Backend::Pipe => {
                let name = common::unique_pipe_name("allocation_identity_write");
                let options = ServerOptions::new(&name);
                let (server, client) = connected_pair(&name, &options);
                let payload = patterned_payload(512);
                let write_ptr = payload.as_ptr() as usize;
                let write_cap = payload.capacity();
                let write = common::block_on(server.write_all(payload));
                assert_success(&write, 512);
                assert_eq!(write.buffer.as_ptr() as usize, write_ptr);
                assert_eq!(write.buffer.capacity(), write_cap);

                let read_buf = Vec::with_capacity(512);
                let read_ptr = read_buf.as_ptr() as usize;
                let read_cap = read_buf.capacity();
                let read = common::block_on(client.read_exact(read_buf));
                assert_success(&read, 512);
                assert_eq!(read.buffer.as_ptr() as usize, read_ptr);
                assert_eq!(read.buffer.capacity(), read_cap);
                drop((server, client));
            }
        }
    }
}
