#!/usr/bin/env python3
"""DCO sign-off check for a WeftDB pull request.

The DCO route of the contribution terms (CONTRIBUTING.md, "Contribution terms") holds when
every commit in the pull request carries a line

    Signed-off-by: Name <email>

whose email matches the commit's author email, compared case-insensitively. That is the
line `git commit -s` writes. The key is matched case-insensitively and the line may sit
anywhere in the message, as the DCO GitHub App accepts it.

Three things pass without a sign-off, and nothing else does:

- A pull request opened by an allowlisted account (the owner and two bots) passes outright.
  GitHub sets the opener from the authenticated account, so this cannot be faked.
- A merge commit GitHub made itself, such as one from the "Update branch" button: committed
  by `web-flow` and signature-verified. Every other merge commit needs a sign-off
  (`git merge --signoff`), because a merge made locally can carry changes of its own. A
  merge made in GitHub's web conflict editor is exempt too, though it can carry the
  resolver's edits; CONTRIBUTING.md asks maintainers to review those.
- A commit authored and committed by the same allowlisted account and signature-verified as
  that account. GitHub links a commit to an account by its email alone, so an unverified
  commit that shows an allowlisted author proves nothing about who wrote it.

A pull request with no commit left to check after that (only GitHub-made merges) does not
pass on this route.

The checker also prepares the CLA route, which CLA Assistant Lite decides when this route
fails. That action drops a commit from its check when the commit's name matches the
allowlist, and it takes the name from the author's GitHub login, else the committer's, else
the git author name, all of which the committer chooses. The checker therefore reports
`cla_allowlist_unsafe` when a commit that lacks its author's sign-off would be dropped that
way without being verified as above, and `commits`, the pull request's commit count,
because the action reads only the first 100 commits. The `contribution-terms` job refuses
the CLA route in either case.

The pull request is read through the GitHub REST API, or, for the tests, from JSON files
of the same shape. Nothing from the pull request is checked out or run, and the only
pull request text printed is the commit SHA, the logins and the email addresses, each
reduced to a safe character set so that it cannot form a workflow command.

With --head-sha, the verdict is tied to that head: if the pull request's head has moved
since the run started, the check does not run (exit 2), and the run for the newer head
decides.

Exit status: 0 the DCO route holds, 1 it does not, 2 the check could not run.

Usage:
    dco_check.py --repo OWNER/NAME --pr NUMBER [--head-sha SHA] [--allow LOGIN,...] [--output FILE]
    dco_check.py --pull-json FILE --commits-json FILE [--head-sha SHA] [--allow LOGIN,...] [--output FILE]
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from dataclasses import dataclass, field
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

# The REST API lists at most 250 commits for a pull request.
API_COMMIT_LIMIT = 250
PER_PAGE = 100

# CLA Assistant Lite (v2.6.1) reads `commits(first: 100)` and never pages.
CLA_COMMIT_LIMIT = 100

# The account GitHub commits as when it makes a commit itself (web UI, "Update branch").
GITHUB_COMMITTER = "web-flow"

SIGNOFF_RE = re.compile(
    r"^signed-off-by:[ \t]*(?P<name>[^<>\r\n]*?)[ \t]*<(?P<email>[^<>\s]+)>[ \t]*$",
    re.IGNORECASE | re.MULTILINE,
)

_UNSAFE = re.compile(r"[^A-Za-z0-9._@+\-\[\] ]")


class StaleHead(Exception):
    """The pull request's head is no longer the one this run was started for."""


def safe(text: object) -> str:
    """Reduce untrusted text to characters that cannot form a workflow command or markup."""
    return _UNSAFE.sub("?", str(text))[:200]


def signoff_emails(message: str) -> list[str]:
    """The lower-cased emails of the message's `Signed-off-by: Name <email>` lines."""
    return [m.group("email").lower() for m in SIGNOFF_RE.finditer(message) if m.group("name").strip()]


def _login(account: object) -> str:
    """The lower-cased login of a REST user object, or "" for null."""
    return (account.get("login") or "").lower() if isinstance(account, dict) else ""


def _verified(c: dict) -> bool:
    return ((c.get("commit") or {}).get("verification") or {}).get("verified") is True


def is_github_merge(c: dict) -> bool:
    """A merge commit GitHub made itself: committed by web-flow, signature verified."""
    return len(c.get("parents") or []) > 1 and _login(c.get("committer")) == GITHUB_COMMITTER and _verified(c)


