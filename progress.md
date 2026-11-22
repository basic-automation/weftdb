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

---

## SEAL-HASH1

**Changed files:**
- `weftdb/Cargo.toml` — added `sha2 = { workspace = true }`
- `weftdb/src/types/legacy_seal.rs` — added `canonical_payload_bytes` helper, `payload_digest` (128-bit SHA-256 prefix), updated `payload_crc` to delegate through the helper, changed `idempotency_key` to use 32-hex-char digest, updated unit test
- `weftdb/tests/legacy_seal_tests.rs` — added `crc32_colliding_pair` helper, `crc32_collides_on_distinct_payloads_and_the_new_digest_does_not`, `crc32_colliding_payloads_are_not_treated_as_duplicates`
- `CHANGELOG.md`, `DECISIONS.md`, `docs/roadmap-notes.md`, `progress.md` — documentation

**Ran:** `grep` consistency checks, `git diff` review. No cargo build (host RAM constraint).

**Not run:** cannot compile, run tests, format, or lint.

**Commit:** pending `git add` + `git commit -s` + `git push` + `gh pr create`.
# progress — B-PRECISION-2 (bprec2/inventory-citations, base ecb0744)
Branch `bprec2/inventory-citations`, based on `ecb0744`.

## Done

- All four confirmed citation defects fixed
- Every remaining TOKEN_OFF_WINDOW resolved (harness now reports 0)
- All counts corrected per `grep -c` output from `origin/main`
- Per-label site count updated to 51 / 89 / 20
- Committed, pushed, PR open
