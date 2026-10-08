#!/usr/bin/env python3
"""Tests for dco_check.py against stubbed GitHub API responses (testdata/).

Run from the repository root:

    python3 -m unittest discover -s scripts/contrib -p 'test_*.py' -v
"""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import dco_check  # noqa: E402

DATA = HERE / "testdata"
ALLOW = {"physics515", "dependabot[bot]", "github-actions[bot]"}


def load(name: str):
    with open(DATA / name, encoding="utf-8") as fh:
        return json.load(fh)


class SignoffParsing(unittest.TestCase):
    def test_reads_the_line_git_commit_s_writes(self):
        msg = "feat: x\n\nSigned-off-by: Ada Lovelace <ada@example.org>\n"
        self.assertEqual(dco_check.signoff_emails(msg), ["ada@example.org"])

    def test_key_and_email_case_are_ignored(self):
        msg = "feat: x\n\nsigned-off-BY: Ada <Ada@Example.ORG>"
        self.assertEqual(dco_check.signoff_emails(msg), ["ada@example.org"])

    def test_every_signoff_is_returned(self):
        msg = "x\n\nSigned-off-by: A <a@example.org>\nSigned-off-by: B <b@example.org>\n"
        self.assertEqual(dco_check.signoff_emails(msg), ["a@example.org", "b@example.org"])

    def test_malformed_lines_do_not_count(self):
        for line in [
            "Signed-off-by: <ada@example.org>",  # no name
            "Signed-off-by: Ada ada@example.org",  # no angle brackets
            "  Signed-off-by: Ada <ada@example.org>",  # not at the start of a line
            "Signed-off-by: Ada <ada@example.org> and more",  # trailing text
            "Reviewed-by: Ada <ada@example.org>",
        ]:
            with self.subTest(line=line):
                self.assertEqual(dco_check.signoff_emails(f"x\n\n{line}\n"), [])


