// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

//! Phase 6 allocation budgets for file and named-pipe I/O.
//!
//! The allocator is thread-scoped for the same reason as `httpsys_alloc`: pipe
//! peer threads are free to allocate while this thread measures one operation.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use winasio::fs::{File, OpenOptions, ReadOutcome};
use winasio::io::TransferResult;
use winasio::iocp::{OpResult, Proactor, Registrar, ThreadPool, ThreadPoolIo};
use winasio::pipe::{ClientOptions, NamedPipe, PipeMode, ServerOptions};

thread_local! {
    // `const` initialisers do not allocate, which matters inside an allocator.
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: delegation preserves the global allocator contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: delegation preserves the global allocator contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: delegation preserves the global allocator contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: delegation preserves the global allocator contract.
        unsafe { System.alloc_zeroed(layout) }
    }
}

fn note() {
    let armed = ARMED.try_with(|a| a.get()).unwrap_or(false);
    if armed {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let value = f();
    ARMED.with(|a| a.set(false));
    (value, COUNT.with(|c| c.get()))
}

static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

const CAPACITIES: [usize; 3] = [64, 4096, 1024 * 1024];

#[derive(Debug, Clone, Copy)]
enum TransferCase {
    Empty,
    Partial,
    Filling,
}

impl TransferCase {
    fn all() -> [Self; 3] {
        [Self::Empty, Self::Partial, Self::Filling]
    }

    fn name(self) -> &'static str {
        match self {
            TransferCase::Empty => "empty",
            TransferCase::Partial => "partial",
            TransferCase::Filling => "filling",
        }
    }

    fn len(self, capacity: usize) -> usize {
        match self {
            TransferCase::Empty => 0,
            TransferCase::Partial => capacity / 2,
            TransferCase::Filling => capacity,
        }
    }
}

fn artifact_path(name: &str) -> PathBuf {
    let n = NEXT_FILE.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::current_dir().unwrap().join("target");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!(
        "winasio-fs-alloc-{}-{name}-{n}.tmp",
        std::process::id()
    ))
}

fn patterned_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn buffer_with_capacity_and_len(capacity: usize, len: usize) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(capacity);
    buffer.extend((0..len).map(|i| (i % 251) as u8));
    buffer
}

fn open_read<R: Registrar>(registrar: &R, path: &PathBuf) -> File<R::Io> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.open(registrar, path).unwrap()
}

fn open_read_write<R: Registrar>(registrar: &R, path: &PathBuf) -> File<R::Io> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    options.open(registrar, path).unwrap()
}

fn drive_proactor<F: Future>(proactor: &Proactor, fut: F) -> F::Output {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out driving caller-driven operation"
        );
        proactor.poll(Some(Duration::from_millis(2))).unwrap();
    }
}

fn drive_two<F, G>(proactor: &Proactor, left: F, right: G) -> (F::Output, G::Output)
where
    F: Future,
    G: Future,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut left = std::pin::pin!(left);
    let mut right = std::pin::pin!(right);
    let mut left_out = None;
    let mut right_out = None;
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if left_out.is_none() {
            if let Poll::Ready(value) = left.as_mut().poll(&mut cx) {
                left_out = Some(value);
            }
        }
        if right_out.is_none() {
            if let Poll::Ready(value) = right.as_mut().poll(&mut cx) {
                right_out = Some(value);
            }
        }
        if left_out.is_some() && right_out.is_some() {
            return (left_out.take().unwrap(), right_out.take().unwrap());
        }
        assert!(
            Instant::now() < deadline,
            "timed out driving caller-driven operation pair"
        );
        proactor.poll(Some(Duration::from_millis(2))).unwrap();
    }
}

fn connected_thread_pool_pipe(name: &str) -> (NamedPipe<ThreadPoolIo>, NamedPipe<ThreadPoolIo>) {
    let mut server_options = ServerOptions::new(name);
    server_options.pipe_type(PipeMode::Message);
    let server = server_options.create(&ThreadPool).unwrap();
    let accept = server.connect();

    let mut client_options = ClientOptions::new(name);
    client_options.read_mode(PipeMode::Message);
    let client = client_options.connect(&ThreadPool).unwrap();
    let server = common::block_on(accept).unwrap();
    (server, client)
}

fn connected_proactor_pipe(
    proactor: &Rc<Proactor>,
    name: &str,
) -> (NamedPipe<Rc<Proactor>>, NamedPipe<Rc<Proactor>>) {
    let mut server_options = ServerOptions::new(name);
    server_options.pipe_type(PipeMode::Message);
    let server = server_options.create(proactor).unwrap();
    let accept = server.connect();

    let mut client_options = ClientOptions::new(name);
    client_options.read_mode(PipeMode::Message);
    let client = client_options.connect(proactor).unwrap();
    let server = drive_proactor(proactor, accept).unwrap();
    (server, client)
}

