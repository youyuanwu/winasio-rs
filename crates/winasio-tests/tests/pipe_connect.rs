// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

mod common;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use winasio::fs::OpenOptions;
use winasio::iocp::{OpResult, Proactor, Registrar, RegistrationError, ThreadPool, ThreadPoolIo};
use winasio::pipe::{
    AccessDirection, ClientOptions, NamedPipe, PipeMode, ReadOutcome, ServerOptions, SetupError,
};
use windows::Win32::Foundation::ERROR_INVALID_PARAMETER;

static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

type AcceptFuture = Pin<Box<dyn Future<Output = windows::core::Result<NamedPipe<ThreadPoolIo>>>>>;

fn assert_pending<F: Future + ?Sized>(future: &mut Pin<Box<F>>) {
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
        "winasio-pipe-connect-{}-{name}-{n}.tmp",
        std::process::id()
    ))
}

fn connected_pair(name: &str) -> (NamedPipe<ThreadPoolIo>, NamedPipe<ThreadPoolIo>) {
    let server = ServerOptions::new(name).create(&ThreadPool).unwrap();
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

#[test]
fn caller_driven_retry_connects_after_holder_releases() {
    let name = common::unique_pipe_name("caller_driven_retry_connects_after_holder_releases");
    let mut options = ServerOptions::new(&name);
    options.max_instances(1);
    let server = options.create(&ThreadPool).unwrap();
    let first = ClientOptions::new(&name).connect(&ThreadPool).unwrap();
    let server = common::block_on(server.connect()).unwrap();

    let mut first = Some(first);
    let mut occupied = Some(server);
    let mut accept: Option<AcceptFuture> = None;
    let mut saw_busy = false;
    let deadline = Instant::now() + Duration::from_secs(5);

    let second = loop {
        match ClientOptions::new(&name).connect(&ThreadPool) {
            Ok(client) => break client,
            Err(SetupError::Busy) => {
                saw_busy = true;
                if let Some(first) = first.take() {
                    drop(first);
                    let server = occupied.take().unwrap();
                    let mut pending = Box::pin(server.disconnect().unwrap().connect());
                    assert_pending(&mut pending);
                    accept = Some(pending);
                }
                assert!(
                    Instant::now() < deadline,
                    "caller-driven retry should connect after holder release"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("expected busy or success while retrying, got {e:?}"),
        }
    };

    assert!(
        saw_busy,
        "the retry loop must observe the busy category first"
    );
    let server = common::block_on(accept.take().unwrap()).unwrap();
    drop((server, second));
}

#[test]
fn busy_connect_is_distinct_from_not_found() {
    let name = common::unique_pipe_name("busy_connect_is_distinct_from_not_found");
    let mut options = ServerOptions::new(&name);
    options.max_instances(1);
    let server = options.create(&ThreadPool).unwrap();
    let first = ClientOptions::new(&name).connect(&ThreadPool).unwrap();

    assert!(matches!(
        ClientOptions::new(&name).connect(&ThreadPool),
        Err(SetupError::Busy)
    ));

    let absent = common::unique_pipe_name("busy_connect_absent");
    assert!(matches!(
        ClientOptions::new(&absent).connect(&ThreadPool),
        Err(SetupError::NotFound)
    ));
    drop((server, first));
}

#[test]
fn access_denied_direction_mismatch_is_distinct() {
    let name = common::unique_pipe_name("access_denied_direction_mismatch_is_distinct");
    let mut server_options = ServerOptions::new(&name);
    server_options.access(AccessDirection::Inbound);
    let server = server_options.create(&ThreadPool).unwrap();
    let mut client_options = ClientOptions::new(&name);
    client_options.access(AccessDirection::Inbound);
    assert!(matches!(
        client_options.connect(&ThreadPool),
        Err(SetupError::AccessDenied)
    ));
    drop(server);

    let mut busy_options = ServerOptions::new(&name);
    busy_options.max_instances(1);
    let busy_server = busy_options.create(&ThreadPool).unwrap();
    let busy_holder = ClientOptions::new(&name).connect(&ThreadPool).unwrap();
    assert!(matches!(
        ClientOptions::new(&name).connect(&ThreadPool),
        Err(SetupError::Busy)
    ));
    drop((busy_server, busy_holder));

    let absent = common::unique_pipe_name("access_denied_absent");
    assert!(matches!(
        ClientOptions::new(&absent).connect(&ThreadPool),
        Err(SetupError::NotFound)
    ));
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
fn setup_categories_include_already_registered_and_invalid_name() {
    let invalid_too_long = "x".repeat(winasio::pipe::MAX_NAME_COMPONENT_LEN + 1);
    for invalid in [
        "",
        "has\\slash",
        "has/slash",
        "nul\0inside",
        &invalid_too_long,
    ] {
        assert!(matches!(
            ServerOptions::new(invalid).create(&ThreadPool),
            Err(SetupError::InvalidName)
        ));
        assert!(matches!(
            ClientOptions::new(invalid).connect(&ThreadPool),
            Err(SetupError::InvalidName)
        ));
    }

    let name = common::unique_pipe_name("setup_categories_already_registered_server");
    let mut options = ServerOptions::new(&name);
    options.first_instance(true);
    assert!(matches!(
        options.create(&RejectRegistration),
        Err(SetupError::AlreadyRegistered)
    ));
    let server = options.create(&ThreadPool).unwrap();
    drop(server);

    let name = common::unique_pipe_name("setup_categories_already_registered_client");
    let server = ServerOptions::new(&name).create(&ThreadPool).unwrap();
    assert!(matches!(
        ClientOptions::new(&name).connect(&RejectRegistration),
        Err(SetupError::AlreadyRegistered)
    ));
    drop(server);
}

#[test]
fn every_read_outcome_variant_is_observed_without_platform_codes() {
    let byte_name = common::unique_pipe_name("read_outcome_bytes");
    let (server, client) = connected_pair(&byte_name);
    let OpResult(written, _) = common::block_on(client.write(b"bytes".to_vec()));
    assert_eq!(written.unwrap(), 5);
    let OpResult(bytes, _) = common::block_on(server.read(Vec::with_capacity(16)));

    let message_name = common::unique_pipe_name("read_outcome_more_data");
    let (server, client) = message_pair(&message_name);
    let OpResult(written, _) = common::block_on(server.write(b"message".to_vec()));
    assert_eq!(written.unwrap(), 7);
    let OpResult(more_data, _) = common::block_on(client.read(Vec::with_capacity(3)));

    let closed_name = common::unique_pipe_name("read_outcome_closed_peer");
    let (server, client) = connected_pair(&closed_name);
    drop(client);
    let OpResult(closed, _) = common::block_on(server.read(Vec::with_capacity(8)));

    let path = artifact_path("read-outcome-eof");
    std::fs::write(&path, b"").unwrap();
    let mut options = OpenOptions::new();
    options.read(true);
    let file = options.open(&ThreadPool, &path).unwrap();
    let OpResult(eof, _) = common::block_on(file.read_at(0, Vec::with_capacity(8)));
    drop(file);
    let _ = std::fs::remove_file(path);

    let mut saw_bytes = false;
    let mut saw_eof = false;
    let mut saw_closed = false;
    let mut saw_more_data = false;
    for outcome in [
        bytes.unwrap(),
        more_data.unwrap(),
        closed.unwrap(),
        eof.unwrap(),
    ] {
        match outcome {
            ReadOutcome::Bytes(_) => saw_bytes = true,
            ReadOutcome::Eof => saw_eof = true,
            ReadOutcome::ClosedPeer => saw_closed = true,
            ReadOutcome::MoreData(_) => saw_more_data = true,
        }
    }

    assert!(saw_bytes);
    assert!(saw_eof);
    assert!(saw_closed);
    assert!(saw_more_data);
}

enum RaceResult {
    Connected(NamedPipe<ThreadPoolIo>),
    Busy,
    Other(String),
}

#[test]
fn two_clients_racing_for_one_instance_have_one_winner() {
    let name = common::unique_pipe_name("two_clients_racing_for_one_instance_have_one_winner");
    let mut options = ServerOptions::new(&name);
    options.max_instances(1);
    let server = options.create(&ThreadPool).unwrap();

    let barrier = Arc::new(Barrier::new(3));
    let mut joins = Vec::new();
    for _ in 0..2 {
        let name = name.clone();
        let barrier = Arc::clone(&barrier);
        joins.push(std::thread::spawn(move || {
            barrier.wait();
            match ClientOptions::new(&name).connect(&ThreadPool) {
                Ok(client) => RaceResult::Connected(client),
                Err(SetupError::Busy) => RaceResult::Busy,
                Err(e) => RaceResult::Other(format!("{e:?}")),
            }
        }));
    }

    barrier.wait();
    let mut connected = Vec::new();
    let mut busy = 0;
    for join in joins {
        match join.join().unwrap() {
            RaceResult::Connected(client) => connected.push(client),
            RaceResult::Busy => busy += 1,
            RaceResult::Other(e) => panic!("unexpected racing result: {e}"),
        }
    }

    assert_eq!(connected.len(), 1);
    assert_eq!(busy, 1);
    let server = common::block_on(server.connect()).unwrap();
    drop((server, connected));
}

fn assert_busy_is_prompt<R: Registrar>(registrar: &R, name: &str) {
    let mut options = ServerOptions::new(name);
    options.max_instances(1);
    let server = options.create(registrar).unwrap();
    let first = ClientOptions::new(name).connect(registrar).unwrap();

    let started = Instant::now();
    assert!(matches!(
        ClientOptions::new(name).connect(registrar),
        Err(SetupError::Busy)
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "busy connect must report promptly"
    );
    drop((server, first));
}

#[test]
fn busy_connect_is_bounded_on_both_backends_and_has_no_wait_call() {
    let pool_name = common::unique_pipe_name("busy_connect_bounded_thread_pool");
    assert_busy_is_prompt(&ThreadPool, &pool_name);

    let proactor = Rc::new(Proactor::new().unwrap());
    let proactor_name = common::unique_pipe_name("busy_connect_bounded_proactor");
    assert_busy_is_prompt(&proactor, &proactor_name);

    // The connect path must contain no synchronous wait. Scanning for a bare
    // mention would be wrong in both directions: it would miss a call in any
    // file other than the client, and it would trip over the module docs, which
    // deliberately *name* `WaitNamedPipeW` to explain why the crate refuses to
    // use it. Scan the whole library for actual use -- an import or a call --
    // and separately require the rationale to still be documented.
    const SOURCES: [(&str, &str); 5] = [
        (
            "pipe/client.rs",
            include_str!(r"..\..\winasio\src\pipe\client.rs"),
        ),
        (
            "pipe/server.rs",
            include_str!(r"..\..\winasio\src\pipe\server.rs"),
        ),
        (
            "pipe/connected.rs",
            include_str!(r"..\..\winasio\src\pipe\connected.rs"),
        ),
        (
            "pipe/name.rs",
            include_str!(r"..\..\winasio\src\pipe\name.rs"),
        ),
        (
            "iocp/ops/stream.rs",
            include_str!(r"..\..\winasio\src\iocp\ops\stream.rs"),
        ),
    ];
    for (name, source) in SOURCES {
        for line in source.lines() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue; // prose, not use
            }
            assert!(
                !code.contains("WaitNamedPipe"),
                "{name} uses a synchronous pipe wait; connecting must never block"
            );
        }
    }

    let module_docs = include_str!(r"..\..\winasio\src\pipe\mod.rs");
    assert!(
        module_docs.contains("retry with their own runtime's timer"),
        "the caller-driven retry pattern must stay documented"
    );
}
