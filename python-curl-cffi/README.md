# Python curl_cffi 診斷範例

`diagnostic.py` 對指定的診斷端點發出 GET，以有界併發收集指紋，輸出可比較的 JSON Lines。端點須回傳本文定義的 JSON 契約；一般 HTTP API 即使回傳 200，也不一定符合契約。

套件固定為 `curl_cffi==0.16.3`，最低 Python 版本為 3.10。`requirements.txt` 也安裝官方 CLI 與文章對照組使用的 `requests==2.34.2`。這份檔案固定直接相依套件，尚未鎖定所有傳遞相依套件與 wheel hash。

## 執行方式

從此目錄執行：

```bash
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -r requirements.txt
python diagnostic.py \
  --url https://tls.browserleaks.com/json \
  --url https://tls.browserleaks.com/json \
  --profile chrome124 \
  --concurrency 2 \
  --timeout 10 \
  --budget 30 \
  --max-attempts 3 \
  --max-bytes 65536 \
  --output curl-fingerprint-results.jsonl
```

`chrome124` 是為了重現實驗而選擇的固定舊 profile。CLI 只接受此 target，啟動時會檢查套件版本與底層支援情況；這不代表應將 Chrome 124 作為目前瀏覽器的模擬目標。更新 target 時，應同時重新取得指紋觀察值。

| 參數 | 用途與預設值 |
| --- | --- |
| `--url` | 必填，可重複，最多 10000 筆；接受 HTTP／HTTPS，拒絕 URL 帳密、fragment、空白與控制字元。重複 URL 分別執行 |
| `--profile` | Browser Target，固定 `chrome124` |
| `--concurrency` | 每批最多 2 個 task，可設 1–16；同時作為 `max_clients` 的 Curl handle 上限 |
| `--timeout` | 每次傳輸的秒數上限，預設 10，可設 0.01–120 |
| `--budget` | 每筆 URL 從開始執行到重試等待的總秒數預算，預設 30，可設 0.05–300；不含前面批次的排隊時間 |
| `--max-attempts` | 包含第一次傳輸的嘗試次數上限，預設 3，可設 1–5 |
| `--max-bytes` | 解壓後 response body 的 byte 上限，預設 65536，可設 1–1048576 |
| `--output` | UTF-8 JSONL 檔案；預設 `-` 寫入 stdout。檔案必須尚未存在，以免覆寫先前結果 |

只有成功開啟輸出後才開始發出請求。stdout 用於 JSONL 結果，stderr 用於統計或固定的錯誤訊息。URL 不會寫入輸出，但 CLI 參數仍可能出現在 shell history 或程序列表，診斷 URL 應使用不帶敏感 query 的端點。

## 輸入與輸出契約

HTTP 狀態必須是 2xx，response body 必須是 JSON object，且至少包含下列一個頂層欄位。這份 adapter 對應 BrowserLeaks 的欄位命名；其他服務若將資料放在 `tls` 等巢狀物件，須先明確調整映射。

| 指紋欄位 | 接受格式 |
| --- | --- |
| `ja3_hash` | 32 個小寫十六進位字元 |
| `ja3n_hash` | 32 個小寫十六進位字元 |
| `akamai_hash` | 32 個小寫十六進位字元；空字串代表未提供，略過此欄位 |
| `ja4` | TLS JA4 的 `t`／`q` 前綴、版本、SNI 記號、數量與 ALPN，加上兩段各 12 個字元的小寫十六進位 hash |

未知欄位直接丟棄。BrowserLeaks 在未建立 HTTP/2 時可回傳 `akamai_hash=""`，因此只有此欄位的空字串視為未提供，仍可保留有效的 TLS 指紋；略過後若沒有任何有效指紋，回報 `missing_fingerprint`。其他已知欄位空字串、`akamai_hash` 的錯誤型別（包含 `null`）或非空格式錯誤，整筆回報 `invalid_fingerprint`，不保留部分指紋。這裡驗證的是字串格式，沒有重新計算或證明第三方回傳的 hash 正確。

每個 URL 產生一行 JSON，依輸入順序輸出；`index` 從 0 開始，可用原始輸入列表對回目標。下列是有效的精簡輸出示意，延遲僅為示意值：

```json
{"index":0,"client":"curl_cffi/0.16.3","profile":"chrome124","status":200,"http_version":"2","latency_ms":680.0,"attempts":1,"error":null,"fingerprint":{"ja3n_hash":"4c9ce26028c11d7544da00d3f7e4f45c"}}
```