fn assert_thread_pool_budget(operation: &str, capacity: usize, case: TransferCase, count: usize) {
    println!(
        "alloc_count backend=thread_pool op={operation} capacity={capacity} transfer={} count={count}",
        case.name()
    );
    assert_eq!(
        count,
        1,
        "{operation} with capacity {capacity} and {} transfer allocated {count}; \
         NFR-003/SC-045 budget is exactly one operation record. The shared \
         Handle holder must not add anything: cloning it is only a refcount bump.",
        case.name()
    );
}

fn assert_caller_budget(operation: &str, capacity: usize, case: TransferCase, count: usize) {
    println!(
        "alloc_count backend=caller_driven op={operation} capacity={capacity} transfer={} count={count}",
        case.name()
    );
    assert!(
        count <= 2,
        "{operation} with capacity {capacity} and {} transfer allocated {count}; \
         SC-046 allows at most the operation record plus one amortised pending-bookkeeping insertion",
        case.name()
    );
}

fn measure_thread_pool_file_read(capacity: usize, case: TransferCase) -> usize {
    let transfer = case.len(capacity);
    let path = artifact_path("thread-pool-file-read");
    let payload = patterned_payload(transfer);
    std::fs::write(&path, &payload).unwrap();
    let file = open_read(&ThreadPool, &path);
    let buffer = Vec::with_capacity(capacity);

    let (OpResult(result, got), allocations) =
        measure(|| common::block_on(file.read_at(0, buffer)));
    if transfer == 0 {
        assert!(matches!(result.unwrap(), ReadOutcome::Eof));
        assert!(got.is_empty());
    } else {
        assert_eq!(result.unwrap(), ReadOutcome::Bytes(transfer));
        assert_eq!(got, payload);
    }
    drop(file);
    std::fs::remove_file(path).unwrap();
    allocations
}

fn measure_thread_pool_file_write(capacity: usize, case: TransferCase) -> usize {
    let transfer = case.len(capacity);
    let path = artifact_path("thread-pool-file-write");
    let file = open_read_write(&ThreadPool, &path);
    let payload = buffer_with_capacity_and_len(capacity, transfer);

    let (OpResult(result, returned), allocations) =
        measure(|| common::block_on(file.write_at(0, payload)));
    assert_eq!(result.unwrap(), transfer);
    assert_eq!(returned.len(), transfer);
    assert_eq!(returned.capacity(), capacity);
    drop(file);
    std::fs::remove_file(path).unwrap();
    allocations
}

fn measure_thread_pool_pipe_read(capacity: usize, case: TransferCase) -> usize {
    let transfer = case.len(capacity);
    let name = common::unique_pipe_name("alloc_thread_pool_pipe_read");
    let (server, client) = connected_thread_pool_pipe(&name);
    let buffer = Vec::with_capacity(capacity);

    if transfer == 0 {
        drop(server);
        let (OpResult(result, got), allocations) =
            measure(|| common::block_on(client.read(buffer)));
        assert_eq!(result.unwrap(), ReadOutcome::ClosedPeer);
        assert!(got.is_empty());
        allocations
    } else {
        let payload = patterned_payload(transfer);
        let expected = payload.clone();
        let writer = std::thread::spawn(move || {
            let OpResult(result, returned) = common::block_on(server.write(payload));
            assert_eq!(result.unwrap(), returned.len());
            returned
        });

        let (OpResult(result, got), allocations) =
            measure(|| common::block_on(client.read(buffer)));
        assert_eq!(result.unwrap(), ReadOutcome::Bytes(transfer));
        assert_eq!(got, expected);
        assert_eq!(writer.join().unwrap(), expected);
        allocations
    }
}

fn measure_thread_pool_pipe_write(capacity: usize, case: TransferCase) -> usize {
    let transfer = case.len(capacity);
    let name = common::unique_pipe_name("alloc_thread_pool_pipe_write");
    let (server, client) = connected_thread_pool_pipe(&name);
    let payload = buffer_with_capacity_and_len(capacity, transfer);

    if transfer == 0 {
        let (OpResult(result, returned), allocations) =
            measure(|| common::block_on(server.write(payload)));
        assert_eq!(result.unwrap(), 0);
        assert!(returned.is_empty());
        allocations
    } else {
        let reader = std::thread::spawn(move || {
            let OpResult(result, got) = common::block_on(client.read(Vec::with_capacity(capacity)));
            assert_eq!(result.unwrap(), ReadOutcome::Bytes(transfer));
            got
        });
        let (OpResult(result, returned), allocations) =
            measure(|| common::block_on(server.write(payload)));
        assert_eq!(result.unwrap(), transfer);
        assert_eq!(returned.len(), transfer);
        assert_eq!(reader.join().unwrap(), returned);
        allocations
    }
}

