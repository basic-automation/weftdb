"""Tests for check-baseline-control.py (FRE-18c, P1)."""

import hashlib
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

_MODULE_PATH = Path(__file__).with_name("check-baseline-control.py")
spec = importlib.util.spec_from_file_location(
    "check_baseline_control", str(_MODULE_PATH),
)
assert spec is not None and spec.loader is not None
cbc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cbc)


class BaselineControlTest(unittest.TestCase):
    """Digest comparison logic."""

    def _write(self, path: str, content: str) -> str:
        with open(path, "w") as f:
            f.write(content)
        return path

    def test_identical_digests_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            baseline = os.path.join(tmp, "baseline.json")
            digest_file = os.path.join(tmp, "baseline.json.sha256")
            self._write(baseline, '{"a": 1}')
            h = hashlib.sha256()
            with open(baseline, "rb") as f:
                h.update(f.read())
            digest = h.hexdigest()
            self._write(digest_file, f"{digest}  baseline.json")
            rc = cbc.main_with_args(
                baseline, digest_file, base_digest=digest,
            )
            self.assertEqual(rc, 0)

    def test_different_digests_fail(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            baseline = os.path.join(tmp, "baseline.json")
            digest_file = os.path.join(tmp, "baseline.json.sha256")
            self._write(baseline, '{"a": 1}')
            self._write(
                digest_file,
                "0000000000000000000000000000000000000000000000000000000000000000"
                "  baseline.json",
            )
            rc = cbc.main_with_args(
                baseline, digest_file, base_digest="00000000000000000000000000000000"
                "0000000000000000000000000000000000",
            )
            self.assertEqual(rc, 1)

    def test_missing_baseline_exits_2(self) -> None:
        rc = cbc.main_with_args(
            "/nonexistent/baseline.json",
            "/nonexistent/baseline.json.sha256",
            base_digest="0000000000000000000000000000000000000000000000000000000000000000",
        )
        self.assertEqual(rc, 2)

    def test_missing_digest_file_exits_2(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            baseline = os.path.join(tmp, "baseline.json")
            self._write(baseline, '{"a": 1}')
            rc = cbc.main_with_args(
                baseline,
                "/nonexistent/digest.sha256",
                base_digest="0000000000000000000000000000000000000000000000000000000000000000",
            )
            self.assertEqual(rc, 2)


if __name__ == "__main__":
    unittest.main()