| 結果欄位 | 語意 |
| --- | --- |
| `index` | 原始輸入列表中的位置，整數，從 0 開始 |
| `client` | Client 名稱與版本，固定 `curl_cffi/0.16.3` |
| `profile` | 本次套用的固定 profile |
| `status` | 最後一次嘗試取得的 HTTP 狀態碼；未取得時為 `null` |
| `http_version` | 最後一次傳輸的實際協定，字串 `1.0`、`1.1`、`2` 或 `3`；未取得／未辨識時為 `null` |
| `latency_ms` | 這筆工作的總耗時，包含所有嘗試與重試等待，毫秒，取至小數點後 3 位 |
| `attempts` | 實際發起的嘗試次數，包含第一次 |
| `error` | 成功為 `null`，失敗為固定分類字串 |
| `fingerprint` | 成功時保留格式有效的白名單欄位；失敗為空物件 |

結果不含 URL、IP、User-Agent、Cookie、完整 Header、原始診斷 JSON 或 exception 字串。收集的 hash 仍是觀察資料，應依實際使用目的管理保存權限與期限；程式不額外保存原始回應。

## 併發、重試與資源生命週期

每批只建立 `concurrency` 個 task，整批完成並寫出結果後才開始下一批。這能限制 task、Curl handle 與待寫結果數量，但慢請求會延後下一批；此範例沒有每個 host 的 rate limit 協調，也不保證固定 requests per second。

所有 URL 共用一個 `AsyncSession`，固定 profile 與直連路由，使用 `discard_cookies=True` 避免不同工作累積 Cookie。TLS 憑證驗證保持開啟，redirect 不跟隨。除了 `trust_env=False`，還明確使用 `curl_options={CurlOpt.PROXY: ""}` 停用 libcurl 的 proxy 環境設定；0.16.3 的 `trust_env` 不足以代表所有環境設定都已排除，CA bundle 環境變數仍可能生效。

程式使用非 `stream=True` 的 `content_callback` 逐塊接收 body，超過上限時回傳 `CURL_WRITEFUNC_ERROR` 中止。0.16.3 的 callback 回傳 0 只會觸發警告，不能用來可靠中止傳輸。body 緩衝不超過設定上限，但上限不代表整個 Python 程序的記憶體限制：libcurl、Header、目前收到的 chunk 與 JSON 解析仍有額外用量。callback 路徑的 handle 由 `AsyncSession` 在完成、失敗或取消時釋放，外層 `async with` 關閉 session。

只有 GET 會送出。HTTP 429／502／503／504，以及 DNS 解析失敗、連線失敗、timeout、傳送／接收錯誤可重試；其他 HTTP 錯誤、redirect、TLS 錯誤、JSON／schema 錯誤與超量回應不重試。SDK 內建重試設為 0，由程式統一管理次數與預算。

一般退避等待為 `0.25 × 2^(已嘗試次數−1)` 秒，加 0–0.1 秒 jitter。有效的 `Retry-After` 秒數或 HTTP-date 是等待下限；若完整等待時間超出剩餘預算，直接停止，不縮短等待後提前重送。HTTP-date 需要本機時鐘準確；無效的 Header 回到一般退避。總預算使用 monotonic clock，已在途中消耗的時間會扣除。

| 錯誤分類 | 行為 |
| --- | --- |
| `http_error` | HTTP 非 2xx／3xx；只有上述四種狀態可在預算內重試 |
| `redirect_rejected` | 收到 3xx，不跟隨、不重試 |
| `timeout`、`dns_error`、`connect_error` | 暫時性 transport 錯誤，可在預算內重試 |
| `tls_error`、`proxy_error` | TLS／proxy 錯誤，不重試 |
| `transport_error` | 其他 libcurl 錯誤；只有明確列出的傳送／接收錯誤碼可重試 |
| `response_too_large` | 解壓後 body 超量，中止、不解析、不重試 |
| `invalid_json`、`invalid_schema` | 非有效 JSON 或頂層不是 object，不重試 |
| `missing_fingerprint`、`invalid_fingerprint` | 缺少可用欄位或已知指紋格式錯誤，不重試 |
| `retry_budget_exhausted` | 總時間已用完，或剩餘時間不足以完整等待下次重試 |

