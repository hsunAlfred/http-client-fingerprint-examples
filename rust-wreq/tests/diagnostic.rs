use std::{
    io::{self, Write},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use clap::Parser;
use fingerprint_client_rs::{Args, ClientKind, RunError, run, safe_fingerprint};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const VALID: &str = r#"{"ja3_hash":"0123456789abcdef0123456789abcdef","ja3n_hash":"abcdef0123456789abcdef0123456789","ja4":"t13d1516h2_8daaf6152771_02713d6af862","akamai_hash":"","ip":"PRIVATE_IP_MARKER","headers":{"authorization":"SECRET_HEADER_MARKER"}}"#;

struct Server {
    base: String,
    task: JoinHandle<()>,
    total: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let total = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let total_task = total.clone();
        let peak_task = peak.clone();
        let requests_task = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                total_task.fetch_add(1, Ordering::SeqCst);
                let active = active.clone();
                let peak = peak_task.clone();
                let requests = requests_task.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0_u8; 2048];
                    loop {
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                        if request.len() > 16384 {
                            return;
                        }
                    }
                    let request = String::from_utf8_lossy(&request).into_owned();
                    let path = request
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap()
                        .to_owned();
                    requests.lock().unwrap().push(request);
                    peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    let (status, extra, body) = match path.as_str() {
                        "/invalid-json" => ("200 OK", "", b"SECRET_BODY_MARKER".to_vec()),
                        "/invalid-schema" => ("200 OK", "", b"[]".to_vec()),
                        "/invalid-hash" => (
                            "200 OK",
                            "",
                            br#"{"ja3_hash":"SECRET_HASH_MARKER"}"#.to_vec(),
                        ),
                        "/missing-hash" => ("200 OK", "", b"{}".to_vec()),
                        "/redirect" => ("302 Found", "Location: /ok\r\n", Vec::new()),
                        "/retry" => ("503 Service Unavailable", "Retry-After: 0\r\n", Vec::new()),
                        "/too-large" => ("200 OK", "", vec![b'a'; 1024]),
                        "/compressed-large" => (
                            "200 OK",
                            "Content-Encoding: gzip\r\n",
                            vec![
                                31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 75, 76, 28, 5, 163, 96, 20, 140,
                                84, 0, 0, 185, 151, 85, 124, 0, 4, 0, 0,
                            ],
                        ),
                        "/broken-body" => ("200 OK", "", Vec::new()),
                        "/cookie" => (
                            "200 OK",
                            "Set-Cookie: session=SECRET_COOKIE_MARKER; Path=/\r\n",
                            VALID.as_bytes().to_vec(),
                        ),
                        _ => ("200 OK", "", VALID.as_bytes().to_vec()),
                    };
                    if path == "/slow-headers" || path == "/delay" {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                    let length = if path == "/broken-body" {
                        1000
                    } else {
                        body.len()
                    };
                    let headers = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n{extra}\r\n"
                    );
                    let _ = socket.write_all(headers.as_bytes()).await;
                    if path == "/slow-body" {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            base,
            task,
            total,
            peak,
            requests,
        }
    }

    fn args(&self, path: &str, client: &str) -> Args {
        Args::try_parse_from([
            "test",
            "--url",
            &format!("{}{path}", self.base),
            "--client",
            client,
        ])
        .unwrap()
    }
}

fn rows(output: &[u8]) -> Vec<Value> {
    std::str::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn four_clients_emit_only_safe_fields_and_correct_headers() {
    let server = Server::start().await;
    for client in ["reqwest", "reqwest-ua", "wreq", "wreq-chrome"] {
        let args = server.args("/ok?token=SECRET_URL_MARKER", client);
        let mut output = Vec::new();
        let summary = run(&args, &mut output).await.unwrap();
        assert_eq!(summary.success, 1);
        let row = rows(&output).remove(0);
        assert_eq!(row["status"], 200);
        assert_eq!(row["http_version"], "HTTP/1.1");
        assert_eq!(row["attempts"], 1);
        assert_eq!(row["fingerprint"].as_object().unwrap().len(), 3);
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("SECRET_"));
        assert!(!text.contains("PRIVATE_"));
        if client == "wreq-chrome" {
            assert_eq!(row["profile"], "chrome124");
            assert_eq!(row["platform"], "windows");
        } else {
            assert!(row["profile"].is_null());
        }
    }
    let requests = server.requests.lock().unwrap();
    assert!(!requests[0].to_lowercase().contains("chrome/124"));
    assert!(requests[1].contains("Chrome/124.0.0.0"));
    assert!(requests[3].contains("Chrome/124.0.0.0"));
    assert!(
        requests[3]
            .to_lowercase()
            .contains("sec-ch-ua-platform: \"windows\"")
    );
}

