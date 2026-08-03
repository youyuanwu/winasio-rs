// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

mod common;

use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::IntoRawHandle;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use winasio::fs::{File, OpenOptions, ReadOutcome, SetupError};
use winasio::iocp::{
    OpResult, Proactor, Registrar, RegistrationError, Submitter, ThreadPool, ThreadPoolIo, WriteAt,
};
use windows::Win32::Foundation::ERROR_INVALID_PARAMETER;
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn temp_path(name: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("winasio-fs-{}-{name}-{n}.tmp", std::process::id()))
}

fn create_contents(path: &PathBuf, contents: &[u8]) {
    std::fs::write(path, contents).unwrap();
}

fn open_read_write<R: Registrar>(registrar: &R, path: &PathBuf) -> File<R::Io> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    options.open(registrar, path).unwrap()
}

#[test]
fn round_trip_thread_pool() {
    let path = temp_path("round-trip-pool");
    let file = open_read_write(&ThreadPool, &path);

    let payload = b"hello async file".repeat(64);
    let OpResult(written, returned) = common::block_on(file.write_at(128, payload.clone()));
    assert_eq!(written.unwrap(), payload.len());
    assert_eq!(returned, payload);

    let OpResult(read, got) =
        common::block_on(file.read_at(128, Vec::with_capacity(payload.len() + 16)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(payload.len()));
    assert_eq!(got, payload);

    drop(file);
    let _ = std::fs::remove_file(path);
}

#[test]
fn round_trip_caller_driven() {
    let path = temp_path("round-trip-proactor");
    let proactor = Rc::new(Proactor::new().unwrap());
    let file = open_read_write(&proactor, &path);

    let payload = b"caller driven".repeat(80);
    let OpResult(written, returned) = proactor.block_on(file.write_at(32, payload.clone()));
    assert_eq!(written.unwrap(), payload.len());
    assert_eq!(returned, payload);

    let OpResult(read, got) =
        proactor.block_on(file.read_at(32, Vec::with_capacity(payload.len())));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(payload.len()));
    assert_eq!(got, payload);

    drop(file);
    let _ = std::fs::remove_file(path);
}

#[test]
fn open_options_create_new_truncate_and_directory_errors() {
    let path = temp_path("options");
    create_contents(&path, b"not empty");

    let mut create_new = OpenOptions::new();
    create_new.read(true).write(true).create_new(true);
    assert!(create_new.open(&ThreadPool, &path).is_err());

    let mut truncate = OpenOptions::new();
    truncate.read(true).write(true).truncate(true);
    let file = truncate.open(&ThreadPool, &path).unwrap();
    let OpResult(read, buf) = common::block_on(file.read_at(0, Vec::with_capacity(8)));
    assert_eq!(read.unwrap(), ReadOutcome::Eof);
    assert!(buf.is_empty());
    drop(file);

    let dir = temp_path("dir");
    std::fs::create_dir(&dir).unwrap();
    let mut options = OpenOptions::new();
    options.read(true);
    assert!(options.open(&ThreadPool, &dir).is_err());

    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(dir);
}

#[test]
fn eof_zero_byte_and_failed_write_return_buffers() {
    let path = temp_path("eof");
    create_contents(&path, b"abc");

    let mut read_only = OpenOptions::new();
    read_only
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    let file = read_only.open(&ThreadPool, &path).unwrap();

    let OpResult(eof, buf) = common::block_on(file.read_at(3, Vec::with_capacity(16)));
    assert_eq!(eof.unwrap(), ReadOutcome::Eof);
    assert!(buf.is_empty());

    let OpResult(zero, buf) = common::block_on(file.read_at(0, Vec::with_capacity(0)));
    assert_eq!(zero.unwrap(), ReadOutcome::Bytes(0));
    assert_eq!(buf.capacity(), 0);

    let payload = b"cannot write".to_vec();
    let OpResult(write, returned) = common::block_on(file.write_at(0, payload.clone()));
    assert!(write.is_err());
    assert_eq!(returned, payload);

    drop(file);
    let _ = std::fs::remove_file(path);
}

