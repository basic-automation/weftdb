#!/usr/bin/env python3
"""Check that the engine cannot tell it is being benchmarked (anti-Goodhart, roadmap 1.3, #150).

A benchmark is only evidence if the code it measures is the code users run. Two rules keep it so:

1. No engine crate depends on a benchmark crate. Every workspace member except `weft-bench`, and
   anything else in the tree, must not name `weft-bench` or `weft-xbench` in any dependency table,
   so engine code cannot reach benchmark types or switches.
2. No engine source detects a benchmark. Outside the benchmark crates, Rust code (comments aside)
   must not use a `weft_bench`/`weft_xbench` path or read an environment variable whose name says
   BENCH: the shortcut a benchmark-aware engine takes starts with one of those. A string that only
   names the harness (the settings registry documents the bench's test-only variables) is not a
   read, and is allowed.

The benchmark crates themselves, and criterion benches under an engine crate's `benches/`, are
exempt: they are measurement code.

Usage: scripts/bench/check-separation.py [--root .]
"""
from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

BENCH_CRATES = ("weft-bench", "weft-xbench")
DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")
SKIP_DIRS = {"target", ".git", "node_modules", "benches", "compat"}
PATTERNS = [
    (re.compile(r"\bweft_x?bench\b"), "uses a benchmark crate's path"),
    (re.compile(r"env::var(?:_os)?\(\s*\"[^\"]*BENCH"), "reads a BENCH environment variable"),
]


def dependency_names(manifest: dict) -> set[str]:
    names: set[str] = set()
    tables = [manifest] + list(manifest.get("target", {}).values())
    for table in tables:
        for key in DEPENDENCY_TABLES:
            for name, spec in table.get(key, {}).items():
                names.add(spec.get("package", name) if isinstance(spec, dict) else name)
    return names


def manifest_problems(path: Path, manifest: dict) -> list[str]:
    crate = manifest.get("package", {}).get("name")
    if crate in BENCH_CRATES:
        return []
    return [f"{path}: crate {crate or '(workspace)'} depends on {dep}" for dep in sorted(dependency_names(manifest) & set(BENCH_CRATES))]


def code(line: str) -> str:
    """The line without a trailing `//` comment (a `//` inside a string literal is kept)."""
    in_string = False
    for i, ch in enumerate(line):
        if ch == '"' and (i == 0 or line[i - 1] != "\\"):
            in_string = not in_string
        elif not in_string and line.startswith("//", i):
            return line[:i]
    return line


def source_problems(path: Path, text: str) -> list[str]:
    out = []
    for number, line in enumerate(text.splitlines(), 1):
        stripped = code(line)
        for pattern, why in PATTERNS:
            if pattern.search(stripped):
                out.append(f"{path}:{number}: {why}: {line.strip()}")
    return out


def walk(root: Path):
    for path in sorted(root.iterdir()):
        if path.name in SKIP_DIRS or path.name in BENCH_CRATES:
            continue
        if path.is_dir():
            yield from walk(path)
        else:
            yield path


def check(root: Path) -> list[str]:
    problems: list[str] = []
    for path in walk(root):
        rel = path.relative_to(root)
        if path.name == "Cargo.toml":
            problems += manifest_problems(rel, tomllib.loads(path.read_text()))
        elif path.suffix == ".rs":
            problems += source_problems(rel, path.read_text(errors="replace"))
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path("."))
    args = parser.parse_args()
    problems = check(args.root)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        print(f"check-separation: {len(problems)} place(s) where engine code can tell it is benchmarked", file=sys.stderr)
        return 1
    print("check-separation: no engine crate depends on or detects a benchmark crate")
    return 0


if __name__ == "__main__":
    sys.exit(main())
