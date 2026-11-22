"""Tests for check-tracked-scaffolding.py (#885).

The guard exists because `progress.md` — a worker's run log for issue #838 —
reached `main` through a merged agent branch, and nothing in CI objected: the
repo's checks are Rust and OpenAPI gates, so a markdown file with a misleading
authority passes every one of them. `.gitignore` had the right idea and the
wrong power: an ignore hides a path from `git status`, it does not refuse a
commit.

These tests drive the guard with synthetic index listings plus one run against
the real repository tree, which is the regression that matters: it asserts that
the file #876 removed is still gone.
"""

import importlib.util as _util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

_MODULE_PATH = Path(__file__).with_name("check-tracked-scaffolding.py")
_spec = _util.spec_from_file_location("check_tracked_scaffolding", str(_MODULE_PATH))
assert _spec is not None and _spec.loader is not None
cts = _util.module_from_spec(_spec)
_spec.loader.exec_module(cts)

_REPO_ROOT = Path(__file__).resolve().parents[2]


def _run_cli(paths: list[str], root: str) -> subprocess.CompletedProcess:
    """Run the guard as CI runs it, feeding paths on stdin."""
    return subprocess.run(
        [sys.executable, str(_MODULE_PATH), "--paths", "--root", root],
        input="\n".join(paths),
        capture_output=True,
        text=True,
    )


class FilenameRulesTest(unittest.TestCase):
    """What the guard condemns by name."""

    def test_progress_log_at_root_is_found(self) -> None:
        self.assertIsNotNone(cts.path_matches("progress.md"))

    def test_progress_log_nested_is_found(self) -> None:
        """A log dropped into a docs directory is the same class of file."""
        self.assertIsNotNone(cts.path_matches("docs/research/progress.md"))

    def test_suffixed_progress_log_is_found(self) -> None:
        self.assertIsNotNone(cts.path_matches("progress-fre18b.md"))

    def test_handoff_document_is_found(self) -> None:
        self.assertIsNotNone(cts.path_matches("HANDOFF.md"))

    def test_root_handoff_note_is_found(self) -> None:
        self.assertIsNotNone(cts.path_matches(".handoff-p12-213.md"))

    def test_briefs_directory_is_found(self) -> None:
        self.assertIsNotNone(cts.path_matches(".briefs/ttl-expiry-purge-a.md"))

    def test_ordinary_paths_are_clean(self) -> None:
        """A guide about handoffs, and a design note, are not scaffolding."""
        for clean in (
            "docs/guides/handoffs.md",
            "docs/guides/handoff.md",
            "weftdb/src/types/segment_store/maint.rs",
            "DECISIONS.md",
            "progress-notes/handoff.txt",  # not a .md progress log, not a tracked brief
        ):
            self.assertIsNone(cts.path_matches(clean), clean)

    def test_handoff_file_in_a_subdirectory_is_found(self) -> None:
        """The name rule matches the basename, wherever the file lands."""
        self.assertIsNotNone(cts.path_matches("weftdb/HANDOFF.md"))


