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

    use crate::{
        httpsys::{HttpInitializer, Request, RequestQueue, Response, ServerSession, UrlGroup},
        winhttp::HSession,
    };

    async fn handle_request(queue: Arc<RequestQueue>, mut req: Request) {
        let id = req.raw().Base.RequestId;

        let body = String::from("hello world");
        let mut resp = Response::default();
        resp.add_body_chunk(body);

        println!("run_test_server async_send_response");

        let err = queue
            .async_send_response(id, HTTP_SEND_RESPONSE_FLAG_DISCONNECT, &resp)
            .await;
        if err.is_err() {
            println!("send resp failed: {:?}", err.err());
        }
    }

    async fn run_test_server(queue: Arc<RequestQueue>) {
        println!("run_test_server begin");
        loop {
            let mut req = Request::default();

            println!("run_test_server async_receive_request");
            {
                // task can be cancelled here when queue shutdown.
                let err = queue
                    .async_receive_request(0, HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY, &mut req)
                    .await;
                if err.is_err() {
                    println!("receive request failed: {:?}", err.err());
                    continue;
                }
            }
            let queue_cp = queue.clone();
            // task is detached and not joinable.
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
                let _ = HttpInitializer::default();

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
