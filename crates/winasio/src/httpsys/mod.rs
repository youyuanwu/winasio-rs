pub mod ops;

use std::marker::PhantomPinned;
use std::pin::Pin;

use windows::{
    core::{Error, HRESULT, HSTRING},
    Win32::{
        Foundation::HANDLE,
        Networking::HttpServer::{
            HttpAddUrlToUrlGroup, HttpCloseRequestQueue, HttpCloseServerSession, HttpCloseUrlGroup,
            HttpCreateRequestQueue, HttpCreateServerSession, HttpCreateUrlGroup,
            HttpDataChunkFromMemory, HttpInitialize, HttpServerBindingProperty,
            HttpSetUrlGroupProperty, HttpTerminate, HTTPAPI_VERSION, HTTP_BINDING_INFO,
            HTTP_DATA_CHUNK, HTTP_INITIALIZE_CONFIG, HTTP_INITIALIZE_SERVER,
            HTTP_RECEIVE_HTTP_REQUEST_FLAGS, HTTP_REQUEST_V2, HTTP_RESPONSE_V2,
            HTTP_SERVER_PROPERTY,
        },
    },
};

use crate::iocp::{OpResult, Submit, ThreadPoolIo};
pub use ops::{ReceiveRequest, SendResponse};

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
        assert_eq!(err, Error::empty());
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
        assert_eq!(err, Error::empty());
    }
}

pub struct ServerSession {
    id: u64,
}

impl ServerSession {
    pub fn new() -> ServerSession {
        let mut id: u64 = 0;
        let ec =
            unsafe { HttpCreateServerSession(G_HTTP_VERSION, std::ptr::addr_of_mut!(id), None) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::empty());
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
        assert_eq!(err, Error::empty());
    }
}

pub struct UrlGroup<'a> {
    // session can only be deleted after urlgroup deallocates
    _session: &'a ServerSession,
    id: u64,
}

impl UrlGroup<'_> {
    pub fn new(session: &ServerSession) -> UrlGroup<'_> {
        let mut id: u64 = 0;
        let ec = unsafe { HttpCreateUrlGroup(session.id, std::ptr::addr_of_mut!(id), None) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::empty());
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
        let ec = unsafe { HttpAddUrlToUrlGroup(self.id, &url, 0, None) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        err.code().ok()
    }
}

impl Drop for UrlGroup<'_> {
    fn drop(&mut self) {
        let ec = unsafe { HttpCloseUrlGroup(self.id) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::empty());
    }
}

/// A received HTTP request.
///
/// HTTP.sys treats this whole struct as one buffer: it writes the URL, headers
/// and entity metadata into `buff` and stores pointers to that region inside
/// `raw`. Moving the value would dangle every one of those pointers, so a
/// received request is only ever handed back as `Pin<Box<Request>>`.
///
/// The [`PhantomPinned`] is what makes that guarantee real. Without it the type
/// would be [`Unpin`], and safe code could call `Pin::into_inner` to move the
/// value straight back out.
#[repr(C)]
pub struct Request {
    raw: HTTP_REQUEST_V2,
    // additional buffer
    buff: [u8; 1024],
    /// Opts out of `Unpin`; see the type docs.
    _pin: PhantomPinned,
}

impl Default for Request {
    fn default() -> Request {
        Request {
            raw: HTTP_REQUEST_V2::default(),
            buff: [0; 1024],
            _pin: PhantomPinned,
        }
    }
}

impl Request {
    /// Mutable access to the parsed header.
    ///
    /// # Safety
    ///
    /// After a request has been received, `raw` holds pointers into this
    /// allocation's inline buffer. Overwriting them -- for instance with
    /// `mem::replace` or `HTTP_REQUEST_V2::default()` -- leaves
    /// [`Request::raw_ref`] consumers dereferencing arbitrary values.
    ///
    /// Callers must not invalidate those pointers.
    pub unsafe fn raw_mut(&mut self) -> &mut HTTP_REQUEST_V2 {
        &mut self.raw
    }

    /// Read-only view of the parsed request.
    ///
    /// Fields such as `pRawUrl` and the header arrays point into this same
    /// allocation, which is why a received request is handed back pinned.
    pub fn raw_ref(&self) -> &HTTP_REQUEST_V2 {
        &self.raw
    }

    /// Pointer to the embedded header, for handing to HTTP.sys.
    pub(crate) fn raw_ptr(&mut self) -> *mut HTTP_REQUEST_V2 {
        &mut self.raw
    }

    pub fn size() -> u32 {
        std::mem::size_of::<Request>() as u32
    }
}
// request should be safe
unsafe impl Send for Request {}
unsafe impl Sync for Request {}

