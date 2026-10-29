#!/usr/bin/env python3
"""Check that weft-xbench's lock agrees with the workspace's (EVI-13).

weft-xbench is its own workspace so that DuckDB's C++ build stays out of the main one, which
gives it its own Cargo.lock. That lock is *seeded* from the workspace lock: every package both
graphs use must resolve to the same version, or the cross-engine numbers would be measuring a
WeftDB built against different dependencies than the one that ships.

A package present in both lockfiles at different versions is drift and fails the check. A
package only in weft-xbench's lock (DuckDB and what it alone pulls in) is expected. Two
versions are the same resolution when Cargo would treat them as compatible (same major, or
same minor below 1.0); a package DuckDB pulls in at another major version is a separate package
to Cargo and not drift.

Usage: scripts/xbench/check-lock.py [--root Cargo.lock] [--xbench weft-xbench/Cargo.lock]
"""
from __future__ import annotations

import argparse
import sys
import tomllib
from collections import defaultdict
from pathlib import Path


def versions(lock: Path) -> dict[str, set[str]]:
    data = tomllib.loads(lock.read_text())
    out: dict[str, set[str]] = defaultdict(set)
    for package in data.get("package", []):
        out[package["name"]].add(package["version"])
    return out


def compat(version: str) -> tuple[int, ...]:
    """Cargo's semver-compatibility class: the major, or 0.minor, or 0.0.patch."""
    parts = [int(p) for p in version.split("+")[0].split("-")[0].split(".")]
    major, minor, patch = (parts + [0, 0, 0])[:3]
    if major:
        return (major,)
    if minor:
        return (0, minor)
    return (0, 0, patch)


def drift(root: dict[str, set[str]], xbench: dict[str, set[str]]) -> list[str]:
    problems = []
    for name in sorted(root.keys() & xbench.keys()):
        workspace = {compat(v): v for v in root[name]}
        for version in sorted(xbench[name]):
            expected = workspace.get(compat(version))
            if expected is not None and expected != version:
                problems.append(f"{name}: workspace {expected}, weft-xbench {version}")
    return problems


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path("Cargo.lock"))
    parser.add_argument("--xbench", type=Path, default=Path("weft-xbench/Cargo.lock"))
    args = parser.parse_args(argv)
    problems = drift(versions(args.root), versions(args.xbench))
    if problems:
        print("weft-xbench/Cargo.lock has drifted from Cargo.lock; re-seed it:", file=sys.stderr)
        print("  cp Cargo.lock weft-xbench/Cargo.lock && (cd weft-xbench && cargo update -w)", file=sys.stderr)
        for line in problems:
            print(f"  {line}", file=sys.stderr)
        return 1
    print(f"weft-xbench lock: every shared package matches the workspace ({len(versions(args.xbench))} packages)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
