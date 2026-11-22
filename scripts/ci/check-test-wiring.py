#!/usr/bin/env python3
"""
check-test-wiring.py

Ensure every ``test_*.py`` under ``scripts/`` and ``tools/`` is invoked
by at least one workflow step.  Steps are recognised in two forms:

1. direct: the run text contains ``python3 <path>`` where ``<path>`` is
   the exact repo-relative path of the test file.
2. discovery: the run text matches
   ``python3 -m unittest discover -s <dir> -p <pattern>``; a file is
   covered if its parent directory is a descendant of ``<dir>`` and its
   name matches ``<pattern>`` via ``fnmatch``.

Files not covered by any step are printed on ``UNWIRED: <path> ...``
lines; the script exits 1 if any unwired file exists, 0 otherwise.

Usage::

    python3 scripts/ci/check-test-wiring.py
    python3 scripts/ci/check-test-wiring.py --root <repo-root> --workflows <dir>
"""

from __future__ import annotations

import argparse
import fnmatch
import os
import sys

import yaml

_DISCOVER_RE = "python3 -m unittest discover"
_DIR_FLAG = "-s"
_PATTERN_FLAG = "-p"


def _collect_test_files(root: str) -> list[str]:
    """Return sorted repo-relative paths of every ``test_*.py`` under
    ``scripts/`` and ``tools/``, skipping ignored/virtual directories."""
    skip_dirs = {".git", "target", "node_modules", "__pycache__"}
    result: list[str] = []
    for prefix in ("scripts", "tools"):
        base = os.path.join(root, prefix)
        if not os.path.isdir(base):
            continue
        for dirpath, dirnames, filenames in os.walk(base):
            # Filter skip dirs in-place so os.walk does not descend.
            dirnames[:] = [d for d in dirnames if d not in skip_dirs]
            for fn in filenames:
                if fn.startswith("test_") and fn.endswith(".py"):
                    rel = os.path.relpath(os.path.join(dirpath, fn), root)
                    result.append(rel)
    result.sort()
    return result


def _collect_wf_paths(workflows_dir: str) -> list[str]:
    """Return every ``.yml`` and ``.yaml`` file under *workflows_dir*."""
    result: list[str] = []
    if not os.path.isdir(workflows_dir):
        return result
    for fn in sorted(os.listdir(workflows_dir)):
        if fn.endswith((".yml", ".yaml")):
            result.append(os.path.join(workflows_dir, fn))
    return result


def _parse_wiring_steps(wf_paths: list[str]) -> list[dict]:
    """Return a list of wiring clues: each dict has keys
    ``"direct"`` (list of paths) and ``"discover"`` (list of
    ``(dir, pattern)`` tuples)."""
    direct: set[str] = set()
    discover: list[tuple[str, str]] = []
    for wf_path in wf_paths:
        with open(wf_path, encoding="utf-8") as f:
            try:
                data = yaml.safe_load(f)
            except yaml.YAMLError as exc:
                print(
                    f"WARNING: could not parse {wf_path}: {exc}",
                    file=sys.stderr,
                )
                continue
        if not isinstance(data, dict):
            continue
        # Walk all jobs and their steps.
        jobs = data.get("jobs", {})
        if not isinstance(jobs, dict):
            continue
        for _job_name, job in jobs.items():
            if not isinstance(job, dict):
                continue
            steps = job.get("steps", [])
            if not isinstance(steps, list):
                continue
            for step in steps:
                if not isinstance(step, dict):
                    continue
                run = step.get("run")
                if not isinstance(run, str):
                    continue
                _parse_run_text(run, direct, discover)
    return [{"direct": sorted(direct), "discover": sorted(discover)}]


