#!/usr/bin/env python3
"""
check-serial-order.py

Scan every tracked ``*.rs`` file in the repository and verify that every
``#[serial]`` attribute — plain or keyed (``#[serial(key)]``,
``#[serial(key_a, key_b)]``) — sits in the **same attribute block** as a test
attribute (``#[test]``, ``#[tokio::test]``, ``#[tokio::test(...)]``). Either
position in that block is fine: above the test attribute or below it.

Why both positions are accepted (this replaces the earlier "must sit
immediately above" rule, which was wrong):

- ``serial_test``'s own README documents the *below* order::

      #[test]
      #[serial]
      fn one() {}

      #[tokio::test]
      #[serial]
      async fn two() {}

- ``#[tokio::test]`` re-emits the item's outer attributes **before** the
  generated test attribute (``tokio-macros`` ``src/entry.rs``:
  ``input.into_tokens(generated_attrs, body, last_block)`` and ``into_tokens``
  streaming ``outer_attrs`` as-is, then ``#[::core::prelude::v1::test]``), so
  ``#[tokio::test]`` + ``#[serial(k)]`` expands to ``#[serial(k)]`` +
  ``#[test]`` over a *sync* body, which serialises through
  ``local_serial_core``.
- ``#[serial]`` written above ``#[tokio::test]`` serialises the other way:
  ``local_async_serial_core`` awaits the future while holding the guards and
  leaves the inner ``#[tokio::test]`` in place.

What is still a real bug, and what this checker catches: a ``#[serial]`` in a
block with **no** test attribute at all. That attribute does nothing — the test
compiles, passes, and runs concurrently with the tests it was meant to exclude,
which shows up later as an unrelated flake. The keyed form matters most here:
it is what guards the process-global fault injectors
(``backup_fault_points``, ``build_publish``, ``index_txn_fault_points``), and
the earlier version of this checker could not see it at all because its regex
required the closing bracket immediately after ``serial``.

Exits non-zero when a violation is found, printing ``file:line`` of each.

Style rules kept from the earlier version (they are style, not correctness):
one attribute per line, and no blank line inside an attribute block.

Heuristics and known limits:
- A ``#[serial]`` inside a comment or a string is ignored: only a line whose
  first non-space token opens an attribute can start one.
- Multi-line attributes are followed by bracket balance, with no awareness of
  escapes inside strings (a bracket inside a string can throw the balance off
  for that block, which at worst skips one block).
- ``serial_test`` also allows the attribute on a whole ``mod``; that is
  accepted, since it applies to every test in the module.

Usage:
    python3 scripts/ci/check-serial-order.py
    python3 scripts/ci/check-serial-order.py --root . --no-git
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# #[serial] and the keyed #[serial(key)] / #[serial(a, b)] form. ``\b`` keeps
# look-alikes such as #[serializer] out; ``rest`` is whatever follows it on the
# same line.
_SERIAL_RE = re.compile(r"^\s*#\s*\[\s*serial\b(?:\s*\([^)]*\))?\s*\]\s*(?P<rest>.*)$")

# #[test], #[tokio::test], #[tokio::test(...)] at the start of a line.
_TEST_ATTR_RE = re.compile(r"^\s*#\s*\[\s*(?:tokio::)?test\b")

# The same attribute anywhere in an attribute line, for the two-attributes-on
# -one-line style case.
_TEST_ATTR_ANYWHERE = re.compile(r"#\s*\[\s*(?:tokio::)?test\b")

# #[...] or #![...] — any attribute, keyed or not.
_ATTR_START_RE = re.compile(r"^\s*#!?\[")

# A // or /// line.
_COMMENT_RE = re.compile(r"^\s*//")

# #[serial] applied at module level is legitimate: serial_test applies it to
# every test function in the block.
_MOD_ITEM_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:pub\s+)?mod\s")

_NO_TEST = "#[serial] is not on a test function — no #[test] or #[tokio::test] in its attribute block"
_SAME_LINE = "#[serial] has content on the same line (style: one attribute per line)"
_BLANK = "#[serial] has a blank line in the attribute block before the test attribute"


def _line_state(line: str) -> tuple[str, int]:
    """Classify one line and return its bracket balance delta.

    ``kind`` is ``"attr"`` (opens or continues an attribute), ``"comment"``,
    ``"blank"`` or ``"code"``. Strings and ``//`` comments are skipped when
    counting brackets.
    """
    kind = "code"
    stripped = line.strip()
    if not stripped:
        kind = "blank"
    elif _COMMENT_RE.match(line):
        kind = "comment"
    elif _ATTR_START_RE.match(line):
        kind = "attr"

    balance = 0
    in_string = False
    prev = ""
    for ch in line:
        if ch == '"' and prev != "\\":
            in_string = not in_string
        elif not in_string:
            if ch in "[(":
                balance += 1
            elif ch in "])":
                balance -= 1
        prev = ch
    if kind == "code" and balance != 0 and stripped:
        kind = "attr-continuation"
    return kind, balance


def _read_lines(path: Path) -> list[str]:
    try:
        return path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError):
        return []


def check_file(path: Path) -> list[tuple[str, int, str]]:
    """Check one ``.rs`` file. Returns a list of (file, line, detail)."""
    lines = _read_lines(path)
    errors: list[tuple[str, int, str]] = []
    n = len(lines)

    i = 0
    while i < n:
        kind, _balance = _line_state(lines[i])
        if kind != "attr":
            i += 1
            continue

        # Consume one contiguous attribute block: attribute lines (and comment
        # lines between them), following multi-line attributes by balance.
        block: list[tuple[int, str]] = []
        j = i
        balance = 0
        while j < n:
            k, b = _line_state(lines[j])
            if balance > 0:
                block.append((j + 1, lines[j]))
                balance += b
                j += 1
                continue
            if k == "attr":
                block.append((j + 1, lines[j]))
                balance = b
                j += 1
                continue
            if k == "comment":
                block.append((j + 1, lines[j]))
                j += 1
                continue
            break

        item_line = lines[j] if j < n else ""
        _analyse_block(path, block, lines, j, item_line, errors)
        i = j if j > i else i + 1

    return errors


def _analyse_block(
    path: Path,
    block: list[tuple[int, str]],
    lines: list[str],
    after: int,
    item_line: str,
    errors: list[tuple[str, int, str]],
) -> None:
    """Flag every #[serial] in one attribute block that has no test attribute."""
    attrs = [(ln, raw) for ln, raw in block if _line_state(raw)[0] == "attr"]
    serials = []
    for ln, raw in attrs:
        match = _SERIAL_RE.match(raw)
        if match:
            serials.append((ln, match))
    if not serials:
        return

    if any(_TEST_ATTR_ANYWHERE.search(_strip_trailing_comment(raw)) for _ln, raw in attrs):
        # Correctly attached, in either position. Only the style rules apply.
        for ln, match in serials:
            rest = (match.group("rest") or "").strip()
            if rest and not rest.startswith("//"):
                errors.append((str(path), ln, _SAME_LINE))
        return

    if _MOD_ITEM_RE.match(item_line):
        # Module-level #[serial]: supported by the crate, nothing to attach to.
        return

    detail = _NO_TEST
    if _blank_line_before_test(lines, after):
        detail = _BLANK
    for ln, _raw in serials:
        errors.append((str(path), ln, detail))


