// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

mod common;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use winasio::iocp::{
    live_operations, OpResult, Proactor, Registrar, RegistrationError, ThreadPool, ThreadPoolIo,
};
use winasio::pipe::{
    AccessDirection, ClientOptions, NamedPipe, ReadOutcome, ServerOptions, SetupError,
    MAX_NAME_COMPONENT_LEN,
};
use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_OPERATION_ABORTED};

fn assert_pending<F: Future>(future: &mut Pin<Box<F>>) {
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(future.as_mut().poll(&mut cx), Poll::Pending),
        "operation must be pending"
    );
}

fn connected_pair(name: &str) -> (NamedPipe<ThreadPoolIo>, NamedPipe<ThreadPoolIo>) {
    let server = ServerOptions::new(name).create(&ThreadPool).unwrap();
    let mut accept = Box::pin(server.connect());
    assert_pending(&mut accept);
    let client = ClientOptions::new(name).connect(&ThreadPool).unwrap();
    let server = common::block_on(accept).unwrap();
    (server, client)
}

fn wait_for_baseline(baseline: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while live_operations() > baseline && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        live_operations() <= baseline,
        "pipe teardown must not leave additional live operations"
    );
}

#[test]
fn connect_waits_until_client_arrives() {
    let name = common::unique_pipe_name("connect_waits_until_client_arrives");
    let server = ServerOptions::new(name.as_str())
        .create(&ThreadPool)
        .unwrap();
    let mut accept = Box::pin(server.connect());
    assert_pending(&mut accept);

    let client = ClientOptions::new(name.as_str())
        .connect(&ThreadPool)
        .unwrap();
    let server = common::block_on(accept).unwrap();
    drop((server, client));
}

#[test]
fn preconnected_client_is_accepted_deterministically() {
    let name = common::unique_pipe_name("preconnected_client_is_accepted_deterministically");
    let server = ServerOptions::new(name.as_str())
        .create(&ThreadPool)
        .unwrap();
    let client = ClientOptions::new(name.as_str())
        .connect(&ThreadPool)
        .unwrap();

    let accept = server.connect();
    let server = common::block_on(accept).unwrap();
    drop((server, client));
}

