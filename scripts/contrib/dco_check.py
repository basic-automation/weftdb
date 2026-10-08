#!/usr/bin/env python3
"""DCO sign-off check for a WeftDB pull request.

The DCO route of the contribution terms (CONTRIBUTING.md, "Contribution terms") holds when
every commit in the pull request carries a line

    Signed-off-by: Name <email>

whose email matches the commit's author email, compared case-insensitively. That is the
line `git commit -s` writes. The key is matched case-insensitively and the line may sit
anywhere in the message, as the DCO GitHub App accepts it.

Two kinds of commit or pull request pass without a sign-off:

- merge commits (more than one parent), which the DCO App also skips;
- allowlisted accounts: a pull request opened by one passes outright, and a commit whose
  GitHub author is one needs no sign-off. The allowlist is the owner and two bots, so the
  owner's own pull requests and automated ones are not held up.

The pull request is read through the GitHub REST API, or, for the tests, from JSON files
of the same shape. Nothing from the pull request is checked out or run, and the only
pull request text printed is the commit SHA, the logins and the email addresses, each
reduced to a safe character set so that it cannot form a workflow command.

Exit status: 0 the DCO route holds, 1 it does not, 2 the check could not run.

Usage:
    dco_check.py --repo OWNER/NAME --pr NUMBER [--allow LOGIN,...]
    dco_check.py --pull-json FILE --commits-json FILE [--allow LOGIN,...]
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

SIGNOFF_RE = re.compile(
    r"^signed-off-by:[ \t]*(?P<name>[^<>\r\n]*?)[ \t]*<(?P<email>[^<>\s]+)>[ \t]*$",
    re.IGNORECASE | re.MULTILINE,
)

_UNSAFE = re.compile(r"[^A-Za-z0-9._@+\-\[\] ]")


def safe(text: object) -> str:
    """Reduce untrusted text to characters that cannot form a workflow command or markup."""
    return _UNSAFE.sub("?", str(text))[:200]


def signoff_emails(message: str) -> list[str]:
    """The lower-cased emails of the message's `Signed-off-by: Name <email>` lines."""
    return [m.group("email").lower() for m in SIGNOFF_RE.finditer(message) if m.group("name").strip()]


@dataclass
class Result:
    satisfied: bool
    reason: str
    problems: list[tuple[str, str]] = field(default_factory=list)


def check(pull: dict, commits: list[dict], allow: set[str]) -> Result:
    """Decide whether the DCO route holds for `pull` with the listed `commits`."""
    allow = {login.lower() for login in allow}
    opener = ((pull.get("user") or {}).get("login") or "").lower()
    if opener and opener in allow:
        return Result(True, f"opened by allowlisted account {safe(opener)}")

    problems: list[tuple[str, str]] = []
    total = pull.get("commits")
    if isinstance(total, int) and len(commits) < total:
        problems.append(("-", f"the API listed {len(commits)} of {total} commits, so the rest cannot be checked; split the pull request"))

    checked = 0
    for c in commits:
        sha = safe(c.get("sha", ""))[:12]
        if len(c.get("parents") or []) > 1:
            continue
        checked += 1
        login = ((c.get("author") or {}).get("login") or "").lower()
        if login and login in allow:
            continue
        info = c.get("commit") or {}
        author_email = ((info.get("author") or {}).get("email") or "").lower()
        emails = signoff_emails(info.get("message") or "")
        if not emails:
            problems.append((sha, "no Signed-off-by line"))
        elif not author_email or author_email not in emails:
            shown = ", ".join(safe(e) for e in emails)
            problems.append((sha, f"Signed-off-by is for {shown}, but the author email is {safe(author_email) or '(none)'}"))

    if problems:
        return Result(False, f"{len(problems)} problem(s) in {checked} commit(s)", problems)
    return Result(True, f"all {checked} non-merge commit(s) signed off by their authors")


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


def write_summary(path: str, result: Result) -> None:
    lines = ["### DCO sign-off", ""]
    if result.satisfied:
        lines.append(f"Passed: {result.reason}.")
    else:
        lines.append(f"Not satisfied: {result.reason}. The CLA route decides instead.")
        lines += ["", "| Commit | Problem |", "|---|---|"]
        lines += [f"| `{sha}` | {problem} |" for sha, problem in result.problems]
        lines += ["", "To fix it, sign off every commit (`git rebase --signoff <base>`, then force-push), or sign the CLA. See CONTRIBUTING.md, \"Contribution terms\"."]
    with open(path, "a", encoding="utf-8") as fh:
        fh.write("\n".join(lines) + "\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--repo", help="OWNER/NAME (API mode)")
    parser.add_argument("--pr", type=int, help="pull request number (API mode)")
    parser.add_argument("--pull-json", help="pull request JSON file (offline mode)")
    parser.add_argument("--commits-json", help="commit list JSON file (offline mode)")
    parser.add_argument("--allow", default="", help="comma-separated allowlisted GitHub logins")
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

    result = check(pull, commits, allow)
    print(f"dco-check: {'satisfied' if result.satisfied else 'not satisfied'}: {result.reason}")
    for sha, problem in result.problems:
        print(f"  - commit {sha}: {problem}")
    if args.summary:
        write_summary(args.summary, result)
    return 0 if result.satisfied else 1


if __name__ == "__main__":
    sys.exit(main())