def _strip_trailing_comment(line: str) -> str:
    """Drop a trailing ``//`` comment so a test attribute mentioned in prose
    cannot make an unattached #[serial] look attached."""
    cut = line.find("//")
    return line if cut < 0 else line[:cut]


def _blank_line_before_test(lines: list[str], after: int) -> bool:
    """True when a blank line separates the block from a test attribute — the
    attribute block was split, but the caller clearly meant it to apply."""
    i = after
    saw_blank = False
    while i < len(lines):
        k, _b = _line_state(lines[i])
        if k == "blank":
            saw_blank = True
            i += 1
            continue
        if k == "comment":
            i += 1
            continue
        return saw_blank and k == "attr" and bool(_TEST_ATTR_RE.match(lines[i]))
    return False


def _collect_rs_files(root: Path, use_git: bool) -> list[Path] | None:
    if not use_git:
        return sorted(root.rglob("*.rs"))
    try:
        result = subprocess.run(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "*.rs"],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        print("error: git not found — use --no-git to scan by glob", file=sys.stderr)
        return None
    if result.returncode != 0:
        print("error: git ls-files failed — is this a git repository?", file=sys.stderr)
        return None
    return [root / p for p in result.stdout.strip().splitlines() if p]


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check that every #[serial] (plain or keyed) shares an attribute block with a #[test] or #[tokio::test].",
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
    rs_files = _collect_rs_files(root, not args.no_git)
    if rs_files is None:
        return 2

    if not rs_files:
        print("OK: no .rs files found")
        return 0

    all_errors: list[tuple[str, int, str]] = []
    for fpath in rs_files:
        if fpath.is_file():
            all_errors.extend(check_file(fpath))

    if all_errors:
        for fname, lineno, detail in all_errors:
            print(f"{fname}:{lineno}: {detail}")
        print()
        print(f"{len(all_errors)} violation(s) found in {len(rs_files)} .rs file(s)")
        return 1

    print(f"OK: {len(rs_files)} .rs file(s) checked \u2014 every #[serial] is attached to a test attribute")
    return 0


if __name__ == "__main__":
    sys.exit(main())
