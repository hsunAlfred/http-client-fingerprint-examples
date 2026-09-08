use futures_util::StreamExt;
use std::{io, path::Path, time::Duration};
use tokio::io::AsyncWriteExt;
use wreq::Client;
use wreq_util::Emulation;

type Error = Box<dyn std::error::Error + Send + Sync>;

pub async fn download(
    client: &Client,
    url: &str,
    destination: &Path,
    max_bytes: u64,
) -> Result<u64, Error> {
    if max_bytes == 0 {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "max_bytes must be positive").into(),
        );
    }
    let response = client.get(url).send().await?.error_for_status()?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "download requires a 2xx response, received {}",
            response.status()
        ))
        .into());
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // 與目標放在同一個目錄，persist 才能使用同檔案系統的原子替換。
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut output = tokio::fs::File::from_std(temporary.reopen()?);
    let mut stream = response.bytes_stream();
    let mut received = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        received = received
            .checked_add(chunk.len() as u64)
            .filter(|size| *size <= max_bytes)
            .ok_or_else(|| io::Error::other("download exceeds max_bytes"))?;
        output.write_all(&chunk).await?;
    }
    output.flush().await?;
    output.sync_all().await?;
    drop(output);
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(received)
    // 提早回傳錯誤時，NamedTempFile drop 會移除暫存檔。
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: download <URL> <destination>".into());
    }
    let client = Client::builder()
        .emulation(Emulation::Chrome124)
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()?;
    let size = download(&client, &args[1], Path::new(&args[2]), 8 * 1024 * 1024).await?;
    println!("downloaded bytes: {size}");
    Ok(())
}
