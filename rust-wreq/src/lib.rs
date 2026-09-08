//! Finite, GET-only HTTP fingerprint comparisons with bounded output.

use std::{collections::BTreeMap, io::Write, time::Duration};

use clap::{Parser, ValueEnum};
use futures_util::{Stream, StreamExt, stream};
use serde::Serialize;
use thiserror::Error;
use tokio::time::Instant;
use wreq_util::{Emulation, Platform, Profile};

pub const CHROME_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[derive(Clone, Copy, Debug, ValueEnum, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    Reqwest,
    ReqwestUa,
    Wreq,
    WreqChrome,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum BrowserProfile {
    Chrome124,
}

/// Each invocation compares one client against one explicitly chosen endpoint.
#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Args {
    /// HTTP(S) diagnostic endpoint; credentials and fragments are rejected.
    #[arg(long)]
    pub url: String,
    /// Client implementation and optional browser profile.
    #[arg(long, value_enum, default_value = "wreq-chrome")]
    pub client: ClientKind,
    /// Fixed browser target; used only by wreq-chrome, with Windows headers.
    #[arg(long, value_enum, default_value = "chrome124")]
    pub profile: BrowserProfile,
    /// Total GET requests (1..=10000); no automatic retries.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=10000))]
    pub count: u32,
    /// Maximum in-flight request futures (1..=16).
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=16))]
    pub concurrency: u32,
    /// Per-request milliseconds, including response body (10..=120000).
    #[arg(long, default_value_t = 10000, value_parser = clap::value_parser!(u64).range(10..=120000))]
    pub timeout_ms: u64,
    /// Entire batch deadline in milliseconds (10..=600000).
    #[arg(long, default_value_t = 60000, value_parser = clap::value_parser!(u64).range(10..=600000))]
    pub deadline_ms: u64,
    /// Maximum decoded response bytes (1..=1048576).
    #[arg(long, default_value_t = 65536, value_parser = clap::value_parser!(u32).range(1..=1048576))]
    pub max_bytes: u32,
    /// New JSONL file; '-' writes stdout. Existing files are never overwritten.
    #[arg(long, default_value = "-")]
    pub output: String,
}

impl Args {
    /// Returns the shared, normalized URL used by both client implementations.
    pub fn validate(&self) -> Result<reqwest::Url, RunError> {
        let url = reqwest::Url::parse(&self.url).map_err(|_| RunError::Arguments)?;
        if self.url.len() > 8192
            || self
                .url
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
            || !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !(1..=10000).contains(&self.count)
            || !(1..=16).contains(&self.concurrency)
            || !(10..=120000).contains(&self.timeout_ms)
            || !(10..=600000).contains(&self.deadline_ms)
            || !(1..=1048576).contains(&self.max_bytes)
        {
            return Err(RunError::Arguments);
        }
        Ok(url)
    }
}

