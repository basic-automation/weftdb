#!/usr/bin/env python3
"""
check-action-pinning.py

Scan every .yml / .yaml workflow in .github/workflows/ and verify that
every ``uses:`` reference is pinned to a 40-character commit SHA.

Exits non-zero when an unpinned reference is found, printing the file,
line number and offending value.

Usage:
    python3 scripts/ci/check-action-pinning.py
    python3 scripts/ci/check-action-pinning.py --help
"""

import argparse
import glob
import os
import re
import sys

_USES_RE = re.compile(r'^\s*(?:-\s+)?uses\s*:\s*(\S+)')


def _check_lines(fh, label: str, errors: list) -> None:
    """Scan an open file handle for unpinned uses: references."""
    for lineno, line in enumerate(fh, 1):
        m = _USES_RE.search(line)
        if not m:
            continue
        ref = m.group(1)

        # Skip local action paths
        if ref.startswith("./"):
            continue

        # Skip docker:// references
        if ref.startswith("docker://"):
            continue

        # Must match owner/repo@40hex
        at_idx = ref.find("@")
        if at_idx == -1:
            errors.append((label, lineno, ref))
            continue

        sha_part = ref[at_idx + 1:]

        if re.fullmatch(r'[0-9a-f]{40}', sha_part):
            continue

        errors.append((label, lineno, ref))


def check_file(path: str) -> list[tuple[str, int, str]]:
    """Check one workflow file. Returns list of (file, line_num, ref) violations."""
    errors: list[tuple[str, int, str]] = []
    label = path

    try:
        with open(path, encoding="utf-8") as fh:
            _check_lines(fh, label, errors)
    except OSError as exc:
        print(f"{label}: {exc}", file=sys.stderr)

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check that every GitHub Action uses: is pinned to a commit SHA."
    )
    parser.parse_args()

    workflow_dir = os.path.join(
        os.path.dirname(__file__) or os.getcwd(),
        "..", "..", ".github", "workflows",
    )
    workflow_dir = os.path.abspath(workflow_dir)

    if not os.path.isdir(workflow_dir):
        print(f"error: {workflow_dir}: no such directory", file=sys.stderr)
        return 1

    files = sorted(glob.glob(os.path.join(workflow_dir, "*.yml")) +
                   glob.glob(os.path.join(workflow_dir, "*.yaml")))

    if not files:
        print(f"error: no workflow files found in {workflow_dir}", file=sys.stderr)
        return 1

    all_errors: list[tuple[str, int, str]] = []
    count = 0

    for fpath in files:
        errs = check_file(fpath)
        all_errors.extend(errs)
        with open(fpath, encoding="utf-8") as fh:
            for line in fh:
                if _USES_RE.search(line):
                    count += 1

    if all_errors:
        for fname, lineno, ref in all_errors:
            print(f"{fname}:{lineno}: unpinned ref: {ref}")
        print(f"\n{len(all_errors)} unpinned reference(s) found in {count} total uses: line(s)")
        return 1

    print(f"OK: {count} uses: reference(s) checked \u2014 all pinned to a commit SHA")
    return 0


if __name__ == "__main__":
    sys.exit(main())
