// include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

pub mod winhttp {

    // #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    // #![allow(dead_code)]
    // #![allow(unaligned_references)]
    // #![allow(deref_nullptr)]

    // use wchar::wch;

    pub struct HInternet {
        handle: winapi::um::winhttp::HINTERNET,
    }

    impl HInternet {
        pub fn is_null(&self) -> bool {
            return self.handle == std::ptr::null_mut();
        }

        pub fn new() -> HInternet {
            return HInternet {
                handle: std::ptr::null_mut(),
            };
        }
    }

    // #[derive(Copy, Clone)]
    pub enum AccessType {
        WINHTTP_ACCESS_TYPE_NO_PROXY = winapi::um::winhttp::WINHTTP_ACCESS_TYPE_NO_PROXY as isize,
        WINHTTP_ACCESS_TYPE_DEFAULT_PROXY =
            winapi::um::winhttp::WINHTTP_ACCESS_TYPE_DEFAULT_PROXY as isize,
        WINHTTP_ACCESS_TYPE_NAMED_PROXY =
            winapi::um::winhttp::WINHTTP_ACCESS_TYPE_NAMED_PROXY as isize,
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY =
            winapi::um::winhttp::WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY as isize,
    }

    pub struct WinHttpOpenFlag {
        dwFlags: u32,
    }

    impl WinHttpOpenFlag {
        pub fn new() -> WinHttpOpenFlag {
            return WinHttpOpenFlag { dwFlags: 0 };
        }
        pub fn set_WINHTTP_FLAG_ASYNC(mut self) -> WinHttpOpenFlag {
            self.dwFlags |= winapi::um::winhttp::WINHTTP_FLAG_ASYNC;
            return self;
        }
        pub fn set_WINHTTP_FLAG_SECURE_DEFAULTS(mut self) -> WinHttpOpenFlag {
            self.dwFlags |= 0x30000000;
            return self;
        }
    }

    pub const WINHTTP_NO_PROXY_NAME: Option<Vec<wchar::wchar_t>> = None;
    pub const WINHTTP_NO_PROXY_BYPASS: Option<Vec<wchar::wchar_t>> = None;
    pub const WINHTTP_NO_REFERER: Option<Vec<wchar::wchar_t>> = None;

    pub const INTERNET_DEFAULT_HTTPS_PORT: u16 = winapi::um::winhttp::INTERNET_DEFAULT_HTTPS_PORT;

    pub fn WinHttpOpen(
        pszAgentW: Option<Vec<wchar::wchar_t>>,
        dwAccessType: AccessType,
        pszProxyW: Option<Vec<wchar::wchar_t>>,
        pszProxyBypassW: Option<Vec<wchar::wchar_t>>,
        dwFlags: WinHttpOpenFlag,
    ) -> Result<HInternet, win32_error::Win32Error> {
        let h_session: winapi::um::winhttp::HINTERNET;
        unsafe {
            h_session = winapi::um::winhttp::WinHttpOpen(
                match pszAgentW {
                    Some(x) => x.as_ptr(),
                    None => std::ptr::null(),
                },
                dwAccessType as u32,
                match pszProxyW {
                    Some(x) => x.as_ptr(),
                    WINHTTP_NO_PROXY_NAME => std::ptr::null(),
                },
                match pszProxyBypassW {
                    Some(x) => x.as_ptr(),
                    WINHTTP_NO_PROXY_BYPASS => std::ptr::null(),
                },
                dwFlags.dwFlags,
            );
        }

        let handleRet = HInternet { handle: h_session };
        if handleRet.is_null() {
            return Err(win32_error::Win32Error::new());
        } else {
            return Ok(handleRet);
        }
    }

    pub fn WinHttpConnect(
        hSession: &HInternet,
        pswzServerName: Vec<wchar::wchar_t>,
        nServerPort: u16,
        dwReserved: u32,
    ) -> Result<HInternet, win32_error::Win32Error> {
        let h_connect: winapi::um::winhttp::HINTERNET;

        unsafe {
            h_connect = winapi::um::winhttp::WinHttpConnect(
                hSession.handle,
                pswzServerName.as_ptr(),
                nServerPort,
                dwReserved,
            );
        }

        let ret = HInternet { handle: h_connect };
        if ret.is_null() {
            return Err(win32_error::Win32Error::new());
        } else {
            return Ok(ret);
        }
    }

