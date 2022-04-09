// use winapi::shared::minwindef::*;
// use winapi::shared::ntdef::CHAR;
// use winapi::shared::ntdef::LPSTR;
// use winapi::um::errhandlingapi::GetLastError;
// use winapi::um::winhttp::*;

// use wchar::wch;
// mod lib;

use winhttp_rs::winhttp::*;

fn main() {
    println!("Open");
    let mut dw_size: u32;
    let mut dw_downloaded: u32 = 0;
    let h_session: HInternet;
    let h_connect: HInternet;
    let h_request: HInternet;

    h_session = WinHttpOpen(
        Some(String::from("RUST2")),
        AccessType::WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
        WINHTTP_NO_PROXY_NAME,
        WINHTTP_NO_PROXY_BYPASS,
        WinHttpOpenFlag::new(),
    )
    .unwrap();

    println!("Connect");
    h_connect = WinHttpConnect(
        &h_session,
        String::from("api.github.com"),
        INTERNET_DEFAULT_HTTPS_PORT,
        0,
    )
    .unwrap();

    println!("open request");
    h_request = WinHttpOpenRequest(
        &h_connect,
        String::from("GET"),
        None, // Some(String::from("")), //None, //Some(wchar::wchz!("").to_vec()),
        Some(String::from("HTTP/1.1")),
        WINHTTP_NO_REFERER,
        Some(vec![String::from("application/json")]), // String::from("text/plain")
        WinHttpOpenRequestFlag::new().set_WINHTTP_FLAG_SECURE(),
    )
    .unwrap();

    println!("Send Request");
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

    println!("RecieveResponse");
    WinHttpReceiveResponse(&h_request, None).unwrap();

    loop {
        println!("Query data available");
        // Check for available data.
        dw_size = 0;
        WinHttpQueryDataAvailable(&h_request, &mut dw_size).unwrap();
        println!("got dw_size {}", dw_size);
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
