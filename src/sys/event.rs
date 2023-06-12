use windows::{
    core::Error,
    Win32::{
        Foundation::{CloseHandle, BOOLEAN, HANDLE, INVALID_HANDLE_VALUE},
        System::Threading::{
            CreateEventA, RegisterWaitForSingleObject, ResetEvent, SetEvent, UnregisterWaitEx,
            INFINITE, WT_EXECUTEONLYONCE,
        },
    },
};

use super::wait::AsyncWaitObject;

// wrapper for windows event
#[derive(Default)]
pub struct ManualResetEvent {
    h: HANDLE,
}

impl ManualResetEvent {
    pub fn new() -> ManualResetEvent {
        // do not expect this to fail since this is un-named event.
        let h = unsafe { CreateEventA(None, true, false, None) }.unwrap();
        ManualResetEvent { h }
    }

    pub fn assign(&mut self, h: HANDLE) {
        assert!(self.h.is_invalid());
        self.h = h;
    }

    // set the event
    pub fn set(&self) -> Result<(), Error> {
        assert!(!self.h.is_invalid());
        let ok = unsafe { SetEvent(self.h) };
        ok.ok()
    }

    pub fn reset(&self) -> Result<(), Error> {
        assert!(!self.h.is_invalid());
        let ok = unsafe { ResetEvent(self.h) };
        ok.ok()
    }

    // releases the ownership of the handle
    pub fn release(&mut self) -> HANDLE {
        let h = self.h;
        assert!(!h.is_invalid());
        self.h = HANDLE::default();
        h
    }

    // get the private view of handle
    pub fn get(&self) -> HANDLE {
        assert!(!self.h.is_invalid());
        self.h
    }
}

impl Drop for ManualResetEvent {
    fn drop(&mut self) {
        if self.h.is_invalid() {
            return;
        }
        let ok = unsafe { CloseHandle(self.h) };
        if !ok.as_bool() {
            let e = Error::from_win32();
            assert!(e.code().is_ok(), "Error: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ManualResetEvent;

    #[test]
    fn manual_reset_event_test() {
        {
            let e = ManualResetEvent::new();
            e.set().unwrap();
            e.reset().unwrap();
        }
        {
            let mut e1 = ManualResetEvent::default();
            let mut e2 = ManualResetEvent::new();

            let h = e2.release();
            e1.assign(h);
            e1.set().unwrap();
        }
    }
}

// reference boost\asio\detail\impl\win_object_handle_service.ipp

#[repr(C)]
struct PrivateContext {
    as_obj: AsyncWaitObject,

    // wait handle for that callback operation
    // returned by register callback
    wait_obj: HANDLE,
}

impl Drop for PrivateContext {
    fn drop(&mut self) {
        assert_eq!(self.wait_obj, HANDLE::default());
        // println!("Private ctx dropped");
    }
}

// event that is awaitable
pub struct AwaitableObject {
    ctx: Box<PrivateContext>,
}

impl AwaitableObject {
    fn default() -> AwaitableObject {
        AwaitableObject {
            // ctx on stack dows not work. Maybe it gets moved by compiler.
            // allocate on heap the C registered address does not change.
            ctx: Box::new(PrivateContext {
                as_obj: AsyncWaitObject::new(),
                wait_obj: HANDLE::default(),
            }),
        }
    }

    // awaitable event never owns the handle
    // since there are various handle that can be waited
    pub fn new(h: HANDLE) -> AwaitableObject {
        let mut res = AwaitableObject::default();

        // register the callback
        AwaitableObject::register_callback(h, &mut res.ctx).unwrap();

        res
    }

    pub async fn wait(&mut self) {
        self.ctx.as_obj.get_await_token().await;
        Self::unregister(&mut self.ctx);
    }

    // register callback to be executed in windows threadpool
    // callback will wake the waker.
    fn register_callback(h: HANDLE, ctx: &mut PrivateContext) -> Result<(), Error> {
        let wh_ptr: *mut HANDLE = std::ptr::addr_of_mut!(ctx.wait_obj);

        let ctx_ptr: *const PrivateContext = ctx;
        let raw_ctx: *mut ::core::ffi::c_void = ctx_ptr as *mut ::core::ffi::c_void;
        let ok = unsafe {
            RegisterWaitForSingleObject(
                wh_ptr,
                h,
                Some(my_callback),
                Some(raw_ctx),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        assert!(!ctx.wait_obj.is_invalid());
        ok.ok()
    }

    fn unregister(ctx: &mut PrivateContext) {
        if !ctx.wait_obj.is_invalid() {
            // INVALID_HANDLE_VALUE will force wait for the callback to complete
            let ok = unsafe { UnregisterWaitEx(ctx.wait_obj, INVALID_HANDLE_VALUE) };
            ok.unwrap();
            ctx.wait_obj = HANDLE::default();
        }
    }
}

unsafe extern "system" fn my_callback(ctx: *mut ::core::ffi::c_void, timer_or_wait_fired: BOOLEAN) {
    // convert ctx
    let ctx_raw: *mut PrivateContext = ctx as *mut PrivateContext;

    let ctx: &mut PrivateContext = unsafe { &mut *ctx_raw };

    // we always wait for infinite. param true means timed-out.
    assert!(!timer_or_wait_fired.as_bool());

    // we cannot unregister in the callback
    ctx.as_obj.wake();
}

#[cfg(test)]
mod tests2 {
    use super::{AwaitableObject, ManualResetEvent};

    #[test]
    fn awaitable_object_test() {
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(async {
            let e1 = ManualResetEvent::new();
            let h = e1.get();

            let sh = tokio::task::spawn(async move {
                let mut awaitable_obj = Box::new(AwaitableObject::new(h));
                awaitable_obj.wait().await;
            });

            // set event
            e1.set().unwrap();
            // wait for callback complete
            sh.await.unwrap();
        });
    }
}
