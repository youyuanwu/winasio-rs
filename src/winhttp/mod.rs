// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

pub mod fasync;

use windows::core::Error;
use windows::core::HSTRING;
use windows::Win32::Networking::WinHttp::*;

// wrapper for raw handle
struct HInternet {
    handle: *mut ::core::ffi::c_void,
}

// winhttp session
pub struct HSession {
    h: HInternet,
}

// winhttp connection
pub struct HConnect {
    h: HInternet,
}

// winhttp request
pub struct HRequest {
    h: HInternet,
}

impl HSession {
    pub fn new(
        agent: HSTRING,
        access_type: WINHTTP_ACCESS_TYPE,
        proxy: HSTRING,
        proxy_bypass: HSTRING,
        dwflags: u32,
    ) -> Result<HSession, Error> {
        let hi = open_session(agent, access_type, proxy, proxy_bypass, dwflags)?;
        Ok(HSession { h: hi })
    }

    // create winhttp connection
    pub fn connect(
        &self,
        servername: HSTRING,
        serverport: INTERNET_PORT,
    ) -> Result<HConnect, Error> {
        let conn = connect(&self.h, servername, serverport)?;
        Ok(HConnect { h: conn })
    }
}

impl HConnect {
    // creates winhttp request
    pub fn open_request(
        &self,
        verb: HSTRING,
        object_name: HSTRING,
        version: HSTRING,
        referer: HSTRING,
        accept_types: Option<Vec<HSTRING>>,
        dwflags: WINHTTP_OPEN_REQUEST_FLAGS,
    ) -> Result<HRequest, Error> {
        let req = open_request(
            &self.h,
            verb,
            object_name,
            version,
            referer,
            accept_types,
            dwflags,
        )?;
        Ok(HRequest { h: req })
    }
}

impl HRequest {
    // works on connection handle
    // header example: L"Content-Length: 68719476735\r\n"
    // optional is body
    pub fn send(
        &self,
        headers: HSTRING,
        optional: &[u8],
        total_length: u32,
        context: usize,
    ) -> Result<(), Error> {
        // prepare header
        let mut headers_op: Option<&[u16]> = None;
        if !headers.is_empty() {
            headers_op = Some(headers.as_wide());
        }
        let mut optional_op: Option<*const ::core::ffi::c_void> = None;
        // prepare optional body
        if !optional.is_empty() {
            optional_op = Some(optional.as_ptr() as *mut std::ffi::c_void);
        }

        let ok = unsafe {
            WinHttpSendRequest(
                self.h.handle,
                headers_op,
                optional_op,
                optional.len() as u32,
                total_length,
                context,
            )
        };
        ok.ok()
    }

    pub fn receieve_response(&self) -> Result<(), Error> {
        let ok = unsafe { WinHttpReceiveResponse(self.h.handle, std::ptr::null_mut()) };
        ok.ok()
    }

    pub fn query_data_available(
        &self,
        lpdwnumberofbytesavailable: Option<&mut u32>,
    ) -> Result<(), Error> {
        let numberofbytesavailable_op: *mut u32 = match lpdwnumberofbytesavailable {
            Some(op) => op,
            None => std::ptr::null_mut(),
        };
        let ok = unsafe { WinHttpQueryDataAvailable(self.h.handle, numberofbytesavailable_op) };
        ok.ok()
    }

    pub fn read_data(
        &self,
        buffer: &mut [u8],
        dwnumberofbytestoread: u32,
        lpdwnumberofbytesread: Option<&mut u32>,
    ) -> Result<(), Error> {
        let numberofbytesread_op: *mut u32 = match lpdwnumberofbytesread {
            Some(op) => op,
            None => std::ptr::null_mut(),
        };

        let ok = unsafe {
            WinHttpReadData(
                self.h.handle,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                dwnumberofbytestoread,
                numberofbytesread_op,
            )
        };
        ok.ok()
    }

