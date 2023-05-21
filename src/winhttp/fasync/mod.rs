use windows::{
    core::{Error, HRESULT, HSTRING},
    Win32::Networking::WinHttp::*,
};

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use super::HRequest;

const WINHTTP_CALLBACK_FLAG_ALL_COMPLETIONS: u32 = WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE
    | WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE
    | WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE
    | WINHTTP_CALLBACK_STATUS_READ_COMPLETE
    | WINHTTP_CALLBACK_STATUS_WRITE_COMPLETE
    | WINHTTP_CALLBACK_STATUS_REQUEST_ERROR
    | WINHTTP_CALLBACK_STATUS_GETPROXYFORURL_COMPLETE;

// Async request handle
pub struct HRequestAsync {
    h: HRequest,
    ctx: AsyncContext,
}

impl HRequestAsync {
    pub fn new(h: HRequest) -> HRequestAsync {
        let ha = HRequestAsync {
            h,
            ctx: AsyncContext::new(),
        };

        let prev = ha.h.set_status_callback(
            Some(AsyncCallback),
            WINHTTP_CALLBACK_FLAG_ALL_COMPLETIONS,
            0, // reserved
        );

        if let Some(p) = prev {
            let raw: *mut ::core::ffi::c_void = p as *mut std::ffi::c_void;
            let invalid: *mut ::core::ffi::c_void = -1_i64 as *mut std::ffi::c_void;
            if raw == invalid {
                let e = Error::from_win32();
                assert!(e.code().is_ok(), "Fail to set callback: {}", e);
            }
        }

        ha
    }

    // callback case WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE
    pub async fn async_send(
        &mut self,
        headers: HSTRING,
        optional: &[u8],
        total_length: u32,
    ) -> Result<(), Error> {
        // does not need to reset ctx
        self.ctx.state = WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE;
        // case ctx pass to winhttp
        let ctx_ptr: *const AsyncContext = &self.ctx;
        let raw_ctx: *mut ::core::ffi::c_void = ctx_ptr as *mut ::core::ffi::c_void;
        self.h
            .send(headers, optional, total_length, raw_ctx as usize)?;
        // wait for ctx to get signalled
        self.ctx.get_await_token().await;
        self.ctx.err.code().ok()
    }

    pub async fn async_receive_response(&mut self) -> Result<(), Error> {
        self.ctx.reset();
        self.ctx.state = WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE;
        self.h.receieve_response()?;
        self.ctx.get_await_token().await;
        self.ctx.err.code().ok()
    }

    pub async fn async_query_data_available(&mut self) -> Result<u32, Error> {
        self.ctx.reset();
        self.ctx.state = WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE;
        self.h.query_data_available(None)?;
        self.ctx.get_await_token().await;
        self.ctx.err.code().ok()?;
        Ok(self.ctx.len)
    }

    pub async fn async_read_data(
        &mut self,
        buffer: &mut [u8],
        dwnumberofbytestoread: u32,
    ) -> Result<u32, Error> {
        self.ctx.reset();
        self.ctx.state = WINHTTP_CALLBACK_STATUS_READ_COMPLETE;
        self.h.read_data(buffer, dwnumberofbytestoread, None)?;
        self.ctx.get_await_token().await;
        self.ctx.err.code().ok()?;
        Ok(self.ctx.len)
    }

    // buf should be valid until async_write_data finishes.
    // There is no requirement for buf to be valid until callback finishes.
    pub async fn async_write_data(
        &mut self,
        buf: &[u8],
        dwnumberofbytestowrite: u32,
    ) -> Result<u32, Error> {
        self.ctx.reset();
        self.ctx.state = WINHTTP_CALLBACK_FLAG_WRITE_COMPLETE;
        self.h.write_data(buf, dwnumberofbytestowrite, None)?;
        self.ctx.get_await_token().await;
        self.ctx.err.code().ok()?;
        Ok(self.ctx.len)
    }
}

/// Shared state between the future and the waiting thread
#[derive(Debug)]
struct SharedState {
    /// Whether or not the sleep time has elapsed
    completed: bool,

    /// The waker for the task that `TimerFuture` is running on.
    /// The thread can use this after setting `completed = true` to tell
    /// `TimerFuture`'s task to wake up, see that `completed = true`, and
    /// move forward.
    waker: Option<Waker>,
}

struct AsyncContext {
    state: u32,
    shared_state: Arc<Mutex<SharedState>>,
    err: Error, // TODO: handle error
    len: u32,   // len to read
}

struct AwaitableToken {
    shared_state: Arc<Mutex<SharedState>>,
}

impl AsyncContext {
    fn new() -> AsyncContext {
        AsyncContext {
            state: 0,
            shared_state: Arc::new(Mutex::new(SharedState {
                completed: false,
                waker: None,
            })),
            err: Error::from(HRESULT(0)),
            len: 0,
        }
    }

    // notify work is complete
    fn wake(&self) {
        let mut shared_state = self.shared_state.lock().unwrap();
        // Signal that the timer has completed and wake up the last
        // task on which the future was polled, if one exists.
        shared_state.completed = true;
        if let Some(waker) = shared_state.waker.take() {
            waker.wake()
        }
    }

