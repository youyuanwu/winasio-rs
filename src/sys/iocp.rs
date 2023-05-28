// The goal of iocp here is to have a Overlapped struct ptr to be passed in to
// windows API, and when overlapped operation is pending, rs code can do await.
// When pending operation finishes, await finishes, and rs code continues.

use crate::sys::wait::AsyncWaitObject;
use windows::{
    core::Error,
    Win32::{
        Foundation::{HANDLE, WIN32_ERROR},
        System::IO::{BindIoCompletionCallback, OVERLAPPED},
    },
};

// all handles wish to use overlappedObject in this mod needs to call register.
pub fn register_iocp_handle(h: HANDLE) -> Result<(), Error> {
    let ok = unsafe { BindIoCompletionCallback(h, Some(private_callback), 0) };
    ok.ok()
}

unsafe extern "system" fn private_callback(
    dwerrorcode: u32,
    dwnumberofbytestransfered: u32,
    lpoverlapped: *mut OVERLAPPED,
) {
    // println!("private_callback invoked.");
    // convert to wrap struct
    let wrap_ptr: *mut OverlappedWrap = lpoverlapped as *mut OverlappedWrap;
    let wrap: &mut OverlappedWrap = &mut *wrap_ptr;

    // set the result and wake.
    let e = Error::from(WIN32_ERROR(dwerrorcode));
    if e.code().is_err() {
        // println!("private_callback err: {}", e);
        wrap.err = e;
    } else {
        // println!("private_callback no err:");
        wrap.len = dwnumberofbytestransfered;
    }
    wrap.as_obj.wake();
}

// add some unsafe rust def
unsafe impl Send for OverlappedWrap {}
unsafe impl Sync for OverlappedWrap {}

#[repr(C)]
pub struct OverlappedWrap {
    o: OVERLAPPED,
    as_obj: AsyncWaitObject,
    err: Error,
    len: u32,
}

impl Default for OverlappedWrap {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlappedWrap {
    pub fn new() -> Self {
        OverlappedWrap {
            o: OVERLAPPED::default(),
            as_obj: AsyncWaitObject::new(),
            err: Error::OK,
            len: 0,
        }
    }
}

// overlapped object is used in rust code to create overlapped pointer
pub struct OverlappedObject {
    o: OverlappedWrap, // overlapped struct to be passed to windows
}

impl Default for OverlappedObject {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlappedObject {
    pub fn new() -> Self {
        OverlappedObject {
            o: OverlappedWrap::new(),
        }
    }

    // get the reference to overlapped struct to pass to windows.
    // the iocp threadpool callback will wake the AsyncWaitObject,
    // while the front end should await.
    pub fn get(&mut self) -> *mut OVERLAPPED {
        let ow_ptr: *mut OverlappedWrap = std::ptr::addr_of_mut!(self.o);
        let ow_cast_ptr: *mut OVERLAPPED = ow_ptr as *mut OVERLAPPED;
        ow_cast_ptr
    }

    pub async fn wait(&self) {
        self.o.as_obj.get_await_token().await;
    }

    pub fn get_ec(&self) -> Error {
        self.o.err.clone()
    }

    pub fn get_len(&self) -> u32 {
        self.o.len
    }
}

#[cfg(test)]
mod tests {
    use windows::{
        core::{Error, HSTRING},
        w,
        Win32::{
            Foundation::{CloseHandle, ERROR_IO_PENDING, GENERIC_WRITE},
            Storage::FileSystem::{
                CreateFileW, DeleteFileW, GetTempFileNameW, GetTempPathW, ReadFile, WriteFile,
                CREATE_ALWAYS, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_SHARE_NONE,
            },
            System::IO::OVERLAPPED,
        },
    };

    use crate::sys::iocp::{register_iocp_handle, OverlappedObject};

    use super::OverlappedWrap;