#[tokio::test]
async fn four_clients_use_the_same_normalized_request_target() {
    let server = Server::start().await;
    for (path, expected) in [
        ("/a/../ok", "/ok"),
        ("/a/%2e%2e/ok", "/ok"),
        (r"/a\ok", "/a/ok"),
        (
            "/測試?q=中文&encoded=%2F",
            "/%E6%B8%AC%E8%A9%A6?q=%E4%B8%AD%E6%96%87&encoded=%2F",
        ),
    ] {
        for client in ["reqwest", "reqwest-ua", "wreq", "wreq-chrome"] {
            let args = server.args(path, client);
            let summary = run(&args, &mut Vec::new()).await.unwrap();
            assert_eq!(summary.success, 1, "{client} {path}");
            let requests = server.requests.lock().unwrap();
            let request_target = requests.last().unwrap().split_whitespace().nth(1).unwrap();
            assert_eq!(request_target, expected, "{client} {path}");
        }
    }
}

#[tokio::test]
async fn errors_are_classified_without_echoing_response_or_url() {
    let server = Server::start().await;
    for client in ["reqwest", "wreq-chrome"] {
        for (path, expected) in [
            ("/invalid-json", "invalid_json"),
            ("/invalid-schema", "invalid_schema"),
            ("/invalid-hash", "invalid_fingerprint"),
            ("/missing-hash", "missing_fingerprint"),
            ("/redirect", "redirect_rejected"),
            ("/retry", "http_error"),
            ("/too-large", "response_too_large"),
            ("/broken-body", "body_error"),
        ] {
            let mut args = server.args(path, client);
            args.max_bytes = 512;
            let mut output = Vec::new();
            let before = server.total.load(Ordering::SeqCst);
            let summary = run(&args, &mut output).await.unwrap();
            assert_eq!(summary.failed, 1, "{client} {path}");
            assert_eq!(rows(&output)[0]["error"], expected, "{client} {path}");
            assert_eq!(
                server.total.load(Ordering::SeqCst),
                before + 1,
                "no redirect or retry"
            );
            assert!(!String::from_utf8(output).unwrap().contains("SECRET"));
        }
    }
}

#[tokio::test]
async fn timeout_covers_headers_and_body_and_keeps_received_status() {
    let server = Server::start().await;
    for client in ["reqwest", "wreq"] {
        for path in ["/slow-headers", "/slow-body"] {
            let mut args = server.args(path, client);
            args.timeout_ms = 40;
            let mut output = Vec::new();
            run(&args, &mut output).await.unwrap();
            let row = rows(&output).remove(0);
            assert_eq!(row["error"], "timeout");
            if path == "/slow-body" {
                assert_eq!(row["status"], 200);
            } else {
                assert!(row["status"].is_null());
            }
        }
    }
}

#[tokio::test]
async fn concurrency_is_bounded_and_every_index_is_emitted_once() {
    let server = Server::start().await;
    let mut args = server.args("/delay", "wreq-chrome");
    args.count = 6;
    args.concurrency = 2;
    let mut output = Vec::new();
    let summary = run(&args, &mut output).await.unwrap();
    assert_eq!(summary.success, 6);
    assert_eq!(server.peak.load(Ordering::SeqCst), 2);
    let mut indices: Vec<_> = rows(&output)
        .iter()
        .map(|row| row["index"].as_u64().unwrap())
        .collect();
    indices.sort();
    assert_eq!(indices, (0..6).collect::<Vec<_>>());
}

#[tokio::test]
async fn byte_limit_applies_after_decompression() {
    let server = Server::start().await;
    let mut args = server.args("/compressed-large", "wreq-chrome");
    args.max_bytes = 512;
    let mut output = Vec::new();
    run(&args, &mut output).await.unwrap();
    assert_eq!(rows(&output)[0]["error"], "response_too_large");
}

#[tokio::test]
async fn cookies_are_not_persisted_between_requests() {
    let server = Server::start().await;
    for client in ["reqwest", "wreq-chrome"] {
        let mut args = server.args("/cookie", client);
        args.count = 2;
        args.concurrency = 1;
        run(&args, &mut Vec::new()).await.unwrap();
    }
    for request in server.requests.lock().unwrap().iter() {
        assert!(!request.to_lowercase().contains("\r\ncookie:"));
    }
}

#[tokio::test]
async fn batch_deadline_cancels_without_starting_all_requests() {
    let server = Server::start().await;
    let mut args = server.args("/delay", "wreq");
    args.count = 20;
    args.concurrency = 2;
    args.deadline_ms = 40;
    let error = run(&args, &mut Vec::new()).await.unwrap_err();
    assert!(matches!(error, RunError::Deadline));
    assert!(server.total.load(Ordering::SeqCst) <= 2);
}

