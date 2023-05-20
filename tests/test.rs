#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::sync::oneshot::{self, *};
    use windows::{core::HSTRING, Win32::Networking::WinHttp::*};
    use winhttp_rs::{self, winhttp::*};

    use warp::Filter;

    async fn run_test_server() -> (Sender<()>, SocketAddr) {
        // GET /hello/warp => 200 OK with body "Hello, warp!"
        let hello =
            warp::path!("hello" / String).map(|name| format!("{{ \"hello\": \"{}\" }}", name));
        let (tx, rx) = oneshot::channel();

        let (addr, server) =
            warp::serve(hello).bind_with_graceful_shutdown(([127, 0, 0, 1], 3030), async {
                rx.await.ok();
                println!("Graceful shutdown complete")
            });
        // Spawn the server into a runtime
        println!("Server started on {}", addr);
        tokio::task::spawn(server);
        // wait for server to be up
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        return (tx, addr);
    }

    fn send_req(addr: &SocketAddr) {
        let session = HSession::new(
            HSTRING::from("Rust2"),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            HSTRING::new(),
            HSTRING::new(),
            0,
        )
        .unwrap();

        let ip = addr.ip();
        let port = addr.port();
        let conn = session
            .connect(HSTRING::from(ip.to_string()), INTERNET_PORT(port as u32))
            .unwrap();

        let req = conn
            .open_request(
                HSTRING::from("GET"),
                HSTRING::from("hello/world"),
                HSTRING::from("HTTP/1.1"),
                HSTRING::new(),
                Some(vec![HSTRING::from("application/json")]),
                WINHTTP_OPEN_REQUEST_FLAGS(0), // not use WINHTTP_FLAG_SECURE
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
        println!();
    }

    #[test]
    fn server() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, addr) = run_test_server().await;

            // send request to server
            send_req(&addr);

            println!("Shutdown");
            tx.send(()).unwrap();
        });
    }
}
