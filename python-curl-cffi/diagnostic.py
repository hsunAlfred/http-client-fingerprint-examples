"""Bounded, GET-only fingerprint diagnostics for explicitly chosen endpoints."""

import argparse
import asyncio
import json
import math
import random
import re
import sys
import time
from collections import Counter
from contextlib import nullcontext
from datetime import timezone
from email.utils import parsedate_to_datetime
from urllib.parse import urlsplit

import curl_cffi
from curl_cffi import AsyncSession, Curl, CurlOpt
from curl_cffi.curl import CURL_WRITEFUNC_ERROR
from curl_cffi.requests.exceptions import ProxyError, RequestException, SSLError


VERSION = "0.16.3"
PROFILE = "chrome124"
RETRY_STATUSES = {429, 502, 503, 504}
RETRY_CODES = {6, 7, 28, 55, 56}  # DNS, connect, timeout, send, receive
HTTP_VERSIONS = {1: "1.0", 2: "1.1", 3: "2", 30: "3"}
HASH = re.compile(r"[0-9a-f]{32}")
JA4 = re.compile(r"[tq][0-9]{2}[di][0-9]{4}[a-z0-9]{2}_[0-9a-f]{12}_[0-9a-f]{12}")
FINGERPRINT_FIELDS = {
    "ja3_hash": HASH,
    "ja3n_hash": HASH,
    "akamai_hash": HASH,
    "ja4": JA4,
}


class SafeParser(argparse.ArgumentParser):
    def error(self, message):
        # argparse's original message can contain URLs or credentials from argv.
        self.exit(2, "argument_error: 請使用 --help 核對參數與值域。\n")


def number(low, high, *, integer=False):
    def parse(value):
        try:
            result = int(value) if integer else float(value)
            if not math.isfinite(result) or not low <= result <= high:
                raise ValueError
            return result
        except (ValueError, OverflowError):
            raise argparse.ArgumentTypeError("數值超出允許範圍") from None

    return parse


def parse_args(argv=None):
    parser = SafeParser(description=__doc__)
    parser.add_argument("--url", action="append", required=True,
                        help="診斷端點；可重複，最多 10000 筆，不接受帳密或 fragment")
    parser.add_argument("--profile", choices=[PROFILE], default=PROFILE,
                        help="固定的 Browser Target（chrome124）")
    parser.add_argument("--concurrency", type=number(1, 16, integer=True), default=2,
                        help="每批最多幾個請求（1–16，預設 2）")
    parser.add_argument("--timeout", type=number(0.01, 120), default=10.0,
                        help="每次傳輸秒數上限（0.01–120，預設 10）")
    parser.add_argument("--budget", type=number(0.05, 300), default=30.0,
                        help="每筆 URL 的總秒數預算，包含重試等待（預設 30）")
    parser.add_argument("--max-attempts", type=number(1, 5, integer=True), default=3,
                        help="包含首次請求的嘗試次數上限（1–5，預設 3）")
    parser.add_argument("--max-bytes", type=number(1, 1048576, integer=True),
                        default=65536, help="解壓後 response body 上限（預設 65536）")
    parser.add_argument("--output", default="-",
                        help="JSONL 檔案路徑；必須尚未存在，- 代表 stdout")
    args = parser.parse_args(argv)
    if len(args.url) > 10000:
        parser.error("too many URLs")
    for url in args.url:
        try:
            parts = urlsplit(url)
            valid = (parts.scheme in {"http", "https"} and parts.hostname
                     and parts.username is None and parts.password is None
                     and not parts.fragment and not any(ord(c) <= 32 for c in url))
            _ = parts.port  # Reject invalid or out-of-range ports before networking.
        except ValueError:
            valid = False
        if not valid:
            parser.error("invalid URL")
    return args


def safe_fingerprint(body):
    try:
        document = json.loads(body)
    except (ValueError, UnicodeError, RecursionError):
        return {}, "invalid_json"
    if not isinstance(document, dict):
        return {}, "invalid_schema"
    selected = {}
    for key, pattern in FINGERPRINT_FIELDS.items():
        if key not in document:
            continue
        value = document[key]
        # BrowserLeaks uses an empty Akamai hash when HTTP/2 was not negotiated.
        if key == "akamai_hash" and value == "":
            continue
        if not isinstance(value, str) or not pattern.fullmatch(value):
            return {}, "invalid_fingerprint"
        selected[key] = value
    return (selected, None) if selected else ({}, "missing_fingerprint")


def retry_after(value, now=None):
    """Return the full delay; an absent/malformed value has no server constraint."""
    if value is None:
        return None
    value = value.strip()
    if re.fullmatch(r"[0-9]+", value):
        # Huge valid delta-seconds must stop retries, not wrap or become a short wait.
        return math.inf if len(value) > 12 else float(value)
    try:
        date = parsedate_to_datetime(value)
        if date.tzinfo is None:
            date = date.replace(tzinfo=timezone.utc)
        return max(0.0, date.timestamp() - (time.time() if now is None else now))
    except (TypeError, ValueError, OverflowError):
        return None


