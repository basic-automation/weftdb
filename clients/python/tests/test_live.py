#!/usr/bin/env python3
"""The client against a real weft-server: the contract test.

Run: WEFT_SERVER_BIN=target/debug/weft-server python3 -m unittest discover -s clients/python/tests -p 'test_*.py'
Without WEFT_SERVER_BIN, or with it naming no file, the suite is skipped (the unit tests
in test_client.py still run). The server is started on a free loopback port over a
throwaway store and run directory, with a clean environment, and stopped afterwards.
"""

import hashlib
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import weftdb  # noqa: E402
from weftdb import Client  # noqa: E402

SERVER_BIN = os.environ.get("WEFT_SERVER_BIN")

try:
    import pyarrow  # noqa: F401

    HAVE_PYARROW = True
except ImportError:
    HAVE_PYARROW = False


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


@unittest.skipUnless(SERVER_BIN and Path(SERVER_BIN).is_file(), "set WEFT_SERVER_BIN to a built weft-server to run the live contract")
class LiveTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.dir = tempfile.TemporaryDirectory()
        root = Path(cls.dir.name)
        (root / "runs" / "nightly").mkdir(parents=True)
        cls.manifest = b'{"schema_version":23,"generated_at":"2026-11-03T00:00:00Z","claim":"sdk-live"}'
        (root / "runs" / "nightly" / "interp__weftdb.manifest.json").write_bytes(cls.manifest)
        port = free_port()
        env = {"PATH": os.environ.get("PATH", ""), "HOME": root.as_posix(), "WEFT_SERVER_ADDR": f"127.0.0.1:{port}", "WEFT_SEGMENT_STORE_ROOT": (root / "store").as_posix(), "WEFT_BENCH_RUNS_DIR": (root / "runs").as_posix(), "WEFT_GPU_CALIBRATE": "0", "RUST_LOG": "warn"}
        cls.log = open(root / "server.log", "wb")
        cls.server = subprocess.Popen([SERVER_BIN], env=env, stdout=cls.log, stderr=subprocess.STDOUT)
        cls.client = Client(f"http://127.0.0.1:{port}", timeout=10)
        deadline = time.monotonic() + 60
        while True:
            try:
                cls.client.health()
                break
            except weftdb.WeftError:
                if cls.server.poll() is not None or time.monotonic() > deadline:
                    cls.tearDownClass()
                    raise RuntimeError("weft-server did not come up; its log:\n" + (root / "server.log").read_text(errors="replace"))
                time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        cls.server.terminate()
        try:
            cls.server.wait(timeout=20)
        except subprocess.TimeoutExpired:
            cls.server.kill()
            cls.server.wait()
        cls.log.close()
        cls.dir.cleanup()

    def test_probes(self):
        self.assertIn("status", self.client.health())
        ready = self.client.ready()
        self.assertTrue(ready["ready"])
        self.assertTrue(ready["segment_store"])

    def test_a_series_round_trips_losslessly(self):
        self.client.declare_aspect("price", "scaled_i64", scale=2, value_tolerance="0", timestamp_unit="millis")
        # Declaring the identical schema again is a no-op, not a conflict.
        self.client.declare_aspect("price", "scaled_i64", scale=2, value_tolerance="0", timestamp_unit="millis")
        written = self.client.write_points("price", [(1000, Decimal("1.25")), (2000, "2.50"), (3000, 3)], idempotency_key="sdk-live-1")
        self.assertEqual(written["row_count"], 3)
        replay = self.client.write_points("price", [(1000, Decimal("1.25")), (2000, "2.50"), (3000, 3)], idempotency_key="sdk-live-1")
        self.assertTrue(replay["replayed"], "the same key answers the first commit instead of sealing twice")
        self.assertEqual(list(self.client.iter_points("price", 0, 9999, page_size=2)), [(1000, Decimal("1.25")), (2000, Decimal("2.50")), (3000, Decimal("3.00"))])
        self.assertEqual(self.client.at("price", 2000)["value"], "2.50")
        self.assertFalse(self.client.at("price", 2500)["found"])
        self.assertEqual([p["found"] for p in self.client.at_multi("price", [1000, 1500, 3000])], [True, False, True])
        self.assertEqual([t for t, _ in self.client.iter_value_points("price", "2", "4")], [2000, 3000])
        self.assertIn("timestamp", self.client.range("price", 0, 9999, format="csv"))
        down = self.client.stored_downsample("price", resolution="seconds", aggregations=["avg", "max"])
        self.assertEqual(down["buckets"], 3)
        self.assertEqual(self.client.aspect_schema("price")["aspect"]["scale"], 2)

    def test_the_storage_boundary_refuses_what_it_cannot_hold(self):
        self.client.declare_aspect("exact", "scaled_i64", scale=2, value_tolerance="0", timestamp_unit="millis")
        with self.assertRaises(weftdb.BadRequest) as caught:
            self.client.write_points("exact", [(1, "1.234")])
        self.assertEqual(caught.exception.code, "value_not_representable")
        with self.assertRaises(weftdb.NotFound) as caught:
            self.client.aspect_schema("never-declared")
        self.assertEqual(caught.exception.code, "aspect_not_found")
        self.assertTrue(caught.exception.request_id)

    def test_compute(self):
        points = [("1970-01-01T00:00:00Z", 0.0), ("1970-01-01T00:00:30Z", 30.0), ("1970-01-01T00:01:00Z", 60.0)]
        answer = self.client.interpolate(points, spline="linear", resolution="seconds")
        self.assertEqual(answer["output_points"], 61)
        self.assertEqual({p["kind"] for p in answer["points"]}, {"raw", "interpolated"})
        self.assertIn("value", self.client.interpolate_point(points, "1970-01-01T00:00:15Z", spline="linear"))
        self.assertEqual(self.client.downsample(points, resolution="minutes", aggregations=["avg"])["buckets"], 2)
        ilp = "cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n"
        self.assertEqual(self.client.interpolate_ilp(ilp, field="load", precision="s", spline="linear", resolution="seconds")["output_points"], 61)

    @unittest.skipUnless(HAVE_PYARROW, "pyarrow is not installed")
    def test_columnar_answers_decode(self):
        points = [("1970-01-01T00:00:00Z", 0.0), ("1970-01-01T00:01:00Z", 60.0)]
        self.assertEqual(self.client.interpolate(points, spline="linear", format="arrow").num_rows, 61)
        self.assertEqual(self.client.interpolate(points, spline="linear", format="parquet").num_rows, 61)

    def test_bench_runs(self):
        runs = self.client.bench_runs()
        expected = hashlib.sha256(self.manifest).hexdigest()
        self.assertEqual([(run["id"], run["path"], run["claim"]) for run in runs], [(expected, "nightly/interp__weftdb.manifest.json", "sdk-live")])
        self.assertEqual(self.client.bench_run(expected, raw=True), self.manifest)
        with self.assertRaises(weftdb.NotFound) as caught:
            self.client.bench_run(hashlib.sha256(b"edited").hexdigest())
        self.assertEqual(caught.exception.code, "run_not_found")

    def test_metrics(self):
        self.assertIn("weft_", self.client.metrics())
        self.assertIsInstance(self.client.profile(), dict)


if __name__ == "__main__":
    unittest.main()
