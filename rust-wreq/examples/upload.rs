use std::{io, path::Path, time::Duration};
use tokio_util::io::ReaderStream;
use wreq::{Body, Client, Response, multipart};
use wreq_util::Emulation;

type Error = Box<dyn std::error::Error + Send + Sync>;

fn require_success(response: Response) -> Result<Response, Error> {
    let response = response.error_for_status()?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "upload requires a 2xx response, received {}",
            response.status()
        ))
        .into());
    }
    Ok(response)
}

pub async fn multipart_file(client: &Client, url: &str, path: &Path) -> Result<Response, Error> {
    let form = multipart::Form::new()
        .text("description", "article upload")
        .file("file", path)
        .await?;
    require_success(client.post(url).multipart(form).send().await?)
}

pub async fn stream_file(client: &Client, url: &str, path: &Path) -> Result<Response, Error> {
    let file = tokio::fs::File::open(path).await?;
    // 來源檔案在傳輸期間必須維持不變，才可使用 metadata 的長度。
    let length = file.metadata().await?.len();
    let body = Body::wrap_stream(ReaderStream::new(file));
    require_success(
        client
            .put(url)
            .header("content-type", "application/octet-stream")
            .header("content-length", length)
            .body(body)
            .send()
            .await?,
    )
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 || !["multipart", "stream"].contains(&args[1].as_str()) {
        return Err("usage: upload <multipart|stream> <URL> <file>".into());
    }
    let client = Client::builder()
        .emulation(Emulation::Chrome124)
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()?;
    let response = match args[1].as_str() {
        "multipart" => multipart_file(&client, &args[2], Path::new(&args[3])).await?,
        _ => stream_file(&client, &args[2], Path::new(&args[3])).await?,
    };
    println!("upload status: {}", response.status());
    Ok(())
}