    pub fn write_data(
        &self,
        buf: &[u8],
        dwnumberofbytestowrite: u32,
        lpdwnumberofbyteswritten: Option<&mut u32>,
    ) -> Result<(), Error> {
        let len = buf.len();
        let lpdwnumberofbyteswritten_op: *mut u32 = match lpdwnumberofbyteswritten {
            Some(op) => op,
            None => std::ptr::null_mut(),
        };
        assert!(dwnumberofbytestowrite as usize <= len);
        let ok = unsafe {
            WinHttpWriteData(
                self.h.handle,
                Some(buf.as_ptr() as *const std::ffi::c_void),
                dwnumberofbytestowrite,
                lpdwnumberofbyteswritten_op,
            )
        };
        ok.ok()
    }

    pub fn set_status_callback(
        &self,
        lpfninternetcallback: WINHTTP_STATUS_CALLBACK,
        dwnotificationflags: u32,
        dwreserved: usize,
    ) -> WINHTTP_STATUS_CALLBACK {
        unsafe {
            WinHttpSetStatusCallback(
                self.h.handle,
                lpfninternetcallback,
                dwnotificationflags,
                dwreserved,
            )
        }
    }
}

// open session
fn open_session(
    agent: HSTRING,
    access_type: WINHTTP_ACCESS_TYPE,
    proxy: HSTRING,
    proxy_bypass: HSTRING,
    dwflags: u32,
) -> Result<HInternet, Error> {
    let h = unsafe { WinHttpOpen(&agent, access_type, &proxy, &proxy_bypass, dwflags) };
    if h.is_null() {
        return Err(Error::from_win32());
    }
    Ok(HInternet { handle: h })
}

fn connect(
    session: &HInternet,
    servername: HSTRING,
    serverport: INTERNET_PORT,
) -> Result<HInternet, Error> {
    let h = unsafe {
        WinHttpConnect(
            session.handle,
            &servername,
            serverport,
            /*reserved */ 0,
        )
    };
    if h.is_null() {
        return Err(Error::from_win32());
    }
    Ok(HInternet { handle: h })
}

fn open_request(
    connect: &HInternet,
    verb: HSTRING,
    object_name: HSTRING,
    version: HSTRING,
    referer: HSTRING,
    accept_types: Option<Vec<HSTRING>>,
    dwflags: WINHTTP_OPEN_REQUEST_FLAGS,
) -> Result<HInternet, Error> {
    // transform accept_types to c array ptr.
    let mut at: Vec<::windows::core::PCWSTR> = match accept_types {
        // const wchar_t *att[] = { L"text/plain", L"multipart/signed", NULL };
        Some(v) => {
            let mut out = v
                .into_iter()
                .map(|s| ::windows::core::PCWSTR::from_raw(s.as_ptr()))
                .collect::<Vec<_>>();
            out.push(::windows::core::PCWSTR::from_raw(std::ptr::null())); // ending of c array
            out
        }
        None => Vec::new(),
    };

    let mut temp_ptr: *mut ::windows::core::PCWSTR = std::ptr::null_mut();
    if !at.is_empty() {
        temp_ptr = at.as_mut_ptr();
    }

    // there might be a bug in the api where PCWSTR should be accepted
    let at_ptr: *mut ::windows::core::PWSTR = unsafe { ::core::mem::transmute(temp_ptr) };

    let h = unsafe {
        WinHttpOpenRequest(
            connect.handle,
            &verb,
            &object_name,
            &version,
            &referer,
            at_ptr,
            dwflags,
        )
    };
    if h.is_null() {
        return Err(Error::from_win32());
    }
    Ok(HInternet { handle: h })
}

impl Drop for HInternet {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        let ok = unsafe { WinHttpCloseHandle(self.handle) };
        if !ok.as_bool() {
            let e = Error::from_win32();
            assert!(e.code().is_ok(), "Error: {}", e);
        }
    }
}
