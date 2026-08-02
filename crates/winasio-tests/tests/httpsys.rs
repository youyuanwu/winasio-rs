// ------------------------------------------------------------
// Copyright 2023 Youyuan Wu
// Licensed under the MIT License (MIT). See License.txt in the repo root for
// license information.
// ------------------------------------------------------------

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use tokio::sync::oneshot::{self};
    use windows::{
        core::HSTRING,
        Win32::Networking::{
            HttpServer::{HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY, HTTP_SEND_RESPONSE_FLAG_DISCONNECT},
            WinHttp::{WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_OPEN_REQUEST_FLAGS},
        },
    };

    use std::pin::Pin;
    use winasio::{
        httpsys::{HttpInitializer, Request, RequestQueue, Response, ServerSession, UrlGroup},
        winhttp::HSession,
    };

    /// Reads the raw URL back out of a received request.
    ///
    /// This deliberately follows a pointer that HTTP.sys wrote into the
    /// request's own inline buffer. If the request had been moved after
    /// completion, this would read freed or relocated memory -- which is why
    /// it comes back pinned.
    fn raw_url_of(req: &Request) -> String {
        let base = &req.raw_ref().Base;
        if base.pRawUrl.is_null() || base.RawUrlLength == 0 {
            return String::new();
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(base.pRawUrl.0, base.RawUrlLength as usize) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    async fn handle_request(queue: Arc<RequestQueue>, req: Pin<Box<Request>>) {
        let id = req.raw_ref().Base.RequestId;

        // Follow a kernel-written pointer after the request was handed back.
        let url = raw_url_of(&req);
        assert!(
            url.contains("winhttpapitest"),
            "the pinned request's URL pointer must still be valid, got {url:?}"
        );

        let mut resp = Response::default();
        resp.add_body_chunk(String::from("hello world"));

        println!("run_test_server send_response");
        let out = queue
            .async_send_response(id, HTTP_SEND_RESPONSE_FLAG_DISCONNECT, resp)
            .await;
        if let Err(e) = out.as_result() {
            println!("send resp failed: {e:?}");
        }
    }

    async fn run_test_server(queue: Arc<RequestQueue>) {
        println!("run_test_server begin");
        loop {
            println!("run_test_server receive_request");
            // The task can be cancelled here when the queue shuts down.
            let out = queue
                .async_receive_request(0, HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY)
                .await;
            let (result, req) = out.into_parts();
            if let Err(e) = result {
                println!("receive request failed: {e:?}");
                continue;
            }
            let queue_cp = queue.clone();
            // Detached, not joinable.
            let _h = tokio::spawn(async move {
                handle_request(queue_cp, req).await;
            });
        }
    }

    #[test]
    fn server_test() {
        let (tx, rx) = oneshot::channel::<()>();

        let th = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                HttpInitializer::default();

                let session = ServerSession::default();

                let url_group = UrlGroup::new(&session);
                url_group
                    .add_url(HSTRING::from("http://localhost:12356/winhttpapitest/"))
                    .unwrap();

                let request_queue = Arc::new(RequestQueue::new().unwrap());
                request_queue.bind_url_group(&url_group).unwrap();

                tokio::select! {
                  _ = rx =>{
                    println!("Shutdown signal received.")
                  }
                  _ = async{
                    run_test_server(request_queue.clone()).await
                  } => {}
                }
                println!("queue handle out of scope.");
                // rely on drop to close
                // request_queue.close();
            });
        });

        std::thread::sleep(std::time::Duration::from_secs(1));
        // send a basic request using winhttp
        {
            let session = HSession::new(
                HSTRING::from("Rust2"),
                WINHTTP_ACCESS_TYPE_NO_PROXY,
                HSTRING::new(),
                HSTRING::new(),
                0,
            )
            .unwrap();

            let conn = session.connect(HSTRING::from("localhost"), 12356).unwrap();

            let req = conn
                .open_request(
                    HSTRING::from("GET"),
                    HSTRING::from("winhttpapitest"),
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

        std::thread::sleep(std::time::Duration::from_secs(2));
        tx.send(()).unwrap();
        th.join().unwrap();
    }
}
