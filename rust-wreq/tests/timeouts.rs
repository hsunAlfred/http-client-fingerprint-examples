use std::time::Duration;

use futures_util::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{sleep, timeout},
};
use wreq::Client;

const READ_TIMEOUT: Duration = Duration::from_millis(800);
const TEST_DEADLINE: Duration = Duration::from_secs(10);

fn client() -> Client {
    Client::builder()
        .no_proxy()
        .http1_only()
        .redirect(wreq::redirect::Policy::none())
        .retry(wreq::retry::Policy::never())
        // No total timeout: the assertions must exercise read_timeout itself.
        .read_timeout(READ_TIMEOUT)
        .build()
        .unwrap()
}

async fn read_request(socket: &mut TcpStream) {
    let mut request = Vec::new();
    loop {
        request.push(socket.read_u8().await.unwrap());
        assert!(request.len() <= 16 * 1024, "fixture request too large");
        if request.ends_with(b"\r\n\r\n") {
            return;
        }
    }
}

#[tokio::test]
async fn partial_response_headers_do_not_reset_read_timeout() {
    timeout(TEST_DEADLINE, async {
        let client = client();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.set_nodelay(true).unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX-Progress: ")
                .await
                .unwrap();

            let mut fragments = 0;
            // Every gap is much shorter than READ_TIMEOUT, but completing the
            // header block takes at least two seconds. An idle-read timer that
            // reset on each incoming fragment would allow this response.
            for _ in 0..20 {
                sleep(Duration::from_millis(100)).await;
                if socket.write_all(b"x").await.is_err() {
                    // The expected timeout may already have closed the client.
                    return fragments;
                }
                fragments += 1;
            }
            let _ = socket.write_all(b"\r\n\r\n").await;
            fragments
        });

        let error = client.get(url).send().await.unwrap_err();
        assert!(error.is_timeout(), "expected header timeout, got {error:?}");
        let fragments = server.await.unwrap();
        assert!(fragments >= 3, "fixture did not send progressive headers");
    })
    .await
    .expect("header timeout fixture exceeded its deadline");
}

#[tokio::test]
async fn application_pause_after_body_frame_expires_read_timeout() {
    timeout(TEST_DEADLINE, async {
        let client = client();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (first_consumed, wait_first_consumed) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
                .await
                .unwrap();
            // Keep the first frame distinct: the second byte is sent only
            // after the application confirms consuming the first byte.
            wait_first_consumed.await.unwrap();
            socket.write_all(b"b").await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let response = client.get(url).send().await.unwrap();
        let mut body = response.bytes_stream();
        assert_eq!(body.next().await.unwrap().unwrap().as_ref(), b"a");
        first_consumed.send(()).unwrap();
        // The server has finished sending all remaining data before the pause.
        server.await.unwrap();
        sleep(READ_TIMEOUT * 2).await;

        let error = body.next().await.unwrap().unwrap_err();
        assert!(error.is_timeout(), "expected body timeout, got {error:?}");
    })
    .await
    .expect("body timeout fixture exceeded its deadline");
}
