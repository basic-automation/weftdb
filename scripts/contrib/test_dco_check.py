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

    def test_only_ascii_case_is_folded(self):
        # U+017F (long s) folds to "s" under Unicode case folding; git does not read it as a trailer.
        self.assertEqual(dco_check.signoff_emails("x\n\n\u017figned-off-by: M <m@x.example>\n"), [])
        self.assertEqual(dco_check.signoff_emails("x\n\nSigned-off-\u212ay: M <m@x.example>\n"), [])

    def test_crlf_line_endings_are_accepted(self):
        # git commit --cleanup=verbatim keeps the \r of a message written with CRLF endings.
        msg = "feat: x\r\n\r\nSigned-off-by: Ada Lovelace <ada@example.org>\r\n"
        self.assertEqual(dco_check.signoff_emails(msg), ["ada@example.org"])
        self.assertEqual(dco_check.signoff_emails("x\r\n\r\nSigned-off-by: Ada <ada@example.org>"), ["ada@example.org"])


def make_commit(
    sha: str,
    *,
    login: str | None = "eve-x",
    committer_login: str | None = "same",
    name: str = "Eve",
    email: str = "eve@example.org",
    message: str = "feat: x\n",
    verified: bool = False,
    parents: int = 1,
    author_id: int = 1,
) -> dict:
    """A commit as the pull request commits API returns it. A `...[bot]` login is a Bot account."""
    committer_login = login if committer_login == "same" else committer_login

    def account(user: str | None, uid: int) -> dict | None:
        return {"login": user, "id": uid, "type": "Bot" if user.endswith("[bot]") else "User"} if user else None

    return {
        "sha": sha * (40 // len(sha)),
        "commit": {
            "author": {"name": name, "email": email, "date": "2026-10-08T12:00:00Z"},
            "committer": {"name": name, "email": email, "date": "2026-10-08T12:00:00Z"},
            "message": message,
            "verification": {"verified": verified, "reason": "valid" if verified else "unsigned"},
        },
        "author": account(login, author_id),
        "committer": account(committer_login, author_id if committer_login == login else 2),
        "parents": [{"sha": f"{i:040d}"} for i in range(parents)],
    }


def contributor_pull(commits: list[dict], **extra) -> dict:
    return dict(load("pull-contributor.json"), commits=len(commits), **extra)


def repo(full_name: str) -> dict:
    owner = full_name.split("/")[0]
    return {"full_name": full_name, "owner": {"login": owner, "id": 3, "type": "Bot" if owner.endswith("[bot]") else "User"}}


def opened_by(login: str, commits: list[dict], head_repo: str | None = "basic-automation/weftdb") -> dict:
    """A pull request `login` opened, with its head branch in `head_repo` (None: the fork is gone)."""
    head = {"ref": "topic", "sha": commits[-1]["sha"] if commits else "", "repo": repo(head_repo) if head_repo else None}
    return dict(
        load("pull-contributor.json"),
        user={"login": login, "id": 4, "type": "Bot" if login.endswith("[bot]") else "User"},
        head=head,
        commits=len(commits),
    )


class Decision(unittest.TestCase):
    def test_signed_pull_request_passes(self):
        result = dco_check.check(load("pull-contributor.json"), load("commits-signed.json"), ALLOW)
        self.assertTrue(result.satisfied, result.problems)
        # The merge GitHub made is skipped, so three commits are checked.
        self.assertIn("all 3 commit(s)", result.reason)
        self.assertEqual(result.cla_unsafe, [])
        self.assertEqual(result.commit_count, 4)

    def test_missing_and_mismatched_signoffs_fail(self):
        result = dco_check.check(load("pull-contributor.json"), load("commits-unsigned.json"), ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["b" * 12, "c" * 12, "d" * 12])
        self.assertEqual(result.problems[0][1], "no Signed-off-by line")
        self.assertIn("someone@example.org", result.problems[1][1])
        self.assertIn("ada@example.org", result.problems[1][1])

    def test_pull_request_opened_by_the_owner_passes(self):
        result = dco_check.check(load("pull-owner.json"), load("commits-unsigned.json"), ALLOW)
        self.assertTrue(result.satisfied)
        self.assertIn("allowlisted", result.reason)

    def test_allowlist_ignores_login_case(self):
        result = dco_check.check(load("pull-owner.json"), load("commits-unsigned.json"), {"Physics515"})
        self.assertTrue(result.satisfied)

    def test_bot_pull_request_passes(self):
        # Dependabot pushes its branches to this repository.
        commits = load("commits-unsigned.json")
        self.assertTrue(dco_check.check(opened_by("dependabot[bot]", commits), commits, ALLOW).satisfied)

    def test_commits_beyond_the_api_limit_fail(self):
        commits = load("commits-signed.json")
        pull = dict(load("pull-contributor.json"), commits=300)
        result = dco_check.check(pull, commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertIn("4 of 300", result.problems[0][1])
        self.assertEqual(result.commit_count, 300)

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


SIGNED = "feat: x\n\nSigned-off-by: Eve <eve@example.org>\n"


class Merges(unittest.TestCase):
    """Only merges GitHub made pass without a sign-off; a local merge can carry changes."""

    def github_merge(self, sha="9"):
        return make_commit(sha, committer_login="web-flow", verified=True, parents=2, message="Merge branch 'main' into x\n")

    def test_merge_only_pull_request_fails(self):
        # git merge --no-ff --no-commit main, arbitrary edits, commit: the only commit is M.
        commits = [make_commit("m", parents=2, message="Merge branch 'main'\n")]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertIn("merge commit not made in GitHub's web UI", result.problems[0][1])

    def test_unsigned_local_merge_among_signed_commits_fails(self):
        commits = [make_commit("1", message=SIGNED), make_commit("m", parents=2, message="Merge branch 'main'\n")]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["m" * 12])

    def test_signed_off_local_merge_passes(self):
        merge = make_commit("m", parents=2, message="Merge branch 'main'\n\nSigned-off-by: Eve <eve@example.org>\n")
        commits = [make_commit("1", message=SIGNED), merge]
        self.assertTrue(dco_check.check(contributor_pull(commits), commits, ALLOW).satisfied)

    def test_github_update_branch_merge_needs_no_signoff(self):
        commits = [make_commit("1", message=SIGNED), self.github_merge()]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertTrue(result.satisfied, result.problems)
        self.assertIn("all 1 commit(s)", result.reason)

    def test_unverified_web_flow_merge_needs_a_signoff(self):
        merge = make_commit("9", committer_login="web-flow", verified=False, parents=2)
        commits = [make_commit("1", message=SIGNED), merge]
        self.assertFalse(dco_check.check(contributor_pull(commits), commits, ALLOW).satisfied)

    def test_pull_request_of_only_github_merges_fails(self):
        commits = [self.github_merge()]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertIn("no commit to check", result.problems[0][1])

    def test_verified_merge_made_by_github_actions_needs_a_signoff(self):
        # A workflow's GITHUB_TOKEN can create a two-parent commit with any tree through the
        # Git Data API; GitHub signs it, commits it as web-flow, and authors it as the bot.
        merge = make_commit("9", login="github-actions[bot]", committer_login="web-flow", verified=True, parents=2, author_id=41898282)
        commits = [make_commit("1", message=SIGNED), merge]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["9" * 12])
        self.assertEqual([sha for sha, _ in result.cla_unsafe], ["9" * 12])

    def test_verified_merge_made_by_another_app_needs_a_signoff(self):
        merge = make_commit("9", login="some-app[bot]", committer_login="web-flow", verified=True, parents=2)
        commits = [make_commit("1", message=SIGNED), merge]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["9" * 12])
        # The CLA bot does check some-app[bot], which has not signed, so that route stays open.
        self.assertEqual(result.cla_unsafe, [])

    def test_merge_needs_an_author_account_to_be_exempt(self):
        merge = make_commit("9", login=None, committer_login="web-flow", verified=True, parents=2)
        commits = [make_commit("1", message=SIGNED), merge]
        self.assertFalse(dco_check.check(contributor_pull(commits), commits, ALLOW).satisfied)


