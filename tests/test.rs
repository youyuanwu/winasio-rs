// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use tokio::sync::oneshot::{self, *};
    use winasio_rs::{
        self,
        winhttp::{fasync::HRequestAsync, *},
    };
    use windows::{core::HSTRING, Win32::Networking::WinHttp::*};

    use serde_derive::{Deserialize, Serialize};
    use warp::Filter;

    #[derive(Deserialize, Serialize)]
    struct Count {
        count: u32,
    }

    async fn run_test_server() -> (Sender<()>, SocketAddr) {
        static G_COUNT: AtomicUsize = AtomicUsize::new(0);
        // GET /hello/warp => 200 OK with body "Hello, warp!"
        let hello =
            warp::path!("hello" / String).map(|name| format!("{{ \"hello\": \"{}\" }}", name));

        let count = warp::post()
            .and(warp::path("count"))
            .and(warp::body::json())
            .map(|c: Count| {
                G_COUNT.store(c.count as usize, Ordering::SeqCst);

                let reply = Count { count: c.count };
                warp::reply::json(&reply)
            });

        let routes = hello.or(count);

        let (tx, rx) = oneshot::channel();

        let (addr, server) =
            warp::serve(routes).bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async {
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
            req.query_data_available(Some(&mut len)).unwrap();
            if len == 0 {
                break;
            }
            let mut buffer: Vec<u8> = vec![0; len as usize];
            let mut lpdwnumberofbytesread: u32 = 0;
            req.read_data(buffer.as_mut_slice(), len, Some(&mut lpdwnumberofbytesread))
                .unwrap();

            let s = String::from_utf8_lossy(&buffer);
            print!("{}", s);
        }
        println!();
    }

    async fn send_req_async(addr: &SocketAddr) {
        let session = HSession::new(
            HSTRING::from("Rust2"),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            HSTRING::new(),
            HSTRING::new(),
            WINHTTP_FLAG_ASYNC,
        )
        .unwrap();

        let ip = addr.ip();
        let port = addr.port();
        let conn = session
            .connect(HSTRING::from(ip.to_string()), INTERNET_PORT(port as u32))
            .unwrap();
        {
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

            let mut async_req: HRequestAsync = HRequestAsync::new(req);

            async_req.async_send(HSTRING::new(), &[], 0).await.unwrap();

            async_req.async_receive_response().await.unwrap();

            loop {
                let len = async_req.async_query_data_available().await.unwrap();
                if len == 0 {
                    break;
                }
                let mut buffer: Vec<u8> = vec![0; len as usize];
                let len_read = async_req
                    .async_read_data(buffer.as_mut_slice(), len)
                    .await
                    .unwrap();
                assert!(len == len_read);
                let s = String::from_utf8_lossy(&buffer);
                print!("{}", s);
            }
        }
        // post for count
        {
            let req = conn
                .open_request(
                    HSTRING::from("POST"),
                    HSTRING::from("count"),
                    HSTRING::from("HTTP/1.1"),
                    HSTRING::new(),
                    Some(vec![HSTRING::from("application/json")]),
                    WINHTTP_OPEN_REQUEST_FLAGS(0), // not use WINHTTP_FLAG_SECURE
                )
                .unwrap();

            let mut async_req: HRequestAsync = HRequestAsync::new(req);

            let ct: Count = Count { count: 11 };
            let j = serde_json::to_string(&ct).unwrap();

            async_req
                .async_send(HSTRING::new(), &[], j.len() as u32)
                .await
                .unwrap();

            // write one by one
            for char in j.chars() {
                async_req.async_write_data(&[char as u8], 1).await.unwrap();
            }

            async_req.async_receive_response().await.unwrap();

            let mut result = String::new();
            loop {
                let len = async_req.async_query_data_available().await.unwrap();
                if len == 0 {
                    break;
                }
                // read byte by byte
                for _ in 0..len {
                    let mut temp_buf = [0 as u8];
                    let buf = temp_buf.as_mut_slice();
                    let len_read = async_req
                        .async_read_data(buf, buf.len() as u32)
                        .await
                        .unwrap();
                    assert!(buf.len() == len_read as usize);
                    result.push(buf[0] as char);
                }
            }
            println!("{}", result);
        }
    }

    #[test]
    fn sync_test() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, addr) = run_test_server().await;

            // send request to server
            send_req(&addr);

            println!("Shutdown");
            tx.send(()).unwrap();
        });
    }

    #[test]
    fn async_test() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, addr) = run_test_server().await;

            // send request to server
            send_req_async(&addr).await;

            println!("Shutdown");
            tx.send(()).unwrap();
        });
    }
}
