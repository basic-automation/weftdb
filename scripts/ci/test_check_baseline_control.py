"""Tests for check-baseline-control.py (FRE-18c, P1)."""

import hashlib
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

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


class BaselineControlAPITest(unittest.TestCase):
    """Tests that exercise the gh api path (no network)."""

    def _write(self, path: str, content: str) -> str:
        with open(path, "w") as f:
            f.write(content)
        return path

    def test_fetch_relative_path_has_repo_relative_url(self) -> None:
        """A repo-relative path is used as-is in the API URL."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = "ZmFrZQ=="  # base64("fake")
            result = cbc._fetch_base_digest(
                "basic-automation/weftdb",
                "main",
                "compat/openapi/baseline/weftdb.json.sha256",
            )
            self.assertEqual(result, "fake")
            args, _ = mock_run.call_args
            url = args[0][2]
            self.assertEqual(
                url,
                "repos/basic-automation/weftdb/contents/"
                "compat/openapi/baseline/weftdb.json.sha256?ref=main",
            )

    def test_fetch_absolute_path_strips_workspace_from_url(self) -> None:
        """An absolute checkout path becomes repo-relative via GITHUB_WORKSPACE."""
        ws = "/home/runner/work/weftdb/weftdb"
        with patch.dict(os.environ, {"GITHUB_WORKSPACE": ws}, clear=False), \
                patch("subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = "ZmFrZQ=="
            result = cbc._fetch_base_digest(
                "basic-automation/weftdb",
                "main",
                f"{ws}/compat/openapi/baseline/weftdb.json.sha256",
            )
            self.assertEqual(result, "fake")
            args, _ = mock_run.call_args
            url = args[0][2]
            # Must contain the repo-relative portion, not the workspace prefix
            self.assertIn(
                "compat/openapi/baseline/weftdb.json.sha256", url,
            )
            self.assertNotIn("/home/runner/", url)

    def test_no_base_ref_returns_0_with_notice(self) -> None:
        """Empty GITHUB_BASE_REF exits 0 with a notice instead of erroring."""
        with patch.dict(os.environ, {"GITHUB_BASE_REF": ""}, clear=False), \
                tempfile.TemporaryDirectory() as tmp:
            baseline = os.path.join(tmp, "baseline.json")
            digest_file = os.path.join(tmp, "baseline.json.sha256")
            self._write(baseline, '{"a": 1}')
            self._write(digest_file, "deadbeef  baseline.json")
            rc = cbc.main_with_args(
                baseline, digest_file,
                repo="basic-automation/weftdb",
            )
            self.assertEqual(rc, 0)

    def test_gh_api_404_exits_2(self) -> None:
        """API failure (404) still exits 2."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value.returncode = 1
            mock_run.return_value.stderr = "Not Found (HTTP 404)"
            mock_run.return_value.stdout = ""
            with self.assertRaises(SystemExit) as ctx:
                cbc._fetch_base_digest(
                    "basic-automation/weftdb",
                    "main",
                    "compat/openapi/baseline/weftdb.json.sha256",
                )
            self.assertEqual(ctx.exception.code, 2)

    def test_bad_base64_exits_2(self) -> None:
        """Non-base64 content from gh api still exits 2."""
        with patch("subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = "!!!not-base64!!!"
            with self.assertRaises(SystemExit) as ctx:
                cbc._fetch_base_digest(
                    "basic-automation/weftdb",
                    "main",
                    "compat/openapi/baseline/weftdb.json.sha256",
                )
            self.assertEqual(ctx.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
