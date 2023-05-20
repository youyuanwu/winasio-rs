// replacement using windows-rs

use windows::core::Error;
use windows::core::HSTRING;
use windows::Win32::Networking::WinHttp::*;

pub struct HInternet {
    handle: *mut ::core::ffi::c_void,
}

// open session
pub fn open_session(
    agent: HSTRING,
    access_type: WINHTTP_ACCESS_TYPE,
    proxy: HSTRING,
    proxy_bypass: HSTRING,
    dwflags: u32,
) -> Result<HInternet, Error> {
    let h = unsafe { WinHttpOpen(&agent, access_type, &proxy, &proxy_bypass, dwflags) };
    if h == std::ptr::null_mut() {
        return Err(Error::from_win32());
    }
    Ok(HInternet { handle: h })
}

pub fn connect(
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
    if h == std::ptr::null_mut() {
        return Err(Error::from_win32());
    }
    Ok(HInternet { handle: h })
}

pub fn open_request(
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
    if at.len() != 0 {
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
    if h == std::ptr::null_mut() {
        return Err(Error::from_win32());
    }
    Ok(HInternet { handle: h })
}

impl HInternet {
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
                self.handle,
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
        let ok = unsafe { WinHttpReceiveResponse(self.handle, std::ptr::null_mut()) };
        ok.ok()
    }

    pub fn query_data_available(&self, lpdwnumberofbytesavailable: &mut u32) -> Result<(), Error> {
        let ok = unsafe { WinHttpQueryDataAvailable(self.handle, lpdwnumberofbytesavailable) };
        ok.ok()
    }

    pub fn read_data(
        &self,
        buffer: &mut [u8],
        dwnumberofbytestoread: u32,
        lpdwnumberofbytesread: &mut u32,
    ) -> Result<(), Error> {
        let ok = unsafe {
            WinHttpReadData(
                self.handle,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                dwnumberofbytestoread,
                lpdwnumberofbytesread,
            )
        };
        ok.ok()
    }
}

impl Drop for HInternet {
    fn drop(&mut self) {
        if self.handle == std::ptr::null_mut() {
            return;
        }
        let ok = unsafe { WinHttpCloseHandle(self.handle) };
        if !ok.as_bool() {
            let e = Error::from_win32();
            assert!(e.code().is_ok(), "Error: {}", e);
        }
    }
}
