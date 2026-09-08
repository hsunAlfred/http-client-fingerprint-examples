# Rust reqwest／wreq 指紋範例

`fingerprint-client-rs` 是執行完指定次數就結束的 GET 診斷 CLI，用同一個端點比較一般 Client、只改 User-Agent 與 Browser Emulation。配套程式對應[Rust 瀏覽器指紋實作：reqwest 與 wreq 的 TLS／HTTP/2 差異](https://www.alfred.wiki/rust/rust-http-client-browser-fingerprint-reqwest-wreq/)。

## 環境與安裝

- Rust 1.98.0；`rust-toolchain.toml` 固定 toolchain。
- `reqwest 0.13.4`、`wreq 0.16.1`、`wreq-util 0.2.0`；直接相依版本固定，遞迴相依由 `Cargo.lock` 固定。
- Ubuntu／Debian 需 C/C++ compiler、CMake、Perl、pkg-config、libclang 與 Git；wreq 的 BoringSSL 和 reqwest 的 AWS-LC 需要原生建置工具。
- `reqwest` 關閉 default features，明確使用 `rustls` 與 `http2`。`wreq` 使用預設 Tokio runtime／WebPKI roots，並啟用正文範例的 HTTP、Cookie、Proxy、Streaming、WebSocket 與解壓縮 features。

以下指令皆從此 `rust-wreq` 目錄執行：

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake perl pkg-config libclang-dev git ca-certificates
cargo build --locked --release
```

## Quick Start

```bash
cargo run --locked --example quick_start
cargo run --locked -- --url https://tls.browserleaks.com/json --client wreq-chrome
```

四組診斷用相同 URL 分別執行；輸出檔必須尚未存在：

```bash
cargo run --locked -- --url https://tls.browserleaks.com/json --client reqwest --output reqwest.jsonl
cargo run --locked -- --url https://tls.browserleaks.com/json --client reqwest-ua --output reqwest-ua.jsonl
cargo run --locked -- --url https://tls.browserleaks.com/json --client wreq --output wreq.jsonl
cargo run --locked -- --url https://tls.browserleaks.com/json --client wreq-chrome --profile chrome124 --output wreq-chrome.jsonl
```

`reqwest-ua` 使用 Windows Chrome 124 的 User-Agent。`wreq-chrome` 明確指定 `Profile::Chrome124` 與 `Platform::Windows`，避免依賴套件的預設平台。其他兩組未套用 Browser Profile。

## CLI 輸入契約

| 參數 | 用途與範圍 | 預設 |
| --- | --- | --- |
| `--url` | 一個 HTTP(S) 診斷端點，最多 8192 bytes；拒絕帳密、fragment、空白與控制字元 | 必填 |
| `--client` | `reqwest`、`reqwest-ua`、`wreq`、`wreq-chrome` | `wreq-chrome` |
| `--profile` | 固定 Browser Profile，只接受 `chrome124`，供 `wreq-chrome` 使用 | `chrome124` |
| `--count` | GET 總次數，1–10000 | `1` |
| `--concurrency` | 同時執行的 request futures，1–16 | `2` |
| `--timeout-ms` | 每筆請求自開始傳輸到 body 消費完成的毫秒上限，10–120000 | `10000` |
| `--deadline-ms` | 排程開始後整批工作的毫秒上限，10–600000 | `60000` |
| `--max-bytes` | 交給 JSON decoder 的 body bytes 上限，1–1048576；wreq 的上限作用於解壓後資料 | `65536` |
| `--output` | 新建的 UTF-8 JSON Lines 檔案；`-` 表示 stdout | `-` |

CLI 只送 GET，保留憑證驗證，明確停用環境 Proxy、Redirect 與 Client 內建 retry。Cookie store 未啟用。收到 `429`／`503` 等回應時記錄該次結果並結束該筆請求，不自動重送；`attempts` 固定為 `1`，代表應用層嘗試次數，不代表底層封包或 TCP 連線數。

## 處理與資源上限

輸入 URL 通過驗證後，保留 `reqwest::Url` 解析與正規化的結果；reqwest 與 wreq 都使用同一個 `as_str()`，以一致處理 dot segments、反斜線、Unicode path／query 等輸入。

同一次執行只建立一個長生命週期 Client。`buffer_unordered` 只輪詢最多 `concurrency` 個 request futures，從 `0..count` 逐步取入工作，沒有預先 spawn 全部 task。每筆失敗仍產生一行結果，其他請求繼續；輸出寫入失敗或整批 deadline 到期則停止排程，drop 尚未完成的 futures。

每筆回應先保留 HTTP status 與實際 version，再逐 chunk 消費成功回應的 body。超過大小上限立即放棄該筆；非 2xx status 不讀取錯誤本文。這個上限約束程式累積的 body bytes，底層 HTTP、TLS、解壓器與 JSON 物件仍有額外記憶體成本。

成功回應完整讀取，讓連線有機會重用；錯誤／超量／取消時會提早 drop response，可能使該連線無法再次使用。併發數不等於 TCP 連線數，亦不是每秒請求速率限制。`deadline-ms` 在 Client 建立後才開始，使用 Tokio 的合作式 timeout，不能中斷阻塞中的檔案 I/O 或同步 JSON 解析；解析輸入另受 body 大小上限約束。

## JSONL 輸出與錯誤

結果依完成順序逐行寫入並 flush；`index` 是從 0 開始的原始序號。輸出只挑選 `ja3_hash`、`ja3n_hash`、`ja4`、`akamai_hash`：其中 `ja3_hash`、`ja3n_hash`、`akamai_hash` 要求 32 個小寫十六進位字元，`ja4` 另外驗證格式；空字串 `akamai_hash` 視為該協定資料不存在。解析後的任何已知欄位出現 null、錯誤型別或無效格式時整筆拒絕，且至少需要一個有效指紋欄位；成功只代表通過格式契約。其他欄位全部略過。`serde_json::Value` 對重複 JSON key 採最後一個值，再執行上述欄位檢查。

以下為輸出結構示意，摘要值使用測試資料：

```json
{"index":0,"client":"wreq-chrome","profile":"chrome124","platform":"windows","status":200,"http_version":"HTTP/2.0","latency_ms":180,"attempts":1,"error":null,"fingerprint":{"ja3_hash":"0123456789abcdef0123456789abcdef"}}
```

- `status`：收到的 HTTP 狀態碼；尚未收到 headers 時為 `null`。
- `http_version`：Client 回報的 HTTP version；尚未收到 headers 時為 `null`。
- `latency_ms`：該 request future 開始執行到完成的整數毫秒，不含排隊時間。
- `error`：成功為 `null`，失敗為固定分類字串。
- `fingerprint`：通過格式驗證的白名單摘要；失敗時為空物件。

錯誤分類包含 `timeout`、`dns_error`、`tls_error`、`proxy_error`、`connect_error`、`body_error`、`transport_error`、`redirect_rejected`、`http_error`、`response_too_large`、`invalid_json`、`invalid_schema`、`invalid_fingerprint`、`missing_fingerprint`。wreq 提供較細的 DNS／TLS predicates；reqwest 的公開 predicates 無法完整分拆 DNS／TLS，因此部分連線失敗統一為 `connect_error`。wreq 的 is_tls() 也未涵蓋所有包裝為 connect error 的握手／憑證失敗。不可把兩個 Client 的錯誤類別直接當作完全相同的分類體系。

正常跑完整批後，stderr 輸出一行摘要，例如：

```json
{"success":2,"failed":1,"errors":{"timeout":1}}
```

Exit code `0` 代表全部成功、`1` 代表整批完成但有請求失敗、`2` 代表參數、Client 建立、輸出或 batch deadline 造成執行中止。中止時不輸出完整批次摘要，已存在的 JSONL 可能不完整；磁碟或 pipe 寫入失敗也可能留下未完成的一行。CLI 不輸出 URL、IP、Header、Cookie、原始 body 或 library error source chain；應用程式仍可從 typed error 的 source 取得底層錯誤，使用時需自行控制敏感資料。

## 正文 Examples

| 範例 | 執行方式 | 用途 |
| --- | --- | --- |
| `quick_start` | `cargo run --locked --example quick_start` | 公開端點、Browser Profile 與 HTTP version |
| `http` | `cargo run --locked --example http -- <JSON_API_URL>` | Query、JSON、Form 與 response ownership；端點需直接回傳範例定義的 `Item` JSON |
| `download` | `cargo run --locked --example download -- <URL> <DESTINATION>` | byte stream、大小上限、暫存檔與提交 |
| `upload` | `cargo run --locked --example upload -- <multipart\|stream> <URL> <FILE>` | multipart 與檔案 stream |
| `websocket` | `cargo run --locked --example websocket -- <WS_ECHO_URL>` | WebSocket text／binary 與關閉 |
| `network` | `cargo run --locked --example network` | Proxy、CA、mTLS 設定函式的編譯驗證 |

download／upload 範例要求最終 2xx，避免把未追蹤或無法重播的 3xx 當成成功。下載 Future 正常 drop 時由 NamedTempFile 嘗試清理暫存檔；刪除失敗或程序直接終止仍可能留下檔案，原子替換前的舊目的檔不受影響。

`cargo test` 的 fixtures 使用 loopback HTTP／WebSocket server，不依賴外部網站。`quick_start` 與上述公開端點診斷需手動執行，不會混入離線測試。

## 驗證

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

`tests/diagnostic.rs` 的 14 項測試覆蓋四種 Client、URL 正規化一致性、輸出白名單、JSON／schema／hash 錯誤、HTTP 狀態、禁止 redirect／retry、body 超限與解壓後超限、header／body timeout、併發上限、Cookie 不保存、整批取消、輸出失敗、CLI 參數／exit code／stderr 摘要、既存檔案保留、環境 Proxy 停用及連線失敗。

`tests/features.rs` 的 8 項測試驗證正文 HTTP、下載清理與取消、upload、WebSocket，以及下載 302、兩種上傳遇到 307／308 時拒絕成功。`tests/timeouts.rs` 的 2 項測試確認 `read_timeout` 在 Headers 完整返回前不隨片段到達重設，以及 Body frame 之間的應用程式暫停也可能觸發逾時。共 24 項測試。

作者另在文章工作目錄執行程式片段一致性檢查，確認正文範例來自配套 Rust 原始碼與 Cargo.toml。正式文章及文章校驗腳本未包含於這個範例 repository；Python 僅用於作者端文件檢查，不是 Rust CLI 或上述 Cargo 測試的執行依賴。

公開端點的實測紀錄保存在 [observations.json](observations.json)，Hash 為當次觀察值，不當成未來測試的固定答案。

## Container

```bash
docker build -t fingerprint-client-rs .
docker run --rm fingerprint-client-rs --help
docker run --rm fingerprint-client-rs --url https://tls.browserleaks.com/json --client wreq-chrome
docker compose config
```

Image 以非 root 使用者執行 CLI。預設輸出 stdout；若要寫入掛載目錄，目錄需允許 UID `10001` 寫入。Container 預設行為是顯示 CLI help 後結束。
