use std::time::Duration;
use wreq::{
    Client, Proxy,
    tls::trust::{CertStore, Identity},
};
use wreq_util::Emulation;

pub fn proxy_client(proxy_url: &str) -> wreq::Result<Client> {
    Client::builder()
        .emulation(Emulation::Chrome124)
        .no_proxy()
        .proxy(Proxy::all(proxy_url)?)
        .timeout(Duration::from_secs(20))
        .build()
}

pub fn mtls_client(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> wreq::Result<Client> {
    // 這個 store 只信任所提供的 CA bundle；不是在預設 store 上追加。
    let store = CertStore::from_pem_stack(ca_pem)?;
    let identity = Identity::from_pkcs8_pem(cert_pem, key_pem)?;
    Client::builder()
        .emulation(Emulation::Chrome124)
        .no_proxy()
        .tls_cert_store(store)
        .tls_identity(identity)
        .timeout(Duration::from_secs(20))
        .build()
}

fn main() {
    println!(
        "proxy_client 與 mtls_client 是設定範例；請提供實際 proxy、CA 與 client identity。實際連線另行驗證。"
    );
}