    pub const WINHTTP_DEFAULT_ACCEPT_TYPES: Option<Vec<Vec<wchar::wchar_t>>> = None;
    // pub const WINHTTP_NO_REQUEST_DATA: Option<Vec<wchar::wchar_t>> = None;

    pub struct WinHttpOpenRequestFlag {
        dwFlags: u32,
    }

    impl WinHttpOpenRequestFlag {
        pub fn new() -> WinHttpOpenRequestFlag {
            return WinHttpOpenRequestFlag { dwFlags: 0 };
        }
        pub fn set_WINHTTP_FLAG_BYPASS_PROXY_CACHE(mut self) -> WinHttpOpenRequestFlag {
            self.dwFlags |= winapi::um::winhttp::WINHTTP_FLAG_BYPASS_PROXY_CACHE;
            return self;
        }
        pub fn set_WINHTTP_FLAG_ESCAPE_DISABLE(mut self) -> WinHttpOpenRequestFlag {
            self.dwFlags |= winapi::um::winhttp::WINHTTP_FLAG_ESCAPE_DISABLE;
            return self;
        }
        pub fn set_WINHTTP_FLAG_ESCAPE_DISABLE_QUERY(mut self) -> WinHttpOpenRequestFlag {
            self.dwFlags |= winapi::um::winhttp::WINHTTP_FLAG_ESCAPE_DISABLE_QUERY;
            return self;
        }
        pub fn set_WINHTTP_FLAG_ESCAPE_PERCENT(mut self) -> WinHttpOpenRequestFlag {
            self.dwFlags |= winapi::um::winhttp::WINHTTP_FLAG_ESCAPE_PERCENT;
            return self;
        }
        pub fn set_WINHTTP_FLAG_NULL_CODEPAGE(mut self) -> WinHttpOpenRequestFlag {
            self.dwFlags |= winapi::um::winhttp::WINHTTP_FLAG_NULL_CODEPAGE;
            return self;
        }
        pub fn set_WINHTTP_FLAG_REFRESH(mut self) -> WinHttpOpenRequestFlag {
            self.dwFlags |= winapi::um::winhttp::WINHTTP_FLAG_REFRESH;
            return self;
        }
        pub fn set_WINHTTP_FLAG_SECURE(mut self) -> WinHttpOpenRequestFlag {
            self.dwFlags |= winapi::um::winhttp::WINHTTP_FLAG_SECURE;
            return self;
        }
    }

    pub fn WinHttpOpenRequest(
        hConnect: &HInternet,
        pwszVerb: Vec<wchar::wchar_t>, // TODO: maybe use enum?
        pwszObjectName: Option<Vec<wchar::wchar_t>>, // path name
        pwszVersion: Option<Vec<wchar::wchar_t>>, // HTTP/1.1
        pwszReferrer: Option<Vec<wchar::wchar_t>>,
        ppwszAcceptTypes: Option<Vec<Vec<wchar::wchar_t>>>,
        dwFlags: WinHttpOpenRequestFlag,
    ) -> Result<HInternet, win32_error::Win32Error> {
        // const wchar_t *att[] = { L"text/plain", L"multipart/signed", NULL };
        let mut acceptTypes: Vec<*const wchar::wchar_t> = match ppwszAcceptTypes {
            Some(v) => {
                let mut out = v.into_iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
                out.push(std::ptr::null()); // ending of c array
                out
            }
            WINHTTP_DEFAULT_ACCEPT_TYPES => Vec::new(),
        };

        let mut acceptTypesPtr: *mut winapi::shared::ntdef::LPCWSTR = std::ptr::null_mut();
        if acceptTypes.len() != 0 {
            acceptTypesPtr = acceptTypes.as_mut_ptr();
        }

        let h_request: winapi::um::winhttp::HINTERNET;
        unsafe {
            h_request = winapi::um::winhttp::WinHttpOpenRequest(
                hConnect.handle,
                pwszVerb.as_ptr(),
                match pwszObjectName {
                    Some(x) => x.as_ptr(),
                    None => std::ptr::null(),
                },
                match pwszVersion {
                    Some(x) => x.as_ptr(),
                    None => std::ptr::null(),
                },
                match pwszReferrer {
                    Some(x) => x.as_ptr(),
                    WINHTTP_NO_REFERER => std::ptr::null(),
                },
                acceptTypesPtr,
                dwFlags.dwFlags,
            );
        }
        let ret = HInternet { handle: h_request };
        if ret.is_null() {
            return Err(win32_error::Win32Error::new());
        } else {
            return Ok(ret);
        }
    }