class AllowlistedCommits(unittest.TestCase):
    """A commit showing an allowlisted account passes only when GitHub verified that account."""

    def test_unverified_owner_commit_needs_a_signoff(self):
        # GitHub resolves author.login from the email alone, e.g. the owner's noreply address.
        commits = [make_commit("1", message=SIGNED), make_commit("o", login="physics515", email="1010+physics515@users.noreply.github.com")]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["o" * 12])

    def test_owner_commit_verified_as_the_owner_passes(self):
        commits = [make_commit("1", message=SIGNED), make_commit("o", login="physics515", verified=True)]
        self.assertTrue(dco_check.check(contributor_pull(commits), commits, ALLOW).satisfied)

    def test_verified_commit_by_someone_else_does_not_vouch_for_the_owner(self):
        # Authored as the owner, committed and signed by another account.
        commits = [make_commit("1", message=SIGNED), make_commit("o", login="physics515", committer_login="eve-x", verified=True)]
        self.assertFalse(dco_check.check(contributor_pull(commits), commits, ALLOW).satisfied)

    def test_verified_web_flow_commit_does_not_vouch_for_the_owner(self):
        commits = [make_commit("1", message=SIGNED), make_commit("o", login="physics515", committer_login="web-flow", verified=True)]
        self.assertFalse(dco_check.check(contributor_pull(commits), commits, ALLOW).satisfied)


