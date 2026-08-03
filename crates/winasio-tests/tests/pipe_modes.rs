// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

mod common;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use winasio::iocp::{OpResult, ThreadPool, ThreadPoolIo};
use winasio::pipe::{
    AccessDirection, ClientOptions, NamedPipe, PipeMode, ReadOutcome, ServerOptions, SetupError,
};

fn assert_pending<F: Future>(future: &mut Pin<Box<F>>) {
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(future.as_mut().poll(&mut cx), Poll::Pending),
        "operation must be pending"
    );
}

fn connected_pair(
    _name: &str,
    server_options: &ServerOptions,
    client_options: &ClientOptions,
) -> (NamedPipe<ThreadPoolIo>, NamedPipe<ThreadPoolIo>) {
    let server = server_options.create(&ThreadPool).unwrap();
    let mut accept = Box::pin(server.connect());
    assert_pending(&mut accept);
    let client = client_options.connect(&ThreadPool).unwrap();
    let server = common::block_on(accept).unwrap();
    (server, client)
}

fn message_pair(name: &str) -> (NamedPipe<ThreadPoolIo>, NamedPipe<ThreadPoolIo>) {
    let mut server_options = ServerOptions::new(name);
    server_options.pipe_type(PipeMode::Message);
    let mut client_options = ClientOptions::new(name);
    client_options.read_mode(PipeMode::Message);
    connected_pair(name, &server_options, &client_options)
}

#[test]
fn message_smaller_than_buffer_is_single_transfer() {
    let name = common::unique_pipe_name("message_smaller_than_buffer_is_single_transfer");
    let (server, client) = message_pair(&name);

    let OpResult(written, returned) = common::block_on(server.write(b"hello".to_vec()));
    assert_eq!(written.unwrap(), 5);
    assert_eq!(returned, b"hello");

    let OpResult(read, got) = common::block_on(client.read(Vec::with_capacity(16)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(5));
    assert_eq!(got, b"hello");
}

#[test]
fn oversized_message_reports_more_data_then_remainder() {
    let name = common::unique_pipe_name("oversized_message_reports_more_data_then_remainder");
    let (server, client) = message_pair(&name);

    let OpResult(written, _) = common::block_on(server.write(b"abcdefgh".to_vec()));
    assert_eq!(written.unwrap(), 8);

    let OpResult(read, got) = common::block_on(client.read(Vec::with_capacity(3)));
    assert_eq!(read.unwrap(), ReadOutcome::MoreData(3));
    assert_eq!(got.len(), 3);
    assert_eq!(got, b"abc");

    let OpResult(read, got) = common::block_on(client.read(Vec::with_capacity(8)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(5));
    assert_eq!(got, b"defgh");
}

#[test]
fn byte_mode_oversized_payload_is_ordinary_partial_transfer() {
    let name = common::unique_pipe_name("byte_mode_oversized_payload_is_ordinary_partial_transfer");
    let server_options = ServerOptions::new(&name);
    let client_options = ClientOptions::new(&name);
    let (server, client) = connected_pair(&name, &server_options, &client_options);

    let OpResult(written, _) = common::block_on(server.write(b"abcdefgh".to_vec()));
    assert_eq!(written.unwrap(), 8);

    let OpResult(read, got) = common::block_on(client.read(Vec::with_capacity(3)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(3));
    assert_eq!(got, b"abc");
}

#[test]
fn zero_length_message_is_not_end_of_stream() {
    let name = common::unique_pipe_name("zero_length_message_is_not_end_of_stream");
    let (server, client) = message_pair(&name);

    let OpResult(written, returned) = common::block_on(server.write(Vec::<u8>::new()));
    assert_eq!(written.unwrap(), 0);
    assert!(returned.is_empty());

    let OpResult(read, got) = common::block_on(client.read(Vec::with_capacity(8)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(0));
    assert!(got.is_empty());
}

#[test]
fn write_on_read_only_pipe_fails_and_returns_buffer() {
    let name = common::unique_pipe_name("write_on_read_only_pipe_fails_and_returns_buffer");
    let mut server_options = ServerOptions::new(&name);
    server_options.access(AccessDirection::Inbound);
    let mut client_options = ClientOptions::new(&name);
    client_options.access(AccessDirection::Outbound);
    let (server, client) = connected_pair(&name, &server_options, &client_options);

    let payload = b"not allowed".to_vec();
    let OpResult(write, returned) = common::block_on(server.write(payload.clone()));
    assert!(write.is_err());
    assert_eq!(returned, payload);

    drop(client);
}

#[test]
fn server_builder_options_create_and_documented_violations_fail() {
    for access in [
        AccessDirection::Inbound,
        AccessDirection::Outbound,
        AccessDirection::Duplex,
    ] {
        let name = common::unique_pipe_name("server_builder_access_option");
        let mut options = ServerOptions::new(&name);
        options.access(access);
        let server = options.create(&ThreadPool).unwrap();
        drop(server);
    }

    for pipe_type in [PipeMode::Byte, PipeMode::Message] {
        let name = common::unique_pipe_name("server_builder_type_option");
        let mut options = ServerOptions::new(&name);
        options.pipe_type(pipe_type);
        let server = options.create(&ThreadPool).unwrap();
        drop(server);
    }

    let name = common::unique_pipe_name("server_builder_buffer_timeout_options");
    let mut options = ServerOptions::new(&name);
    options
        .max_instances(2)
        .in_buffer_size(512)
        .out_buffer_size(1024)
        .default_timeout(25);
    let first = options.create(&ThreadPool).unwrap();
    let second = options.create(&ThreadPool).unwrap();
    drop((first, second));

    let name = common::unique_pipe_name("server_builder_max_instances_violation");
    let mut limited = ServerOptions::new(&name);
    limited.max_instances(1);
    let first = limited.create(&ThreadPool).unwrap();
    assert!(matches!(limited.create(&ThreadPool), Err(SetupError::Busy)));
    drop(first);

    let name = common::unique_pipe_name("server_builder_first_instance_violation");
    let mut first_only = ServerOptions::new(&name);
    first_only.max_instances(2).first_instance(true);
    let first = first_only.create(&ThreadPool).unwrap();
    assert!(matches!(
        first_only.create(&ThreadPool),
        Err(SetupError::AccessDenied)
    ));
    drop(first);
}
