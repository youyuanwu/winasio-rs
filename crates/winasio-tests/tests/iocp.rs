// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

use std::sync::Arc;

use windows::{
    core::{w, Error, HSTRING},
    Win32::{
        Foundation::{CloseHandle, ERROR_IO_PENDING, GENERIC_WRITE},
        Storage::FileSystem::{
            CreateFileW, DeleteFileW, GetTempFileNameW, GetTempPathW, ReadFile, WriteFile,
            CREATE_ALWAYS, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_SHARE_NONE,
        },
    },
};

use winasio::sys::iocp::{register_iocp_handle, OverlappedObject};

#[test]
fn async_file_test() {
    let mut path_buff = vec![0u16; 100];
    // create a temp file
    let len = unsafe { GetTempPathW(Some(path_buff.as_mut_slice())) };
    assert_ne!(len, 0);
    assert!(len <= 100);
    path_buff.truncate(len as usize);
    let temp_path = HSTRING::from_wide(&path_buff);
    assert_eq!(temp_path.len(), len as usize);

    let mut temp_file: [u16; 260] = [0; 260];
    let len2 = unsafe { GetTempFileNameW(&temp_path, w!("async_file_test"), 0, &mut temp_file) };
    assert_ne!(len2, 0);
    let temp_file = HSTRING::from_wide(&temp_file);
    println!("temp file is: {}", temp_file);

    // create this file:
    let hfile = unsafe {
        CreateFileW(
            &temp_file,
            FILE_GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            CREATE_ALWAYS,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .unwrap();

    register_iocp_handle(hfile).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // get a big string so that we hit the pending case
        let data: String = "HelloWorld".repeat(200);
        // write file
        {
            println!("writing to file");
            let optr = Arc::new(OverlappedObject::new());
            // try async read and write to this file
            let ok = unsafe { WriteFile(hfile, Some(data.as_bytes()), None, Some(optr.get_mut())) };
            match ok {
                Err(e) => {
                    if e == Error::from(ERROR_IO_PENDING) {
                        // forget one ref and let callback handle it.
                        std::mem::forget(optr.clone());
                        // println!("IO pending");
                        optr.wait().await;
                        assert_eq!(optr.get_ec(), Error::empty());
                    } else {
                        // callback might not be invoked for some errors here.
                        // if we wait here, and callback is not invoked, we are stuck
                        // if we do not wait here, and callback is invoked, we have dangling ptr in callback.
                        // !!!currently we rely/assume that this case callback is not invoked.
                        // A safer impl is to allocate the optr on heap.
                        // println!("Other error: {}", e);
                        assert_eq!(e, Error::empty());
                    }
                }
                Ok(()) => {
                    // completed synchronously
                    // println!("No error: Completed synchronously");
                    // callback is invoked when success.
                    std::mem::forget(optr.clone());
                    optr.wait().await;
                }
            }
        }
        // read file
        {
            println!("Reading file.");
            let optr = Arc::new(OverlappedObject::new());
            let mut buffer: Vec<u8> = vec![0; data.len()];
            let ok = unsafe {
                ReadFile(
                    hfile,
                    Some(buffer.as_mut_slice()),
                    None,
                    Some(optr.get_mut()),
                )
            };
            match ok {
                Err(e) => {
                    if e == Error::from(ERROR_IO_PENDING) {
                        //println!("IO pending");
                        std::mem::forget(optr.clone());
                        optr.wait().await;
                        assert_eq!(optr.get_ec(), Error::empty());
                    } else {
                        //println!("Other error: {}", e);
                        assert_eq!(e, Error::empty());
                    }
                }
                Ok(()) => {
                    // completed synchronously
                    // println!("No error: Completed synchronously");
                    // callback is invoked when success.
                    std::mem::forget(optr.clone());
                    optr.wait().await;
                }
            }

            // read complete
            let read_str = String::from_utf8_lossy(&buffer);
            assert_eq!(data, read_str);
        }
    });

    let ok = unsafe { CloseHandle(hfile) };
    assert!(ok.is_ok());

    // delete the temp file
    let ok = unsafe { DeleteFileW(&temp_file) };
    assert!(ok.is_ok());
}