    fn reset(&mut self) {
        self.shared_state = Arc::new(Mutex::new(SharedState {
            completed: false,
            waker: None,
        }));
        self.state = 0;
        self.len = 0;
        self.err = Error::OK;
    }

    // make ctx unchanged when doing wait
    fn get_await_token(&self) -> AwaitableToken {
        AwaitableToken {
            shared_state: self.shared_state.clone(),
        }
    }
}

impl Future for AwaitableToken {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Look at the shared state to see if the timer has already completed.
        let mut shared_state = self.shared_state.lock().unwrap();
        if shared_state.completed {
            Poll::Ready(())
        } else {
            // Set waker so that the thread can wake up the current task
            // when the timer has completed, ensuring that the future is polled
            // again and sees that `completed = true`.
            //
            // It's tempting to do this once rather than repeatedly cloning
            // the waker each time. However, the `TimerFuture` can move between
            // tasks on the executor, which could cause a stale waker pointing
            // to the wrong task, preventing `TimerFuture` from waking up
            // correctly.
            //
            // N.B. it's possible to check for this using the `Waker::will_wake`
            // function, but we omit that here to keep things simple.
            shared_state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[no_mangle]
extern "system" fn AsyncCallback(
    _hinternet: *mut ::core::ffi::c_void,
    dwcontext: usize,
    dwinternetstatus: u32,
    lpvstatusinformation: *mut ::core::ffi::c_void,
    dwstatusinformationlength: u32,
) {
    assert_ne!(dwcontext, 0);

    // convert ctx
    let ctx_raw: *mut AsyncContext = dwcontext as *mut AsyncContext;
    let ctx: &mut AsyncContext = unsafe { &mut *ctx_raw };
    assert_ne!(ctx.state, 0);

    match dwinternetstatus {
        WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE => {
            //println!("WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE");
            assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE);
            ctx.wake();
        }
        WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE => {
            //println!("WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE");
            assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE);
            ctx.wake();
        }
        WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE => {
            //println!("WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE");
            assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE);
            assert_eq!(
                dwstatusinformationlength as usize,
                std::mem::size_of::<u32>()
            );
            let temp_info: *mut u32 = lpvstatusinformation as *mut u32;
            let data_len: u32 = unsafe { *temp_info };
            //println!("Avaliable len: {}", data_len);
            ctx.len = data_len;
            ctx.wake();
        }
        WINHTTP_CALLBACK_FLAG_WRITE_COMPLETE => {
            assert_eq!(ctx.state, WINHTTP_CALLBACK_FLAG_WRITE_COMPLETE);
            assert_eq!(
                dwstatusinformationlength as usize,
                std::mem::size_of::<u32>()
            );
            let temp_info: *mut u32 = lpvstatusinformation as *mut u32;
            let data_len: u32 = unsafe { *temp_info };
            ctx.len = data_len;
            ctx.wake();
        }
        WINHTTP_CALLBACK_STATUS_READ_COMPLETE => {
            // Data was successfully read from the server. The lpvStatusInformation
            // parameter contains a pointer to the buffer specified in the call to
            // WinHttpReadData. The dwStatusInformationLength parameter contains the
            // number of bytes read. When used by WinHttpWebSocketReceive, the
            // lpvStatusInformation parameter contains a pointer to a
            // WINHTTP_WEB_SOCKET_STATUS structure, 	and the
            // dwStatusInformationLength
            // parameter indicates the size of lpvStatusInformation.

            //println!("WINHTTP_CALLBACK_STATUS_READ_COMPLETE");
            assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_READ_COMPLETE);
            ctx.len = dwstatusinformationlength;
            ctx.wake();
        }
        WINHTTP_CALLBACK_STATUS_REQUEST_ERROR => {
            // previous front end action results in error
            //WINHTTP_ASYNC_RESULT *pAR = (WINHTTP_ASYNC_RESULT *)lpvStatusInformation;
            let temp_res = lpvstatusinformation as *mut &WINHTTP_ASYNC_RESULT;
            let res = unsafe { *temp_res };
            let err = Error::from(HRESULT(res.dwError as i32));
            assert!(err.code().is_err());
            match res.dwResult as u32 {
                API_QUERY_DATA_AVAILABLE => {
                    assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_DATA_AVAILABLE);
                }
                API_RECEIVE_RESPONSE => {
                    assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE);
                }
                API_SEND_REQUEST => {
                    assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE);
                }
                API_READ_DATA => {
                    assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_READ_COMPLETE);
                }
                API_WRITE_DATA => {
                    assert_eq!(ctx.state, WINHTTP_CALLBACK_STATUS_WRITE_COMPLETE);
                }
                _ => {
                    panic!("Unknown dwResult {}", res.dwResult);
                }
            }
            // assign err and finish so that front end can pick up err
            ctx.err = err;
            ctx.wake();
        }
        _ => {
            panic!("Unknown callback case {}", dwinternetstatus);
        }
    }
}
