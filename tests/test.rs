#[cfg(test)]
mod tests {
    use windows::{core::HSTRING, Win32::Networking::WinHttp::*};
    use winhttp_rs::{self, winhttp::*};

    #[test]
    fn simple() {
        let session = open_session(
            HSTRING::from("Rust2"),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            HSTRING::new(),
            HSTRING::new(),
            0,
        )
        .unwrap();

        let conn = connect(
            &session,
            HSTRING::from("api.github.com"),
            INTERNET_DEFAULT_HTTPS_PORT,
        )
        .unwrap();

        let req = open_request(
            &conn,
            HSTRING::from("GET"),
            HSTRING::new(),
            HSTRING::from("HTTP/1.1"),
            HSTRING::new(),
            Some(vec![HSTRING::from("application/json")]),
            WINHTTP_FLAG_SECURE,
        )
        .unwrap();

        req.send(HSTRING::new(), &[], 0, 0).unwrap();

        req.receieve_response().unwrap();

        loop {
            let mut len = 0;
            req.query_data_available(&mut len).unwrap();
            if len == 0 {
                break;
            }
            let mut buffer: Vec<u8> = vec![0; len as usize];
            let mut lpdwnumberofbytesread: u32 = 0;
            req.read_data(buffer.as_mut_slice(), len, &mut lpdwnumberofbytesread)
                .unwrap();

            let s = String::from_utf8_lossy(&buffer);
            print!("{}", s);
        }
    }
}