達到嘗試次數上限時，保留最後一次錯誤分類；`attempts` 顯示已執行次數。stderr 統計成功筆數、失敗筆數與各錯誤分類數量。exit code 0 表示全部成功；1 表示至少一筆診斷失敗；2 表示參數、版本、profile、輸出或內部錯誤；130 表示使用者中止。輸出中途失敗時可能留下部分 JSONL，stderr 會明確標示，不能將已有檔案視為整批成功。

## 驗證紀錄

驗證時間：2026-09-08 09:33 UTC；HTTP/2 指紋空值修正後於 09:41 UTC 重跑 unittest。環境：Ubuntu 22.04.5 LTS、Python 3.10.12、`curl_cffi==0.16.3`。

完成前述安裝後，可在此目錄重跑 CLI 測試、相依檢查及公開端點診斷：

```bash
source .venv/bin/activate
PYTHONDONTWRITEBYTECODE=1 python -m unittest -v test_diagnostic.py
python -m pip check
PYTHONDONTWRITEBYTECODE=1 python diagnostic.py \
  --url https://tls.browserleaks.com/json \
  --concurrency 1 --max-attempts 1 --timeout 15 --budget 20
```

- 17 個 stdlib unittest 全數通過，使用本機 HTTP fixture，驗證成功／失敗、指紋白名單、HTTP/2 指紋未提供與無效值的區別、body 上限、有界併發、順序、Cookie 丟棄、proxy 環境變數停用、重試與預算、redirect、timeout、取消後釋放 handle、輸出失敗及敏感資料不回顯；另以模擬例外驗證 TLS／proxy 錯誤分類與不重試行為。
- `pip check` 回報 `No broken requirements found`。
- 公開 BrowserLeaks 診斷實測 exit code 0、HTTP 200、HTTP/2；JA3N 為 `4c9ce26028c11d7544da00d3f7e4f45c`，Akamai hash 為 `52d84b11737d980aef856699f885ca86`。這是當次端點觀察，並非對所有環境成立的測試斷言。
- 尚未以封包擷取驗證真實 ClientHello、HTTP/2 SETTINGS 或 HTTP/3；本機 fixture 的 hash 是固定測試資料，不能當成 TLS 模擬證據。此 CLI 沒有實測 proxy、mTLS、CA 失敗或 DNS 故障；不是長期服務，因此沒有新增 Docker 部署流程。

## API 依據

對應文章：[Python curl_cffi 實作：Browser Impersonation、Session、Asyncio 與 WebSocket](https://www.alfred.wiki/python/python-curl-cffi-browser-impersonation-asyncio-websocket/)。

- [curl_cffi Quick Start](https://curl-cffi.readthedocs.io/en/stable/quick_start.html)：Requests-like API、callback 與 Session。
- [0.16.3 Session 原始碼](https://github.com/lexiforest/curl_cffi/blob/v0.16.3/curl_cffi/requests/session.py)：版本對應的 AsyncSession 與資源釋放。
- [0.16.3 Curl 原始碼](https://github.com/lexiforest/curl_cffi/blob/v0.16.3/curl_cffi/curl.py)：callback 的中止語意。
- [RFC 9110 Retry-After](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after)：HTTP-date 與 delay-seconds 格式。

## 作者端文章範例驗證紀錄

作者另在文章工作目錄中，直接讀取 Python code fences 並於本機 fixture 執行。這項驗證使用 Python 3.10.12、`openssl` 與 `websockets==15.0.1`；正式文章、文章校驗腳本及其測試相依檔未包含於這個範例 repository。前節的 `test_diagnostic.py` 則可在此目錄直接執行。

- HTTP 範例：7 段通過，涵蓋 Header、Body、Redirect、Cookie、Multipart、HTTP Proxy 路由與自有 mTLS。只替換 httpbin URL 或提供範例原本定義的環境變數，保留程式邏輯與 assertion；測試憑證和私鑰在結束時移除。
- Async 與進階範例：8 段通過，涵蓋有界 batch、stream iterator、callback 下載與輸出保留、async upload、WebSocket、Low-level Curl、READFUNCTION 及自有 Fingerprint 序列化。函式型範例由測試直接呼叫，沒有改寫函式內容；部分頂層 asyncio.run 示範呼叫改由測試 event loop 執行。
- 這些作者端測試只使用 loopback，不驗證公開站 TLS／HTTP/2／HTTP/3 或真實 WSS 指紋；其中 HTTP Proxy 與 mTLS 的結果屬於文章範例驗證，不代表 `diagnostic.py` 已完成相同連線驗證。
- `.gitignore` 排除本目錄的 venv 與 Python bytecode，測試不需產生可提交的私鑰或流量紀錄。