    pub const WINHTTP_NO_ADDITIONAL_HEADERS: Option<Vec<wchar::wchar_t>> = None;
    pub const WINHTTP_NO_REQUEST_DATA: Option<&mut Vec<std::os::raw::c_char>> = None;

    pub fn WinHttpSendRequest(
        hRequest: &HInternet,
        lpszHeaders: Option<Vec<wchar::wchar_t>>,
        dwHeadersLength: u32,
        lpOptional: Option<&mut Vec<std::os::raw::c_char>>, // body
        dwOptionalLength: Option<u32>,                      // body length
        dwTotalLength: Option<u32>,                         // content-length
        dwContext: Option<usize>,                           //DWORD_PTR TODO: test this
    ) -> Result<(), win32_error::Win32Error> {
        let b_results: winapi::shared::minwindef::BOOL;
        unsafe {
            b_results = winapi::um::winhttp::WinHttpSendRequest(
                hRequest.handle,
                match lpszHeaders {
                    Some(x) => x.as_ptr(),
                    WINHTTP_NO_ADDITIONAL_HEADERS => std::ptr::null(),
                },
                dwHeadersLength,
                match lpOptional {
                    Some(x) => x.as_mut_ptr() as winapi::shared::minwindef::LPVOID,
                    WINHTTP_NO_REQUEST_DATA => std::ptr::null_mut(),
                },
                match dwOptionalLength {
                    Some(x) => x,
                    None => 0,
                },
                match dwTotalLength {
                    Some(x) => x,
                    None => 0,
                },
                match dwContext {
                    Some(x) => {
                        x // TODO: this should be turn to a ptr
                    }
                    None => 0,
                },
            );
        }
        if b_results != 1 {
            return Err(win32_error::Win32Error::new());
        } else {
            return Ok(());
        }
    }

    pub fn WinHttpReceiveResponse(
        hRequest: &HInternet,
        lpReserved: Option<u32>, //LPVOID,
    ) -> Result<(), win32_error::Win32Error> {
        let b_results: winapi::shared::minwindef::BOOL;
        unsafe {
            b_results = winapi::um::winhttp::WinHttpReceiveResponse(
                hRequest.handle,
                match lpReserved {
                    Some(_) => {
                        panic!("use of reserved param");
                    }
                    None => std::ptr::null_mut(),
                },
            );
        }
        if b_results != 1 {
            return Err(win32_error::Win32Error::new());
        } else {
            return Ok(());
        }
    }

    pub fn WinHttpQueryDataAvailable(
        hRequest: &HInternet,
        lpdwNumberOfBytesAvailable: &mut u32,
    ) -> Result<(), win32_error::Win32Error> {
        let b_results: winapi::shared::minwindef::BOOL;
        let dw_size_ptr: *mut u32 = lpdwNumberOfBytesAvailable;
        unsafe {
            b_results =
                winapi::um::winhttp::WinHttpQueryDataAvailable(hRequest.handle, dw_size_ptr);
        }
        if b_results != 1 {
            return Err(win32_error::Win32Error::new());
        } else {
            return Ok(());
        }
    }

    pub fn WinHttpReadData(
        hRequest: &HInternet,
        lpBuffer: &mut Vec<std::os::raw::c_char>,
        dwNumberOfBytesToRead: u32,
        lpdwNumberOfBytesRead: &mut u32,
    ) -> Result<(), win32_error::Win32Error> {
        let b_results: winapi::shared::minwindef::BOOL;
        let psz_out_buffer = lpBuffer.as_mut_ptr();
        let dw_downloaded_ptr: *mut u32 = lpdwNumberOfBytesRead;
        unsafe {
            b_results = winapi::um::winhttp::WinHttpReadData(
                hRequest.handle,
                psz_out_buffer as winapi::shared::minwindef::LPVOID,
                dwNumberOfBytesToRead,
                dw_downloaded_ptr,
            );
        }
        if b_results != 1 {
            return Err(win32_error::Win32Error::new());
        } else {
            if dwNumberOfBytesToRead != *lpdwNumberOfBytesRead {
                panic!("winhttp data read length mismatch");
            }
            return Ok(());
        }
    }

    pub fn WinHttpCloseHandle(handle: HInternet) -> bool {
        if handle.handle == std::ptr::null_mut() {
            return true;
        }
        return unsafe { winapi::um::winhttp::WinHttpCloseHandle(handle.handle) } == 1;
    }
}