fn measure_caller_file_read(capacity: usize, case: TransferCase) -> usize {
    let transfer = case.len(capacity);
    let path = artifact_path("caller-file-read");
    let payload = patterned_payload(transfer);
    std::fs::write(&path, &payload).unwrap();
    let proactor = Rc::new(Proactor::new().unwrap());
    let file = open_read(&proactor, &path);
    let buffer = Vec::with_capacity(capacity);

    let (OpResult(result, got), allocations) =
        measure(|| drive_proactor(&proactor, file.read_at(0, buffer)));
    if transfer == 0 {
        assert!(matches!(result.unwrap(), ReadOutcome::Eof));
        assert!(got.is_empty());
    } else {
        assert_eq!(result.unwrap(), ReadOutcome::Bytes(transfer));
        assert_eq!(got, payload);
    }
    drop(file);
    std::fs::remove_file(path).unwrap();
    allocations
}

fn measure_caller_file_write(capacity: usize, case: TransferCase) -> usize {
    let transfer = case.len(capacity);
    let path = artifact_path("caller-file-write");
    let proactor = Rc::new(Proactor::new().unwrap());
    let file = open_read_write(&proactor, &path);
    let payload = buffer_with_capacity_and_len(capacity, transfer);

    let (OpResult(result, returned), allocations) =
        measure(|| drive_proactor(&proactor, file.write_at(0, payload)));
    assert_eq!(result.unwrap(), transfer);
    assert_eq!(returned.len(), transfer);
    assert_eq!(returned.capacity(), capacity);
    drop(file);
    std::fs::remove_file(path).unwrap();
    allocations
}

fn measure_caller_pipe_read(capacity: usize, case: TransferCase) -> usize {
    let transfer = case.len(capacity);
    let proactor = Rc::new(Proactor::new().unwrap());
    let name = common::unique_pipe_name("alloc_caller_pipe_read");
    let (server, client) = connected_proactor_pipe(&proactor, &name);
    let buffer = Vec::with_capacity(capacity);

    if transfer == 0 {
        drop(server);
        let (OpResult(result, got), allocations) =
            measure(|| drive_proactor(&proactor, client.read(buffer)));
        assert_eq!(result.unwrap(), ReadOutcome::ClosedPeer);
        assert!(got.is_empty());
        allocations
    } else {
        let payload = patterned_payload(transfer);
        let expected = payload.clone();
        let writer = server.write(payload);
        let ((OpResult(result, got), OpResult(written, returned)), allocations) =
            measure(|| drive_two(&proactor, client.read(buffer), writer));
        assert_eq!(result.unwrap(), ReadOutcome::Bytes(transfer));
        assert_eq!(got, expected);
        assert_eq!(written.unwrap(), transfer);
        assert_eq!(returned, expected);
        allocations
    }
}

fn measure_caller_pipe_write(capacity: usize, case: TransferCase) -> usize {
    let transfer = case.len(capacity);
    let proactor = Rc::new(Proactor::new().unwrap());
    let name = common::unique_pipe_name("alloc_caller_pipe_write");
    let (server, client) = connected_proactor_pipe(&proactor, &name);
    let payload = buffer_with_capacity_and_len(capacity, transfer);

    if transfer == 0 {
        let (OpResult(result, returned), allocations) =
            measure(|| drive_proactor(&proactor, server.write(payload)));
        assert_eq!(result.unwrap(), 0);
        assert!(returned.is_empty());
        allocations
    } else {
        let reader = client.read(Vec::with_capacity(capacity));
        let ((OpResult(result, returned), OpResult(read, got)), allocations) =
            measure(|| drive_two(&proactor, server.write(payload), reader));
        assert_eq!(result.unwrap(), transfer);
        assert_eq!(returned.len(), transfer);
        assert_eq!(read.unwrap(), ReadOutcome::Bytes(transfer));
        assert_eq!(got, returned);
        allocations
    }
}

