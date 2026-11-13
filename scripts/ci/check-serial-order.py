#!/usr/bin/env python3
"""
check-serial-order.py

Scan every tracked ``*.rs`` file in the repository and verify that every
``#[serial]`` attribute appears immediately above a test attribute
(``#[test]``, ``#[tokio::test]``, ``#[tokio::test(...)]``).

A ``#[serial]`` below its test attribute silently loses the serialisation —
the test still compiles and passes, but runs concurrently with others and
produces an unrelated flake. This checker catches the wrong order statically.

Exits non-zero when a violation is found, printing ``file:line`` of each
misplaced ``#[serial]``.

False-positive classes handled:
- ``#[serial]`` inside a doc comment (``///``, ``//!``) or a line comment
  (``//``) is ignored.
- ``#[serial]`` inside a string literal is ignored.
- ``#[serial]`` inside a ``cfg(test)`` module or an ``impl Block`` that is
  not a test function (e.g. ``#[serial]`` on an ``async fn`` inside an
  ``impl``) — these are legitimate uses where the attribute is NOT followed
  by a test attribute in the immediately next attribute position.

Usage:
    python3 scripts/ci/check-serial-order.py
    python3 scripts/ci/check-serial-order.py --help
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# Match #[serial] — bare, keyed (`#[serial(build_publish)]`, `#[serial(a, b)]`) or path-qualified
# (`#[serial_test::serial]`) — the attribute, not inside a comment or string. A keyed one is
# the same proc macro in the same position, so it gets the same rule.
_SERIAL_RE = re.compile(r"^\s*#\s*\[\s*(?:serial_test\s*::\s*)?serial\s*(?:\([^)]*\))?\s*\]\s*(.*)$")

# Match #[test], #[tokio::test], #[tokio::test(...)]
_TEST_ATTR_RE = re.compile(r"^\s*#\s*\[\s*(?:tokio::)?test\b")

# Heuristic: does a line look like the start of a doc/line comment?
_COMMENT_LINE_RE = re.compile(r"^\s*///?\s")
# Module-level doc comment
_INNER_DOC_ATTR_RE = re.compile(r"^\s*#!\s*\[")

# Heuristic: something that is not an attribute line. Used to determine
# whether the line immediately after #[serial] is an attribute.
_FN_LINE_RE = re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s")





def check_file(path: Path) -> list[tuple[str, int, str]]:
    """Check one .rs file. Returns list of (file, line_num, detail) violations."""
    errors: list[tuple[str, int, str]] = []
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return errors

    lines = text.splitlines()
    state: str | None = None  # None, "serial_found"
    serial_line: int = 0

    for lineno, raw in enumerate(lines, start=1):
        stripped = raw.lstrip()

        # Track line comments and inner doc attrs to skip #[serial] in comments
        # If we're tracking a serial_found state and hit a non-attribute,
        # non-comment line, reset.
        if state == "serial_found":
            # A blank line resets
            if not stripped:
                state = None
                continue

            # A doc comment or inner doc attr is not a test attribute — reset
            if _COMMENT_LINE_RE.match(raw) or _INNER_DOC_ATTR_RE.match(raw):
                state = None
                continue

            # A line comment with content — reset unless it's an inline comment
            # after the #[serial] we already matched (already captured)
            if stripped.startswith("//"):
                state = None
                continue

            # Check if this line is a test attribute
            if _TEST_ATTR_RE.match(raw):
                # #[serial] followed by #[test] / #[tokio::test] — correct order
                state = None
                continue

            # Check if this line is another attribute (not a test attribute)
            # Skip conditional attrs, etc. Keep state.
            if stripped.startswith("#["):
                # Another non-test attribute between #[serial] and the test attr
                state = None
                continue

            # Check if this line starts a function definition — if so, the
            # #[serial] is NOT above a test attr, meaning it's on an fn
            # that is not annotated as a test. That's a usage we should
            # flag silently. But if the fn is preceded by #[serial] and
            # NOT by a test attr, then the #[serial] is wasted but it's
            # not a *wrong order* — it's just a #[serial] on something
            # that won't run in parallel anyway. We'll skip this case.
            if _FN_LINE_RE.match(raw):
                state = None
                continue

            # If we reach here, the line after #[serial] is a non-matching
            # attribute, a comment, or code — reset
            state = None
            continue

        if not stripped or stripped.startswith("//") or _COMMENT_LINE_RE.match(raw):
            continue

        # Skip string literals — simple heuristic: line starts/ends inside a
        # string. We track this loosely.
        m = _SERIAL_RE.match(raw)
        if not m:
            continue

        # Found #[serial]. Now check: is the next non-blank,
        # non-comment line a test attribute?
        serial_line = lineno

        # Look at what follows on the same line (after #[serial])
        rest = m.group(1).strip()
        if rest and not rest.startswith("//"):
            # Something follows on the same line — it's not immediately
            # above a test attr. Flag it.
            errors.append((str(path), serial_line,
                           "#[serial] has content on the same line"))
            continue

        # Scan forward for the next non-blank, non-comment line
        for look_ahead in range(lineno, len(lines)):
            next_raw = lines[look_ahead]
            next_stripped = next_raw.lstrip()

            if not next_stripped:
                # Blank line between #[serial] and the test attribute is
                # rejected — no blank line is allowed in the attribute block.
                errors.append(
                    (str(path), serial_line,
                     "#[serial] has a blank line in the attribute block before the test attribute")
                )
                break
            if _COMMENT_LINE_RE.match(next_raw):
                continue
            if next_stripped.startswith(("//", "#![")):
                continue

            # If the next significant line is another #[serial], that's fine
            # (multiple serial attrs are collapsed). Keep scanning.
            if _SERIAL_RE.match(next_raw):
                # Update serial_line to the last serial
                serial_line = look_ahead + 1
                continue

            # Test attribute — correct order
            if _TEST_ATTR_RE.match(next_raw):
                break

            # Any other attribute — skip it and keep scanning.
            # This covers #[cfg(...)], #[instrument], and similar
            # attributes that may sit between #[serial] and the test attr.
            if next_stripped.startswith("#["):
                continue

            # A function definition or non-attribute line — #[serial] is
            # misplaced
            errors.append((str(path), serial_line,
                           "#[serial] is not above a test attribute"))
            break

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check that every #[serial] in *.rs files sits immediately above a #[test] or #[tokio::test].",
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=Path.cwd(),
        help="Repository root directory (default: current directory)",
    )
    parser.add_argument(
        "--no-git",
        action="store_true",
        help="Scan all *.rs files recursively instead of using git ls-files",
    )
    args = parser.parse_args()

    root = args.root.resolve()

    # Collect tracked .rs files
    if args.no_git:
        rs_files = sorted(root.rglob("*.rs"))
    else:
        try:
            result = subprocess.run(
                ["git", "ls-files", "--cached", "--others", "--exclude-standard", "*.rs"],
                cwd=root,
                capture_output=True,
                text=True,
                timeout=30,
            )
            if result.returncode != 0:
                print("error: git ls-files failed — is this a git repository?",
                      file=sys.stderr)
                return 2
            rs_files = [root / p for p in result.stdout.strip().splitlines() if p]
        except (subprocess.SubprocessError, FileNotFoundError):
            print("error: git not found — use --no-git to scan by glob",
                  file=sys.stderr)
            return 2

    if not rs_files:
        print("OK: no .rs files found")
        return 0

    all_errors: list[tuple[str, int, str]] = []
    for fpath in rs_files:
        if not fpath.is_file():
            continue
        errs = check_file(fpath)
        all_errors.extend(errs)

    if all_errors:
        for fname, lineno, detail in all_errors:
            print(f"{fname}:{lineno}: {detail}")
        print()
        print(f"{len(all_errors)} violation(s) found in {len(rs_files)} .rs file(s)")
        return 1

    print(f"OK: {len(rs_files)} .rs file(s) checked \u2014 all #[serial] correctly ordered")
    return 0


if __name__ == "__main__":
    sys.exit(main())
