#!/usr/bin/env python3
"""Lint the per-figure claims registry (roadmap EVI-2).

Every performance figure in README.md must be vouched for by an entry in
bench/claims.json, and every entry must still point at a figure that is really
in the README. The README carries the machine-readable side of the pairing as an
HTML comment next to the paragraph it belongs to: `<!-- claim:some-id -->`
(invisible when GitHub renders the file).

Usage: tools/check_claims.py [--registry bench/claims.json] [--readme README.md]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

WINDOW = 12  # lines around a marker within which the figure text must appear
KINDS = {"reproducible", "local-measurement", "corrected-claim"}
MARKER = re.compile(r"<!--\s*claim:([a-z0-9][a-z0-9-]*)\s*-->")
LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)")
SOURCE_LINE = re.compile(r"^\*Source:\s*\[`?([^`\]]+)`?\]")
DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--registry", default="bench/claims.json")
    ap.add_argument("--readme", default="README.md")
    args = ap.parse_args()

    root = Path(args.readme).resolve().parent
    lines = Path(args.readme).read_text(encoding="utf-8").splitlines()
    doc = "\n".join(lines)

    try:
        registry = json.loads(Path(args.registry).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"claims: cannot read {args.registry}: {exc}")
        return 1

    claims = registry.get("claims", [])
    errors: list[str] = []

    # Marker positions in the README, and the registry ids they name.
    marked: dict[str, int] = {}
    for no, line in enumerate(lines):
        for mid in MARKER.findall(line):
            if mid in marked:
                errors.append(f"claim marker {mid!r} appears twice (lines {marked[mid] + 1} and {no + 1})")
            marked[mid] = no

    ids = [c.get("id") for c in claims]
    for dup in {i for i in ids if ids.count(i) > 1}:
        errors.append(f"registry lists claim id {dup!r} more than once")

    for mid in sorted(set(ids or []) - set(marked)):
        errors.append(f"registry entry {mid!r} has no `<!-- claim:{mid} -->` marker in {args.readme}")
    for mid in sorted(set(marked) - set(ids or [])):
        errors.append(f"{args.readme}:{marked[mid] + 1} marks claim {mid!r} that the registry does not define")

    for claim in claims:
        cid = claim.get("id", "<missing id>")
        if claim.get("kind") not in KINDS:
            errors.append(f"{cid}: kind {claim.get('kind')!r} must be one of {sorted(KINDS)}")
        if not DATE.match(str(claim.get("measured", ""))):
            errors.append(f"{cid}: measured {claim.get('measured')!r} is not YYYY-MM-DD")

        figures = claim.get("figures") or []
        if not figures:
            errors.append(f"{cid}: figures is empty, so nothing vouches for this entry")
        if cid in marked:
            lo = max(0, marked[cid] - WINDOW)
            hi = min(len(lines), marked[cid] + WINDOW + 1)
            window = "\n".join(lines[lo:hi])
            for fig in figures:
                if fig not in window:
                    where = "not in the README at all" if fig not in doc else f"not within {WINDOW} lines of its marker"
                    errors.append(f"{cid}: figure {fig!r} is {where}")

        source = claim.get("source")
        if source and not (root / source).exists():
            errors.append(f"{cid}: source path {source!r} does not exist")
        if claim.get("reproducible_from_clone"):
            if not source:
                errors.append(f"{cid}: reproducible_from_clone is true but source is null")
            if "not distributed" in str(claim.get("dataset", "")):
                errors.append(f"{cid}: reproducible_from_clone is true but the dataset is not distributed")

    # A figure paragraph that names a bench must be registered: catches a new
    # subsection added without an entry.
    registered_sources = {c.get("source") for c in claims}
    for no, line in enumerate(lines):
        m = SOURCE_LINE.match(line)
        if m and m.group(1) not in registered_sources:
            errors.append(f"{args.readme}:{no + 1} cites source {m.group(1)!r}, which no registry entry claims")

    # Relative links must resolve against the tree. Anchors are checked only when the
    # target is this same file; GitHub computes heading slugs with rules that produce
    # false positives for `->` and apostrophes, which is why the anchor half of EVI-1's
    # audit was left as prose.
    for no, line in enumerate(lines):
        for target in LINK.findall(line):
            if "://" in target or target.startswith(("#", "mailto:")):
                continue
            path = target.split("#", 1)[0]
            if path and not (root / path).exists():
                errors.append(f"{args.readme}:{no + 1} links to {target!r}, which is not in the tree")

    for err in errors:
        print(f"claims: {err}")
    if errors:
        print(f"claims: {len(errors)} problem(s) in {len(claims)} registered figure(s)")
        return 1
    print(f"claims: {len(claims)} figure(s) registered, every one paired with a marker and a source")
    return 0


if __name__ == "__main__":
    sys.exit(main())
