"""Tests for check-test-wiring.py (CI-GATES, #853; hardening, #859).

Every test writes a fixture YAML workflow into a ``tempfile.TemporaryDirectory``
and runs ``_run_check`` against a set of stub test files created in the same
temporary tree.  The last test (``test_real_repo_passes``) runs the guard
against the actual repository tree — that is the regression that matters.

The two ``…_not_wired`` / ``blanket`` cases below are the #859 regressions:
both pass the guard before the hardening, and fail it after.
"""

import importlib.util as _util
import os
import tempfile
import unittest
from pathlib import Path

import yaml

_MODULE_PATH = Path(__file__).with_name("check-test-wiring.py")
spec = _util.spec_from_file_location("check_test_wiring", str(_MODULE_PATH))
assert spec is not None and spec.loader is not None
cwt = _util.module_from_spec(spec)
spec.loader.exec_module(cwt)


def _make_tree(tmpdir: str, test_files: list[str], wf_steps: list[dict]) -> tuple[str, str]:
    """Create a temporary repository tree with stub test files and a single
    workflow YAML, then return the root path and the workflows dir path."""
    root = os.path.join(tmpdir, "repo")
    wf_dir = os.path.join(root, ".github", "workflows")
    os.makedirs(wf_dir, exist_ok=True)

    # Create stub test files.
    for tf in test_files:
        fpath = os.path.join(root, tf)
        os.makedirs(os.path.dirname(fpath), exist_ok=True)
        with open(fpath, "w") as f:
            f.write("# stub\n")

    # Write the workflow.
    wf = {
        "name": "CI",
        "on": {"push": {"branches": ["main"]}},
        "jobs": {
            "test": {
                "name": "test job",
                "runs-on": "ubuntu-latest",
                "steps": [{"uses": "actions/checkout@v4"}] + wf_steps,
            },
        },
    }
    wf_path = os.path.join(wf_dir, "ci.yml")
    with open(wf_path, "w") as f:
        yaml.dump(wf, f)

    return root, wf_dir


class CheckTestWiringTest(unittest.TestCase):
    """Fixture-driven wiring checks."""

    def test_direct_invocation_wired(self) -> None:
        """A file run directly via ``python3 scripts/x/test_a.py`` is wired."""
        with tempfile.TemporaryDirectory() as tmp:
            root, wf_dir = _make_tree(
                tmp,
                ["scripts/x/test_a.py"],
                [{"run": "python3 scripts/x/test_a.py"}],
            )
            rc = cwt._run_check(root, wf_dir)
            self.assertEqual(rc, 0)

    def test_discover_wildcard_wired(self) -> None:
        """A file covered by ``python3 -m unittest discover -s scripts -p 'test_*.py'``
        is wired."""
        with tempfile.TemporaryDirectory() as tmp:
            root, wf_dir = _make_tree(
                tmp,
                ["scripts/x/test_a.py"],
                [{"run": "python3 -m unittest discover -s scripts -p 'test_*.py' -v"}],
            )
            rc = cwt._run_check(root, wf_dir)
            self.assertEqual(rc, 0)

    def test_narrow_pattern_misses_sibling(self) -> None:
        """A narrow ``-p 'test_a.py'`` pattern covers only that file; a sibling
        ``test_b.py`` is reported unwired (exit 1)."""
        with tempfile.TemporaryDirectory() as tmp:
            root, wf_dir = _make_tree(
                tmp,
                ["scripts/x/test_a.py", "scripts/x/test_b.py"],
                [{"run": "python3 -m unittest discover -s scripts -p 'test_a.py' -v"}],
            )
            rc = cwt._run_check(root, wf_dir)
            self.assertEqual(rc, 1)

    def test_no_mention_unwired(self) -> None:
        """A file with no workflow mention at all is unwired (exit 1)."""
        with tempfile.TemporaryDirectory() as tmp:
            root, wf_dir = _make_tree(
                tmp,
                ["scripts/x/test_a.py"],
                [{"run": "python3 scripts/other/guard.py"}],
            )
            rc = cwt._run_check(root, wf_dir)
            self.assertEqual(rc, 1)

    def test_basename_copy_is_not_wired(self) -> None:
        """#859: a step that runs ``scripts/x/test_a.py`` must not credit a
        different file that merely shares its basename.

        ``scripts/bench/test_a.py`` is a separate test nobody invited, so the
        guard must report it unwired (exit 1).  Before #859 the guard also
        recorded the bare basename and this case passed (exit 0).
        """
        with tempfile.TemporaryDirectory() as tmp:
            root, wf_dir = _make_tree(
                tmp,
                ["scripts/x/test_a.py", "scripts/bench/test_a.py"],
                [{"run": "python3 scripts/x/test_a.py"}],
            )
            rc = cwt._run_check(root, wf_dir)
            self.assertEqual(rc, 1)

    def test_blanket_discover_is_refused(self) -> None:
        """#859: ``discover -s .`` would cover every policed file, so it is
        reported as a blanket rule and does not wire anything (exit 1).

        Before #859 a single ``-s .`` step in any workflow credited every
        ``test_*.py`` under ``scripts/`` and ``tools/``, which made the whole
        guard vacuous.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root, wf_dir = _make_tree(
                tmp,
                ["scripts/x/test_a.py", "tools/y/test_b.py"],
                [{"run": "python3 -m unittest discover -s . -p 'test_*.py' -v"}],
            )
            rc = cwt._run_check(root, wf_dir)
            self.assertEqual(rc, 1)

    def test_bounded_discover_root_still_wires(self) -> None:
        """#859 keeps the allowed case: a discover rooted at a bounded
        directory (``tools``, not the repository root) still covers its test
        files (exit 0)."""
        with tempfile.TemporaryDirectory() as tmp:
            root, wf_dir = _make_tree(
                tmp,
                ["tools/y/nested/test_b.py"],
                [{"run": "python3 -m unittest discover -s tools -p 'test_*.py' -v"}],
            )
            rc = cwt._run_check(root, wf_dir)
            self.assertEqual(rc, 0)

    def test_real_repo_passes(self) -> None:
        """Run the guard against the actual repository tree.  This is the
        regression that matters — it must exit 0 after ``test_check_baseline_control.py``
        is wired (the step added by this PR makes it pass)."""
        here = os.path.dirname(os.path.abspath(__file__))
        # Walk up to the repo root (where .github/workflows lives).
        cur = here
        while True:
            wf_dir = os.path.join(cur, ".github", "workflows")
            if os.path.isdir(wf_dir):
                rc = cwt._run_check(cur, wf_dir)
                self.assertEqual(rc, 0)
                return
            parent = os.path.dirname(cur)
            if parent == cur:
                self.fail("could not find repo root")
            cur = parent


if __name__ == "__main__":
    unittest.main()