#[test]
fn byte_exchange_both_directions() {
    let name = common::unique_pipe_name("byte_exchange_both_directions");
    let (server, client) = connected_pair(&name);

    let OpResult(written, returned) = common::block_on(client.write(b"request".to_vec()));
    assert_eq!(written.unwrap(), 7);
    assert_eq!(returned, b"request");

    let OpResult(read, got) = common::block_on(server.read(Vec::with_capacity(32)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(7));
    assert_eq!(got, b"request");

    let OpResult(written, returned) = common::block_on(server.write(b"response".to_vec()));
    assert_eq!(written.unwrap(), 8);
    assert_eq!(returned, b"response");

    let OpResult(read, got) = common::block_on(client.read(Vec::with_capacity(32)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(8));
    assert_eq!(got, b"response");
}

#[test]
fn disconnect_and_reuse_serves_two_clients() {
    let name = common::unique_pipe_name("disconnect_and_reuse_serves_two_clients");
    let server = ServerOptions::new(name.as_str())
        .create(&ThreadPool)
        .unwrap();

    let mut accept = Box::pin(server.connect());
    assert_pending(&mut accept);
    let first = ClientOptions::new(name.as_str())
        .connect(&ThreadPool)
        .unwrap();
    let server = common::block_on(accept).unwrap();
    let OpResult(written, _) = common::block_on(first.write(b"one".to_vec()));
    assert_eq!(written.unwrap(), 3);
    let OpResult(read, got) = common::block_on(server.read(Vec::with_capacity(8)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(3));
    assert_eq!(got, b"one");
    drop(first);

    let server = server.disconnect().unwrap();
    let mut accept = Box::pin(server.connect());
    assert_pending(&mut accept);
    let second = ClientOptions::new(name.as_str())
        .connect(&ThreadPool)
        .unwrap();
    let server = common::block_on(accept).unwrap();
    let OpResult(written, _) = common::block_on(second.write(b"two".to_vec()));
    assert_eq!(written.unwrap(), 3);
    let OpResult(read, got) = common::block_on(server.read(Vec::with_capacity(8)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(3));
    assert_eq!(got, b"two");
    drop((server, second));
}

#[test]
fn closed_peer_is_read_outcome() {
    let name = common::unique_pipe_name("closed_peer_is_read_outcome");
    let (server, client) = connected_pair(&name);
    drop(client);

    let OpResult(read, buf) = common::block_on(server.read(Vec::with_capacity(8)));
    assert_eq!(read.unwrap(), ReadOutcome::ClosedPeer);
    assert!(buf.is_empty());
}

#[test]
fn server_options_cover_limits_and_first_instance() {
    let max_name = common::unique_pipe_name("server_options_max_instances");
    let mut limited = ServerOptions::new(max_name.as_str());
    limited
        .max_instances(1)
        .in_buffer_size(512)
        .out_buffer_size(1024)
        .default_timeout(25);
    let first = limited.create(&ThreadPool).unwrap();
    assert!(matches!(
        limited.create(&ThreadPool),
        Err(SetupError::Busy) | Err(SetupError::Win32(_))
    ));
    drop(first);

    let first_name = common::unique_pipe_name("server_options_first_instance");
    let mut first_only = ServerOptions::new(first_name.as_str());
    first_only.first_instance(true);
    let first = first_only.create(&ThreadPool).unwrap();
    assert!(
        first_only.create(&ThreadPool).is_err(),
        "FILE_FLAG_FIRST_PIPE_INSTANCE must reject a second first instance"
    );
    drop(first);
}

#[test]
fn client_busy_is_reported_without_waiting() {
    let name = common::unique_pipe_name("client_busy_is_reported_without_waiting");
    let mut options = ServerOptions::new(name.as_str());
    options.max_instances(1);
    let server = options.create(&ThreadPool).unwrap();
    let first = ClientOptions::new(name.as_str())
        .connect(&ThreadPool)
        .unwrap();

    let started = Instant::now();
    assert!(matches!(
        ClientOptions::new(name.as_str()).connect(&ThreadPool),
        Err(SetupError::Busy)
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "busy client connect must not wait"
    );

    let absent = common::unique_pipe_name("client_busy_absent_name");
    assert!(matches!(
        ClientOptions::new(absent.as_str()).connect(&ThreadPool),
        Err(SetupError::NotFound)
    ));
    drop((server, first));
}

#[test]
fn byte_mode_is_default_on_both_ends() {
    let name = common::unique_pipe_name("byte_mode_is_default_on_both_ends");
    let (server, client) = connected_pair(&name);

    let OpResult(written, _) = common::block_on(client.write(b"abcdefgh".to_vec()));
    assert_eq!(written.unwrap(), 8);
    let OpResult(read, got) = common::block_on(server.read(Vec::with_capacity(3)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(3));
    assert_eq!(got, b"abc");

    let OpResult(written, _) = common::block_on(server.write(b"12345678".to_vec()));
    assert_eq!(written.unwrap(), 8);
    let OpResult(read, got) = common::block_on(client.read(Vec::with_capacity(4)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(4));
    assert_eq!(got, b"1234");
}

#[test]
fn name_validation_and_bare_name_interop() {
    let too_long = "x".repeat(MAX_NAME_COMPONENT_LEN + 1);
    for invalid in ["", "has\\slash", "has/slash", "nul\0inside", &too_long] {
        assert!(matches!(
            ServerOptions::new(invalid).create(&ThreadPool),
            Err(SetupError::InvalidName)
        ));
        assert!(matches!(
            ClientOptions::new(invalid).connect(&ThreadPool),
            Err(SetupError::InvalidName)
        ));
    }

    let name = common::unique_pipe_name("name_validation_and_bare_name_interop");
    let (server, client) = connected_pair(&name);
    let OpResult(written, _) = common::block_on(client.write(b"bare".to_vec()));
    assert_eq!(written.unwrap(), 4);
    let OpResult(read, got) = common::block_on(server.read(Vec::with_capacity(8)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(4));
    assert_eq!(got, b"bare");
}

#[test]
fn thread_pool_pipe_moves_and_shares_across_threads() {
    let name = common::unique_pipe_name("thread_pool_pipe_moves_and_shares_across_threads");
    let (server, client) = connected_pair(&name);
    let shared = Arc::new(server);
    let reader = Arc::clone(&shared);
    let join = std::thread::spawn(move || {
        let OpResult(read, got) = common::block_on(reader.read(Vec::with_capacity(16)));
        (read.unwrap(), got)
    });

    let OpResult(written, _) = common::block_on(client.write(b"shared".to_vec()));
    assert_eq!(written.unwrap(), 6);
    let (outcome, got) = join.join().unwrap();
    assert_eq!(outcome, ReadOutcome::Bytes(6));
    assert_eq!(got, b"shared");
}

struct RejectRegistration;

impl Registrar for RejectRegistration {
    type Io = ThreadPoolIo;

    fn register(
        &self,
        _handle: windows::Win32::Foundation::HANDLE,
    ) -> Result<Self::Io, RegistrationError> {
        Err(RegistrationError::AlreadyRegistered(
            windows::core::Error::from_hresult(ERROR_INVALID_PARAMETER.to_hresult()),
        ))
    }
}

#[test]
fn registration_failure_after_pipe_create_releases_handle() {
    let name = common::unique_pipe_name("registration_failure_after_pipe_create_releases_handle");
    let mut options = ServerOptions::new(name.as_str());
    options.first_instance(true);

    assert!(matches!(
        options.create(&RejectRegistration),
        Err(SetupError::AlreadyRegistered)
    ));
    let server = options.create(&ThreadPool).unwrap();
    drop(server);
}

#[test]
fn teardown_of_connected_and_unconnected_instances_is_bounded() {
    let _guard = common::serial();
    let baseline = live_operations();

    let unconnected_name = common::unique_pipe_name("teardown_unconnected");
    let unconnected = ServerOptions::new(unconnected_name.as_str())
        .create(&ThreadPool)
        .unwrap();
    let started = Instant::now();
    drop(unconnected);
    assert!(started.elapsed() < Duration::from_secs(1));
    wait_for_baseline(baseline);

    let connected_name = common::unique_pipe_name("teardown_connected");
    let (server, _client) = connected_pair(&connected_name);
    let mut read = Box::pin(server.read(Vec::with_capacity(16)));
    assert_pending(&mut read);

    let started = Instant::now();
    drop(server);
    assert!(started.elapsed() < Duration::from_secs(1));

    let OpResult(result, buffer) = common::block_on(read);
    match result {
        Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => {}
        other => panic!("expected cancellation after owner drop, got {other:?}"),
    }
    assert!(buffer.capacity() >= 16);
    wait_for_baseline(baseline);
}

#[test]
fn caller_driven_pipe_round_trip_is_bounded() {
    let name = common::unique_pipe_name("caller_driven_pipe_round_trip_is_bounded");
    let proactor = Rc::new(Proactor::new().unwrap());
    let server = ServerOptions::new(name.as_str()).create(&proactor).unwrap();
    let mut accept = Box::pin(server.connect());
    assert_pending(&mut accept);
    let client = ClientOptions::new(name.as_str())
        .connect(&proactor)
        .unwrap();
    let server = common::drive_proactor(&proactor, accept).unwrap();

    let OpResult(written, _) = common::drive_proactor(&proactor, client.write(b"hello".to_vec()));
    assert_eq!(written.unwrap(), 5);
    let OpResult(read, got) = common::drive_proactor(&proactor, server.read(Vec::with_capacity(8)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(5));
    assert_eq!(got, b"hello");
}

#[test]
fn access_direction_builders_are_available() {
    let name = common::unique_pipe_name("access_direction_builders_are_available");
    let mut server_options = ServerOptions::new(name.as_str());
    server_options.access(AccessDirection::Outbound);
    let server = server_options.create(&ThreadPool).unwrap();

    let mut client_options = ClientOptions::new(name.as_str());
    client_options.access(AccessDirection::Inbound);
    let client = client_options.connect(&ThreadPool).unwrap();
    let server = common::block_on(server.connect()).unwrap();

    let OpResult(written, _) = common::block_on(server.write(b"direction".to_vec()));
    assert_eq!(written.unwrap(), 9);
    let OpResult(read, got) = common::block_on(client.read(Vec::with_capacity(16)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(9));
    assert_eq!(got, b"direction");
    drop((server, client));
}

#[test]
fn dropping_a_connect_future_tears_the_instance_down_in_order() {
    // `connect(self)` parks the instance's state inside the returned future.
    // Dropping that future before a client arrives must still run the
    // documented teardown -- cancel, drop the submitter, release the handle
    // reference last -- rather than letting the fields drop implicitly, which
    // would skip the cancellation and free the handle in declaration order.
    //
    // Observed through the pipe name rather than the operation counter: the
    // counter is process-global and other tests in this binary run in parallel,
    // but the name can only become free again once the handle is genuinely
    // closed, which cannot happen while the accept is still outstanding.
    let name = common::unique_pipe_name("dropping_a_connect_future");
    let server = ServerOptions::new(&name)
        .first_instance(true)
        .create(&ThreadPool)
        .expect("create server instance");

    let mut accept = Box::pin(server.connect());

    // No client will ever arrive, so the accept is genuinely outstanding.
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(accept.as_mut().poll(&mut cx), Poll::Pending),
        "the accept must be pending before the drop is exercised"
    );

    // While it is outstanding the name is taken: a first-instance create must
    // fail, which is what makes the success after the drop meaningful.
    assert!(
        ServerOptions::new(&name)
            .first_instance(true)
            .create(&ThreadPool)
            .is_err(),
        "the name must be held while the instance is alive"
    );

    drop(accept);

    // The cancellation the guard issues is what releases the handle; without it
    // the accept would stay outstanding and the name held until process exit.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if ServerOptions::new(&name)
            .first_instance(true)
            .create(&ThreadPool)
            .is_ok()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "dropping the accept future must cancel and close the handle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