def vouched(c: dict, allow: set[str]) -> bool:
    """Authored and committed by one allowlisted account, and verified as that account.

    GitHub verifies a signature against the committer: the key must belong to the account
    that owns the committer email. So a verified commit whose committer is an allowlisted
    account was made by that account, and requiring the same author keeps a verified
    committer from vouching for someone else's authorship.
    """
    author, committer = _login(c.get("author")), _login(c.get("committer"))
    return bool(author) and author == committer and author in allow and _verified(c)


def cla_name(c: dict) -> str:
    """The name CLA Assistant Lite gives a commit and matches against its allowlist.

    Its graphql.ts takes `author.user || committer.user || author`, then `login || name`.
    """
    author, committer = _login(c.get("author")), _login(c.get("committer"))
    if author:
        return author
    if committer:
        return committer
    return (((c.get("commit") or {}).get("author") or {}).get("name") or "").lower()


def signed_off(c: dict) -> tuple[bool, str]:
    """Whether the commit carries its author's sign-off, and what is wrong if not."""
    info = c.get("commit") or {}
    author_email = ((info.get("author") or {}).get("email") or "").lower()
    emails = signoff_emails(info.get("message") or "")
    if not emails:
        return False, "no Signed-off-by line"
    if not author_email or author_email not in emails:
        shown = ", ".join(safe(e) for e in emails)
        return False, f"Signed-off-by is for {shown}, but the author email is {safe(author_email) or '(none)'}"
    return True, ""


@dataclass
class Result:
    satisfied: bool
    reason: str
    problems: list[tuple[str, str]] = field(default_factory=list)
    # Commits the CLA action would drop as allowlisted on a basis anyone can fake.
    cla_unsafe: list[tuple[str, str]] = field(default_factory=list)
    commit_count: int = 0


def check(pull: dict, commits: list[dict], allow: set[str], head_sha: str | None = None) -> Result:
    """Decide whether the DCO route holds for `pull` with the listed `commits`.

    Raises StaleHead when `head_sha` is given and the pull request has moved past it.
    """
    allow = {login.lower() for login in allow}
    total = pull.get("commits")
    complete = not isinstance(total, int) or len(commits) >= total
    count = total if isinstance(total, int) else len(commits)

    if head_sha:
        current = ((pull.get("head") or {}).get("sha") or "").lower()
        if current != head_sha.lower():
            raise StaleHead(f"the pull request's head is now {safe(current)[:12] or '(unknown)'}, not {safe(head_sha)[:12]}")
        if complete and commits and head_sha.lower() not in {(c.get("sha") or "").lower() for c in commits}:
            raise StaleHead(f"the commit list does not contain the head {safe(head_sha)[:12]}; it moved while being read")

    opener = _login(pull.get("user"))
    if opener and opener in allow:
        return Result(True, f"opened by allowlisted account {safe(opener)}", commit_count=count)

    problems: list[tuple[str, str]] = []
    cla_unsafe: list[tuple[str, str]] = []
    if not complete:
        problems.append(("-", f"the API listed {len(commits)} of {total} commits, so the rest cannot be checked; split the pull request"))

    checked = 0
    for c in commits:
        sha = safe(c.get("sha", ""))[:12]
        if is_github_merge(c):
            continue
        checked += 1
        if vouched(c, allow):
            continue
        ok, problem = signed_off(c)
        if ok:
            continue
        if len(c.get("parents") or []) > 1:
            problem = f"merge commit not made by GitHub: {problem}"
        problems.append((sha, problem))
        name = cla_name(c)
        if name and name in allow:
            cla_unsafe.append((sha, f"shows allowlisted {safe(name)} but is not signature-verified as that account"))

    if checked == 0:
        problems.append(("-", "no commit to check besides merges GitHub made"))
    if problems:
        return Result(False, f"{len(problems)} problem(s) in {checked} commit(s)", problems, cla_unsafe, count)
    return Result(True, f"all {checked} commit(s) signed off by their authors", commit_count=count)


def _get_json(url: str, token: str | None):
    req = Request(url)
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("X-GitHub-Api-Version", "2022-11-28")
    req.add_header("User-Agent", "weftdb-dco-check")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urlopen(req, timeout=30) as resp:
        return json.load(resp)


