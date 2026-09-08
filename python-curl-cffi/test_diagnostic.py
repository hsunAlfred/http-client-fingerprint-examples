"""Local HTTP fixtures test client control flow, not real TLS fingerprints."""

import asyncio
import contextlib
import io
import json
import os
import tempfile
import threading
import time
import unittest
from collections import Counter
from email.utils import formatdate
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import AsyncMock, patch
from urllib.parse import urlsplit

import diagnostic
from curl_cffi import AsyncSession, CurlOpt


GOOD = {"ja3_hash": "a" * 32, "ja3n_hash": "b" * 32,
        "ja4": "t13d1516h2_8daaf6152771_b0da82dd1658", "akamai_hash": "c" * 32}
SECRET = "sensitive-fixture-value"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        server = self.server
        path = urlsplit(self.path).path
        with server.lock:
            server.counts[path] += 1
            count = server.counts[path]
            server.active += 1
            server.peak = max(server.peak, server.active)
            server.cookies.append(self.headers.get("Cookie"))
        try:
            status, headers = 200, {"Set-Cookie": f"session={SECRET}; Path=/"}
            document = {**GOOD, "ip": SECRET, "url": SECRET, "user_agent": SECRET,
                        "cookie": SECRET, "nested": {"ja3_hash": SECRET}}
            body = json.dumps(document).encode()
            if path == "/invalid-json":
                body = b"not JSON: " + SECRET.encode()
            elif path == "/non-object":
                body = b"[]"
            elif path == "/missing":
                body = json.dumps({"tls": GOOD, "secret": SECRET}).encode()
            elif path == "/invalid-hash":
                body = json.dumps({"ja3_hash": SECRET}).encode()
            elif path == "/http1-fingerprint":
                body = json.dumps({**GOOD, "akamai_hash": ""}).encode()
            elif path == "/empty-akamai":
                body = json.dumps({"akamai_hash": ""}).encode()
            elif path == "/invalid-akamai-type":
                body = json.dumps({**GOOD, "akamai_hash": None}).encode()
            elif path == "/invalid-akamai-format":
                body = json.dumps({**GOOD, "akamai_hash": SECRET}).encode()
            elif path == "/empty-ja3":
                body = json.dumps({**GOOD, "ja3_hash": "", "akamai_hash": ""}).encode()
            elif path == "/oversized":
                body = b"x" * 200000
            elif path == "/retry" and count == 1:
                status, headers["Retry-After"] = 503, "1"
            elif path == "/budget":
                status, headers["Retry-After"] = 429, "3600"
            elif path == "/unauthorized":
                status = 401
            elif path == "/unavailable":
                status = 503
            elif path == "/redirect":
                status, headers["Location"] = 302, "/must-not-be-requested"
            elif path.startswith("/slow"):
                time.sleep(0.15)
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            for name, value in headers.items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with server.lock:
                server.active -= 1


class DiagnosticTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.server.lock = threading.Lock()
        cls.server.counts = Counter()
        cls.server.active = cls.server.peak = 0
        cls.server.cookies = []
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base = f"http://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def setUp(self):
        # Let deliberately aborted slow fixture requests finish before the next test.
        deadline = time.monotonic() + 1
        while self.server.active and time.monotonic() < deadline:
            time.sleep(0.01)
        self.server.counts.clear()
        self.server.cookies.clear()
        self.server.peak = 0

    def invoke(self, paths, *options):
        arguments = []
        for path in paths:
            arguments.extend(["--url", self.base + path])
        output, errors = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            code = diagnostic.main(arguments + list(options))
        text = output.getvalue() + errors.getvalue()
        self.assertNotIn(SECRET, text)
        self.assertNotIn(self.base, text)
        return code, [json.loads(line) for line in output.getvalue().splitlines()], errors.getvalue()

    def test_success_output_contract_and_sanitization(self):
        code, rows, summary = self.invoke([f"/ok?token={SECRET}"])
        self.assertEqual(code, 0)
        self.assertEqual(rows[0]["fingerprint"], GOOD)
        self.assertEqual(set(rows[0]), {"index", "client", "profile", "status",
                                     "http_version", "latency_ms", "attempts", "error",
                                     "fingerprint"})
        self.assertEqual(rows[0]["status"], 200)
        self.assertEqual(rows[0]["http_version"], "1.1")
        self.assertEqual(rows[0]["attempts"], 1)
        self.assertIsNone(rows[0]["error"])
        self.assertEqual(json.loads(summary)["success"], 1)

    def test_application_failures_are_not_successful(self):
        paths = ["/ok", "/invalid-json", "/non-object", "/missing", "/invalid-hash"]
        code, rows, summary = self.invoke(paths)
        self.assertEqual(code, 1)
        self.assertEqual([r["error"] for r in rows],
                         [None, "invalid_json", "invalid_schema", "missing_fingerprint",
                          "invalid_fingerprint"])
        self.assertEqual(json.loads(summary)["failed"], 4)
        self.assertTrue(all(row["attempts"] == 1 for row in rows))

    def test_oversize_aborts_and_next_request_reuses_capacity(self):
        code, rows, _ = self.invoke(["/oversized", "/ok"], "--max-bytes", "1024",
                                    "--concurrency", "1")
        self.assertEqual(code, 1)
        self.assertEqual(rows[0]["error"], "response_too_large")
        self.assertEqual(rows[0]["attempts"], 1)
        self.assertIsNone(rows[1]["error"])

    def test_absent_http2_hash_preserves_tls_but_invalid_values_still_fail(self):
        paths = ["/http1-fingerprint", "/empty-akamai", "/invalid-akamai-type",
                 "/invalid-akamai-format", "/empty-ja3"]
        code, rows, _ = self.invoke(paths)
        self.assertEqual(code, 1)
        self.assertEqual(rows[0]["fingerprint"],
                         {key: value for key, value in GOOD.items() if key != "akamai_hash"})
        self.assertEqual([r["error"] for r in rows],
                         [None, "missing_fingerprint", "invalid_fingerprint",
                          "invalid_fingerprint", "invalid_fingerprint"])
        self.assertTrue(all(row["attempts"] == 1 for row in rows))

    def test_retry_after_is_not_shortened(self):
        started = time.monotonic()
        code, rows, _ = self.invoke(["/retry"])
        self.assertEqual(code, 0)
        self.assertEqual(rows[0]["attempts"], 2)
        self.assertGreaterEqual(time.monotonic() - started, 1)

    def test_retry_after_exceeding_budget_stops_without_waiting(self):
        code, rows, _ = self.invoke(["/budget"], "--budget", "0.1")
        self.assertEqual(code, 1)
        self.assertEqual(rows[0]["error"], "retry_budget_exhausted")
        self.assertEqual(rows[0]["status"], 429)
        self.assertEqual(rows[0]["attempts"], 1)
        self.assertLess(rows[0]["latency_ms"], 500)

    def test_permanent_http_error_and_redirect_are_not_retried(self):
        code, rows, _ = self.invoke(["/unauthorized", "/redirect"])
        self.assertEqual(code, 1)
        self.assertEqual([r["error"] for r in rows], ["http_error", "redirect_rejected"])
        self.assertEqual([r["attempts"] for r in rows], [1, 1])
        self.assertEqual(self.server.counts["/must-not-be-requested"], 0)

    def test_maximum_attempts_and_per_attempt_timeout(self):
        code, rows, _ = self.invoke(["/unavailable", "/slow"], "--max-attempts", "2",
                                    "--timeout", "0.03")
        self.assertEqual(code, 1)
        self.assertEqual([r["attempts"] for r in rows], [2, 2])
        self.assertEqual([r["error"] for r in rows], ["http_error", "timeout"])

    def test_total_budget_cancels_transfer(self):
        code, rows, _ = self.invoke(["/slow", "/ok"], "--budget", "0.05",
                                    "--concurrency", "1")
        self.assertEqual(code, 1)
        self.assertIn(rows[0]["error"], {"timeout", "retry_budget_exhausted"})
        self.assertEqual(rows[0]["attempts"], 1)
        self.assertLess(rows[0]["latency_ms"], 200)
        self.assertIsNone(rows[1]["error"])

    def test_bounded_concurrency_preserves_input_order_and_discards_cookies(self):
        code, rows, _ = self.invoke([f"/slow/{i}" for i in range(8)], "--concurrency", "3")
        self.assertEqual(code, 0)
        self.assertEqual([r["index"] for r in rows], list(range(8)))
        self.assertGreater(self.server.peak, 1)
        self.assertLessEqual(self.server.peak, 3)
        self.assertTrue(all(value is None for value in self.server.cookies))

    def test_environment_proxy_does_not_change_route(self):
        with patch.dict(os.environ, {"http_proxy": "http://127.0.0.1:1",
                                     "HTTP_PROXY": "http://127.0.0.1:1",
                                     "all_proxy": "http://127.0.0.1:1", "no_proxy": "",
                                     "NO_PROXY": ""}):
            code, _, _ = self.invoke(["/ok"])
        self.assertEqual(code, 0)

    def test_output_file_creation_and_refusal_to_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = os.path.join(directory, "results.jsonl")
            code, rows, summary = self.invoke(["/ok"], "--output", destination)
            self.assertEqual((code, rows), (0, []))
            with open(destination, encoding="utf-8") as stream:
                saved = stream.read()
            self.assertEqual(json.loads(saved)["status"], 200)
            code, _, error = self.invoke(["/ok"], "--output", destination)
            self.assertEqual(code, 2)
            self.assertIn("output_error", error)
            self.assertNotIn(directory, error)
            with open(destination, encoding="utf-8") as stream:
                self.assertEqual(saved, stream.read())

    def test_write_failure_closes_session(self):
        closed = []

        class TrackedSession(AsyncSession):
            async def close(self):
                await super().close()
                closed.append(True)

        class FailedWriter(io.StringIO):
            def write(self, value):
                raise OSError(SECRET)

        errors = io.StringIO()
        with patch.object(diagnostic, "AsyncSession", TrackedSession):
            with contextlib.redirect_stdout(FailedWriter()), contextlib.redirect_stderr(errors):
                code = diagnostic.main(["--url", self.base + "/ok"])
        self.assertEqual(code, 2)
        self.assertEqual(closed, [True])
        self.assertNotIn(SECRET, errors.getvalue())

    def test_cancellation_releases_the_only_curl_handle(self):
        async def scenario():
            args = diagnostic.parse_args(["--url", self.base + "/slow"])
            async with AsyncSession(max_clients=1, curl_options={CurlOpt.PROXY: ""}) as session:
                task = asyncio.create_task(diagnostic.diagnose(session, 0, args.url[0], args))
                await asyncio.sleep(0.02)
                task.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await task
                result = await asyncio.wait_for(
                    diagnostic.diagnose(session, 1, self.base + "/ok", args), timeout=1)
                self.assertIsNone(result["error"])

        asyncio.run(scenario())

    def test_retry_after_date_and_invalid_values(self):
        now = 1700000000
        self.assertEqual(diagnostic.retry_after(formatdate(now + 60, usegmt=True), now), 60)
        self.assertEqual(diagnostic.retry_after(formatdate(now - 60, usegmt=True), now), 0)
        self.assertEqual(diagnostic.retry_after("1"), 1)
        self.assertEqual(diagnostic.retry_after("9" * 100), float("inf"))
        for invalid in (None, "garbage", "-1", "1.5"):
            self.assertIsNone(diagnostic.retry_after(invalid))

    def test_tls_and_proxy_errors_keep_their_classification_without_retry(self):
        async def scenario():
            args = diagnostic.parse_args(["--url", self.base + "/ok"])
            for error, expected in ((diagnostic.ProxyError(SECRET, 56), "proxy_error"),
                                    (diagnostic.SSLError(SECRET, 59), "tls_error")):
                session = AsyncMock()
                session.get.side_effect = error
                result = await diagnostic.diagnose(session, 0, args.url[0], args)
                self.assertEqual(result["error"], expected)
                self.assertEqual(result["attempts"], 1)
                self.assertNotIn(SECRET, json.dumps(result))

        asyncio.run(scenario())

    def test_validation_errors_never_echo_arguments(self):
        variants = [["--url", f"https://user:{SECRET}@example.test/"],
                    ["--url", self.base, "--timeout", SECRET],
                    ["--url", self.base, "--profile", SECRET],
                    ["--url", self.base, "--unknown", SECRET],
                    ["--url", self.base, "--budget", "nan"]]
        for argv in variants:
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()) as errors:
                with self.assertRaises(SystemExit) as raised:
                    diagnostic.parse_args(argv)
                self.assertEqual(raised.exception.code, 2)
                self.assertNotIn(SECRET, errors.getvalue())


if __name__ == "__main__":
    unittest.main()