def _parse_run_text(
    run: str,
    direct: set[str],
    discover: list[tuple[str, str]],
) -> None:
    """Extract wiring clues from a single run command."""
    run_stripped = run.strip()
    # Discovery pattern: python3 -m unittest discover -s <dir> -p <pattern>
    if run_stripped.startswith(_DISCOVER_RE):
        args = run_stripped.split()
        seen_s = False
        seen_p = False
        s_val: str | None = None
        p_val: str | None = None
        for i, tok in enumerate(args):
            if tok == _DIR_FLAG and i + 1 < len(args):
                s_val = args[i + 1]
                seen_s = True
            elif tok == _PATTERN_FLAG and i + 1 < len(args):
                p_val = args[i + 1]
                seen_p = True
        if seen_s and seen_p and s_val is not None and p_val is not None:
            # Strip surrounding quotes (YAML run strings often contain
            # literal single-quoted values).
            s_val = s_val.strip("'\"")
            p_val = p_val.strip("'\"")
            discover.append((s_val, p_val))
        return

    # Direct pattern: look for "python3 <path>" where <path> is a .py file.
    tokens = run_stripped.split()
    for i, tok in enumerate(tokens):
        if tok == "python3" and i + 1 < len(tokens):
            candidate = tokens[i + 1]
            # Accept only .py files (test_*.py or any .py that could be
            # a test guard).
            if candidate.endswith(".py"):
                # Strip any leading ./ or ../ for normalisation.
                normed = os.path.normpath(candidate)
                direct.add(normed)
                # Also add the bare basename, since a step may be run
                # from the repo root.
                direct.add(os.path.basename(candidate))


def _is_wired(
    test_path: str,
    direct: set[str],
    discover: list[tuple[str, str]],
) -> bool:
    """Return True if *test_path* is covered by any wiring clue."""
    # Direct match: exact repo-relative path, or bare basename.
    if test_path in direct:
        return True
    if os.path.basename(test_path) in direct:
        return True

    # Discovery match.
    test_dir = os.path.dirname(test_path)
    test_name = os.path.basename(test_path)
    for discover_dir, pattern in discover:
        # Normalise both sides so comparisons work.
        norm_dir = os.path.normpath(discover_dir)
        norm_test_dir = os.path.normpath(test_dir)
        # The test file's directory must be under the discovery directory.
        # Use commonpath to check containment.
        try:
            common = os.path.commonpath([norm_dir, norm_test_dir])
        except ValueError:
            continue
        # If discover_dir is ".", it matches everything.
        covered = (
            norm_dir == "."
            or (
                len(common) >= len(norm_dir)
                and norm_test_dir.startswith(norm_dir)
                and fnmatch.fnmatch(test_name, pattern)
            )
        )
        if covered:
            return True

    return False


def _run_check(root: str, workflows_dir: str) -> int:
    """Run the wiring check.  Returns 0 if all files are wired, 1 otherwise."""
    test_files = _collect_test_files(root)
    wf_paths = _collect_wf_paths(workflows_dir)
    wiring = _parse_wiring_steps(wf_paths)
    direct: set[str] = set()
    discover: list[tuple[str, str]] = []
    for w in wiring:
        direct.update(w["direct"])
        discover.extend(w["discover"])

    unwired: list[str] = []
    for tf in test_files:
        if not _is_wired(tf, direct, discover):
            unwired.append(tf)

    wf_count = len(wf_paths)
    total = len(test_files)
    if unwired:
        for u in unwired:
            print(
                f"UNWIRED: {u} (no direct invocation and no discover rule"
                f" covers it)",
            )
        print(
            f"FAILED: {len(unwired)} of {total} test script(s) unwired"
            f" across {wf_count} workflow(s)",
            file=sys.stderr,
        )
        return 1

    print(
        f"OK: {total} test script(s) wired by {wf_count} workflow(s)",
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Ensure every Python test file is invoked by a workflow step.",
    )
    parser.add_argument(
        "--root",
        default=None,
        help="Repository root (default: inferred from script location, "
        "walked up until .github/workflows is found)",
    )
    parser.add_argument(
        "--workflows",
        default=None,
        help="Directory with workflow YAML files "
        "(default: <root>/.github/workflows)",
    )
    args = parser.parse_args()

    # Infer root from __file__ if not provided.
    if args.root is not None:
        root = os.path.abspath(args.root)
    else:
        script_dir = os.path.dirname(os.path.abspath(__file__))
        # Walk up until we find .github/workflows or hit the filesystem root.
        cur = script_dir
        while True:
            candidate_gh = os.path.join(cur, ".github", "workflows")
            if os.path.isdir(candidate_gh):
                root = cur
                break
            parent = os.path.dirname(cur)
            if parent == cur:
                root = cur
                break
            cur = parent
        else:
            root = cur

    if args.workflows is not None:
        wf_dir = os.path.abspath(args.workflows)
    else:
        wf_dir = os.path.join(root, ".github", "workflows")

    return _run_check(root, wf_dir)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        sys.exit(2)