// respose wrapper
#[derive(Default)]
#[repr(C)]
pub struct Response {
    raw: HTTP_RESPONSE_V2,
    data_chunks: Box<HTTP_DATA_CHUNK>,
    strings: String,
}
// resp should be safe
unsafe impl Send for Response {}
unsafe impl Sync for Response {}

impl Response {
    pub fn raw(&self) -> *const HTTP_RESPONSE_V2 {
        &self.raw
    }

    // only support 1 chunk for now.
    pub fn add_body_chunk(&mut self, data: String) {
        self.strings = data;

        let mut chunk = Box::<HTTP_DATA_CHUNK>::default();
        chunk.DataChunkType = HttpDataChunkFromMemory;
        chunk.Anonymous.FromMemory.BufferLength = self.strings.len() as u32;
        chunk.Anonymous.FromMemory.pBuffer = self.strings.as_mut_ptr() as *mut std::ffi::c_void;

        self.raw.Base.EntityChunkCount = 1;
        self.raw.Base.pEntityChunks = &mut *chunk;

        self.data_chunks = chunk;
    }
}

pub struct RequestQueue {
    h: HANDLE,
    /// Completions are delivered by the Win32 thread pool.
    ///
    /// A request queue is shared across tasks on a multi-threaded runtime, so
    /// the caller-driven `Proactor` -- which is `!Send` and needs someone to
    /// poll it -- would not fit.
    io: Option<ThreadPoolIo>,
}

// resp should be safe
unsafe impl Send for RequestQueue {}
unsafe impl Sync for RequestQueue {}

impl RequestQueue {
    pub fn new() -> Result<RequestQueue, Error> {
        let mut h: HANDLE = HANDLE::default();
        let ec = unsafe {
            HttpCreateRequestQueue(G_HTTP_VERSION, None, None, None, std::ptr::addr_of_mut!(h))
        };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        if err.code().is_err() {
            Err(err)
        } else {
            assert!(!h.is_invalid());
            let io = ThreadPoolIo::new(h).map_err(Error::from)?;
            Ok(RequestQueue { h, io: Some(io) })
        }
    }

    pub fn bind_url_group(&self, url_group: &UrlGroup) -> Result<(), Error> {
        let info = HTTP_BINDING_INFO {
            Flags: windows::Win32::Networking::HttpServer::HTTP_PROPERTY_FLAGS { _bitfield: 1 },
            RequestQueueHandle: self.h,
        };
        url_group.set_binding_info(&info)
    }

    /// Receive the next request.
    ///
    /// The request is returned pinned: HTTP.sys writes pointers into its own
    /// inline buffer, so moving it out would leave them dangling.
    pub fn receive_request(
        &self,
        requestid: u64,
        flags: HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
    ) -> Submit<ReceiveRequest> {
        self.io
            .as_ref()
            .expect("request queue is open")
            .submit(ReceiveRequest::new(self.h, requestid, flags))
    }

    /// Send a response, taking ownership of it for the operation's duration.
    pub fn send_response(
        &self,
        requestid: u64,
        flags: u32,
        response: Response,
    ) -> Submit<SendResponse> {
        self.io
            .as_ref()
            .expect("request queue is open")
            .submit(SendResponse::new(self.h, requestid, flags, response))
    }

    /// Await the next request, returning it with the transferred byte count.
    pub async fn async_receive_request(
        &self,
        requestid: u64,
        flags: HTTP_RECEIVE_HTTP_REQUEST_FLAGS,
    ) -> OpResult<usize, Pin<Box<Request>>> {
        self.receive_request(requestid, flags)
            .await
            .map_state(|op| {
                use crate::iocp::IntoInner;
                op.into_inner()
            })
    }

    /// Await sending a response, getting it back afterwards.
    pub async fn async_send_response(
        &self,
        requestid: u64,
        flags: u32,
        response: Response,
    ) -> OpResult<usize, Response> {
        self.send_response(requestid, flags, response)
            .await
            .map_state(|op| {
                use crate::iocp::IntoInner;
                op.into_inner()
            })
    }

    pub fn close(&mut self) {
        if self.h.is_invalid() {
            return;
        }
        // Release the thread-pool registration first: it cancels and drains
        // outstanding operations, so the kernel is no longer holding pointers
        // into them when the handle closes.
        self.io = None;
        let ec = unsafe { HttpCloseRequestQueue(self.h) };
        let err = Error::from(HRESULT(ec.try_into().unwrap()));
        assert_eq!(err, Error::empty());
        self.h = HANDLE::default();
    }
}

impl Drop for RequestQueue {
    fn drop(&mut self) {
        self.close()
    }
}