def transport_error(code):
    if code == 28:
        return "timeout"
    if code == 6:
        return "dns_error"
    if code == 7:
        return "connect_error"
    if code in {35, 51, 58, 60, 77, 80, 82, 83, 90, 91, 98}:
        return "tls_error"
    if code in {5, 97}:
        return "proxy_error"
    return "transport_error"


async def diagnose(session, index, url, args):
    started = time.monotonic()
    deadline = started + args.budget
    result = {"index": index, "client": f"curl_cffi/{VERSION}",
              "profile": args.profile, "status": None, "http_version": None,
              "latency_ms": 0, "attempts": 0, "error": None, "fingerprint": {}}
    while result["attempts"] < args.max_attempts:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            result["error"] = "retry_budget_exhausted"
            break
        body = bytearray()
        oversized = False

        def receive(chunk):
            nonlocal oversized
            if len(body) + len(chunk) > args.max_bytes:
                oversized = True
                return CURL_WRITEFUNC_ERROR
            body.extend(chunk)
            return len(chunk)

        result["attempts"] += 1
        result.update(status=None, http_version=None)
        response = None
        delay_from_server = None
        retryable = False
        try:
            response = await asyncio.wait_for(
                session.get(url, timeout=min(args.timeout, remaining),
                            content_callback=receive),
                timeout=remaining,
            )
            if 200 <= response.status_code < 300:
                result["fingerprint"], result["error"] = safe_fingerprint(body)
            else:
                result["error"] = ("redirect_rejected" if 300 <= response.status_code < 400
                                   else "http_error")
                retryable = response.status_code in RETRY_STATUSES
                delay_from_server = retry_after(response.headers.get("Retry-After"))
        except asyncio.TimeoutError:
            result["error"] = "retry_budget_exhausted"
        except RequestException as exc:
            response = exc.response
            if oversized:
                result["error"] = "response_too_large"
            elif isinstance(exc, ProxyError):
                result["error"] = "proxy_error"
            elif isinstance(exc, SSLError):
                result["error"] = "tls_error"
            else:
                result["error"] = transport_error(exc.code)
            retryable = (result["error"] not in {"response_too_large", "proxy_error", "tls_error"}
                         and exc.code in RETRY_CODES)
        if response is not None:
            result["status"] = response.status_code or None
            result["http_version"] = HTTP_VERSIONS.get(response.http_version)
        if not retryable or result["attempts"] >= args.max_attempts:
            break
        # Small exponential backoff plus jitter; Retry-After is a lower bound.
        delay = 0.25 * 2 ** (result["attempts"] - 1) + random.uniform(0, 0.1)
        delay = max(delay, delay_from_server or 0)
        if delay >= deadline - time.monotonic():
            result["error"] = "retry_budget_exhausted"
            break
        await asyncio.sleep(delay)
    result["latency_ms"] = round((time.monotonic() - started) * 1000, 3)
    return result


async def run(args, output):
    errors = Counter()
    success = 0
    async with AsyncSession(
        impersonate=args.profile, max_clients=args.concurrency,
        verify=True, trust_env=False, allow_redirects=False, retry=0,
        discard_cookies=True, curl_options={CurlOpt.PROXY: ""},
    ) as session:
        for offset in range(0, len(args.url), args.concurrency):
            batch = args.url[offset:offset + args.concurrency]
            results = await asyncio.gather(*(
                diagnose(session, offset + i, url, args) for i, url in enumerate(batch)
            ))
            for result in results:
                output.write(json.dumps(result, ensure_ascii=True, allow_nan=False) + "\n")
                output.flush()
                if result["error"] is None:
                    success += 1
                else:
                    errors[result["error"]] += 1
    summary = {"success": success, "failed": sum(errors.values()), "errors": dict(errors)}
    print(json.dumps(summary, sort_keys=True), file=sys.stderr)
    return 1 if errors else 0


def main(argv=None):
    args = parse_args(argv)
    try:
        if curl_cffi.__version__ != VERSION:
            print("version_error: 請依 requirements.txt 安裝固定版本。", file=sys.stderr)
            return 2
        curl = Curl()
        try:
            if curl.impersonate(args.profile) != 0:
                print("profile_error: 固定 Browser Target 不受支援。", file=sys.stderr)
                return 2
        finally:
            curl.close()
        destination = (nullcontext(sys.stdout) if args.output == "-" else
                       open(args.output, "x", encoding="utf-8"))
        with destination as output:
            return asyncio.run(run(args, output))
    except OSError:
        print("output_error: 無法建立或寫入輸出；已有內容可能不完整。", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("interrupted: 工作已中止；已有內容可能不完整。", file=sys.stderr)
        return 130
    except Exception:
        # Do not print exception strings, which may embed endpoints or credentials.
        print("internal_error: 工作失敗；已有內容可能不完整。", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