/// Sources remain available to trusted application code, never printed by the CLI.
#[derive(Debug, Error)]
pub enum DiagnosticError {
    #[error("reqwest transport failed")]
    Reqwest(#[source] reqwest::Error),
    #[error("wreq transport failed")]
    Wreq(#[source] wreq::Error),
    #[error("reqwest response body failed")]
    ReqwestBody(#[source] reqwest::Error),
    #[error("wreq response body failed")]
    WreqBody(#[source] wreq::Error),
    #[error("request deadline exceeded")]
    Timeout,
    #[error("redirect rejected")]
    Redirect,
    #[error("unsuccessful HTTP status")]
    HttpStatus,
    #[error("response body exceeds the byte limit")]
    TooLarge,
    #[error("invalid JSON")]
    Json(#[source] serde_json::Error),
    #[error("JSON root must be an object")]
    InvalidSchema,
    #[error("invalid fingerprint field")]
    InvalidFingerprint,
    #[error("no supported fingerprint field")]
    MissingFingerprint,
}

impl DiagnosticError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Reqwest(error) if error.is_timeout() => "timeout",
            Self::Wreq(error) if error.is_timeout() => "timeout",
            Self::ReqwestBody(error) if error.is_timeout() => "timeout",
            Self::WreqBody(error) if error.is_timeout() => "timeout",
            Self::Wreq(error) if error.is_dns() => "dns_error",
            Self::Wreq(error) if error.is_tls() => "tls_error",
            Self::Wreq(error) if error.is_proxy_connect() => "proxy_error",
            Self::Reqwest(error) if error.is_connect() => "connect_error",
            Self::Wreq(error) if error.is_connect() => "connect_error",
            Self::Reqwest(error) if error.is_body() => "body_error",
            Self::Wreq(error) if error.is_body() => "body_error",
            Self::Reqwest(_) | Self::Wreq(_) => "transport_error",
            Self::ReqwestBody(_) | Self::WreqBody(_) => "body_error",
            Self::Timeout => "timeout",
            Self::Redirect => "redirect_rejected",
            Self::HttpStatus => "http_error",
            Self::TooLarge => "response_too_large",
            Self::Json(_) => "invalid_json",
            Self::InvalidSchema => "invalid_schema",
            Self::InvalidFingerprint => "invalid_fingerprint",
            Self::MissingFingerprint => "missing_fingerprint",
        }
    }
}

#[derive(Debug, Error)]
pub enum RunError {
    #[error("argument_error: use --help to check the accepted arguments")]
    Arguments,
    #[error("client_build_error: unable to construct the selected client")]
    Client(#[source] DiagnosticError),
    #[error("output_error: unable to create or write output; output may be incomplete")]
    Output(#[source] std::io::Error),
    #[error("serialization_error: unable to encode a diagnostic result")]
    Serialization(#[source] serde_json::Error),
    #[error("batch_deadline: incomplete batch; emitted JSONL rows remain valid")]
    Deadline,
}

enum HttpClient {
    Reqwest(reqwest::Client),
    Wreq(wreq::Client),
}

impl HttpClient {
    fn new(args: &Args) -> Result<Self, DiagnosticError> {
        let timeout = Duration::from_millis(args.timeout_ms);
        match args.client {
            ClientKind::Reqwest | ClientKind::ReqwestUa => {
                let mut builder = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .connect_timeout(timeout.min(Duration::from_secs(5)))
                    .timeout(timeout);
                if args.client == ClientKind::ReqwestUa {
                    builder = builder.user_agent(CHROME_UA);
                }
                builder
                    .build()
                    .map(Self::Reqwest)
                    .map_err(DiagnosticError::Reqwest)
            }
            ClientKind::Wreq | ClientKind::WreqChrome => {
                let mut builder = wreq::Client::builder()
                    .no_proxy()
                    .redirect(wreq::redirect::Policy::none())
                    .retry(wreq::retry::Policy::never())
                    .connect_timeout(timeout.min(Duration::from_secs(5)))
                    .timeout(timeout);
                if args.client == ClientKind::WreqChrome {
                    builder = builder.emulation(
                        Emulation::builder()
                            .profile(Profile::Chrome124)
                            .platform(Platform::Windows)
                            .build(),
                    );
                }
                builder
                    .build()
                    .map(Self::Wreq)
                    .map_err(DiagnosticError::Wreq)
            }
        }
    }

    async fn fetch(&self, args: &Args, url: &str, row: &mut Record) -> Result<(), DiagnosticError> {
        match self {
            Self::Reqwest(client) => {
                let response = client
                    .get(url)
                    .send()
                    .await
                    .map_err(DiagnosticError::Reqwest)?;
                row.status = Some(response.status().as_u16());
                row.http_version = Some(format!("{:?}", response.version()));
                check_status(response.status().as_u16())?;
                row.fingerprint = read_body(
                    response
                        .bytes_stream()
                        .map(|r| r.map_err(DiagnosticError::ReqwestBody)),
                    args.max_bytes as usize,
                )
                .await?;
            }
            Self::Wreq(client) => {
                let response = client
                    .get(url)
                    .send()
                    .await
                    .map_err(DiagnosticError::Wreq)?;
                row.status = Some(response.status().as_u16());
                row.http_version = Some(format!("{:?}", response.version()));
                check_status(response.status().as_u16())?;
                row.fingerprint = read_body(
                    response
                        .bytes_stream()
                        .map(|r| r.map_err(DiagnosticError::WreqBody)),
                    args.max_bytes as usize,
                )
                .await?;
            }
        }
        Ok(())
    }
}

fn check_status(status: u16) -> Result<(), DiagnosticError> {
    match status {
        200..=299 => Ok(()),
        300..=399 => Err(DiagnosticError::Redirect),
        _ => Err(DiagnosticError::HttpStatus),
    }
}

async fn read_body<S, B>(
    stream: S,
    max_bytes: usize,
) -> Result<BTreeMap<String, String>, DiagnosticError>
where
    S: Stream<Item = Result<B, DiagnosticError>>,
    B: AsRef<[u8]>,
{
    let mut body = Vec::new();
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let bytes = chunk.as_ref();
        if bytes.len() > max_bytes - body.len() {
            return Err(DiagnosticError::TooLarge);
        }
        body.extend_from_slice(bytes);
    }
    safe_fingerprint(&body)
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_ja4(value: &str) -> bool {
    let parts: Vec<_> = value.split('_').collect();
    if parts.len() != 3 || parts[0].len() != 10 {
        return false;
    }
    let prefix = parts[0].as_bytes();
    matches!(prefix[0], b't' | b'q')
        && prefix[1..3].iter().all(u8::is_ascii_digit)
        && matches!(prefix[3], b'd' | b'i')
        && prefix[4..8].iter().all(u8::is_ascii_digit)
        && prefix[8..10]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && lower_hex(parts[1], 12)
        && lower_hex(parts[2], 12)
}

pub fn safe_fingerprint(body: &[u8]) -> Result<BTreeMap<String, String>, DiagnosticError> {
    let json: serde_json::Value = serde_json::from_slice(body).map_err(DiagnosticError::Json)?;
    let object = json.as_object().ok_or(DiagnosticError::InvalidSchema)?;
    let mut selected = BTreeMap::new();
    for name in ["ja3_hash", "ja3n_hash", "ja4", "akamai_hash"] {
        let Some(value) = object.get(name) else {
            continue;
        };
        let value = value.as_str().ok_or(DiagnosticError::InvalidFingerprint)?;
        // BrowserLeaks uses an empty Akamai hash when HTTP/2 is absent.
        if name == "akamai_hash" && value.is_empty() {
            continue;
        }
        let valid = if name == "ja4" {
            valid_ja4(value)
        } else {
            lower_hex(value, 32)
        };
        if !valid {
            return Err(DiagnosticError::InvalidFingerprint);
        }
        selected.insert(name.to_owned(), value.to_owned());
    }
    if selected.is_empty() {
        return Err(DiagnosticError::MissingFingerprint);
    }
    Ok(selected)
}

#[derive(Debug, Serialize)]
pub struct Record {
    pub index: u32,
    pub client: ClientKind,
    pub profile: Option<&'static str>,
    pub platform: Option<&'static str>,
    pub status: Option<u16>,
    pub http_version: Option<String>,
    pub latency_ms: u128,
    pub attempts: u32,
    pub error: Option<&'static str>,
    pub fingerprint: BTreeMap<String, String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub success: u32,
    pub failed: u32,
    pub errors: BTreeMap<&'static str, u32>,
}

async fn diagnose(client: &HttpClient, args: &Args, url: &str, index: u32) -> Record {
    let start = Instant::now();
    let emulated = args.client == ClientKind::WreqChrome;
    let mut row = Record {
        index,
        client: args.client,
        profile: emulated.then_some("chrome124"),
        platform: emulated.then_some("windows"),
        status: None,
        http_version: None,
        latency_ms: 0,
        attempts: 1,
        error: None,
        fingerprint: BTreeMap::new(),
    };
    let result = tokio::time::timeout(
        Duration::from_millis(args.timeout_ms),
        client.fetch(args, url, &mut row),
    )
    .await
    .unwrap_or(Err(DiagnosticError::Timeout));
    row.error = result.err().map(|error| error.category());
    row.latency_ms = start.elapsed().as_millis();
    row
}

/// Emits completion-order JSONL, with `index` identifying the original request.
/// Output errors drop the bounded stream and all remaining request futures.
pub async fn run(args: &Args, output: &mut impl Write) -> Result<Summary, RunError> {
    let url = args.validate()?;
    let client = HttpClient::new(args).map_err(RunError::Client)?;
    let work = async {
        let mut pending = stream::iter(0..args.count)
            .map(|index| diagnose(&client, args, url.as_str(), index))
            .buffer_unordered(args.concurrency as usize);
        let mut summary = Summary::default();
        while let Some(row) = pending.next().await {
            let encoded = serde_json::to_vec(&row).map_err(RunError::Serialization)?;
            output.write_all(&encoded).map_err(RunError::Output)?;
            output.write_all(b"\n").map_err(RunError::Output)?;
            output.flush().map_err(RunError::Output)?;
            if let Some(category) = row.error {
                summary.failed += 1;
                *summary.errors.entry(category).or_default() += 1;
            } else {
                summary.success += 1;
            }
        }
        Ok(summary)
    };
    tokio::time::timeout(Duration::from_millis(args.deadline_ms), work)
        .await
        .map_err(|_| RunError::Deadline)?
}