struct BrokenOutput;
impl Write for BrokenOutput {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "SECRET_OUTPUT_MARKER",
        ))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn output_failure_stops_scheduling_and_has_safe_display() {
    let server = Server::start().await;
    let mut args = server.args("/ok", "reqwest");
    args.count = 100;
    args.concurrency = 2;
    let error = run(&args, &mut BrokenOutput).await.unwrap_err();
    assert!(matches!(error, RunError::Output(_)));
    assert!(!error.to_string().contains("SECRET"));
    assert!(server.total.load(Ordering::SeqCst) <= 2);
}

#[test]
fn schema_rejects_invalid_known_fields_and_ignores_unknown_fields() {
    assert_eq!(safe_fingerprint(VALID.as_bytes()).unwrap().len(), 3);
    for invalid in [
        r#"{"ja3_hash":42}"#,
        r#"{"ja3_hash":"ABCDEF0123456789ABCDEF0123456789"}"#,
        r#"{"ja4":"t13d1516h2_8daaf6152771_02713d6af862SECRET"}"#,
        r#"{"ja4":"t13d1516h2_8daaf6152771_02713d6af86é"}"#,
        r#"{"ja3_hash":"0123456789abcdef0123456789abcdef","ja3n_hash":null}"#,
    ] {
        assert_eq!(
            safe_fingerprint(invalid.as_bytes()).unwrap_err().category(),
            "invalid_fingerprint"
        );
    }
    assert_eq!(
        safe_fingerprint(br#"{"akamai_hash":""}"#)
            .unwrap_err()
            .category(),
        "missing_fingerprint"
    );
}

#[test]
fn arguments_reject_credentials_fragments_schemes_and_out_of_range_values() {
    for url in [
        "file:///tmp/private",
        "https://user:SECRET@example.com/",
        "https://example.com/#secret",
        "https://example.com/ bad",
        "http://127.0.0.1:99999/",
    ] {
        let args = Args::try_parse_from(["test", "--url", url]).unwrap();
        assert!(args.validate().is_err());
    }
    for (flag, value) in [
        ("--count", "0"),
        ("--concurrency", "17"),
        ("--timeout-ms", "0"),
        ("--max-bytes", "1048577"),
        ("--profile", "chrome999"),
    ] {
        assert!(
            Args::try_parse_from(["test", "--url", "https://example.com/", flag, value]).is_err()
        );
    }
}

#[test]
fn cli_argument_and_output_errors_never_echo_sensitive_values_or_overwrite() {
    let binary = env!("CARGO_BIN_EXE_fingerprint-client-rs");
    let bad = Command::new(binary)
        .args([
            "--url",
            "https://user:SECRET@example.com/",
            "--count",
            "SECRET",
        ])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&bad.stderr).contains("SECRET"));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing.jsonl");
    std::fs::write(&path, b"preserve-me").unwrap();
    let output = Command::new(binary)
        .args(["--url", "http://127.0.0.1:1/?token=SECRET", "--output"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(std::fs::read(path).unwrap(), b"preserve-me");
    assert!(String::from_utf8_lossy(&output.stderr).contains("output_error"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_writes_jsonl_summary_exit_codes_and_ignores_proxy_environment() {
    let server = Server::start().await;
    for (path, exit_code) in [("/ok", 0), ("/invalid-json", 1)] {
        let url = format!("{}{path}", server.base);
        let output = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_fingerprint-client-rs"))
                .args(["--url", &url, "--client", "wreq-chrome"])
                .env("HTTP_PROXY", "http://127.0.0.1:1")
                .env("http_proxy", "http://127.0.0.1:1")
                .env("ALL_PROXY", "http://127.0.0.1:1")
                .env("NO_PROXY", "")
                .env("no_proxy", "")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(output.status.code(), Some(exit_code));
        assert_eq!(rows(&output.stdout).len(), 1);
        let summary: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(summary["success"], u32::from(exit_code == 0));
        assert_eq!(summary["failed"], u32::from(exit_code == 1));
    }
}

#[tokio::test]
async fn connection_failure_preserves_safe_category() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut args =
        Args::try_parse_from(["test", "--url", &format!("http://{address}/?token=SECRET")])
            .unwrap();
    for kind in [ClientKind::Reqwest, ClientKind::Wreq] {
        args.client = kind;
        let mut output = Vec::new();
        run(&args, &mut output).await.unwrap();
        assert_eq!(rows(&output)[0]["error"], "connect_error");
        assert!(!String::from_utf8(output).unwrap().contains("SECRET"));
    }
}
