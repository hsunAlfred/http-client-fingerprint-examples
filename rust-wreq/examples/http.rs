use serde::{Deserialize, Serialize};
use std::time::Duration;
use wreq::{Client, redirect::Policy};
use wreq_util::{Emulation, Platform, Profile};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub name: String,
    pub count: u32,
}

pub fn build_client() -> wreq::Result<Client> {
    Client::builder()
        .emulation(
            Emulation::builder()
                .profile(Profile::Chrome124)
                .platform(Platform::Windows)
                .build(),
        )
        .no_proxy()
        .cookie_store(true)
        .redirect(Policy::limited(3))
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
}

pub async fn create_item(client: &Client, url: &str, item: &Item) -> wreq::Result<Item> {
    let response = client
        .post(url)
        .query(&[("source", "article"), ("view", "compact")])
        .header("x-example", "rust-wreq")
        .json(item)
        .send()
        .await?
        .error_for_status()?;

    // status()、headers() 只借用 response；json() 消費 body 的 ownership。
    println!("status: {}", response.status());
    println!("content-type: {:?}", response.headers().get("content-type"));
    response.json::<Item>().await
}

pub async fn submit_form(client: &Client, url: &str) -> wreq::Result<String> {
    client
        .post(url)
        .form(&[("title", "Rust HTTP"), ("lang", "zh-TW")])
        .send()
        .await?
        .error_for_status()?
        .text()
        .await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("usage: http <JSON echo URL>")?;
    let client = build_client()?;
    let item = Item {
        name: "example".into(),
        count: 2,
    };
    let echoed = create_item(&client, &url, &item).await?;
    println!("item: {echoed:?}");
    Ok(())
}