class BotCommits(unittest.TestCase):
    """In a contributor's pull request, a commit by a bot account fails the DCO route outright.

    Anyone's workflow can make GitHub sign a commit as github-actions[bot], so "verified"
    proves nothing for a bot, and a bot cannot certify the DCO.
    """

    def bot_commit(self, sha="b", login="github-actions[bot]", **kw):
        kw.setdefault("email", "41898282+github-actions[bot]@users.noreply.github.com")
        return make_commit(sha, login=login, name=login, verified=True, author_id=41898282 if login == "github-actions[bot]" else 5, **kw)

    def test_pull_request_of_verified_github_actions_commits_fails(self):
        commits = [self.bot_commit()]
        result = dco_check.check(opened_by("mallory", commits, "mallory/weftdb"), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertIn("bot account github-actions[bot]", result.problems[0][1])
        self.assertEqual([sha for sha, _ in result.cla_unsafe], ["b" * 12])

    def test_verified_dependabot_commit_in_a_contributors_pull_request_fails(self):
        dep = self.bot_commit("d", login="dependabot[bot]", email="49699333+dependabot[bot]@users.noreply.github.com")
        commits = [make_commit("1", message=SIGNED), dep]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["d" * 12])
        self.assertEqual([sha for sha, _ in result.cla_unsafe], ["d" * 12])

    def test_a_bot_signoff_does_not_count(self):
        email = "41898282+github-actions[bot]@users.noreply.github.com"
        bot = self.bot_commit(message=f"feat: x\n\nSigned-off-by: github-actions[bot] <{email}>\n")
        commits = [make_commit("1", message=SIGNED), bot]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["b" * 12])

    def test_github_actions_is_unsafe_for_the_cla_route_even_off_the_allowlist(self):
        # CLA Assistant Lite drops account id 41898282 whatever its allowlist says.
        commits = [make_commit("1", message=SIGNED), self.bot_commit()]
        result = dco_check.check(contributor_pull(commits), commits, {"physics515"})
        self.assertEqual([sha for sha, _ in result.cla_unsafe], ["b" * 12])

    def test_bot_committed_commit_by_a_person_is_left_to_the_cla(self):
        # The CLA bot checks the author's account, so only the DCO route is closed to it.
        commit = make_commit("p", login="ada-l", committer_login="some-app[bot]", message=SIGNED)
        commits = [make_commit("1", message=SIGNED), commit]
        result = dco_check.check(contributor_pull(commits), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertIn("bot account some-app[bot]", result.problems[0][1])
        self.assertEqual(result.cla_unsafe, [])


class OpenerAllowlist(unittest.TestCase):
    """An allowlisted opener passes only when it controls the head branch."""

    def test_owner_pull_request_from_this_repository_passes(self):
        commits = [make_commit("u", login="ada-l")]
        self.assertTrue(dco_check.check(opened_by("physics515", commits), commits, ALLOW).satisfied)

    def test_owner_pull_request_from_the_owners_fork_passes(self):
        commits = [make_commit("u", login="ada-l")]
        self.assertTrue(dco_check.check(opened_by("physics515", commits, "Physics515/weftdb"), commits, ALLOW).satisfied)

    def test_owner_pull_request_from_a_contributors_fork_is_checked_commit_by_commit(self):
        # gh pr create --head mallory:branch: mallory can keep pushing to the head.
        owner = make_commit("o", login="physics515", verified=True)
        pushed = make_commit("m", login="mallory")
        commits = [owner, pushed]
        result = dco_check.check(opened_by("physics515", commits, "mallory/weftdb"), commits, ALLOW)
        self.assertFalse(result.satisfied)
        self.assertEqual([sha for sha, _ in result.problems], ["m" * 12])

    def test_signed_off_commits_in_a_contributors_fork_still_pass(self):
        commits = [make_commit("1", message=SIGNED)]
        self.assertTrue(dco_check.check(opened_by("physics515", commits, "mallory/weftdb"), commits, ALLOW).satisfied)

    def test_head_in_a_deleted_fork_is_checked_commit_by_commit(self):
        commits = [make_commit("u", login="ada-l")]
        self.assertFalse(dco_check.check(opened_by("physics515", commits, None), commits, ALLOW).satisfied)

    def test_bot_pull_request_from_a_fork_is_checked_commit_by_commit(self):
        commits = [make_commit("u", login="ada-l")]
        self.assertFalse(dco_check.check(opened_by("dependabot[bot]", commits, "mallory/weftdb"), commits, ALLOW).satisfied)


class ClaAllowlistSafety(unittest.TestCase):
    """cla_unsafe lists commits CLA Assistant Lite would drop as allowlisted on a fakeable basis."""

    def unsafe(self, *commits):
        commits = [make_commit("1", message=SIGNED), *commits]
        result = dco_check.check(contributor_pull(list(commits)), list(commits), ALLOW)
        self.assertFalse(result.satisfied)
        return [sha for sha, _ in result.cla_unsafe]

    def test_git_author_name_set_to_a_bot(self):
        # git config user.name 'dependabot[bot]', with an email linked to no account.
        fake = make_commit("f", login=None, committer_login=None, name="dependabot[bot]", email="x@unlinked.example")
        self.assertEqual(self.unsafe(fake), ["f" * 12])

    def test_git_author_name_set_to_the_owner(self):
        fake = make_commit("f", login=None, committer_login=None, name="Physics515", email="x@unlinked.example")
        self.assertEqual(self.unsafe(fake), ["f" * 12])

    def test_author_email_set_to_the_owners_address(self):
        fake = make_commit("f", login="physics515", email="1010+physics515@users.noreply.github.com")
        self.assertEqual(self.unsafe(fake), ["f" * 12])

    def test_unlinked_author_with_an_allowlisted_committer(self):
        # The action falls back to the committer's account when the author has none.
        fake = make_commit("f", login=None, committer_login="github-actions[bot]", email="x@unlinked.example")
        self.assertEqual(self.unsafe(fake), ["f" * 12])

    def test_ordinary_unsigned_commit_is_left_to_the_cla(self):
        self.assertEqual(self.unsafe(make_commit("u", login="ada-l")), [])

    def test_signed_off_commit_is_not_flagged(self):
        commits = [make_commit("u", login="ada-l"), make_commit("s", login=None, committer_login=None, name="physics515", message=SIGNED)]
        self.assertEqual(self.unsafe(*commits), [])

    def test_verified_owner_commit_is_not_flagged(self):
        commits = [make_commit("u", login="ada-l"), make_commit("o", login="physics515", verified=True)]
        self.assertEqual(self.unsafe(*commits), [])

    def test_fixture_owner_commit_is_flagged(self):
        result = dco_check.check(load("pull-contributor.json"), load("commits-unsigned.json"), ALLOW)
        self.assertEqual([sha for sha, _ in result.cla_unsafe], ["d" * 12])


class HeadBinding(unittest.TestCase):
    def test_matching_head_runs(self):
        commits = [make_commit("1", message=SIGNED)]
        pull = contributor_pull(commits, head={"sha": commits[0]["sha"]})
        self.assertTrue(dco_check.check(pull, commits, ALLOW, head_sha=commits[0]["sha"].upper()).satisfied)

    def test_moved_head_raises(self):
        commits = [make_commit("1", message=SIGNED)]
        pull = contributor_pull(commits, head={"sha": commits[0]["sha"]})
        with self.assertRaises(dco_check.StaleHead):
            dco_check.check(pull, commits, ALLOW, head_sha="2" * 40)

    def test_head_missing_from_the_commit_list_raises(self):
        # The pull request moved between reading it and reading its commits.
        commits = [make_commit("1", message=SIGNED)]
        pull = contributor_pull(commits, head={"sha": "2" * 40})
        with self.assertRaises(dco_check.StaleHead):
            dco_check.check(pull, commits, ALLOW, head_sha="2" * 40)

    def test_head_is_checked_before_the_opener_allowlist(self):
        commits = [make_commit("1")]
        pull = opened_by("physics515", commits)
        self.assertTrue(dco_check.check(pull, commits, ALLOW, head_sha=commits[0]["sha"]).satisfied)
        with self.assertRaises(dco_check.StaleHead):
            dco_check.check(pull, commits, ALLOW, head_sha="2" * 40)


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
            self.assertIn("The CLA route cannot vouch", text)

    def test_outputs_are_written_for_the_workflow(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            proc = self.offline("pull-contributor.json", "commits-unsigned.json", "--output", str(out))
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertEqual(out.read_text(encoding="utf-8"), "satisfied=false\ncla_allowlist_unsafe=true\ncommits=4\n")
            out.unlink()
            proc = self.offline("pull-contributor.json", "commits-signed.json", "--output", str(out))
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(out.read_text(encoding="utf-8"), "satisfied=true\ncla_allowlist_unsafe=false\ncommits=4\n")

    def test_exit_status_is_2_when_the_head_has_moved(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            pull = Path(tmp) / "pull.json"
            pull.write_text(json.dumps(dict(load("pull-contributor.json"), head={"sha": "1" * 40})), encoding="utf-8")
            args = ["--pull-json", str(pull), "--commits-json", str(DATA / "commits-signed.json"), "--output", str(out)]
            proc = self.run_cli(*args, "--head-sha", "2" * 40)
            self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
            self.assertIn("newer head decides", proc.stderr)
            self.assertFalse(out.exists())
            self.assertEqual(self.run_cli(*args, "--head-sha", "1" * 40).returncode, 0)


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
