#!/usr/bin/env python3
"""
check-baseline-control.py

Compare the committed baseline SHA-256 digest against the base-ref copy
fetched via ``gh api``.  This is the non-tautological baseline control
for FRE-18c: if the baseline file changed from the base commit, the
existing digest assertion (file vs committed sha256) would still pass
(because both file and digest were updated in the same PR), so this
additional step catches silent baseline moves.

Usage:
    python3 scripts/ci/check-baseline-control.py \\
        --baseline compat/openapi/baseline/weftdb.json \\
        --digest-file compat/openapi/baseline/weftdb.json.sha256

In CI the script uses ``gh api`` with ``${{ github.repository }}`` and
``${{ github.base_ref }}`` to retrieve the base-branch copy.

For local probing:
    python3 scripts/ci/check-baseline-control.py \\
        --baseline PATH \\
        --digest-file PATH \\
        --base-digest \"<sha256-from-base>\"

Exit codes:
    0 — baseline matches the base commit (no change, or bump with human
        review documented in the PR body).
    1 — baseline differs from the base commit — the file moved without
        an accompanying bump declaration.
    2 — missing file, unreadable, or API failure.
"""

import argparse
import hashlib
import os
import subprocess
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_DEFAULT_BASELINE = os.path.join(
    _HERE, "..", "..", "compat", "openapi", "baseline", "weftdb.json",
)
_DEFAULT_DIGEST_FILE = _DEFAULT_BASELINE + ".sha256"


def _sha256_of(path: str) -> str:
    """Return hex SHA-256 of *path*."""
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def _read_digest_file(path: str) -> str:
    """Read the first whitespace-separated token from a .sha256 file."""
    with open(path, encoding="utf-8") as f:
        line = f.readline().strip()
    parts = line.split()
    return parts[0] if parts else ""


def _fetch_base_digest(repo: str, ref: str, path: str) -> str:
    """Fetch the digest file content from the base branch via gh api.

    Returns the sha256 hex string found in the response's ``content``
    field (base64-decoded, first whitespace token).
    """
    # If the path is absolute (e.g. from a default path built from
    # __file__ in CI), strip the GITHUB_WORKSPACE prefix so the URL
    # is repo-relative.
    workspace = os.environ.get("GITHUB_WORKSPACE", "")
    if workspace and path.startswith(workspace + "/"):
        path = path[len(workspace) + 1:]
    api_url = f"repos/{repo}/contents/{path.lstrip('/')}?ref={ref}"
    result = subprocess.run(
        ["gh", "api", api_url, "--jq", ".content"],
        capture_output=True, text=True, timeout=30,
    )
    if result.returncode != 0:
        print(
            f"ERROR: gh api failed (exit {result.returncode}): "
            f"{result.stderr.strip()}",
            file=sys.stderr,
        )
        sys.exit(2)
    # GitHub's content API returns base64-encoded content
    import base64
    try:
        raw = result.stdout.strip()
        decoded = base64.b64decode(raw).decode("utf-8")
    except Exception as exc:
        print(
            f"ERROR: could not decode base64 response: {exc}",
            file=sys.stderr,
        )
        sys.exit(2)
    parts = decoded.split()
    return parts[0] if parts else ""


def main_with_args(
    baseline: str,
    digest_file: str,
    base_digest: str | None = None,
    repo: str | None = None,
    base_ref: str | None = None,
) -> int:
    """Entry point that accepts paths directly (no argparser), for tests."""
    # Validate files exist
    if not os.path.isfile(baseline):
        print(f"ERROR: baseline not found: {baseline}", file=sys.stderr)
        return 2
    if not os.path.isfile(digest_file):
        print(f"ERROR: digest file not found: {digest_file}", file=sys.stderr)
        return 2

    local_digest = _sha256_of(baseline)

    if base_digest is not None:
        bdigest = base_digest.strip()
    else:
        r = repo or os.environ.get("GITHUB_REPOSITORY", "")
        br = base_ref or os.environ.get("GITHUB_BASE_REF", "")
        if not br:
            print("no base ref: baseline control not applicable")
            return 0
        if not r:
            print(
                "ERROR: --repo and --base-ref are required when "
                "--base-digest is not provided (or set GITHUB_REPOSITORY "
                "and GITHUB_BASE_REF).",
                file=sys.stderr,
            )
            return 2
        bdigest = _fetch_base_digest(r, br, digest_file)

    if local_digest != bdigest:
        print(
            f"FAIL: baseline has changed from base commit.\n"
            f"  Base ref digest: {bdigest}\n"
            f"  Local  baseline: {local_digest}\n"
            f"  File:            {baseline}\n\n"
            f"The baseline is a copy of the spec it polices.  A PR that "
            f"updates the baseline must also update the digest file and "
            f"say which breaking changes it accepts and why "
            f"(see compat/openapi/baseline/README.md).",
            file=sys.stderr,
        )
        return 1

    print(f"OK: baseline matches base commit ({local_digest})")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Non-tautological baseline control for the OpenAPI "
        "breaking-change gate.",
    )
    parser.add_argument(
        "--baseline", default=_DEFAULT_BASELINE,
        help=f"Baseline file path (default: {_DEFAULT_BASELINE})",
    )
    parser.add_argument(
        "--digest-file", default=_DEFAULT_DIGEST_FILE,
        help=f"SHA-256 digest file path (default: {_DEFAULT_DIGEST_FILE})",
    )
    parser.add_argument(
        "--base-digest", default=None,
        help="Override: provide the base-branch digest as a string "
        "(for local probing without gh api).",
    )
    parser.add_argument(
        "--repo", default=None,
        help="GitHub repository (owner/name) for gh api calls. "
        "Defaults to ${{ github.repository }}.",
    )
    parser.add_argument(
        "--base-ref", default=None,
        help="Base branch ref for gh api calls. "
        "Defaults to ${{ github.base_ref }}.",
    )
    args = parser.parse_args()
    return main_with_args(
        args.baseline,
        args.digest_file,
        base_digest=args.base_digest,
        repo=args.repo,
        base_ref=args.base_ref,
    )


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        sys.exit(2)
