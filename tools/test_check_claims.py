#!/usr/bin/env python3
"""Tests for the EVI-4 artifact-reference checks in tools/check_claims.py.

Run: python3 -m unittest discover -s tools -p 'test_*.py'
The lint is exercised as a subprocess against throwaway trees, so the tests
cover the real argument handling and workflow discovery, not a stub.
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
LINT = HERE / "check_claims.py"


def workflow(name, artifact=None):
    """A minimal workflow file: a name, and optionally one upload step."""
    lines = [f"name: {name}", "on: [push]", "jobs:", "  gate:", "    runs-on: ubuntu-latest", "    steps:"]
    if artifact is not None:
        lines += [
            "      - uses: actions/upload-artifact@v7",
            "        with:",
            f"          name: {artifact}",
            "          path: bench-stage",
        ]
    else:
        lines += ["      - run: true"]
    return "\n".join(lines) + "\n"


def registry(artifact_present=True, artifact=None):
    claim = {
        "id": "test-1",
        "section": "Test",
        "kind": "local-measurement",
        "figures": ["42 ms"],
        "source": None,
        "command": None,
        "dataset": "generated",
        "measured": "2026-10-26",
        "reproducible_from_clone": False,
        "notes": "Fixture for the artifact checks.",
    }
    if artifact_present:
        claim["artifact"] = artifact
    return {"schema": 1, "claims": [claim]}


class ArtifactCheckTests(unittest.TestCase):
    def run_lint(self, reg, workflows):
        """Build a temp tree (README + registry + workflows) and run the lint."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "README.md").write_text(
                "<!-- claim:test-1 -->\n\nThe run took **42 ms**.\n", encoding="utf-8"
            )
            reg_path = root / "claims.json"
            reg_path.write_text(json.dumps(reg), encoding="utf-8")
            for filename, text in workflows.items():
                path = root / ".github" / "workflows" / filename
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")
            proc = subprocess.run(
                [sys.executable, str(LINT), "--registry", str(reg_path), "--readme", str(root / "README.md")],
                capture_output=True,
                text=True,
                check=False,
            )
            return proc.returncode, proc.stdout

    def test_null_artifact_passes(self):
        code, out = self.run_lint(registry(artifact=None), {"bench-regression.yml": workflow("benchmark regression", "bench-report")})
        self.assertEqual(code, 0, out)

    def test_valid_reference_passes(self):
        code, out = self.run_lint(
            registry(artifact={"name": "bench-report", "workflow": "benchmark regression"}),
            {"bench-regression.yml": workflow("benchmark regression", "bench-report")},
        )
        self.assertEqual(code, 0, out)

    def test_missing_field_fails(self):
        code, out = self.run_lint(registry(artifact_present=False), {})
        self.assertNotEqual(code, 0)
        self.assertIn("missing artifact field", out)

    def test_unknown_workflow_fails(self):
        code, out = self.run_lint(
            registry(artifact={"name": "bench-report", "workflow": "no such workflow"}),
            {"bench-regression.yml": workflow("benchmark regression", "bench-report")},
        )
        self.assertNotEqual(code, 0)
        self.assertIn("matches no workflow", out)

    def test_workflow_not_publishing_name_fails(self):
        code, out = self.run_lint(
            registry(artifact={"name": "other-artifact", "workflow": "benchmark regression"}),
            {"bench-regression.yml": workflow("benchmark regression", "bench-report")},
        )
        self.assertNotEqual(code, 0)
        self.assertIn("publishes no artifact named 'other-artifact'", out)

    def test_workflow_without_upload_fails(self):
        code, out = self.run_lint(
            registry(artifact={"name": "bench-report", "workflow": "benchmark regression"}),
            {"bench-regression.yml": workflow("benchmark regression")},
        )
        self.assertNotEqual(code, 0)
        self.assertIn("publishes no artifact named 'bench-report'", out)

    def test_unknown_key_fails(self):
        code, out = self.run_lint(
            registry(artifact={"name": "bench-report", "workflow": "benchmark regression", "retention": 90}),
            {"bench-regression.yml": workflow("benchmark regression", "bench-report")},
        )
        self.assertNotEqual(code, 0)
        self.assertIn("unknown key 'retention'", out)


if __name__ == "__main__":
    unittest.main()