    // tests wrapped obj can be directly accessed.
    #[test]
    fn wrapper_object_test() {
        let mut ow = OverlappedWrap::new();
        ow.o.Internal = 10;
        ow.o.InternalHigh = 11;
        ow.o.Anonymous.Anonymous.Offset = 12;
        ow.o.Anonymous.Anonymous.OffsetHigh = 13;

        let ow_ptr: *mut OverlappedWrap = std::ptr::addr_of_mut!(ow);

        let ow_cast_ptr: *mut OVERLAPPED = ow_ptr as *mut OVERLAPPED;

        unsafe { test_fn(ow_cast_ptr) };
    }

    unsafe extern "system" fn test_fn(lpoverlapped: *mut OVERLAPPED) -> () {
        let ol: &mut OVERLAPPED = &mut *lpoverlapped;
        assert_eq!(ol.Internal, 10);
        assert_eq!(ol.InternalHigh, 11);
        assert_eq!(ol.Anonymous.Anonymous.Offset, 12);
        assert_eq!(ol.Anonymous.Anonymous.OffsetHigh, 13);

        // check if we convert to wrap type
        let wrap_ptr: *mut OverlappedWrap = lpoverlapped as *mut OverlappedWrap;
        let wrap: &mut OverlappedWrap = &mut *wrap_ptr;

        let _ = wrap.as_obj.get_await_token();
        wrap.as_obj.wake();
    }

    #[test]
    fn async_file_test() {
        let mut path_buff = Vec::<u16>::new();
        path_buff.resize(100, 0);
        // create a temp file
        let len = unsafe { GetTempPathW(Some(path_buff.as_mut_slice())) };
        assert_ne!(len, 0);
        assert!(len <= 100);
        path_buff.truncate(len as usize);
        let temp_path = HSTRING::from_wide(&path_buff).unwrap();
        assert_eq!(temp_path.len(), len as usize);

        let mut temp_file: [u16; 260] = [0; 260];
        let len2 =
            unsafe { GetTempFileNameW(&temp_path, w!("async_file_test"), 0, &mut temp_file) };
        assert_ne!(len2, 0);
        let temp_file = HSTRING::from_wide(&temp_file).unwrap();
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
                let mut optr = OverlappedObject::new();
                // try async read and write to this file
                let ok = unsafe { WriteFile(hfile, Some(data.as_bytes()), None, Some(optr.get())) };
                let err = ok.ok().err();
                match err {
                    Some(e) => {
                        if e == Error::from(ERROR_IO_PENDING) {
                            // println!("IO pending");
                            optr.wait().await;
                            assert_eq!(optr.o.err, Error::OK);
                        } else {
                            // callback might not be invoked for some errors here.
                            // if we wait here, and callback is not invoked, we are stuck
                            // if we do not wait here, and callback is invoked, we have dangling ptr in callback.
                            // !!!currently we rely/assume that this case callback is not invoked.
                            // A safer impl is to allocate the optr on heap.
                            // println!("Other error: {}", e);
                            assert_eq!(e, Error::OK);
                        }
                    }
                    None => {
                        // completed synchronously
                        // println!("No error: Completed synchronously");
                        // callback is invoked when success.
                        optr.wait().await;
                    }
                }
            }
            // read file
            {
                println!("Reading file.");
                let mut optr = OverlappedObject::new();
                let mut buffer: Vec<u8> = vec![0; data.len()];
                let ok = unsafe {
                    ReadFile(
                        hfile,
                        Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
                        buffer.len() as u32,
                        None,
                        Some(optr.get()),
                    )
                };
                match ok.ok().err() {
                    Some(e) => {
                        if e == Error::from(ERROR_IO_PENDING) {
                            //println!("IO pending");
                            optr.wait().await;
                            assert_eq!(optr.o.err, Error::OK);
                        } else {
                            //println!("Other error: {}", e);
                            assert_eq!(e, Error::OK);
                        }
                    }
                    None => {
                        // completed synchronously
                        // println!("No error: Completed synchronously");
                        // callback is invoked when success.
                        optr.wait().await;
                    }
                }

                // read complete
                let read_str = String::from_utf8_lossy(&buffer);
                assert_eq!(data, read_str);
            }
        });

        let ok = unsafe { CloseHandle(hfile) };
        assert!(ok.as_bool());

        // delete the temp file
        let ok = unsafe { DeleteFileW(&temp_file) };
        assert!(ok.as_bool());
    }
}
