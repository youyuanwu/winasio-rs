// use winhttp_rs::*;

#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

pub type HInternet = crate::winhttpw::HInternet;
pub type AccessType = crate::winhttpw::AccessType;
pub type WinHttpOpenFlag = crate::winhttpw::WinHttpOpenFlag;

pub const WINHTTP_NO_PROXY_NAME: Option<Vec<u8>> = None;
pub const WINHTTP_NO_PROXY_BYPASS: Option<Vec<u8>> = None;
pub const WINHTTP_NO_REFERER: Option<Vec<u8>> = None;

pub const INTERNET_DEFAULT_HTTPS_PORT: u16 = crate::winhttpw::INTERNET_DEFAULT_HTTPS_PORT;

pub const WINHTTP_DEFAULT_ACCEPT_TYPES: Option<Vec<Vec<u8>>> = None;
pub type WinHttpOpenRequestFlag = crate::winhttpw::WinHttpOpenRequestFlag;

pub const WINHTTP_NO_ADDITIONAL_HEADERS: Option<Vec<u8>> = None;
pub const WINHTTP_NO_REQUEST_DATA: Option<&mut Vec<i8>> = None;

fn OptionNarrowToWide(narrow: Option<Vec<u8>>) -> Option<Vec<u16>> {
    return match narrow {
        None => None,
        Some(x) => Some(NarrowToWide(x)),
    };
}

fn NarrowToWide(narrow: Vec<u8>) -> Vec<u16> {
    let s = String::from_utf8(narrow).unwrap();
    return s.encode_utf16().collect();
}

fn VecNarrowToWide(narrow: Vec<Vec<u8>>) -> Vec<Vec<u16>> {
    return narrow.into_iter().map(|v| NarrowToWide(v)).collect();
}

pub fn WinHttpOpen(
    pszAgentW: Option<Vec<u8>>,
    dwAccessType: AccessType,
    pszProxyW: Option<Vec<u8>>,
    pszProxyBypassW: Option<Vec<u8>>,
    dwFlags: WinHttpOpenFlag,
) -> Result<HInternet, win32_error::Win32Error> {
    return crate::winhttpw::WinHttpOpen(
        OptionNarrowToWide(pszAgentW),
        dwAccessType,
        OptionNarrowToWide(pszProxyW),
        OptionNarrowToWide(pszProxyBypassW),
        dwFlags,
    );
}

pub fn WinHttpConnect(
    hSession: &HInternet,
    pswzServerName: Vec<u8>,
    nServerPort: u16,
    dwReserved: u32,
) -> Result<HInternet, win32_error::Win32Error> {
    return crate::winhttpw::WinHttpConnect(
        hSession,
        NarrowToWide(pswzServerName),
        nServerPort,
        dwReserved,
    );
}

pub fn WinHttpOpenRequest(
    hConnect: &HInternet,
    pwszVerb: Vec<u8>,               // TODO: maybe use enum?
    pwszObjectName: Option<Vec<u8>>, // path name
    pwszVersion: Option<Vec<u8>>,    // HTTP/1.1
    pwszReferrer: Option<Vec<u8>>,
    ppwszAcceptTypes: Option<Vec<Vec<u8>>>,
    dwFlags: WinHttpOpenRequestFlag,
) -> Result<HInternet, win32_error::Win32Error> {
    return crate::winhttpw::WinHttpOpenRequest(
        hConnect,
        NarrowToWide(pwszVerb),
        OptionNarrowToWide(pwszObjectName),
        OptionNarrowToWide(pwszVersion),
        OptionNarrowToWide(pwszReferrer),
        match ppwszAcceptTypes {
            None => None,
            Some(x) => Some(VecNarrowToWide(x)),
        },
        dwFlags,
    );
}

pub fn WinHttpSendRequest(
    hRequest: &HInternet,
    lpszHeaders: Option<Vec<u8>>,
    dwHeadersLength: u32,
    lpOptional: Option<&mut Vec<i8>>, // body
    dwOptionalLength: Option<u32>,    // body length
    dwTotalLength: Option<u32>,       // content-length
    dwContext: Option<usize>,         // DWORD_PTR TODO: test this
) -> Result<(), win32_error::Win32Error> {
    return crate::winhttpw::WinHttpSendRequest(
        hRequest,
        OptionNarrowToWide(lpszHeaders),
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
    return crate::winhttpw::WinHttpReceiveResponse(hRequest, lpReserved);
}

pub fn WinHttpQueryDataAvailable(
    hRequest: &HInternet,
    lpdwNumberOfBytesAvailable: &mut u32,
) -> Result<(), win32_error::Win32Error> {
    return crate::winhttpw::WinHttpQueryDataAvailable(hRequest, lpdwNumberOfBytesAvailable);
}

pub fn WinHttpReadData(
    hRequest: &HInternet,
    lpBuffer: &mut Vec<i8>,
    dwNumberOfBytesToRead: u32,
    lpdwNumberOfBytesRead: &mut u32,
) -> Result<(), win32_error::Win32Error> {
    return crate::winhttpw::WinHttpReadData(
        hRequest,
        lpBuffer,
        dwNumberOfBytesToRead,
        lpdwNumberOfBytesRead,
    );
}

pub fn WinHttpCloseHandle(handle: HInternet) -> bool {
    return crate::winhttpw::WinHttpCloseHandle(handle);
}
