use std::time::Duration;
use wreq::Client;
use wreq_util::Emulation;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .emulation(Emulation::Chrome124)
        .timeout(Duration::from_secs(15))
        .build()?;

    let response = client
        .get("https://tls.browserleaks.com/json")
        .send()
        .await?
        .error_for_status()?;
    println!("HTTP version: {:?}", response.version());
    let data: serde_json::Value = response.json().await?;
    println!("JA3N: {}", data["ja3n_hash"]);
    Ok(())
}