class ContentRulesTest(unittest.TestCase):
    """A committed document that opens with instructions to the next agent."""

    def test_handoff_heading_is_found(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            doc = Path(tmp) / "audit.md"
            doc.write_text("# Audit\n\n## Handoff\n\nPick up from step 3.\n", encoding="utf-8")
            self.assertIsNotNone(cts.content_matches("audit.md", Path(tmp)))

    def test_handoff_word_in_prose_is_not_a_heading(self) -> None:
        """`## Handoff costs` is a heading about handoffs, not a handoff."""
        with tempfile.TemporaryDirectory() as tmp:
            doc = Path(tmp) / "guide.md"
            doc.write_text("## Handoff costs are borne by the reader\n", encoding="utf-8")
            self.assertIsNone(cts.content_matches("guide.md", Path(tmp)))

    def test_handoff_heading_with_trailing_text_is_still_a_handoff(self) -> None:
        """`## Handoff ` with trailing whitespace is the same section."""
        with tempfile.TemporaryDirectory() as tmp:
            doc = Path(tmp) / "run.md"
            doc.write_text("# Run\n\n## Handoff   \n\nrewrite if needed\n", encoding="utf-8")
            self.assertIsNotNone(cts.content_matches("run.md", Path(tmp)))

    def test_rust_file_is_not_content_checked(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "lib.rs"
            src.write_text("// ## Handoff\n", encoding="utf-8")
            self.assertIsNone(cts.content_matches("lib.rs", Path(tmp)))

    def test_tracked_but_absent_file_is_not_a_crash(self) -> None:
        self.assertIsNone(cts.content_matches("gone.md", Path(tempfile.mkdtemp())))


class CliTest(unittest.TestCase):
    """Exit codes and the message CI sees."""

    def test_clean_input_exits_zero(self) -> None:
        proc = _run_cli(["docs/x.md", "weftdb/src/lib.rs"], str(_REPO_ROOT))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("OK:", proc.stdout)

    def test_scaffolding_exits_one_and_names_the_rule(self) -> None:
        proc = _run_cli(["progress.md"], str(_REPO_ROOT))
        self.assertEqual(proc.returncode, 1)
        self.assertIn("tracked scaffolding filename `progress.md`", proc.stdout)
        self.assertIn("1 tracked scaffolding item(s) found", proc.stdout)
        self.assertIn("belongs in the agent channel", proc.stdout)

    def test_heading_rule_is_a_review_rule_not_a_ci_rule(self) -> None:
        """`check` — what CI runs — polices the two rules separately, so the content
        rule can be dropped from CI without deleting it. Making CI fail on a
        `## Handoff` heading would break `DECISIONS.md`, which records the #876 incident
        in prose: `origin/main:DECISIONS.md` quotes one while describing the leak."""
        self.assertFalse(
            cts.CONTENT_RULES_APPLIED_BY_CI,
            "the shipped policy is filenames-only; if you flip this, delete one of the two "
            "rules below so the suite still says which rule CI enforces",
        )

    def test_filename_rule_is_enforced_by_check(self) -> None:
        findings = dict(cts.check(["progress.md"], _REPO_ROOT))
        self.assertEqual(list(findings), ["progress.md"])

    def test_heading_rule_reaches_check_only_by_explicit_request(self) -> None:
        """The detector still works, and :func:`check` can be asked to use it — that is
        how a pre-commit hook would wire it, and it is not how CI runs it."""
        with tempfile.TemporaryDirectory() as tmp:
            doc = Path(tmp) / "run_log.md"
            doc.write_text("## Handoff\n\ntodo\n", encoding="utf-8")
            self.assertEqual(cts.check(["run_log.md"], Path(tmp)), [])
            self.assertIsNotNone(cts.content_matches("run_log.md", Path(tmp)))

    def test_every_named_class_is_reported_in_one_run(self) -> None:
        proc = _run_cli(
            [
                "progress.md",
                "docs/research/progress.md",
                "HANDOFF.md",
                ".handoff-p12-213.md",
                ".briefs/slice.md",
                "progress-cut2.md",
            ],
            str(_REPO_ROOT),
        )
        self.assertEqual(proc.returncode, 1)
        self.assertIn("6 tracked scaffolding item(s) found", proc.stdout)


class RealRepoTest(unittest.TestCase):
    """The regression that makes the guard worth having."""

    def test_repository_index_is_free_of_scaffolding(self) -> None:
        """No run log, handoff note or brief is tracked — #876 removed the last one."""
        paths = cts.tracked_paths()
        self.assertGreater(len(paths), 100)
        named = [p for p in paths if cts.path_matches(p)]
        self.assertEqual(named, [], f"tracked scaffolding in the index: {named}")

    def test_progress_md_is_not_tracked(self) -> None:
        self.assertNotIn("progress.md", cts.tracked_paths())

    def test_briefs_are_not_tracked(self) -> None:
        self.assertFalse(
            any(p.startswith(".briefs/") for p in cts.tracked_paths()),
            ".briefs/ is the coordination channel, not repo content",
        )

    def test_quoted_incident_narration_is_not_a_violation(self) -> None:
        """#876 removed `progress.md` and `DECISIONS.md` records how it got there, naming the
        section it contained. The guard polices files, not the write-up of the incident that
        motivated it — which is why the rule is a filename rule."""
        findings = cts.check(["DECISIONS.md"], _REPO_ROOT)
        self.assertEqual(findings, [])


if __name__ == "__main__":
    unittest.main()
