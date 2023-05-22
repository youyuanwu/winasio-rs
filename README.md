# README
![ci](https://github.com/youyuanwu/winhttp-rs/actions/workflows/build.yaml/badge.svg)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://raw.githubusercontent.com/youyuanwu/winhttp-rs/main/LICENSE)

# Winhttp
Winhttp in async mode with rust async await wrapper.
Example snippit:
```rs
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
```
See full working code in [example test](./tests/test.rs)

# MISC
C++ counterpart of this lib: [winasio](https://github.com/youyuanwu/winasio)

# License
MIT License