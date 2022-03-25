use winhttp_rs::winhttpraw::*;
use wchar::{wch, wchz, wchar_t};

fn main() {
    println!("Start");
    let mut dwSize : DWORD = 0;
    let mut dwDownloaded : DWORD = 0;
    let mut pszOutBuffer : LPSTR;
    let mut bResults : bool = false;
    let mut hSession : HINTERNET = std::ptr::null_mut();
    let mut hConnect : HINTERNET = std::ptr::null_mut();
    let mut hRequest : HINTERNET = std::ptr::null_mut();
    
    unsafe{
        hSession = WinHttpOpen(wch!("Rust\0").as_ptr(), 
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            std::ptr::null(), //WINHTTP_NO_PROXY_NAME,
            std::ptr::null(), // WINHTTP_NO_PROXY_BYPASS,
            0
        );
    }

    if hSession != std::ptr::null_mut() {
        unsafe{
        hConnect = WinHttpConnect(hSession, wch!("api.github.com\0").as_ptr(),
            INTERNET_DEFAULT_HTTPS_PORT.try_into().unwrap(), 0);
        }
    }else{
        eprintln!("fail to open session");
    }

    if hConnect != std::ptr::null_mut(){
        unsafe{
            hRequest = WinHttpOpenRequest(hConnect, wch!("GET\0").as_ptr(), 
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),//WINHTTP_NO_REFERER,
            std::ptr::null_mut(), //WINHTTP_DEFAULT_ACCEPT_TYPES,
            WINHTTP_FLAG_SECURE);
        }
    }else{
        eprintln!("fail to connect");
    }

    if hRequest != std::ptr::null_mut() {
        unsafe{
        bResults = WinHttpSendRequest(hRequest,
            std::ptr::null(),// WINHTTP_NO_ADDITIONAL_HEADERS,
            0,
            std::ptr::null_mut(),//WINHTTP_NO_REQUEST_DATA, 
            0,
            0, 0) == 1;
        }
    }else{
        eprintln!("fail to open request");
    }

    if bResults
    {
        unsafe{
            bResults = WinHttpReceiveResponse(hRequest, std::ptr::null_mut()) == 1;
        }
    }else{
        eprintln!("fail to send request");
    }

    if bResults
    {
        while true
        {
            // Check for available data.
            dwSize = 0;
            let dwSize_ptr: *mut u32 = &mut dwSize;
            if unsafe{WinHttpQueryDataAvailable(hRequest, dwSize_ptr)} == 0{
                
                println!("Error {} in WinHttpQueryDataAvailable.\n", unsafe{GetLastError()});
            }
            // println!("got dwSize {}", dwSize);
            if dwSize == 0 {
                break;
            }
            // Allocate space for the buffer.
            let mut vec: Vec<CHAR> = vec![0; (dwSize + 1) as usize];
            
            pszOutBuffer = vec.as_mut_ptr();

            // Read the data.
            // ZeroMemory(pszOutBuffer, dwSize + 1);

            let dwDownloaded_ptr: * mut u32 = &mut dwDownloaded;
            if unsafe{WinHttpReadData(hRequest, pszOutBuffer as LPVOID ,
                dwSize, dwDownloaded_ptr)} != 1
            {
                print!("Error {} in WinHttpReadData.\n", unsafe{GetLastError()});
            }
            else
            {
                vec.pop(); // skip the null terminator
                let res = match String::from_utf8(unsafe{std::mem::transmute(vec)}){ 
                    Ok(res)  => res,
                    Err(e) => { eprintln!("Parse error"); panic!("fail");},
                };
                print!("{}", res);
            }
        }
    }else{
        eprintln!("Fail to recieve response");
    }
}
