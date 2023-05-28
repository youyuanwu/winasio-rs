mod test;

use windows::{
    core::{Error, HRESULT, HSTRING},
    Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, HANDLE, NO_ERROR, WIN32_ERROR},
        Networking::HttpServer::{
            HttpAddUrlToUrlGroup, HttpCloseRequestQueue, HttpCloseServerSession, HttpCloseUrlGroup,
            HttpCreateRequestQueue, HttpCreateServerSession, HttpCreateUrlGroup, HttpInitialize,
            HttpReceiveHttpRequest, HttpSendHttpResponse, HttpServerBindingProperty,
            HttpSetUrlGroupProperty, HttpTerminate, HTTPAPI_VERSION, HTTP_BINDING_INFO,
            HTTP_CACHE_POLICY, HTTP_INITIALIZE_CONFIG, HTTP_INITIALIZE_SERVER, HTTP_LOG_DATA,
            HTTP_RECEIVE_HTTP_REQUEST_FLAGS, HTTP_REQUEST_V2, HTTP_RESPONSE_V2,
            HTTP_SERVER_PROPERTY,
        },
    },
};

use crate::sys::iocp::{register_iocp_handle, OverlappedObject};

static G_HTTP_VERSION: HTTPAPI_VERSION = HTTPAPI_VERSION {
    HttpApiMajorVersion: 2,
    HttpApiMinorVersion: 0,
};

pub struct HttpInitializer {}

impl HttpInitializer {
    pub fn default() {
        let ec = unsafe {
            HttpInitialize(
                G_HTTP_VERSION,
                HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG,
                None,
            )
        };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::OK);
    }

    // pub fn create_request_queue() -> Result<HANDLE, Error> {
    //     let mut h: HANDLE = HANDLE::default();
    //     let ec = unsafe {
    //         HttpCreateRequestQueue(G_HTTP_VERSION, None, None, 0, std::ptr::addr_of_mut!(h))
    //     };
    //     let err = Error::from(HRESULT(ec.try_into().unwrap()));
    //     if err.code().is_err() {
    //         Err(err)
    //     } else {
    //         assert!(!h.is_invalid());
    //         Ok(h)
    //     }
    // }
}

impl Drop for HttpInitializer {
    fn drop(&mut self) {
        let ec = unsafe { HttpTerminate(HTTP_INITIALIZE_SERVER | HTTP_INITIALIZE_CONFIG, None) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::OK);
    }
}

pub struct ServerSession {
    id: u64,
}

impl ServerSession {
    pub fn new() -> ServerSession {
        let mut id: u64 = 0;
        let ec = unsafe { HttpCreateServerSession(G_HTTP_VERSION, std::ptr::addr_of_mut!(id), 0) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::OK);
        ServerSession { id }
    }
}
impl Default for ServerSession {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        let ec = unsafe { HttpCloseServerSession(self.id) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::OK);
    }
}

pub struct UrlGroup<'a> {
    // session can only be deleted after urlgroup deallocates
    _session: &'a ServerSession,
    id: u64,
}

impl UrlGroup<'_> {
    pub fn new(session: &ServerSession) -> UrlGroup {
        let mut id: u64 = 0;
        let ec = unsafe { HttpCreateUrlGroup(session.id, std::ptr::addr_of_mut!(id), 0) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::OK);
        UrlGroup {
            _session: session,
            id,
        }
    }

    unsafe fn set_property(
        &self,
        property: HTTP_SERVER_PROPERTY,
        propertyinformation: *const ::core::ffi::c_void,
        propertyinformationlength: u32,
    ) -> Result<(), Error> {
        let ec = unsafe {
            HttpSetUrlGroupProperty(
                self.id,
                property,
                propertyinformation,
                propertyinformationlength,
            )
        };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        err.code().ok()
    }

    pub fn set_binding_info(&self, info: &HTTP_BINDING_INFO) -> Result<(), Error> {
        let info_ptr: *const HTTP_BINDING_INFO = info;
        unsafe {
            self.set_property(
                HttpServerBindingProperty,
                info_ptr as *const std::ffi::c_void,
                std::mem::size_of::<HTTP_BINDING_INFO>() as u32,
            )
        }
    }

    pub fn add_url(&self, url: HSTRING) -> Result<(), Error> {
        let ec = unsafe { HttpAddUrlToUrlGroup(self.id, &url, 0, 0) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        err.code().ok()
    }
}

