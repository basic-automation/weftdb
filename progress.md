# progress — FRE-18A

## Done

All deliverables for issue #838 (FRE-18a) are implemented, committed, pushed, and the PR is open.

- **Baseline copy** — `compat/openapi/baseline/weftdb.json` (byte-identical to the committed spec) + `compat/openapi/baseline/README.md` explaining the convention.
- **Checker** — `scripts/ci/check-openapi-breaking.py` (Python 3 stdlib only). Exits 2 (missing baseline), 1 (breaking change found), 0 (clean).
- **Tests** — `scripts/ci/test_check_openapi_breaking.py` (unittest, 16 tests, all pass).
- **CI** — the `claims` job in `.github/workflows/ci.yml` runs the checker + its tests.
- **Docs** — `docs/stability.md`, `docs/roadmap-notes.md`, `DECISIONS.md` updated.
- **Commit**: `c724281` on `fre18a/openapi-breaking-check`, with `Signed-off-by: physics515 <physics515@users.noreply.github.com>` and `Closes #838`.
- **PR**: https://github.com/basic-automation/weftdb/pull/839

## Verified locally

- `python3 -m unittest discover -s scripts/ci -p 'test_*.py' -v` → 50 tests OK (16 new + 34 existing)
- `python3 scripts/ci/check-openapi-breaking.py` → exit 0
- `python3 scripts/ci/check-action-pinning.py` → OK
- No staged files remain, working tree clean.

## Not done

- **Do not merge** — left for reviewer.
- CI workflow will trigger automatically on push (no runner constraints for Python steps).