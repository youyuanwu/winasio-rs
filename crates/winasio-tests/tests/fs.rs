// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

mod common;

use std::future::Future;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
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
    FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
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

trait FileTestRegistrar: Registrar {
    const ROUND_TRIP_TAG: &'static str;

    fn drive<F: Future>(&self, future: F) -> F::Output;
}

impl FileTestRegistrar for ThreadPool {
    const ROUND_TRIP_TAG: &'static str = "round-trip-pool";

    fn drive<F: Future>(&self, future: F) -> F::Output {
        common::block_on(future)
    }
}

impl FileTestRegistrar for Rc<Proactor> {
    const ROUND_TRIP_TAG: &'static str = "round-trip-proactor";

    fn drive<F: Future>(&self, future: F) -> F::Output {
        common::drive_proactor(self.as_ref(), future)
    }
}

fn round_trip_body<R: FileTestRegistrar>(registrar: &R) {
    let path = temp_path(R::ROUND_TRIP_TAG);
    let file = open_read_write(registrar, &path);

    let payload = b"safe async file".repeat(72);
    let OpResult(written, returned) = registrar.drive(file.write_at(64, payload.clone()));
    assert_eq!(written.unwrap(), payload.len());
    assert_eq!(returned, payload);

    let OpResult(read, got) =
        registrar.drive(file.read_at(64, Vec::with_capacity(payload.len() + 16)));
    assert_eq!(read.unwrap(), ReadOutcome::Bytes(payload.len()));
    assert_eq!(got, payload);

    drop(file);
    let _ = std::fs::remove_file(path);
}

#[test]
fn round_trip_thread_pool() {
    round_trip_body(&ThreadPool);
}

#[test]
fn round_trip_caller_driven() {
    let proactor = Rc::new(Proactor::new().unwrap());
    round_trip_body(&proactor);
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
fn open_options_share_mode_and_custom_attributes_have_effect() {
    let share_path = temp_path("share-mode");
    create_contents(&share_path, b"locked");
    let mut exclusive = OpenOptions::new();
    exclusive.read(true).share_mode(FILE_SHARE_NONE);
    let file = exclusive.open(&ThreadPool, &share_path).unwrap();
    assert!(
        std::fs::OpenOptions::new()
            .read(true)
            .open(&share_path)
            .is_err(),
        "FILE_SHARE_NONE must prevent a second reader while the handle is open"
    );
    drop(file);
    std::fs::OpenOptions::new()
        .read(true)
        .open(&share_path)
        .unwrap();
    let _ = std::fs::remove_file(&share_path);

    let attr_path = temp_path("custom-attributes");
    let mut custom = OpenOptions::new();
    custom
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags_and_attributes(FILE_ATTRIBUTE_TEMPORARY);
    let file = custom.open(&ThreadPool, &attr_path).unwrap();
    let attributes = std::fs::metadata(&attr_path).unwrap().file_attributes();
    assert_ne!(
        attributes & FILE_ATTRIBUTE_TEMPORARY.0,
        0,
        "custom_flags_and_attributes must pass file attributes to CreateFileW"
    );
    drop(file);
    let _ = std::fs::remove_file(attr_path);
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
    let written = common::drive_proactor(
        proactor.as_ref(),
        proactor.submit(WriteAt::new(file.handle(), 0, payload)),
    );
    assert_eq!(written.0.unwrap(), b"low level".len());

    let OpResult(read, got) =
        common::drive_proactor(proactor.as_ref(), file.read_at(0, Vec::with_capacity(32)));
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
