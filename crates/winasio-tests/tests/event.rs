// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

use winasio::sys::event::{AwaitableObject, ManualResetEvent};
use windows::Win32::Foundation::HANDLE;

#[test]
fn awaitable_object_test() {
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async {
        let e1 = ManualResetEvent::new();
        // HANDLE is a raw pointer and not Send, so pass it across the task boundary
        // as an integer and rebuild the handle inside the task.
        let h_raw = e1.get().0 as usize;

        let sh = tokio::task::spawn(async move {
            let h = HANDLE(h_raw as *mut std::ffi::c_void);
            let mut awaitable_obj = Box::new(AwaitableObject::new(h));
            awaitable_obj.wait().await;
        });

        // set event
        e1.set().unwrap();
        // wait for callback complete
        sh.await.unwrap();
    });
}
