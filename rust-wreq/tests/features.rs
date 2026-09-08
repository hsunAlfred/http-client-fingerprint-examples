use std::{collections::BTreeMap, time::Duration};

use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use wreq::Client;

#[path = "../examples/download.rs"]
#[allow(dead_code)]
mod download;
#[path = "../examples/http.rs"]
#[allow(dead_code)]
mod http;
#[path = "../examples/upload.rs"]
#[allow(dead_code)]
mod upload;
#[path = "../examples/websocket.rs"]
#[allow(dead_code)]
mod websocket;

struct Request {
    line: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct Reply {
    status: &'static str,
    headers: &'static str,
    body: Vec<u8>,
    declared_length: Option<usize>,
}

impl Reply {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "200 OK",
            headers: "",
            body: body.into(),
            declared_length: None,
        }
    }
}

struct Fixture {
    url: String,
    task: Option<JoinHandle<Vec<Request>>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Fixture {
    async fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                let header_end = loop {
                    let byte = socket.read_u8().await.unwrap();
                    data.push(byte);
                    assert!(data.len() < 32 * 1024, "fixture header too large");
                    if data.ends_with(b"\r\n\r\n") {
                        break data.len();
                    }
                };
                let head = std::str::from_utf8(&data[..header_end]).unwrap();
                let mut lines = head.split("\r\n");
                let line = lines.next().unwrap().to_owned();
                let headers: BTreeMap<String, String> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                    .collect();
                assert!(
                    !headers.contains_key("transfer-encoding"),
                    "fixture expects fixed-size bodies"
                );
                let size: usize = headers
                    .get("content-length")
                    .map_or(0, |value| value.parse().unwrap());
                assert!(size <= 1024 * 1024, "fixture body too large");
                let mut body = vec![0; size];
                socket.read_exact(&mut body).await.unwrap();
                requests.push(Request {
                    line,
                    headers,
                    body,
                });
                let length = reply.declared_length.unwrap_or(reply.body.len());
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {length}\r\nConnection: close\r\n{}\r\n",
                    reply.status, reply.headers
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&reply.body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            requests
        });
        Self {
            url,
            task: Some(task),
        }
    }

    async fn finish(mut self) -> Vec<Request> {
        timeout(Duration::from_secs(5), self.task.as_mut().unwrap())
            .await
            .unwrap()
            .unwrap()
    }
}

fn plain_client() -> Client {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
}