class Decision(unittest.TestCase):
    def test_signed_pull_request_passes(self):
        result = dco_check.check(load("pull-contributor.json"), load("commits-signed.json"), ALLOW)
        self.assertTrue(result.satisfied, result.problems)
        # The merge commit is skipped, so three commits are checked.
        self.assertIn("all 3 non-merge commit(s)", result.reason)

    def test_missing_and_mismatched_signoffs_fail(self):
        result = dco_check.check(load("pull-contributor.json"), load("commits-unsigned.json"), ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["b" * 12, "c" * 12])
        self.assertEqual(result.problems[0][1], "no Signed-off-by line")
        self.assertIn("someone@example.org", result.problems[1][1])
        self.assertIn("ada@example.org", result.problems[1][1])

    def test_allowlisted_commit_author_needs_no_signoff(self):
        commits = [c for c in load("commits-unsigned.json") if c["sha"].startswith("a") or c["sha"].startswith("d")]
        pull = dict(load("pull-contributor.json"), commits=len(commits))
        self.assertTrue(dco_check.check(pull, commits, ALLOW).satisfied)
        self.assertFalse(dco_check.check(pull, commits, set()).satisfied)

    def test_pull_request_opened_by_the_owner_passes(self):
        result = dco_check.check(load("pull-owner.json"), load("commits-unsigned.json"), ALLOW)
        self.assertTrue(result.satisfied)
        self.assertIn("allowlisted", result.reason)

    def test_allowlist_ignores_login_case(self):
        result = dco_check.check(load("pull-owner.json"), load("commits-unsigned.json"), {"Physics515"})
        self.assertTrue(result.satisfied)

    def test_bot_pull_request_passes(self):
        pull = dict(load("pull-contributor.json"), user={"login": "dependabot[bot]", "type": "Bot"})
        self.assertTrue(dco_check.check(pull, load("commits-unsigned.json"), ALLOW).satisfied)

    def test_commits_beyond_the_api_limit_fail(self):
        commits = load("commits-signed.json")
        pull = dict(load("pull-contributor.json"), commits=300)
        result = dco_check.check(pull, commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertIn("4 of 300", result.problems[0][1])

    def test_untrusted_text_cannot_form_a_workflow_command(self):
        for text in ["::set-output name=satisfied::true", "x ##[add-mask]y", "a|b`c<d>"]:
            with self.subTest(text=text):
                cleaned = dco_check.safe(text)
                self.assertNotIn("::", cleaned)
                self.assertNotIn("##[", cleaned)
                self.assertFalse(set(cleaned) & set("|`<>"))

    def test_hostile_signoff_email_is_reported_safely(self):
        commits = load("commits-signed.json")[:1]
        commits[0]["commit"]["message"] = "x\n\nSigned-off-by: Eve <::warning::pwned>\n"
        pull = dict(load("pull-contributor.json"), commits=1)
        result = dco_check.check(pull, commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertNotIn("::", result.problems[0][1])


class CommandLine(unittest.TestCase):
    def run_cli(self, *args):
        env = {k: v for k, v in os.environ.items() if k != "GITHUB_STEP_SUMMARY"}
        return subprocess.run(
            [sys.executable, str(HERE / "dco_check.py"), *args],
            capture_output=True,
            text=True,
            env=env,
            check=False,
        )

    def offline(self, pull, commits, *extra):
        return self.run_cli("--pull-json", str(DATA / pull), "--commits-json", str(DATA / commits), "--allow", ",".join(sorted(ALLOW)), *extra)

    def test_exit_status_is_0_when_signed(self):
        proc = self.offline("pull-contributor.json", "commits-signed.json")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("satisfied", proc.stdout)

    def test_exit_status_is_1_when_not_signed(self):
        proc = self.offline("pull-contributor.json", "commits-unsigned.json")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("not satisfied", proc.stdout)
        self.assertIn("commit cccccccccccc", proc.stdout)

    def test_exit_status_is_2_when_the_input_cannot_be_read(self):
        proc = self.run_cli("--pull-json", str(DATA / "missing.json"), "--commits-json", str(DATA / "commits-signed.json"))
        self.assertEqual(proc.returncode, 2)

    def test_summary_lists_the_failing_commits(self):
        with tempfile.TemporaryDirectory() as tmp:
            summary = Path(tmp) / "summary.md"
            proc = self.offline("pull-contributor.json", "commits-unsigned.json", "--summary", str(summary))
            self.assertEqual(proc.returncode, 1)
            text = summary.read_text(encoding="utf-8")
            self.assertIn("| `bbbbbbbbbbbb` | no Signed-off-by line |", text)
            self.assertIn("git rebase --signoff", text)


class FakeResponse(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


class ApiFetch(unittest.TestCase):
    def test_reads_the_pull_request_and_every_page_of_commits(self):
        pull = load("pull-contributor.json")
        commits = load("commits-signed.json")
        pages = {1: commits[:2], 2: commits[2:4], 3: []}
        seen = []

        def fake_urlopen(req, timeout):
            seen.append((req.full_url, req.get_header("Authorization")))
            if req.full_url.endswith("/pulls/42"):
                body = pull
            else:
                page = int(req.full_url.rsplit("page=", 1)[1])
                body = pages[page]
            return FakeResponse(json.dumps(body).encode())

        with mock.patch.object(dco_check, "urlopen", fake_urlopen), mock.patch.object(dco_check, "PER_PAGE", 2):
            got_pull, got_commits = dco_check.fetch("https://api.example/", "owner/repo", 42, "t0ken")

        self.assertEqual(got_pull, pull)
        self.assertEqual(got_commits, commits)
        self.assertEqual(
            [url for url, _ in seen],
            [
                "https://api.example/repos/owner/repo/pulls/42",
                "https://api.example/repos/owner/repo/pulls/42/commits?per_page=2&page=1",
                "https://api.example/repos/owner/repo/pulls/42/commits?per_page=2&page=2",
                "https://api.example/repos/owner/repo/pulls/42/commits?per_page=2&page=3",
            ],
        )
        self.assertTrue(all(auth == "Bearer t0ken" for _, auth in seen))

    def test_stops_at_the_api_limit(self):
        calls = []

        def fake_urlopen(req, timeout):
            calls.append(req.full_url)
            if req.full_url.endswith("/pulls/7"):
                return FakeResponse(json.dumps({"user": {"login": "x"}, "commits": 999}).encode())
            return FakeResponse(json.dumps([{"sha": "f" * 40}] * dco_check.PER_PAGE).encode())

        with mock.patch.object(dco_check, "urlopen", fake_urlopen):
            _, commits = dco_check.fetch("https://api.example", "o/r", 7, None)
        self.assertGreaterEqual(len(commits), dco_check.API_COMMIT_LIMIT)
        self.assertEqual(len(calls), 1 + 3)


if __name__ == "__main__":
    unittest.main()