fn warm_cold_machinery() {
    let path = artifact_path("warm-thread-pool");
    std::fs::write(&path, b"warm").unwrap();
    let file = open_read(&ThreadPool, &path);
    let OpResult(result, _) = common::block_on(file.read_at(0, Vec::with_capacity(8)));
    assert_eq!(result.unwrap(), ReadOutcome::Bytes(4));
    drop(file);
    std::fs::remove_file(path).unwrap();

    let path = artifact_path("warm-thread-pool-write");
    let file = open_read_write(&ThreadPool, &path);
    let OpResult(result, _) = common::block_on(file.write_at(0, b"warm".to_vec()));
    assert_eq!(result.unwrap(), 4);
    drop(file);
    std::fs::remove_file(path).unwrap();

    let name = common::unique_pipe_name("alloc_warm_thread_pool_pipe");
    let (server, client) = connected_thread_pool_pipe(&name);
    let writer = std::thread::spawn(move || common::block_on(server.write(b"warm".to_vec())));
    let OpResult(result, _) = common::block_on(client.read(Vec::with_capacity(8)));
    assert_eq!(result.unwrap(), ReadOutcome::Bytes(4));
    assert_eq!(writer.join().unwrap().0.unwrap(), 4);

    let path = artifact_path("warm-caller");
    std::fs::write(&path, b"warm").unwrap();
    let proactor = Rc::new(Proactor::new().unwrap());
    let file = open_read(&proactor, &path);
    let OpResult(result, _) = drive_proactor(&proactor, file.read_at(0, Vec::with_capacity(8)));
    assert_eq!(result.unwrap(), ReadOutcome::Bytes(4));
    drop(file);
    std::fs::remove_file(path).unwrap();

    let path = artifact_path("warm-caller-write");
    let file = open_read_write(&proactor, &path);
    let OpResult(result, _) = drive_proactor(&proactor, file.write_at(0, b"warm".to_vec()));
    assert_eq!(result.unwrap(), 4);
    drop(file);
    std::fs::remove_file(path).unwrap();

    let name = common::unique_pipe_name("alloc_warm_caller_pipe");
    let (server, client) = connected_proactor_pipe(&proactor, &name);
    let (OpResult(read, _), OpResult(written, _)) = drive_two(
        &proactor,
        client.read(Vec::with_capacity(8)),
        server.write(b"warm".to_vec()),
    );
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(4));
    assert_eq!(written.unwrap(), 4);
}

#[test]
fn thread_pool_single_operations_allocate_exactly_once() {
    let _guard = common::serial();
    warm_cold_machinery();

    for capacity in CAPACITIES {
        for case in TransferCase::all() {
            assert_thread_pool_budget(
                "file_read",
                capacity,
                case,
                measure_thread_pool_file_read(capacity, case),
            );
            assert_thread_pool_budget(
                "file_write",
                capacity,
                case,
                measure_thread_pool_file_write(capacity, case),
            );
            assert_thread_pool_budget(
                "pipe_read",
                capacity,
                case,
                measure_thread_pool_pipe_read(capacity, case),
            );
            assert_thread_pool_budget(
                "pipe_write",
                capacity,
                case,
                measure_thread_pool_pipe_write(capacity, case),
            );
        }
    }
}

#[test]
fn caller_driven_single_operations_allocate_at_most_two() {
    let _guard = common::serial();
    warm_cold_machinery();

    for capacity in CAPACITIES {
        for case in TransferCase::all() {
            assert_caller_budget(
                "file_read",
                capacity,
                case,
                measure_caller_file_read(capacity, case),
            );
            assert_caller_budget(
                "file_write",
                capacity,
                case,
                measure_caller_file_write(capacity, case),
            );
            assert_caller_budget(
                "pipe_read",
                capacity,
                case,
                measure_caller_pipe_read(capacity, case),
            );
            assert_caller_budget(
                "pipe_write",
                capacity,
                case,
                measure_caller_pipe_write(capacity, case),
            );
        }
    }
}

#[test]
fn read_to_end_counts_only_operations_and_accumulator_growth() {
    let _guard = common::serial();
    warm_cold_machinery();

    let payload = patterned_payload(4097);
    let path = artifact_path("read-to-end");
    std::fs::write(&path, &payload).unwrap();
    let file = open_read(&ThreadPool, &path);

    let warm = common::block_on(file.read_to_end(0, Vec::new()));
    assert_read_to_end_success(warm, &payload);

    let (result, allocations) = measure(|| common::block_on(file.read_to_end(0, Vec::new())));
    assert_read_to_end_success(result, &payload);

    let expected_operations = 3;
    let expected_growth_reallocations = 2;
    let expected = expected_operations + expected_growth_reallocations;
    println!(
        "alloc_count backend=thread_pool op=read_to_end payload=4097 operations={expected_operations} growth_reallocations={expected_growth_reallocations} count={allocations}"
    );
    assert_eq!(
        allocations, expected,
        "read_to_end allocated {allocations}; expected {expected_operations} operation records plus \
         {expected_growth_reallocations} accumulator growth reallocations and no more"
    );

    drop(file);
    std::fs::remove_file(path).unwrap();
}

fn assert_read_to_end_success(result: TransferResult<Vec<u8>>, payload: &[u8]) {
    assert!(result.result.is_ok(), "{:?}", result.result);
    assert_eq!(result.transferred, payload.len());
    assert_eq!(result.buffer, payload);
}