impl Drop for UrlGroup<'_> {
    fn drop(&mut self) {
        let ec = unsafe { HttpCloseUrlGroup(self.id) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::OK);
    }
}

pub struct RequestQueue {
    h: HANDLE,
    optr: OverlappedObject,
}

impl RequestQueue {
    pub fn new() -> Result<RequestQueue, Error> {
        let mut h: HANDLE = HANDLE::default();
        let ec = unsafe {
            HttpCreateRequestQueue(G_HTTP_VERSION, None, None, 0, std::ptr::addr_of_mut!(h))
        };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        if err.code().is_err() {
            Err(err)
        } else {
            assert!(!h.is_invalid());
            // register with iocp thread pool callback
            register_iocp_handle(h).unwrap();
            Ok(RequestQueue {
                h,
                optr: OverlappedObject::new(),
            })
        }
    }

    pub fn bind_url_group(&self, url_group: &UrlGroup) -> Result<(), Error> {
        let info = HTTP_BINDING_INFO {
            Flags: windows::Win32::Networking::HttpServer::HTTP_PROPERTY_FLAGS { _bitfield: 1 },
            RequestQueueHandle: self.h,
        };
        url_group.set_binding_info(&info)
    }

    pub async fn async_receive_request(
        &mut self,
        requestid: u64,
        flags: HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
        requestbuffer: &mut HTTP_REQUEST_V2,
        requestbufferlength: u32,
    ) -> Result<u32, Error> {
        // !!! not thread safe. assume only one thread now.
        // we need to store in self because when server shutdown, await is cancelled,
        // but callback is invoked on this optr, and will result access violation if
        // put on stack, since the stack might be gone if await is cancelled.
        // TODO: maybe other functions the optr needs to be on self or heap as well.
        self.optr = OverlappedObject::new();
        let ec = unsafe {
            HttpReceiveHttpRequest(
                self.h,
                requestid,
                flags,
                requestbuffer,
                requestbufferlength,
                None,
                Some(self.optr.get()),
            )
        };
        let err = WIN32_ERROR(ec);
        if err == ERROR_IO_PENDING || err == NO_ERROR {
            //println!("HttpReceiveHttpRequest waiting. {:?}", err);
            self.optr.wait().await;
            let async_err = self.optr.get_ec();
            //println!("HttpReceiveHttpRequest waiting complete . {:?}", async_err);
            if async_err == Error::OK {
                Ok(self.optr.get_len())
            } else {
                Err(async_err)
            }
        } else {
            // we do not handle insufficent buffer, caller needs to pass buffer size greater than xxx?
            assert_ne!(err, ERROR_INSUFFICIENT_BUFFER);

            Err(Error::from(err))
        }
    }

    pub async fn async_send_response(
        &self,
        requestid: u64,
        flags: u32,
        httpresponse: *const HTTP_RESPONSE_V2,
        cachepolicy: ::core::option::Option<*const HTTP_CACHE_POLICY>,
        logdata: ::core::option::Option<*const HTTP_LOG_DATA>,
    ) -> Result<u32, Error> {
        let mut optr = OverlappedObject::new();
        let ec = unsafe {
            HttpSendHttpResponse(
                self.h,
                requestid,
                flags,
                httpresponse,
                cachepolicy,
                None,
                None,
                0,
                Some(optr.get()),
                logdata,
            )
        };
        let err = WIN32_ERROR(ec);
        if err == ERROR_IO_PENDING || err == NO_ERROR {
            optr.wait().await;
            let async_err = optr.get_ec();
            if async_err == Error::OK {
                Ok(optr.get_len())
            } else {
                Err(async_err)
            }
        } else {
            Err(Error::from(err))
        }
    }

    pub fn close(&mut self) {
        if self.h.is_invalid() {
            return;
        }
        let ec = unsafe { HttpCloseRequestQueue(self.h) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::OK);
        self.h = HANDLE(0);
    }
}

impl Drop for RequestQueue {
    fn drop(&mut self) {
        self.close()
    }
}