def fetch(api: str, repo: str, number: int, token: str | None) -> tuple[dict, list[dict]]:
    """Read the pull request and its commits (up to the API's limit) from the REST API."""
    base = f"{api.rstrip('/')}/repos/{repo}/pulls/{number}"
    pull = _get_json(base, token)
    commits: list[dict] = []
    page = 1
    while len(commits) < API_COMMIT_LIMIT:
        batch = _get_json(f"{base}/commits?per_page={PER_PAGE}&page={page}", token)
        commits.extend(batch)
        if len(batch) < PER_PAGE:
            break
        page += 1
    return pull, commits


def write_outputs(path: str, result: Result) -> None:
    """Append the job outputs. Every value is a fixed word or an integer."""
    lines = [
        f"satisfied={'true' if result.satisfied else 'false'}",
        f"cla_allowlist_unsafe={'true' if result.cla_unsafe else 'false'}",
        f"commits={int(result.commit_count)}",
    ]
    with open(path, "a", encoding="utf-8") as fh:
        fh.write("\n".join(lines) + "\n")


def write_summary(path: str, result: Result) -> None:
    lines = ["### DCO sign-off", ""]
    if result.satisfied:
        lines.append(f"Passed: {result.reason}.")
    else:
        lines.append(f"Not satisfied: {result.reason}. The CLA route decides instead.")
        lines += ["", "| Commit | Problem |", "|---|---|"]
        lines += [f"| `{sha}` | {problem} |" for sha, problem in result.problems]
        lines += ["", "To fix it, sign off every commit (`git rebase --signoff <base>`, then force-push), or sign the CLA. See CONTRIBUTING.md, \"Contribution terms\"."]
        if result.cla_unsafe:
            lines += ["", "The CLA route cannot vouch for these commits, because the allowlisted account they show is not verified; sign them off:", ""]
            lines += [f"- `{sha}`: {problem}" for sha, problem in result.cla_unsafe]
        if result.commit_count > CLA_COMMIT_LIMIT:
            lines += ["", f"The CLA route reads only the first {CLA_COMMIT_LIMIT} commits, and this pull request has {result.commit_count}; sign off every commit or split it."]
    with open(path, "a", encoding="utf-8") as fh:
        fh.write("\n".join(lines) + "\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--repo", help="OWNER/NAME (API mode)")
    parser.add_argument("--pr", type=int, help="pull request number (API mode)")
    parser.add_argument("--pull-json", help="pull request JSON file (offline mode)")
    parser.add_argument("--commits-json", help="commit list JSON file (offline mode)")
    parser.add_argument("--head-sha", default="", help="the head commit this run is for; exit 2 if the pull request has moved")
    parser.add_argument("--allow", default="", help="comma-separated allowlisted GitHub logins")
    parser.add_argument("--output", help="append satisfied=, cla_allowlist_unsafe= and commits= to this file (GITHUB_OUTPUT)")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY"), help="append a Markdown summary to this file")
    args = parser.parse_args(argv)
    allow = {a.strip() for a in args.allow.split(",") if a.strip()}

    try:
        if args.pull_json or args.commits_json:
            if not (args.pull_json and args.commits_json):
                parser.error("--pull-json and --commits-json go together")
            with open(args.pull_json, encoding="utf-8") as fh:
                pull = json.load(fh)
            with open(args.commits_json, encoding="utf-8") as fh:
                commits = json.load(fh)
        else:
            if not (args.repo and args.pr):
                parser.error("give --repo and --pr, or --pull-json and --commits-json")
            api = os.environ.get("GITHUB_API_URL", "https://api.github.com")
            pull, commits = fetch(api, args.repo, args.pr, os.environ.get("GITHUB_TOKEN"))
    except (HTTPError, URLError, OSError, ValueError) as err:
        print(f"dco-check: could not read the pull request: {safe(err)}", file=sys.stderr)
        return 2

    try:
        result = check(pull, commits, allow, args.head_sha.strip() or None)
    except StaleHead as err:
        print(f"dco-check: not run: {err}. The run for the newer head decides.", file=sys.stderr)
        return 2
    print(f"dco-check: {'satisfied' if result.satisfied else 'not satisfied'}: {result.reason}")
    for sha, problem in result.problems:
        print(f"  - commit {sha}: {problem}")
    for sha, problem in result.cla_unsafe:
        print(f"  - CLA route cannot vouch for commit {sha}: {problem}")
    if args.output:
        write_outputs(args.output, result)
    if args.summary:
        write_summary(args.summary, result)
    return 0 if result.satisfied else 1


if __name__ == "__main__":
    sys.exit(main())
