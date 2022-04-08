// use winapi::shared::minwindef::*;
// use winapi::shared::ntdef::CHAR;
// use winapi::shared::ntdef::LPSTR;
// use winapi::um::errhandlingapi::GetLastError;
// use winapi::um::winhttp::*;

// use wchar::wch;
// mod lib;

use winhttp_rs::winhttpw::*;

fn main() {
    println!("Start");
    let mut dw_size: u32;
    let mut dw_downloaded: u32 = 0;
    let h_session: HInternet;
    let h_connect: HInternet;
    let h_request: HInternet;

    h_session = WinHttpOpen(
        Some(wchar::wchz!("RUST1").to_vec()),
        AccessType::WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
        WINHTTP_NO_PROXY_NAME,
        WINHTTP_NO_PROXY_BYPASS,
        WinHttpOpenFlag::new(),
    )
    .unwrap();

    h_connect = WinHttpConnect(
        &h_session,
        wchar::wchz!("api.github.com").to_vec(),
        INTERNET_DEFAULT_HTTPS_PORT,
        0,
    )
    .unwrap();

    h_request = WinHttpOpenRequest(
        &h_connect,
        wchar::wchz!("GET").to_vec(),
        None, //Some(wchar::wchz!("").to_vec()),
        Some(wchar::wchz!("HTTP/1.1").to_vec()),
        WINHTTP_NO_REFERER,
        Some(vec![wchar::wchz!("application/json").to_vec()]), //wchar::wchz!("text/plain").to_vec(),
        WinHttpOpenRequestFlag::new().set_WINHTTP_FLAG_SECURE(),
    )
    .unwrap();

    WinHttpSendRequest(
        &h_request,
        WINHTTP_NO_ADDITIONAL_HEADERS,
        0,
        WINHTTP_NO_REQUEST_DATA,
        None,
        None,
        None,
    )
    .unwrap();

    WinHttpReceiveResponse(&h_request, None).unwrap();

    loop {
        // Check for available data.
        dw_size = 0;
        WinHttpQueryDataAvailable(&h_request, &mut dw_size).unwrap();
        // println!("got dw_size {}", dw_size);
        if dw_size == 0 {
            break;
        }
        // Allocate space for the buffer.
        let mut vec: Vec<std::os::raw::c_char> = vec![0; (dw_size + 1) as usize];

        // Read the data.
        // ZeroMemory(psz_out_buffer, dw_size + 1);
        WinHttpReadData(&h_request, &mut vec, dw_size, &mut dw_downloaded).unwrap();

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
