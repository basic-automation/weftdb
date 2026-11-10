#!/usr/bin/env python3
"""Unit tests for the weftdb client, against an in-process fake server.

Run: python3 -m unittest discover -s clients/python/tests -p 'test_*.py'
The fake records each request and answers from a table, so these pin what goes on the
wire (paths, query, headers, body) and how answers and refusals come back. The live
contract is tests/test_live.py, against a real weft-server.
"""

import hashlib
import json
import sys
import threading
import unittest
from datetime import datetime, timezone
from decimal import Decimal
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import weftdb  # noqa: E402
from weftdb import Client  # noqa: E402


class Fake:
    """A server whose answers are set per test, and which keeps every request it saw."""

    def __init__(self):
        self.requests = []
        self.answers = {}
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _serve(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                url = urlsplit(self.path)
                fake.requests.append({"method": self.command, "path": url.path, "query": parse_qs(url.query), "headers": dict(self.headers.items()), "body": body})
                answer = fake.answers.get((self.command, url.path))
                if callable(answer):
                    answer = answer(len(fake.requests))
                status, headers, payload = answer or (404, {}, {"error": "no route", "code": "route_not_found"})
                data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
                self.send_response(status)
                for key, value in headers.items():
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = _serve

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def last(self):
        return self.requests[-1]


class ClientTest(unittest.TestCase):
    def setUp(self):
        self.fake = Fake()
        self.client = Client(self.fake.url, api_key="weft_test", max_retries=1)

    def tearDown(self):
        self.fake.close()

    def answer(self, method, path, payload, status=200, headers=None):
        self.fake.answers[(method, path)] = (status, headers or {}, payload)

    def test_the_key_goes_in_the_authorization_header_and_never_the_url(self):
        self.answer("GET", "/health", {"status": "ok"})
        self.assertEqual(self.client.health(), {"status": "ok"})
        sent = self.fake.last()
        self.assertEqual(sent["headers"]["Authorization"], "Bearer weft_test")
        self.assertEqual(sent["query"], {})
        with self.assertRaises(ValueError):
            Client("http://127.0.0.1:1/?api_key=weft_x")

    def test_a_refusal_raises_the_class_its_envelope_names(self):
        self.answer("GET", "/api/v1/storage/missing/schema", {"error": "no such aspect", "code": "aspect_not_found", "request_id": "req-1"}, status=404)
        with self.assertRaises(weftdb.NotFound) as caught:
            self.client.aspect_schema("missing")
        self.assertEqual(caught.exception.code, "aspect_not_found")
        self.assertEqual(caught.exception.status, 404)
        self.assertEqual(caught.exception.request_id, "req-1")
        self.answer("POST", "/api/v1/storage/aspects", {"error": "conflict", "code": "schema_conflict"}, status=409)
        with self.assertRaises(weftdb.Conflict):
            self.client.declare_aspect("price", "scaled_i64", scale=2, timestamp_unit="millis")
        self.answer("GET", "/metrics", {"error": "role too low", "code": "forbidden"}, status=403)
        with self.assertRaises(weftdb.Forbidden):
            self.client.metrics()

    def test_headroom_reads_the_route(self):
        body = {"total_bytes": 10000, "available_bytes": 1000, "reserve_bytes": 400, "writable_bytes": 600, "refusing": False}
        self.answer("GET", "/api/v1/storage/headroom", body)
        self.assertEqual(self.client.headroom(), body)

    def test_an_overloaded_read_is_retried_after_retry_after(self):
        self.fake.answers[("GET", "/api/v1/storage/stats")] = lambda n: (503, {"Retry-After": "0"}, {"error": "full", "code": "overloaded"}) if n == 1 else (200, {}, {"aspects": 0})
        self.assertEqual(self.client.stats(), {"aspects": 0})
        self.assertEqual(len(self.fake.requests), 2)

    def test_an_overloaded_write_is_not_retried(self):
        self.answer("POST", "/api/v1/storage/price/points", {"error": "full", "code": "overloaded"}, status=503, headers={"Retry-After": "0"})
        with self.assertRaises(weftdb.Overloaded) as caught:
            self.client.write_points("price", [(1, "1")])
        self.assertEqual(caught.exception.retry_after, 0.0)
        self.assertEqual(len(self.fake.requests), 1)

    def test_stored_values_are_sent_as_lossless_text(self):
        self.answer("POST", "/api/v1/storage/price/points", {"row_count": 3})
        self.client.write_points("price", [(1000, Decimal("1.25")), {"timestamp": 2000, "value": "2.50"}, (3000, None)], require_sorted=True, idempotency_key="batch-7")
        sent = self.fake.last()
        self.assertEqual(json.loads(sent["body"]), {"points": [{"timestamp": 1000, "value": "1.25"}, {"timestamp": 2000, "value": "2.50"}, {"timestamp": 3000, "value": None}], "require_sorted": True})
        self.assertEqual(sent["headers"]["Idempotency-Key"], "batch-7")

    def test_a_float_is_refused_at_the_storage_boundary(self):
        with self.assertRaises(TypeError):
            self.client.write_points("price", [(1000, 1.1)])
        with self.assertRaises(TypeError):
            self.client.write_points("price", [(1.5, "1")])
        self.assertEqual(self.fake.requests, [], "nothing is sent")

    def test_aspect_names_are_one_path_segment(self):
        self.answer("GET", "/api/v1/storage/a%2Fb%20c/schema", {"name": "a/b c"})
        self.client.aspect_schema("a/b c")
        self.assertEqual(self.fake.last()["path"], "/api/v1/storage/a%2Fb%20c/schema")

    def test_compute_points_are_rfc3339(self):
        self.answer("POST", "/api/v1/interpolate", {"points": []})
        self.client.interpolate([(datetime(1970, 1, 1, tzinfo=timezone.utc), 0), ("1970-01-01T00:01:00Z", 60), (120, 1.5)], spline="linear", resolution="seconds", end="1970-01-01T00:03:00Z")
        body = json.loads(self.fake.last()["body"])
        self.assertEqual([p["timestamp"] for p in body["points"]], ["1970-01-01T00:00:00Z", "1970-01-01T00:01:00Z", "1970-01-01T00:02:00Z"])
        self.assertEqual(body["points"][2]["value"], 1.5)
        self.assertEqual(body["end"], "1970-01-01T00:03:00Z")
        with self.assertRaises(ValueError):
            self.client.interpolate([(datetime(1970, 1, 1), 0)])

    def test_iter_points_follows_the_cursor_to_the_end(self):
        pages = {None: {"points": [{"timestamp": 1, "value": "1.0"}, {"timestamp": 2, "value": None}], "next_cursor": "c2"}, "c2": {"points": [{"timestamp": 3, "value": "3.5"}]}}

        def page(_n):
            cursor = self.fake.requests[-1]["query"].get("cursor", [None])[0]
            return 200, {}, pages[cursor]

        self.fake.answers[("GET", "/api/v1/storage/price/points")] = page
        self.assertEqual(list(self.client.iter_points("price", 0, 10, page_size=2)), [(1, Decimal("1.0")), (2, None), (3, Decimal("3.5"))])
        self.assertEqual([r["query"].get("limit") for r in self.fake.requests], [["2"], ["2"]])

    def test_downsample_formats_pick_the_suffix(self):
        self.answer("GET", "/api/v1/storage/price/downsample.csv", b"bucket,avg\n")
        self.assertEqual(self.client.stored_downsample("price", resolution="hours", aggregations=["avg", "p99"], format="csv"), "bucket,avg\n")
        self.assertEqual(self.fake.last()["query"]["agg"], ["avg,p99"])
        self.answer("GET", "/api/v1/storage/price/range.parquet", b"PAR1")
        self.assertEqual(self.client.range("price", 0, 1, format="parquet", raw=True), b"PAR1")
        with self.assertRaises(ValueError):
            self.client.range("price", 0, 1, format="json")

    def test_a_bench_run_is_checked_against_its_id(self):
        manifest = b'{"schema_version":23}'
        run_id = hashlib.sha256(manifest).hexdigest()
        self.answer("GET", f"/bench/runs/{run_id}", manifest)
        self.assertEqual(self.client.bench_run(run_id), {"schema_version": 23})
        other = hashlib.sha256(b"other").hexdigest()
        self.answer("GET", f"/bench/runs/{other}", manifest)
        with self.assertRaises(weftdb.IntegrityError):
            self.client.bench_run(other)

    def test_ready_returns_the_document_on_503(self):
        self.answer("GET", "/ready", {"ready": False, "incompatible_store": True}, status=503)
        self.assertEqual(self.client.ready()["ready"], False)

    def test_an_unreachable_server_is_a_weft_error(self):
        self.fake.close()
        with self.assertRaises(weftdb.WeftError):
            Client(self.fake.url, timeout=2).health()
        self.fake = Fake()  # tearDown closes one


if __name__ == "__main__":
    unittest.main()
