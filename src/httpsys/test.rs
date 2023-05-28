#[cfg(test)]
mod tests {

    use tokio::sync::oneshot::{self};
    use windows::{
        core::{HSTRING, PCSTR},
        Win32::Networking::{
            HttpServer::{
                HttpDataChunkFromMemory, HTTP_DATA_CHUNK, HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY,
                HTTP_REQUEST_V2, HTTP_RESPONSE_V2, HTTP_SEND_RESPONSE_FLAG_DISCONNECT,
            },
            WinHttp::{WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_OPEN_REQUEST_FLAGS},
        },
    };

    use crate::{
        httpsys::{HttpInitializer, RequestQueue, ServerSession, UrlGroup},
        winhttp::HSession,
    };

    async fn run_test_server(queue: &mut RequestQueue) {
        println!("run_test_server begin");
        loop {
            let mut buff = vec![0; std::mem::size_of::<HTTP_REQUEST_V2>() + 1024 as usize];
            let req_ptr: &mut HTTP_REQUEST_V2 =
                unsafe { &mut *(buff.as_mut_ptr() as *mut HTTP_REQUEST_V2) };
            println!("run_test_server async_receive_request");
            let err = queue
                .async_receive_request(
                    0,
                    HTTP_RECEIVE_REQUEST_FLAG_COPY_BODY,
                    req_ptr,
                    buff.len() as u32,
                )
                .await;
            if err.is_err() {
                println!("receive request failed: {:?}", err.err());
                continue;
            }
            let req: &mut HTTP_REQUEST_V2 = &mut *req_ptr;
            let id = req.Base.RequestId;

            let mut body = String::from("hello world");
            //let mut resp = HTTP_RESPONSE_V2::default();
            let mut resp: Box<HTTP_RESPONSE_V2> = Box::new(HTTP_RESPONSE_V2::default());
            resp.Base.StatusCode = 200;
            let ok_str = "OK";
            resp.Base.pReason = PCSTR(ok_str.as_ptr());

            let mut chunk = HTTP_DATA_CHUNK::default();
            chunk.DataChunkType = HttpDataChunkFromMemory;
            chunk.Anonymous.FromMemory.BufferLength = body.len() as u32;
            chunk.Anonymous.FromMemory.pBuffer = body.as_mut_ptr() as *mut std::ffi::c_void;
            resp.Base.EntityChunkCount = 1;
            resp.Base.pEntityChunks = &mut chunk;

            let resp_ptr: *const HTTP_RESPONSE_V2 = &*resp;
            println!("run_test_server async_send_response");
            let err = queue
                .async_send_response(id, HTTP_SEND_RESPONSE_FLAG_DISCONNECT, resp_ptr, None, None)
                .await;
            if err.is_err() {
                println!("receive request failed: {:?}", err.err());
                continue;
            }
        }
    }

    #[test]
    fn server_test() {
        let _ = HttpInitializer::default();

        let session = ServerSession::default();

        let url_group = UrlGroup::new(&session);
        url_group
            .add_url(HSTRING::from("http://localhost:12356/winhttpapitest/"))
            .unwrap();

        let mut request_queue = RequestQueue::new().unwrap();
        request_queue.bind_url_group(&url_group).unwrap();

        let (tx, rx) = oneshot::channel::<()>();

        let th = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                tokio::select! {
                  _ = rx =>{
                    println!("Shutdown signal received.")
                  }
                  _ = async{
                    run_test_server(&mut request_queue).await
                  } => {}
                }
                println!("closing queue handle");
                request_queue.close()
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