#[test]
fn concurrent_reads_use_shared_reference() {
    let path = temp_path("concurrent");
    let left = b"left side".repeat(20);
    let right = b"right side".repeat(20);
    let mut all = left.clone();
    all.extend_from_slice(&right);
    create_contents(&path, &all);

    let mut options = OpenOptions::new();
    options.read(true);
    let file = options.open(&ThreadPool, &path).unwrap();

    let first = file.read_at(0, Vec::with_capacity(left.len()));
    let second = file.read_at(left.len() as u64, Vec::with_capacity(right.len()));

    let OpResult(first_result, first_buf) = common::block_on(first);
    let OpResult(second_result, second_buf) = common::block_on(second);
    assert_eq!(first_result.unwrap(), ReadOutcome::Bytes(left.len()));
    assert_eq!(second_result.unwrap(), ReadOutcome::Bytes(right.len()));
    assert_eq!(first_buf, left);
    assert_eq!(second_buf, right);

    drop(file);
    let _ = std::fs::remove_file(path);
}

#[test]
fn borrowed_handle_works_with_low_level_operation() {
    let path = temp_path("handle");
    let proactor = Rc::new(Proactor::new().unwrap());
    let file = open_read_write(&proactor, &path);

    let payload = b"low level".to_vec();
    let written = proactor.block_on(proactor.submit(WriteAt::new(file.handle(), 0, payload)));
    assert_eq!(written.0.unwrap(), b"low level".len());

    let OpResult(read, got) = proactor.block_on(file.read_at(0, Vec::with_capacity(32)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(b"low level".len()));
    assert_eq!(got, b"low level");

    drop(file);
    let _ = std::fs::remove_file(path);
}

#[test]
fn from_handle_adopts_and_closes() {
    let path = temp_path("adopt");
    let std_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(FILE_FLAG_OVERLAPPED.0)
        .open(&path)
        .unwrap();
    let raw = std_file.into_raw_handle();

    // SAFETY: the standard file was opened with `FILE_FLAG_OVERLAPPED`,
    // `into_raw_handle` transfers unique ownership, and it has not been
    // registered or used for overlapped I/O yet.
    let file = unsafe {
        File::<ThreadPoolIo>::from_handle(&ThreadPool, windows::Win32::Foundation::HANDLE(raw))
    }
    .unwrap();
    let OpResult(written, _) = common::block_on(file.write_at(0, b"adopted".to_vec()));
    assert_eq!(written.unwrap(), 7);
    drop(file);

    std::fs::remove_file(path).unwrap();
}

#[test]
fn thread_pool_file_moves_and_shares_across_threads() {
    let path = temp_path("send-sync");
    create_contents(&path, b"threaded");

    let mut options = OpenOptions::new();
    options.read(true);
    let file = Arc::new(options.open(&ThreadPool, &path).unwrap());
    let other = Arc::clone(&file);
    let handle = std::thread::spawn(move || {
        let OpResult(read, buf) = common::block_on(other.read_at(0, Vec::with_capacity(32)));
        (read.unwrap(), buf)
    });

    let (outcome, buf) = handle.join().unwrap();
    assert_eq!(outcome, ReadOutcome::Bytes(8));
    assert_eq!(buf, b"threaded");

    drop(file);
    let _ = std::fs::remove_file(path);
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
fn registration_failure_after_open_releases_handle() {
    let path = temp_path("registration-failure");
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .share_mode(FILE_SHARE_NONE);

    assert!(matches!(
        options.open(&RejectRegistration, &path),
        Err(SetupError::AlreadyRegistered)
    ));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn new_safe_files_do_not_add_unsafe_send_or_sync_impls() {
    for path in [
        "crates\\winasio\\src\\fs\\file.rs",
        "crates\\winasio\\src\\fs\\options.rs",
        "crates\\winasio\\src\\iocp\\ops\\stream.rs",
    ] {
        let source = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join(path),
        )
        .unwrap();
        assert!(!source.contains("unsafe impl Send"));
        assert!(!source.contains("unsafe impl Sync"));
    }
}
