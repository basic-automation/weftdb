#!/usr/bin/env python3
"""Lint the per-figure claims registry (roadmap EVI-2).

Every performance figure in README.md must be vouched for by an entry in
bench/claims.json, and every entry must still point at a figure that is really
in the README. The README carries the machine-readable side of the pairing as an
HTML comment next to the paragraph it belongs to: `<!-- claim:some-id -->`
(invisible when GitHub renders the file).

EVI-4 artifact wiring: every entry also carries an `artifact` field. It is
null when no pipeline artifact publishes the figure (a dated local measurement
has none, by disclosure), and otherwise an object naming the artifact and the
workflow that publishes it: `{"name": "bench-report", "workflow": "benchmark
regression"}`. This lint checks the reference is real: the named workflow must
exist under .github/workflows/ and must contain an upload-artifact step for an
artifact of that name. The registry never invents provenance, and a workflow
that stops publishing an artifact it is cited for fails here.

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
WORKFLOW_NAME = re.compile(r"^name:\s*[\"']?([^\"'\n]+)[\"']?\s*$")
UPLOAD_ARTIFACT = "upload-artifact"
ARTIFACT_NAME = re.compile(r"^\s*name:\s*[\"']?([^\"'\n]+)[\"']?\s*$")
ARTIFACT_KEYS = {"name", "workflow"}


def workflow_artifact_names(workflows_dir: Path) -> dict[str, set[str]]:
    """Map each workflow's declared name to the artifact names it uploads.

    The parse is deliberately line-based, not YAML: the lint runs on a runner
    with only python3 guaranteed, and the shape it needs is small — the
    workflow's top-level `name:` and, after each `upload-artifact` step, the
    `name:` in that step's `with:` block. A step whose upload name is an
    expression (`weftdb-${{ matrix.target }}`) is not a literal artifact name
    and is not recorded, so it can never satisfy a registry reference.
    """
    published: dict[str, set[str]] = {}
    if not workflows_dir.is_dir():
        return published
    for path in sorted(workflows_dir.glob("*.yml")) + sorted(workflows_dir.glob("*.yaml")):
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        workflow_name = None
        for line in lines:
            m = WORKFLOW_NAME.match(line)
            if m:
                workflow_name = m.group(1).strip()
                break
        if workflow_name is None:
            continue
        names = published.setdefault(workflow_name, set())
        for i, line in enumerate(lines):
            if UPLOAD_ARTIFACT not in line:
                continue
            for following in lines[i + 1 : i + 16]:
                if following.strip().startswith("- "):
                    break  # a new step: this upload step names no artifact
                m = ARTIFACT_NAME.match(following)
                if m:
                    candidate = m.group(1).strip()
                    if "${{" not in candidate:
                        names.add(candidate)
                    break
    return published


def check_artifact(claim: dict, published: dict[str, set[str]]) -> list[str]:
    """Validate one entry's EVI-4 artifact reference against the workflows."""
    cid = claim.get("id", "<missing id>")
    if "artifact" not in claim:
        return [f"{cid}: missing artifact field (null when no pipeline artifact publishes it)"]
    artifact = claim["artifact"]
    if artifact is None:
        return []
    if not isinstance(artifact, dict):
        return [f"{cid}: artifact must be an object or null, not {type(artifact).__name__}"]
    errors = []
    for key in sorted(set(artifact) - ARTIFACT_KEYS):
        errors.append(f"{cid}: artifact has unknown key {key!r} (only 'name' and 'workflow')")
    name = artifact.get("name")
    workflow = artifact.get("workflow")
    if not isinstance(name, str) or not name:
        errors.append(f"{cid}: artifact name must be a non-empty string")
    if not isinstance(workflow, str) or not workflow:
        errors.append(f"{cid}: artifact workflow must be a non-empty string")
    if errors:
        return errors
    if workflow not in published:
        return [f"{cid}: artifact workflow {workflow!r} matches no workflow under .github/workflows/"]
    if name not in published[workflow]:
        return [
            f"{cid}: workflow {workflow!r} publishes no artifact named {name!r} "
            f"(it publishes {sorted(published[workflow]) or 'nothing'})"
        ]
    return []
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

    published = workflow_artifact_names(root / ".github" / "workflows")
    for claim in claims:
        errors.extend(check_artifact(claim, published))

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
