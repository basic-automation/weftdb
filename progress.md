# progress — FRE-18A / FRE-18B

## Done (FRE-18a)

All original deliverables for issue #838 (FRE-18a) are implemented, committed, pushed, and the PR is open.

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

## Handoff — FRE-18b (this run)

- **All code changes are in `weft-server/src/ingest.rs`**, `CHANGELOG.md`, `DECISIONS.md`.
- **To open a PR:** `gh pr create --repo basic-automation/weftdb --base main --head p12/ingest-disconnect-4xx` with a body that says "CI is the first execution; this code was never compiled locally."
- **To run CI:** push the branch; CI will compile and run the tests. Key test to watch: `ingest_body_read_past_the_total_deadline_is_408` (must stay green — it was not modified, only duplicated).
- **Next command after this handoff:** `cargo fmt` to format the code, then commit and push.

---

## Handoff — FRE-18c (this run)

- **All code changes are Python + docs + CI yml only.** No Rust touched.
- **Fixes #843:** requestBody.required deltas (P0), combinator descent (P1), non-tautological baseline control (P1), docs corrected (P2).
- **PR**: https://github.com/basic-automation/weftdb/pull/845
- **CI run**: https://github.com/basic-automation/weftdb/actions/runs/44988826200
- **Local verification**: `python3 -m unittest discover -s scripts/ci -p 'test_*.py'` → 85 tests, all pass; `check-action-pinning.py` → OK; all probes exit 1.
- **No staged files**, working tree clean.
- **Key files changed**:
  - `scripts/ci/check-openapi-breaking.py` — combinator descent + requestBody.required delta detection
  - `scripts/ci/test_check_openapi_breaking.py` — 8 new tests (47 total)
  - `scripts/ci/check-baseline-control.py` — new (baseline control via gh api)
  - `scripts/ci/test_check_baseline_control.py` — new (4 tests)
  - `.github/workflows/ci.yml` — wired baseline control into claims job
  - `docs/stability.md` — corrected gap list
  - `DECISIONS.md`, `docs/roadmap-notes.md` — updated
- **Remaining**: monitor CI run; no expected failures.

# progress — bprec1/bigdecimal-inventory

## Done

- **`docs/research/bigdecimal-inventory.md`** — comprehensive inventory of every BigDecimal precision boundary in the workspace. Covers 12 crates, ~1,620 BigDecimal mentions. Each site labelled exact, lossy by construction, or lossy by policy. Serde feature differences documented.
- **`docs/roadmap-notes.md`** — appended a dated line under the B-precision backlog entry (~line 454).
- **`DECISIONS.md`** — appended a section documenting key classification decisions.
- **PR not yet opened** — waiting for final verification of line numbers.

## Not done

- CI has not run (no cargo builds — host constraint).
- PR not opened (will open after line-number verification).
- `ROADMAP.md:283` left unticked (per brief: inventory half only).
- No code changes (per brief: docs only).
**Commit range:** b81b2a8..4666aab on `fre18a/openapi-breaking-check`

**Done:**
- AC-1: Baseline digest (`sha256`) + CI `run:` step + bump protocol in README.
- AC-2: Symmetric nullable rule (both directions, both spellings `nullable: true|false` and `type: [..., "null"]`).
- AC-3: 12 constraint keys (minLength, maxLength, minimum, maximum, exclusiveMinimum, exclusiveMaximum, pattern, enum, maxItems, minItems, uniqueItems, format) with two-direction tests.
- AC-4: Response status-code additions (INFO, non-breaking) and new required request body (BREAKING).
- AC-5: Named gaps in docstring and `docs/stability.md`.
- AC-6: Missing tests (nullable-reverse, optional→required, empty paths, unreadable, truncated JSON, key-reorder-only, version-only).
- Exit code: only BREAKING findings return 1, INFO-only findings return 0.
- Default-added-to-existing-param now flagged (was gap #2, now implemented).
- All 39 checker tests pass (34 + 5 new).
- All 73 discovery tests pass.
- Probe matrix: 20/20 exit codes match expectations.
- PR body updated, comment with probe matrix posted.
- Pushed: 3 new commits on `fre18a/openapi-breaking-check`.

**Not done:**
- **Do not merge** — left for reviewer.
- `ROADMAP.md:150` unticked (this is one clause of FRE-18, not the freeze cut).
- CI workflow will trigger automatically on push.

# progress — FRE-18c-fix

## Checkpoint (2026-11-22, ~35 min into 45-min run)

**Changes made:**
- P0-1: `_fetch_base_digest` now strips `GITHUB_WORKSPACE` prefix from absolute paths before building the API URL.
- P0-2: `main_with_args` now prints notice and exits 0 when `GITHUB_BASE_REF` is empty.
- CI: `claims` job step passes `--digest-file compat/openapi/baseline/weftdb.json.sha256` for a repo-relative path.
- P2: 5 new tests (URL construction for relative/absolute paths, no-base-ref exits 0, 404 exits 2, bad base64 exits 2).

**Verification:**
- 90/90 tests pass (was 85, 5 new).
- `check-openapi-breaking.py` exit 0.
- `check-action-pinning.py` exit 0.
- `--repo basic-automation/weftdb --base-ref main` → exit 2 (expected: no token, 404 — per brief).
- Empty env → exit 0 with "no base ref: baseline control not applicable".

**Commit ready:** `git commit -s` with message `[FRE-18C] Fix the baseline control path and the no-base-ref exit, and cover both (#843)`.
**Push:** `git push origin HEAD`.
**Not done:** `ROADMAP.md:163` stays unchecked per brief. No merge, no PR body edit.
