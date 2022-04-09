#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

pub type HInternet = crate::winhttpn::HInternet;
pub type AccessType = crate::winhttpn::AccessType;
pub type WinHttpOpenFlag = crate::winhttpn::WinHttpOpenFlag;

pub const WINHTTP_NO_PROXY_NAME: Option<String> = None;
pub const WINHTTP_NO_PROXY_BYPASS: Option<String> = None;
pub const WINHTTP_NO_REFERER: Option<String> = None;

pub const INTERNET_DEFAULT_HTTPS_PORT: u16 = crate::winhttpn::INTERNET_DEFAULT_HTTPS_PORT;

pub const WINHTTP_DEFAULT_ACCEPT_TYPES: Option<Vec<Vec<String>>> = None;
pub type WinHttpOpenRequestFlag = crate::winhttpn::WinHttpOpenRequestFlag;
pub const WINHTTP_NO_ADDITIONAL_HEADERS: Option<String> = None;
pub const WINHTTP_NO_REQUEST_DATA: Option<&mut Vec<i8>> = None;

fn OptionStringToVecN(x: Option<String>) -> Option<Vec<u8>> {
    match x {
        None => None,
        Some(s) => Some(StringToVecN(s)),
    }
}
fn StringToVecN(x: String) -> Vec<u8> {
    // because the c interface needs null terminator
    let mut y = x.as_bytes().to_vec();
    let last = y.last().copied();
    match last {
        Some(x) => {
            if x != 0 as u8 {
                // no terminator
                y.push(0);
            }
        }
        None => {
            assert_eq!(0, y.len());
            y.push(0); // empty c string
        }
    }
    return y;
}

fn VecStringToVecN(narrow: Vec<String>) -> Vec<Vec<u8>> {
    return narrow.into_iter().map(|v| StringToVecN(v)).collect();
}

pub fn WinHttpOpen(
    pszAgentW: Option<String>,
    dwAccessType: AccessType,
    pszProxyW: Option<String>,
    pszProxyBypassW: Option<String>,
    dwFlags: WinHttpOpenFlag,
) -> Result<HInternet, win32_error::Win32Error> {
    return crate::winhttpn::WinHttpOpen(
        OptionStringToVecN(pszAgentW),
        dwAccessType,
        OptionStringToVecN(pszProxyW),
        OptionStringToVecN(pszProxyBypassW),
        dwFlags,
    );
}

pub fn WinHttpConnect(
    hSession: &HInternet,
    pswzServerName: String,
    nServerPort: u16,
    dwReserved: u32,
) -> Result<HInternet, win32_error::Win32Error> {
    return crate::winhttpn::WinHttpConnect(
        hSession,
        StringToVecN(pswzServerName),
        nServerPort,
        dwReserved,
    );
}

pub fn WinHttpOpenRequest(
    hConnect: &HInternet,
    pwszVerb: String,               // TODO: maybe use enum?
    pwszObjectName: Option<String>, // path name
    pwszVersion: Option<String>,    // HTTP/1.1
    pwszReferrer: Option<String>,
    ppwszAcceptTypes: Option<Vec<String>>,
    dwFlags: WinHttpOpenRequestFlag,
) -> Result<HInternet, win32_error::Win32Error> {
    return crate::winhttpn::WinHttpOpenRequest(
        hConnect,
        StringToVecN(pwszVerb),
        OptionStringToVecN(pwszObjectName),
        OptionStringToVecN(pwszVersion),
        OptionStringToVecN(pwszReferrer),
        match ppwszAcceptTypes {
            None => None,
            Some(x) => Some(VecStringToVecN(x)),
        },
        dwFlags,
    );
}

pub fn WinHttpSendRequest(
    hRequest: &HInternet,
    lpszHeaders: Option<String>,
    dwHeadersLength: u32,
    lpOptional: Option<&mut Vec<i8>>, // body
    dwOptionalLength: Option<u32>,    // body length
    dwTotalLength: Option<u32>,       // content-length
    dwContext: Option<usize>,         // DWORD_PTR TODO: test this
) -> Result<(), win32_error::Win32Error> {
    return crate::winhttpn::WinHttpSendRequest(
        hRequest,
        OptionStringToVecN(lpszHeaders),
        dwHeadersLength,
        lpOptional,
        dwOptionalLength,
        dwTotalLength,
        dwContext,
    );
}

pub fn WinHttpReceiveResponse(
    hRequest: &HInternet,
    lpReserved: Option<u32>, //LPVOID,
) -> Result<(), win32_error::Win32Error> {
    return crate::winhttpn::WinHttpReceiveResponse(hRequest, lpReserved);
}

pub fn WinHttpQueryDataAvailable(
    hRequest: &HInternet,
    lpdwNumberOfBytesAvailable: &mut u32,
) -> Result<(), win32_error::Win32Error> {
    return crate::winhttpn::WinHttpQueryDataAvailable(hRequest, lpdwNumberOfBytesAvailable);
}

pub fn WinHttpReadData(
    hRequest: &HInternet,
    lpBuffer: &mut Vec<i8>,
    dwNumberOfBytesToRead: u32,
    lpdwNumberOfBytesRead: &mut u32,
) -> Result<(), win32_error::Win32Error> {
    return crate::winhttpn::WinHttpReadData(
        hRequest,
        lpBuffer,
        dwNumberOfBytesToRead,
        lpdwNumberOfBytesRead,
    );
}

pub fn WinHttpCloseHandle(handle: HInternet) -> bool {
    return crate::winhttpn::WinHttpCloseHandle(handle);
}
