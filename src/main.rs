use winapi::shared::minwindef::*;
use winapi::shared::ntdef::CHAR;
use winapi::shared::ntdef::LPSTR;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::winhttp::*;

use wchar::wch;

fn main() {
    println!("Start");
    let mut dw_size: DWORD;
    let mut dw_downloaded: DWORD = 0;
    let mut psz_out_buffer: LPSTR;
    let mut b_results: bool = false;
    let h_session: HINTERNET;
    let mut h_connect: HINTERNET = std::ptr::null_mut();
    let mut h_request: HINTERNET = std::ptr::null_mut();

    unsafe {
        h_session = WinHttpOpen(
            wch!("Rust\0").as_ptr(),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            std::ptr::null(), //WINHTTP_NO_PROXY_NAME,
            std::ptr::null(), // WINHTTP_NO_PROXY_BYPASS,
            0,
        );
    }

    if h_session != std::ptr::null_mut() {
        unsafe {
            h_connect = WinHttpConnect(
                h_session,
                wch!("api.github.com\0").as_ptr(),
                INTERNET_DEFAULT_HTTPS_PORT.try_into().unwrap(),
                0,
            );
        }
    } else {
        eprintln!("fail to open session");
    }

    if h_connect != std::ptr::null_mut() {
        unsafe {
            h_request = WinHttpOpenRequest(
                h_connect,
                wch!("GET\0").as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),     //WINHTTP_NO_REFERER,
                std::ptr::null_mut(), //WINHTTP_DEFAULT_ACCEPT_TYPES,
                WINHTTP_FLAG_SECURE,
            );
        }
    } else {
        eprintln!("fail to connect");
    }

    if h_request != std::ptr::null_mut() {
        unsafe {
            b_results = WinHttpSendRequest(
                h_request,
                std::ptr::null(), // WINHTTP_NO_ADDITIONAL_HEADERS,
                0,
                std::ptr::null_mut(), //WINHTTP_NO_REQUEST_DATA,
                0,
                0,
                0,
            ) == 1;
        }
    } else {
        eprintln!("fail to open request");
    }

    if b_results {
        unsafe {
            b_results = WinHttpReceiveResponse(h_request, std::ptr::null_mut()) == 1;
        }
    } else {
        eprintln!("fail to send request");
    }

    if b_results {
        loop {
            // Check for available data.
            dw_size = 0;
            let dw_size_ptr: *mut u32 = &mut dw_size;
            if unsafe { WinHttpQueryDataAvailable(h_request, dw_size_ptr) } == 0 {
                println!("Error {} in WinHttpQueryDataAvailable.\n", unsafe {
                    GetLastError()
                });
            }
            // println!("got dw_size {}", dw_size);
            if dw_size == 0 {
                break;
            }
            // Allocate space for the buffer.
            let mut vec: Vec<CHAR> = vec![0; (dw_size + 1) as usize];

            psz_out_buffer = vec.as_mut_ptr();

            // Read the data.
            // ZeroMemory(psz_out_buffer, dw_size + 1);

            let dw_downloaded_ptr: *mut u32 = &mut dw_downloaded;
            if unsafe {
                WinHttpReadData(
                    h_request,
                    psz_out_buffer as LPVOID,
                    dw_size,
                    dw_downloaded_ptr,
                )
            } != 1
            {
                print!("Error {} in WinHttpReadData.\n", unsafe { GetLastError() });
            } else {
                vec.pop(); // skip the null terminator
                let res = match String::from_utf8(unsafe { std::mem::transmute(vec) }) {
                    Ok(res) => res,
                    Err(e) => {
                        eprintln!("Parse error");
                        panic!("fail {}", e);
                    }
                };
                print!("{}", res);
            }
        }
    } else {
        eprintln!("Fail to recieve response");
    }
}
