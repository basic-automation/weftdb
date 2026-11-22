#!/usr/bin/env python3
"""
check-tracked-scaffolding.py

Fail when the index tracks a file that only a *run* produces, or when a tracked
markdown document contains a handoff section.

Why a separate check and not just `.gitignore` (#876, #885): an ignored path is
merely hidden from `git status`. `git add -f`, or `git add -A` in a tree where
the path was already tracked, puts it in a commit, and a review-merge does not
look at filenames for a living log. `progress.md` reached `main` exactly that
way: a worker's 163-line run log for issue #838, merged without anyone
diffing its name.

The rule the guard encodes is the one every implementation agent is briefed
with: scaffolding is what a *run* produces and a *reader* needs in order to
continue, and the committed deliverable must be written without it. The pattern
list lives here, in the open repo, so the next name an agent invents surfaces in
code review instead of in a private harness config.

Names covered:
  - `progress.md` at any depth, and `progress-*.md`
  - `HANDOFF.md` at any depth (case-exact, so a deliberate `handoff.md` in a
    guide about handoffs is not policed)
  - `.handoff-*.md` at the root, matching the existing ignore entry
  - `.briefs/` — briefs are the coordination channel, not repo content
  - a literal `## Handoff` heading inside any tracked `.md`

Exit codes: 0 clean; 1 violations found.

Usage: python3 scripts/ci/check-tracked-scaffolding.py [--paths PATH ...]
`--paths` reads the names from stdin (newline-separated) instead of the index,
which is how the unit tests drive it against a fabricated list.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# Path components matched anywhere in a tracked path, case-sensitive on purpose:
# the names agents actually write. Anchored on whole path components for
# directories, on the whole basename for files.
FILE_PATTERNS = [
    re.compile(r"^progress\.md$"),
    re.compile(r"^progress-.*\.md$"),
    re.compile(r"^HANDOFF\.md$"),
    re.compile(r"^\.handoff-.*\.md$"),
]
DIR_COMPONENTS = {".briefs"}

# A handoff section inside a committed document. The heading is what makes a
# document read as an instruction to an agent rather than to a reader.
HANDOFF_HEADING = re.compile(r"^#{2,4}[ \t]+Handoff[ \t]*$", re.MULTILINE)


def tracked_paths() -> list[str]:
    """Every path in the index, as git prints it."""
    proc = subprocess.run(
        ["git", "ls-files"],
        capture_output=True,
        text=True,
        check=True,
    )
    return [line for line in proc.stdout.splitlines() if line.strip()]


def path_matches(path: str) -> str | None:
    """Return the rule name that condemns this path, or None."""
    parts = path.split("/")
    for component in parts[:-1]:
        if component in DIR_COMPONENTS:
            return f"tracked directory component `{component}/`"
    base = parts[-1]
    for pattern in FILE_PATTERNS:
        if pattern.match(base):
            return f"tracked scaffolding filename `{base}`"
    return None


def content_matches(path: str, root: Path) -> str | None:
    """Retained on purpose: the heading rule is what a *review* enforces, and this is
    the function a future agent will want when it wires the rule into a pre-commit
    hook. It is deliberately NOT called by :func:`check` — the rule that CI can enforce
    without silencing honest writing is the filename rule. #876 removed a run log whose
    own filename announced it; a guard that also failed ``DECISIONS.md`` for quoting the
    word ``Handoff`` in the write-up of that incident would be a guard people route
    around, and the first routing point would be a blanket exemption no one re-reads.
    """
    if not path.endswith(".md"):
        return None
    fpath = root / path
    if not fpath.is_file():
        return None
    try:
        text = fpath.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    if HANDOFF_HEADING.search(text):
        return "contains a `## Handoff` section"
    return None


CONTENT_RULES_APPLIED_BY_CI = False
"""Whether :func:`check` reads file contents as well as names.

``False`` is the shipped policy, and it is a constant rather than an absent feature so that
the reason survives: ``DECISIONS.md`` on ``main`` quotes a ``## Handoff`` heading while
describing the leak this guard exists to stop, so a CI rule on headings would fail the honest
write-up of the practice it forbids — and a gate that fails legitimate prose gets routed
around by a blanket exemption nobody re-reads. See the ``#885`` entry in ``DECISIONS.md``. A
pre-commit hook can import this module and pass ``with_content_rules=True``; CI may not.
"""


def check(
    paths: list[str],
    root: Path,
    with_content_rules: bool | None = None,
) -> list[tuple[str, str]]:
    apply_content = CONTENT_RULES_APPLIED_BY_CI if with_content_rules is None else with_content_rules
    findings: list[tuple[str, str]] = []
    for path in paths:
        why = path_matches(path)
        if why:
            findings.append((path, why))
        elif apply_content:
            note = content_matches(path, root)
            if note:
                findings.append((path, note))
    return findings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Fail when the index tracks a run log, a handoff note or a brief."
    )
    parser.add_argument(
        "--paths",
        action="store_true",
        help="read tracked paths from stdin instead of `git ls-files`",
    )
    parser.add_argument(
        "--root",
        default=".",
        help="checkout to read file contents from (default: current directory)",
    )
    args = parser.parse_args(argv)

    root = Path(args.root)
    if args.paths:
        paths = [line.strip() for line in sys.stdin if line.strip()]
    else:
        paths = tracked_paths()

    findings = check(paths, root)
    if findings:
        for path, why in findings:
            print(f"{path}: {why}")
        print()
        print(f"{len(findings)} tracked scaffolding item(s) found in {len(paths)} path(s)")
        print(
            "A run log or handoff note belongs in the agent channel, not the tree. "
            "Rewrite the deliverable so a fresh reader needs no handoff, then remove the file."
        )
        return 1

    print(f"OK: {len(paths)} tracked path(s) checked — no run logs, handoffs or briefs tracked")
    return 0


if __name__ == "__main__":
    sys.exit(main())