#[tokio::test]
async fn http_json_query_form_raw_status_redirect_and_cookie() {
    let fixture = Fixture::new(vec![
        Reply {
            headers: "Content-Type: application/json\r\n",
            ..Reply::ok(r#"{"name":"sample","count":2}"#)
        },
        Reply::ok("form accepted"),
        Reply {
            headers: "Set-Cookie: session=fixture; Path=/; HttpOnly\r\n",
            ..Reply::ok("set")
        },
        Reply::ok("cookie request"),
        Reply::ok("raw accepted"),
        Reply {
            status: "401 Unauthorized",
            ..Reply::ok("unauthorized")
        },
        Reply {
            status: "302 Found",
            headers: "Location: /final\r\n",
            ..Reply::ok("")
        },
        Reply::ok("final"),
    ])
    .await;
    let client = http::build_client().unwrap();
    let item = http::Item {
        name: "sample".into(),
        count: 2,
    };
    assert_eq!(
        http::create_item(&client, &format!("{}/items", fixture.url), &item)
            .await
            .unwrap(),
        item
    );
    assert_eq!(
        http::submit_form(&client, &format!("{}/form", fixture.url))
            .await
            .unwrap(),
        "form accepted"
    );
    client
        .get(format!("{}/set", fixture.url))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    client
        .get(format!("{}/cookies", fixture.url))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        client
            .put(format!("{}/raw", fixture.url))
            .body("raw payload")
            .bearer_auth("fixture-token")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "raw accepted"
    );
    let failure = client
        .get(format!("{}/status", fixture.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap_err();
    assert_eq!(failure.status(), Some(wreq::StatusCode::UNAUTHORIZED));
    assert_eq!(
        client
            .get(format!("{}/redirect", fixture.url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "final"
    );
    let requests = fixture.finish().await;
    assert_eq!(
        requests[0].line,
        "POST /items?source=article&view=compact HTTP/1.1"
    );
    assert_eq!(requests[0].headers["x-example"], "rust-wreq");
    assert_eq!(requests[0].headers["content-type"], "application/json");
    assert_eq!(
        serde_json::from_slice::<http::Item>(&requests[0].body).unwrap(),
        item
    );
    assert_eq!(
        requests[1].headers["content-type"],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(
        std::str::from_utf8(&requests[1].body).unwrap(),
        "title=Rust+HTTP&lang=zh-TW"
    );
    assert_eq!(requests[3].headers["cookie"], "session=fixture");
    assert_eq!(requests[4].body, b"raw payload");
    assert_eq!(requests[4].headers["authorization"], "Bearer fixture-token");
    assert_eq!(requests[7].line, "GET /final HTTP/1.1");
}

#[tokio::test]
async fn cloned_client_shares_cookies_independent_client_does_not() {
    let fixture = Fixture::new(vec![
        Reply {
            headers: "Set-Cookie: session=fixture; Path=/\r\n",
            ..Reply::ok("")
        },
        Reply::ok(""),
        Reply::ok(""),
    ])
    .await;
    let client = http::build_client().unwrap();
    client
        .get(&fixture.url)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    client
        .clone()
        .get(&fixture.url)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    http::build_client()
        .unwrap()
        .get(&fixture.url)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let requests = fixture.finish().await;
    assert_eq!(requests[1].headers["cookie"], "session=fixture");
    assert!(!requests[2].headers.contains_key("cookie"));
}

#[tokio::test]
async fn download_replaces_only_after_success_and_cleans_up_failures() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("result.bin");
    std::fs::write(&destination, "old").unwrap();
    let fixture = Fixture::new(vec![
        Reply::ok("new data"),
        Reply::ok("too much data"),
        Reply {
            declared_length: Some(10),
            ..Reply::ok("short")
        },
        Reply {
            status: "500 Internal Server Error",
            ..Reply::ok("failure")
        },
    ])
    .await;
    let client = plain_client();
    assert_eq!(
        download::download(&client, &fixture.url, &destination, 8)
            .await
            .unwrap(),
        8
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"new data");
    for limit in [8, 20, 20] {
        assert!(
            download::download(&client, &fixture.url, &destination, limit)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"new data");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    fixture.finish().await;
    assert!(
        download::download(&client, "http://127.0.0.1:1", &destination, 0)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn download_rejects_unfollowed_redirect_without_replacing_destination() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("result.bin");
    std::fs::write(&destination, "old").unwrap();
    let fixture = Fixture::new(vec![Reply {
        status: "302 Found",
        headers: "Location: /final\r\n",
        ..Reply::ok("redirect body")
    }])
    .await;
    let client = Client::builder()
        .no_proxy()
        .redirect(wreq::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let error = download::download(&client, &fixture.url, &destination, 100)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("302"));
    assert_eq!(std::fs::read(&destination).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    assert_eq!(fixture.finish().await.len(), 1);
}

#[tokio::test]
async fn dropping_download_future_removes_partial_file_and_keeps_destination() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("result.bin");
    std::fs::write(&destination, "old").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/download", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(socket.read_u8().await.unwrap());
            assert!(request.len() < 32 * 1024);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
            .await
            .unwrap();
        // 保留未完成的 Body，等候測試主動 drop 下載 Future。
        let mut byte = [0];
        let _ = socket.read(&mut byte).await;
    });
    let client = plain_client();
    let mut pending = Box::pin(download::download(&client, &url, &destination, 100));
    timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                result = &mut pending => panic!("download completed before cancellation: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    let partial_written = std::fs::read_dir(directory.path())
                        .unwrap()
                        .map(Result::unwrap)
                        .any(|entry| entry.path() != destination && entry.metadata().unwrap().len() > 0);
                    if partial_written {
                        break;
                    }
                }
            }
        }
    })
    .await
    .unwrap();
    drop(pending);
    assert_eq!(std::fs::read(&destination).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    server.abort();
}

#[tokio::test]
async fn multipart_and_stream_upload_send_original_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("payload.bin");
    std::fs::write(&path, b"file-content\0\xff").unwrap();
    let fixture = Fixture::new(vec![Reply::ok("accepted"), Reply::ok("accepted")]).await;
    let client = plain_client();
    upload::multipart_file(&client, &fixture.url, &path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    upload::stream_file(&client, &fixture.url, &path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let requests = fixture.finish().await;
    let multipart = &requests[0];
    assert!(multipart.headers["content-type"].starts_with("multipart/form-data; boundary="));
    assert!(
        multipart
            .body
            .windows(b"name=\"file\"; filename=\"payload.bin\"".len())
            .any(|part| part == b"name=\"file\"; filename=\"payload.bin\"")
    );
    assert!(
        multipart
            .body
            .windows(b"file-content\0\xff".len())
            .any(|part| part == b"file-content\0\xff")
    );
    assert_eq!(requests[1].line, "PUT / HTTP/1.1");
    assert_eq!(requests[1].body, b"file-content\0\xff");
    assert_eq!(
        requests[1].headers["content-type"],
        "application/octet-stream"
    );
}

#[tokio::test]
async fn file_uploads_reject_redirects_that_cannot_replay_the_body() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("payload.bin");
    std::fs::write(&path, b"original bytes").unwrap();
    let client = plain_client();
    for status in ["307 Temporary Redirect", "308 Permanent Redirect"] {
        let fixture = Fixture::new(vec![
            Reply {
                status,
                headers: "Location: /final\r\n",
                ..Reply::ok("")
            },
            Reply {
                status,
                headers: "Location: /final\r\n",
                ..Reply::ok("")
            },
        ])
        .await;
        let multipart_error = upload::multipart_file(&client, &fixture.url, &path)
            .await
            .unwrap_err();
        let stream_error = upload::stream_file(&client, &fixture.url, &path)
            .await
            .unwrap_err();
        for error in [multipart_error, stream_error] {
            assert!(error.to_string().contains(&status[..3]));
        }
        let requests = fixture.finish().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].line, "POST / HTTP/1.1");
        assert_eq!(requests[1].line, "PUT / HTTP/1.1");
    }
}

#[tokio::test]
async fn websocket_echo_ping_pong_and_close_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut websocket = accept_async(socket).await.unwrap();
        let (mut text, mut binary, mut ping) = (false, false, false);
        while let Some(message) = websocket.next().await {
            match message.unwrap() {
                Message::Text(value) => {
                    text = true;
                    websocket.send(Message::Text(value)).await.unwrap();
                }
                Message::Binary(value) => {
                    binary = true;
                    websocket.send(Message::Binary(value)).await.unwrap();
                }
                Message::Ping(value) => {
                    ping = true;
                    assert_eq!(value.as_ref(), [9]);
                    websocket.flush().await.unwrap();
                }
                Message::Close(_) => {
                    websocket.flush().await.unwrap();
                    break;
                }
                _ => {}
            }
        }
        assert!(text && binary && ping);
    });
    websocket::exchange(&plain_client(), &url).await.unwrap();
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}
