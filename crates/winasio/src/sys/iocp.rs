// The goal of iocp here is to have a Overlapped struct ptr to be passed in to
// windows API, and when overlapped operation is pending, rs code can do await.
// When pending operation finishes, await finishes, and rs code continues.

use std::sync::Arc;

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
    unsafe { BindIoCompletionCallback(h, Some(private_callback), 0) }
}

unsafe extern "system" fn private_callback(
    dwerrorcode: u32,
    dwnumberofbytestransfered: u32,
    lpoverlapped: *mut OVERLAPPED,
) {
    let e = Error::from(WIN32_ERROR(dwerrorcode));
    if e.code().is_err() {
        // TODO: observe which error code this is when cancel operation.
        // TODO: need to check with cpp implmentation.
        // private_callback: operation failed 3221225760: Attempt to release mutex not owned by caller. (0x80070120).
        // println!("private_callback: operation failed {}: {}.", dwerrorcode, e);
    }

    // println!("private_callback invoked.");
    // convert to wrap struct
    let wrap_ptr: *mut OverlappedWrap = lpoverlapped as *mut OverlappedWrap;

    // this is to make sure we free it since we forget it when constructing the wrap in the front end.
    let _wrap = Arc::from_raw(wrap_ptr);

    let wrap: &mut OverlappedWrap = &mut *wrap_ptr;

    // set the result and wake.
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
            err: Error::empty(),
            len: 0,
        }
    }
}

// overlapped object is used in rust code to create overlapped pointer.
// This needs to be used in Arc and need to reserve a ref count when the io requires the callback to complete,
// and the callback is responsible to release once.
// This makes sure that the Overlapped ptr is valid in callback, especially to tolerate cancelled await.
// TODO: this requires some more mem leak testing.
pub struct OverlappedObject {
    // cannot arc this because it is hard to deref.
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
    pub fn get(&self) -> *const OVERLAPPED {
        let ow_ptr: *const OverlappedWrap = std::ptr::addr_of!(self.o);
        let ow_cast_ptr: *const OVERLAPPED = ow_ptr as *const OVERLAPPED;
        ow_cast_ptr
    }

    // get a mut pointer
    pub fn get_mut(&self) -> *mut OVERLAPPED {
        let ow_ptr: *const OverlappedWrap = std::ptr::addr_of!(self.o);
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
    use windows::Win32::System::IO::OVERLAPPED;

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

    unsafe extern "system" fn test_fn(lpoverlapped: *mut OVERLAPPED) {
        let ol: &mut OVERLAPPED = &mut *lpoverlapped;
        assert_eq!(ol.Internal, 10);
        assert_eq!(ol.InternalHigh, 11);
        assert_eq!(ol.Anonymous.Anonymous.Offset, 12);
        assert_eq!(ol.Anonymous.Anonymous.OffsetHigh, 13);

        // check if we convert to wrap type
        let wrap_ptr: *mut OverlappedWrap = lpoverlapped as *mut OverlappedWrap;
        let wrap: &mut OverlappedWrap = &mut *wrap_ptr;

        drop(wrap.as_obj.get_await_token());
        wrap.as_obj.wake();
    }
}
