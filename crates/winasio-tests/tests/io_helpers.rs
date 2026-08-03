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
use winasio::pipe::{ClientOptions, NamedPipe, PipeMode, ReadOutcome, ServerOptions};

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

fn message_pair(name: &str) -> (NamedPipe<ThreadPoolIo>, NamedPipe<ThreadPoolIo>) {
    let mut server_options = ServerOptions::new(name);
    server_options.pipe_type(PipeMode::Message);
    let server = server_options.create(&ThreadPool).unwrap();
    let mut accept = Box::pin(server.connect());
    assert_pending(&mut accept);
    let mut client_options = ClientOptions::new(name);
    client_options.read_mode(PipeMode::Message);
    let client = client_options.connect(&ThreadPool).unwrap();
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

/// A peer that went away mid-transfer is a different condition from a stream
/// that simply ended, and FR-033 requires the caller be able to tell them apart.
fn assert_closed_peer<B>(result: &TransferResult<B>, expected: usize) {
    assert!(
        matches!(&result.result, Err(TransferError::ClosedPeer)),
        "expected closed peer, got {:?}",
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
                let (server, client) = message_pair(&name);
                let expected = payload.clone();
                let reader = std::thread::spawn(move || {
                    let mut got = Vec::new();
                    let mut messages = 0;
                    while got.len() < expected.len() {
                        let OpResult(read, chunk) =
                            common::block_on(client.read(Vec::with_capacity(expected.len())));
                        match read.unwrap() {
                            ReadOutcome::Bytes(n) => {
                                assert_eq!(n, chunk.len());
                                assert!(n > 0, "reader must make progress");
                                got.extend_from_slice(&chunk);
                                messages += 1;
                            }
                            ReadOutcome::MoreData(_) => {
                                panic!("reader buffer should fit every write_all message")
                            }
                            other => panic!("unexpected read outcome: {other:?}"),
                        }
                    }
                    (got, messages)
                });

                let result = common::block_on(server.write_all(payload.clone()));
                assert_success(&result, payload.len());
                assert_eq!(result.buffer, payload);
                drop(server);
                let (got, messages) = reader.join().unwrap();
                assert_eq!(got, payload);
                assert!(
                    messages > 1,
                    "message-mode reads count the underlying write_all submissions"
                );
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
fn read_to_end_preserves_zero_length_message_and_reads_later_data() {
    let name = common::unique_pipe_name("read_to_end_zero_message_then_data");
    let (server, client) = message_pair(&name);

    let OpResult(empty, returned) = common::block_on(server.write(Vec::<u8>::new()));
    assert_eq!(empty.unwrap(), 0);
    assert!(returned.is_empty());

    let payload = b"after empty message".to_vec();
    let OpResult(written, returned) = common::block_on(server.write(payload.clone()));
    assert_eq!(written.unwrap(), payload.len());
    assert_eq!(returned, payload);
    drop(server);

    let result = common::block_on(client.read_to_end(Vec::new()));
    assert_success(&result, payload.len());
    assert_eq!(result.buffer, payload);
}

#[test]
fn read_to_end_uses_accumulator_spare_capacity_without_chunk_allocations() {
    let payload = patterned_payload(12 * 1024 + 321);
    let path = artifact_path("read-to-end-allocations");
    std::fs::write(&path, &payload).unwrap();
    let file = open_read(&path);

    let buffer = Vec::with_capacity(payload.len() + 1);
    let ptr = buffer.as_ptr() as usize;
    let capacity = buffer.capacity();
    let result = common::block_on(file.read_to_end(0, buffer));
    assert_success(&result, payload.len());
    assert_eq!(result.buffer, payload);
    assert_eq!(result.buffer.as_ptr() as usize, ptr);
    assert_eq!(result.buffer.capacity(), capacity);

    let source = include_str!(r"..\..\winasio\src\io.rs");
    assert!(
        !source.contains("Vec::with_capacity(HELPER_CHUNK)"),
        "read_to_end must not allocate a per-iteration chunk"
    );
    assert!(
        !source.contains("extend_from_slice(&chunk)"),
        "read_to_end must not copy from a temporary chunk"
    );

    drop(file);
    std::fs::remove_file(path).unwrap();
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
                // The server was dropped, so this is a closed peer -- not the
                // plain end-of-stream the file branch above sees.
                assert_closed_peer(&result, short.len());
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
                assert_closed_peer(&result, short.len());
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

#[test]
fn read_exact_preserves_zero_length_message_and_reads_later_data() {
    // A zero-length message is a real message, not end-of-stream. Treating it
    // as EOF made `read_exact` fail with UnexpectedEof and a transferred count
    // of zero while the requested bytes were still on their way -- the same
    // defect `read_to_end` had.
    let name = common::unique_pipe_name("read_exact_zero_message_then_data");
    let (server, client) = message_pair(&name);

    let OpResult(empty, returned) = common::block_on(server.write(Vec::<u8>::new()));
    assert_eq!(empty.unwrap(), 0);
    assert!(returned.is_empty());

    let payload = b"exactly-this".to_vec();
    let OpResult(written, returned) = common::block_on(server.write(payload.clone()));
    assert_eq!(written.unwrap(), payload.len());
    assert_eq!(returned, payload);

    let result = common::block_on(client.read_exact(Vec::with_capacity(payload.len())));
    assert_success(&result, payload.len());
    assert_eq!(result.buffer, payload);

    drop(server);
}
