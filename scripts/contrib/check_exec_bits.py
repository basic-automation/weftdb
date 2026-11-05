#!/usr/bin/env python3
"""Check that every script a workflow runs directly is executable (CI).

A `run:` step that starts with a repository path - `scripts/clients/test-r.sh target/debug/
weft-server` - is the shell executing that file, so it needs the executable bit in the tree.
Git stores the bit per file and an editor or an agent that writes the file does not set it,
and then the check fails on every pull request with the least informative error a runner has:

    scripts/clients/test-r.sh: Permission denied
    ##[error]Process completed with exit code 126.

That is what happened to the R client step when the R client landed: the ubuntu test job went
red for every open pull request, and nothing pointed at the file mode. Only `run:` bodies are
scanned - a `paths:` filter entry naming a script is a path, not a command - and only the
first token of a command line, because a script invoked through an interpreter
(`python3 scripts/crates/check.py`, `bash scripts/crash-matrix.sh`) needs no bit. A quoted
path, a glob, and a line folded from the one before it with a backslash are arguments, not
commands; and a path whose directory is not in the tree is not this repository's file at all,
it is a checkout a sibling step lays down (release.yml's `tools/scripts/release/stage.sh`).

Usage: scripts/contrib/check_exec_bits.py [--root .] [--workflows .github/workflows]
"""
from __future__ import annotations

import argparse
import os
import re
import sys
from collections.abc import Callable
from pathlib import Path

_INLINE_RUN = re.compile(r"^(?P<indent>\s*)(?:-\s+)?run:\s+(?P<body>\S.*)$")
_BLOCK_RUN = re.compile(r"^(?P<indent>\s*)(?:-\s+)?run:\s*[|>]")
_SCRIPT_TOKEN = re.compile(r"^(?:\./)?((?:scripts|tools|clients)/\S+\.(?:sh|py))$")
_NOT_A_COMMAND = ("*", "?", "[", "$", '"', "'")


def command_heads(text: str) -> list[tuple[int, str]]:
    """Every (line number, first token) in `text` that starts a shell command."""
    heads: list[tuple[int, str]] = []
    block_indent: int | None = None  # indentation of the `run:` key whose block we are in
    continued = False
    for number, raw in enumerate(text.splitlines(), start=1):
        stripped = raw.strip()
        if block_indent is None:
            block = _BLOCK_RUN.match(raw)
            if block:
                block_indent = len(block.group("indent"))
                continued = False
                continue
            inline = _INLINE_RUN.match(raw)
            if inline:
                heads.append((number, inline.group("body").split(" ")[0]))
                continued = inline.group("body").rstrip().endswith("\\")
                continue
            if stripped.startswith("#"):
                continue
            continue

        # Inside a `run: |` body: the block ends at the first non-blank line that is not
        # indented deeper than its `run:` key.
        if not stripped:
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        if indent <= block_indent:
            block_indent = None
            continued = False
            # Re-match this line as a step line: it is the next step or a new key.
            block = _BLOCK_RUN.match(raw)
            if block:
                block_indent = len(block.group("indent"))
                continue
            inline = _INLINE_RUN.match(raw)
            if inline:
                heads.append((number, inline.group("body").split(" ")[0]))
                continued = inline.group("body").rstrip().endswith("\\")
            continue

        if continued:
            continued = False
            continue
        continued = stripped.rstrip().endswith("\\")
        if stripped.startswith("#"):
            continue
        heads.append((number, stripped.split(" ")[0]))
    return heads


def direct_script_calls(workflows: dict[str, str]) -> list[tuple[str, int, str]]:
    """Every (workflow file, line number, script path) a workflow executes directly."""
    found: list[tuple[str, int, str]] = []
    for name, text in sorted(workflows.items()):
        for number, token in command_heads(text):
            token = token.rstrip("\\")
            if any(character in token for character in _NOT_A_COMMAND):
                continue
            match = _SCRIPT_TOKEN.match(token)
            if match:
                found.append((name, number, match.group(1)))
    return found


def problems(
    calls: list[tuple[str, int, str]],
    is_executable: Callable[[str], bool],
    in_tree: Callable[[str], bool],
    parent_in_tree: Callable[[str], bool],
) -> list[str]:
    """One line per call site whose script is not executable, or vanished from its directory."""
    out: list[str] = []
    for workflow, number, script in calls:
        if not in_tree(script):
            # A rename left a call site behind; a path in a directory nothing here creates is
            # another checkout's file (release.yml checks out tools/ at that path) - not ours.
            if parent_in_tree(script):
                out.append(f"{workflow}:{number}: {script} is run directly but is not in the tree")
            continue
        if not is_executable(script):
            out.append(f"{workflow}:{number}: {script} is run directly but is not executable")
    return sorted(set(out))


def is_executable(path: Path) -> bool:
    """True when the shell could execute `path`; a broken link or unreadable parent is simply not."""
    try:
        return os.access(path, os.X_OK)
    except OSError:
        return False


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="check that every script a workflow runs directly is executable"
    )
    parser.add_argument("--root", type=Path, default=Path("."))
    parser.add_argument("--workflows", type=Path, default=Path(".github/workflows"))
    args = parser.parse_args(argv)

    root, workflows = args.root.resolve(), args.workflows
    sources = {p.name: p.read_text() for p in sorted(workflows.glob("*.yml"))}
    calls = direct_script_calls(sources)
    found = problems(
        calls,
        lambda script: is_executable(root / script),
        lambda script: (root / script).exists(),
        lambda script: (root / script).parent.is_dir(),
    )
    if found:
        print(
            "a workflow runs a repository script directly, so it needs the executable bit "
            "(`git update-index --chmod=+x <path>`):",
            file=sys.stderr,
        )
        for line in found:
            print(f"  {line}", file=sys.stderr)
        return 1
    print(f"every script a workflow runs directly is executable ({len(calls)} call sites)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
