use futures_util::SinkExt;
use std::{io, time::Duration};
use tokio::time::timeout;
use wreq::{Client, ws::message::Message};
use wreq_util::Emulation;

type Error = Box<dyn std::error::Error + Send + Sync>;

pub async fn exchange(client: &Client, url: &str) -> Result<(), Error> {
    timeout(Duration::from_secs(15), async {
        let mut socket = client
            .websocket(url)
            .max_message_size(64 * 1024)
            .max_frame_size(16 * 1024)
            .send()
            .await?
            .into_websocket()
            .await?;

        socket.send(Message::text("hello")).await?;
        socket.send(Message::binary(vec![1, 2, 3])).await?;
        socket.send(Message::ping(vec![9])).await?;
        let (mut text, mut binary, mut pong) = (false, false, false);
        while !(text && binary && pong) {
            let message = socket
                .recv()
                .await
                .ok_or_else(|| io::Error::other("peer closed early"))??;
            match message {
                Message::Text(value) => text |= value.as_str() == "hello",
                Message::Binary(value) => binary |= value.as_ref() == [1, 2, 3],
                Message::Pong(value) => pong |= value.as_ref() == [9],
                // tungstenite 自動排入 Pong；flush 確保及時送出。
                Message::Ping(_) => socket.flush().await?,
                Message::Close(_) => return Err(io::Error::other("peer closed before echo").into()),
            }
        }

        // 保留 socket，送出 Close 後繼續讀取對端的 Close 回覆。
        socket.send(Message::Close(None)).await?;
        loop {
            match socket.recv().await {
                Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(error.into()),
                None => return Err(io::Error::other("missing peer Close reply").into()),
            }
        }
        Ok::<(), Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let url = std::env::args()
        .nth(1)
        .ok_or("usage: websocket <WebSocket echo URL>")?;
    let client = Client::builder()
        .emulation(Emulation::Chrome124)
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    exchange(&client, &url).await?;
    println!("text, binary, Ping/Pong, Close: OK");
    Ok(())
}
