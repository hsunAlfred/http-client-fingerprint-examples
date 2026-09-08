# HTTP Client Fingerprint Examples

使用 Python 與 Rust 觀察 HTTP Client 的 TLS／HTTP 指紋，比較一般 Client、只修改 User-Agent，以及套用 Browser Profile 後的差異。

範例包含可執行的診斷 CLI、本機測試與 Rust Docker image。診斷端點須回傳支援的指紋 JSON；收到 HTTP 200 不代表一般 API 就符合此資料契約。Profile 與 hash 用於實驗比較，不能單憑診斷成功判定請求等同真實瀏覽器。

| 目錄 | 套件與用途 | 詳細說明 |
| --- | --- | --- |
| `python-curl-cffi/` | `curl_cffi 0.16.3`；有界併發、重試預算與 JSONL 指紋診斷 | [Python README](python-curl-cffi/README.md) |
| `rust-wreq/` | `reqwest 0.13.4`、`wreq 0.16.1`、`wreq-util 0.2.0`；四組 Client 比較及 HTTP／Streaming／WebSocket 範例 | [Rust README](rust-wreq/README.md) |

## 系列文章

1. [HTTP Client 與瀏覽器指紋：TLS ClientHello、JA3／JA4、HTTP/2 與 HTTP/3](https://www.alfred.wiki/others/http-client-browser-fingerprint-tls-ja3-http2-http3/)
2. [Python curl_cffi 實作：Browser Impersonation、Session、Asyncio 與 WebSocket](https://www.alfred.wiki/python/python-curl-cffi-browser-impersonation-asyncio-websocket/)
3. [Rust 瀏覽器指紋實作：reqwest 與 wreq 的 TLS／HTTP/2 差異](https://www.alfred.wiki/rust/rust-http-client-browser-fingerprint-reqwest-wreq/)

## 取得程式

```bash
git clone https://github.com/hsunAlfred/http-client-fingerprint-examples.git
cd http-client-fingerprint-examples
```

以下各節從 repository 根目錄開始；進入子目錄完成操作後，可用 `cd ..` 返回。指令採用 Bash 語法。

## Python Quick Start

需要 Python 3.10 以上。

```bash
cd python-curl-cffi
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements.txt
.venv/bin/python diagnostic.py \
  --url https://tls.browserleaks.com/json \
  --profile chrome124 \
  --max-attempts 1
```

`--url` 指定診斷端點，可重複提供；上述指令只診斷一筆 URL。`--profile` 指定 Browser Target，目前只接受 `chrome124`。`--max-attempts` 是包含首次請求的嘗試次數上限，設為 `1` 時不重試；省略時預設為 `3`。

預設將 JSONL 結果寫入 stdout，統計或錯誤訊息寫入 stderr。若要保存結果，可加上 `--output results.jsonl`；檔案必須尚未存在。併發、時間預算、body 大小上限與完整輸出契約見 [Python README](python-curl-cffi/README.md)。

`requirements.txt` 固定直接相依版本，包含官方 CLI 與 Requests 對照組；未鎖定所有傳遞相依套件。

## Rust Quick Start

需要已安裝 rustup；目錄內的 `rust-toolchain.toml` 固定 Rust 1.98.0。Ubuntu／Debian 的原生建置依賴可用以下指令安裝：

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake perl pkg-config libclang-dev git ca-certificates
```

執行輕量範例，查看實際 HTTP 版本與 JA3N：

```bash
cd rust-wreq
cargo run --locked --example quick_start
```

在 `rust-wreq/` 內，以相同端點分別執行四組診斷：

```bash
cargo run --locked -- --url https://tls.browserleaks.com/json --client reqwest
cargo run --locked -- --url https://tls.browserleaks.com/json --client reqwest-ua
cargo run --locked -- --url https://tls.browserleaks.com/json --client wreq
cargo run --locked -- --url https://tls.browserleaks.com/json --client wreq-chrome --profile chrome124
```

| `--client` | 行為 |
| --- | --- |
| `reqwest` | 一般 reqwest Client |
| `reqwest-ua` | reqwest Client，只設定 Windows Chrome 124 的 User-Agent |
| `wreq` | 未套用 Browser Profile 的 wreq Client |
| `wreq-chrome` | wreq Client，指定 Chrome 124 Profile 與 Windows 平台 |

輕量 `quick_start` 使用 Profile 預設的 macOS 平台；`wreq-chrome` 診斷組則明確指定 Windows。

Rust CLI 每次接受一個 `--url`，以 `--count` 設定 GET 次數，預設為 `1`；不自動重試。JSONL 結果寫入 stdout，摘要寫入 stderr；也可加上 `--output results.jsonl` 寫入尚未存在的檔案。完整參數、其他 examples 與錯誤分類見 [Rust README](rust-wreq/README.md)。

兩種語言的 Diagnostic CLI 都採用固定的 `chrome124` 實驗目標，方便對照；這個舊 Profile 不代表目前 Chrome 的連線行為。公開端點回傳的 hash 是當次觀察值，可能隨版本、環境與服務端處理改變。

## 本機測試

Python 測試使用安裝套件後的虛擬環境：

```bash
cd python-curl-cffi
.venv/bin/python -m unittest -v test_diagnostic.py
.venv/bin/python -m pip check
```

Rust 測試與品質檢查：

```bash
cd rust-wreq
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

Python 的 17 項測試與 Rust 的 24 項測試使用本機 fixtures 或模擬，不呼叫公開診斷網站。它們驗證 Client 控制流程與範例行為；真實 TLS／HTTP 指紋仍須另外觀察。首次安裝套件或建置時需要下載相依套件。

正文程式片段的校驗由文章專案維護，不屬於此 repository 的標準測試。

## Rust Docker

Docker image 執行 Rust Diagnostic CLI，使用非 root 使用者；預設顯示 help 後結束。

```bash
cd rust-wreq
docker build -t fingerprint-client-rs .
docker run --rm fingerprint-client-rs --help
docker run --rm fingerprint-client-rs \
  --url https://tls.browserleaks.com/json \
  --client wreq-chrome
```

同一目錄也提供 Compose 設定，預設驗證 CLI help 啟動流程：

```bash
docker compose config
docker compose up --abort-on-container-exit --exit-code-from fingerprint-client
docker compose down
```

Container 預設輸出至 stdout；若指定掛載目錄保存 JSONL，該目錄須允許 UID `10001` 寫入，輸出檔仍須尚未存在。
