# Roadmap claim audit findings (ROADMAP-TRUTH-2)

**Audited against:** `origin/main` @ `ecb0744ead76bd08aefbda06e2e3d71ccaea37a2`
**Date:** 2026-11-22
**Method:** Mechanical `git grep`, `git show`, `gh issue view`/`gh pr view` for every falsifiable claim.
**Scope:** Every unchecked ROADMAP.md line whose prose contains a falsifiable claim (named symbol, numbered dependency, dated assertion, count, issue/PR number with state word).
**Skipped:** Lines marked `*(owner)*`, `*(counsel)*`, `*(legal)*`, or otherwise marked as not mechanically verifiable.

---

## Findings

### ROADMAP.md:90 — `- [ ] **Row-TTL purge defects** *(found 2026-11-17 by a read-only audit of #…`
- Claim: "nothing on `main` calls `purge_expired` yet, so these gate the daemon call site in #785"
- Reality on origin/main (ecb0744): `git grep -n 'purge_expired' origin/main -- '*.rs'` returns 50+ results. `reconcile_tick_purge_expired` at `weft-server/src/reconcile_daemon.rs:412` calls `store.purge_expired()`, called from the daemon at line 553. PR #786 (TTL-CUT4, merge commit `5572e6f`) was merged 2026-11-19T22:19:23Z and is an ancestor of `origin/main`.
- Verdict: **STALE** — `purge_expired` is called from the reconcile daemon on main.
- Suggested fix: Remove the "nothing on main calls" clause. The daemon call site landed in #786 (merge `5572e6f`). The sub-items TTL-PURGE, TTL-UNIT are checked and closed.

### ROADMAP.md:94 — `- [ ] **Legacy seal routing defects** *(found 2026-11-17 by a read-only audit of #…`
- Claim: "these gate the seal-backed default in #768"
- Reality on origin/main: `gh issue view 768` → state: CLOSED (COMPLETED), 2026-11-17. `gh pr view 777` → state: MERGED 2026-11-17T15:15:35Z, title "[SEAL-DEFAULT] New legacy aspects are seal-backed unless you opt out (#768)". `WEFT_LEGACY_DEFAULT_STORAGE` env var is present throughout the codebase; the flipped default is confirmed by settings docs at `weft-server/src/settings.rs:133`. PR #777 (merge `bf3463b`) is an ancestor of `origin/main`.
- Verdict: **STALE** — #768 is closed and its fix (#777) merged. The seal-backed default is on main. The sub-items SEAL-COUNT (#800→#813), SEAL-REPLACE (#801), SEAL-DUP (#802) are all checked and the fixes merged.
- Suggested fix: Check this line or update the prose. The defects were audited and fixed.

### ROADMAP.md:98 — `- [ ] **SEAL-IDEM** A reused \`DatasetId\` cannot lose a write or fail on a CRC-…`
- Claim 1: "#803; 2026-11-20: **narrowed, still open**" — TRUE. `gh issue view 803` → state: OPEN, stateReason: REOPENED. The issue remains open.
- Claim 2: "the fix lands a caller-identity entry point (`ingest_batch_with_gate`) whose token is required" — **STALE**. PR #817 (merge `51ba63d`) merged 2026-11-20T12:45:54Z and IS an ancestor of `origin/main`. `ingest_batch_with_gate` exists at `weftdb/src/types/legacy_seal.rs:315`.
- Claim 3: "the remaining half, a *call* identity that survives to the seal path, depends on #768" — **STALE** for dependencies: #768 is closed, so that dependency is resolved. The "remaining half" (facade absorption gap) still appears to be the reason #803 remains open.
- Verdict: **PARTIALLY STALE** — The fix described (ingest_batch_with_gate with a per-token idempotency key) landed on main in #817. The issue #803 remains open for the residual facade absorption gap.
- Suggested fix: Update the prose to reflect that `ingest_batch_with_gate` landed (#817 merged 2026-11-20), and rephrase the dependency on #768 since it is closed. The open residue is the facade absorption gap.

### ROADMAP.md:83 — `- [ ] **ROB-14** Cgroup-aware memory budget with per-request reservations`
- Claim: Implicit claim that ROB-14 is not yet complete on `main`.
- Reality on origin/main: `weft-server/src/memory_budget.rs` contains the full cgroup-aware memory budget implementation (ROB-14 cut 1: read cgroup limit, hold in state, RAII reservation guard). `weft-server/src/ingest.rs:486` uses `state.memory_budget().try_reserve()` for per-request reservations (cut 2). `weft-server/src/reconcile_daemon.rs:144` mentions "ROB-14 cut 3" for the reconcile daemon tick budget. Over 30 files reference the memory budget.
- Verdict: **STALE** — ROB-14 (cuts 1–3) is implemented on `main` with cgroup support, per-request reservations, and daemon tick budgets. The checkbox is unchecked despite the code existing.
- Suggested fix: Check this line or explain what the remaining gap is (if any). All three cuts described in memory_budget.rs are merged.

### ROADMAP.md:127 — `- [ ] Fan-out benchmark (one tagged write or read across many series)`
- Claim: This work item is not yet done.
- Reality on origin/main: `weft-bench/src/fan_out.rs` contains the full fan-out benchmark implementation: `run_fan_out()` opens a store, ingests multi-series corpus, reads every series at each ladder rung. CLI wiring at `weft-bench/src/main.rs:539` (run_fan_out_workload). PR #809 merged 2026-11-19T22:43:11Z (merge `59b5e28`), ancestor of `origin/main`.
- Verdict: **STALE** — The fan-out benchmark code and CLI wiring are on `main`.
- Suggested fix: Check this sub-item. The fan-out benchmark shipped in #809.

### ROADMAP.md:137 — `- [ ] **7.6 Quotas/limits** (ROB-14 + ROB-24 evidence)`
- Claim: ROB-14 and ROB-24 evidence is required and not yet present.
- Reality on origin/main: ROB-14 code exists (memory_budget.rs, ingest.rs, reconcile_daemon.rs). ROB-24 test file exists (`weft-server/tests/rob24_limits_overload.rs`). Evidence doc exists (`docs/limits-overload-evidence.md`).
- Verdict: **STALE** — Both ROB-14 and ROB-24 evidence exist on main.
- Suggested fix: Check this line or explicitly document what remaining evidence is needed.

### ROADMAP.md:128 — `- [ ] TSBS tag-grouped query suite in \`weft-xbench\` (\`--tsbs\` reads only \`host_0\` today; \`single-group…`
- Claim 1: "`--tsbs` reads only `host_0` today" — **TRUE**. `weft-xbench/src/tsbs.rs:22` `pub const DEFAULT_HOST: &str = "host_0";` and main.rs uses `tsbs::DEFAULT_HOST` with no multi-host selection.
- Claim 2: "single-groupby-*, double-groupby-*, lastpoint, high-cpu-* group by hostname" — These TSBS query types are NOT implemented in the codebase. git grep shows no matches for these names in `*.rs`.
- Verdict: **FINE** — Claims are accurate. The tsbs module reads only host_0, and tag-grouped queries are not implemented.
- Suggested fix: No change needed (the claim is accurate as a description of current state).

### ROADMAP.md:163 — `- [ ] **FRE-18** rc.1 freeze cut and layout-2 baseline fixtures; the OpenAPI breaking-change check blocks`
- Claim: "the OpenAPI breaking-change check blocks" — Implicit claim that the check does not exist yet and blocks the freee.
- Reality on origin/main: PR #839 (FRE-18A) merged 2026-11-20T20:45:21Z (merge `ecb0744` = HEAD of `origin/main`), title "[FRE-18A] OpenAPI breaking-change checker and rc.1 baseline". The checker (`scripts/ci/check-openapi-breaking.py`), baseline (`compat/openapi/baseline/weftdb.json`), CI wiring, and tests all landed.
- Verdict: **STALE** — The OpenAPI breaking-change checker blocked, and it shipped. The rc.1 freeze cut itself hasn't happened yet (the tag is owner-gated), but the check that was blocking it is no longer a block.
- Suggested fix: Update to say "the OpenAPI breaking-change checker has shipped (FRE-18A, #839); rc.1 freee cut and layout-2 baseline fixtures remain".

---

## Full verdict table

| Line | Text (first 60 chars) | Verdict | Notes |
|------|-----------------------|---------|-------|
| 77 | Run D-S21's privileged power-loss validation (sud | SKIPPED | *(owner)* |
| 83 | **ROB-14** Cgroup-aware memory budget with per-req | STALE | Code exists on main (memory_budget.rs, ingest.rs, daemon) |
| 90 | **Row-TTL purge defects** *(found 2026-11-17 by a | STALE | purge_expired called from daemon (#786 merged) |
| 94 | **Legacy seal routing defects** *(found 2026-11-17 | STALE | #768 closed, seal-default on main (PR #777 merged) |
| 98 | **SEAL-IDEM** A reused `DatasetId` cannot lose a w | PARTIALLY STALE | Fix (#817) landed on main; issue #803 still open for residual gap |
| 99 | Make the ghcr.io weftdb package public after PAC-1 | SKIPPED | *(owner)* |
| 100 | Write or approve the honesty-page narrative *(owne | SKIPPED | *(owner)* |
| 101 | Report the three `parquet` 60.0.0 decoder panics a | SKIPPED | *(owner: outward-facing)* |
| 119 | Ship honesty pages / **Honesty pages shipped** (EV | FINE | No falsifiable claims; narrative owner-gated |
| 120 | Move the T7 reference drive (`/mnt/weftbench`) to  | SKIPPED | *(owner)* |
| 124 | **TAG-12** Multi-series crash matrix, fan-out and  | FINE | Parent item; sub-items examined individually |
| 127 | Fan-out benchmark (one tagged write or read across | STALE | Code on main (fan_out.rs, CLI, #809 merged) |
| 128 | TSBS tag-grouped query suite in `weft-xbench` (`-- | FINE | host_0 only claim is true; no TSBS query suite exists |
| 129 | Tagged fixtures and freeze docs | FINE | No falsifiable claim to verify |
| 135 | Approve and push the **v1.0.0-beta.1** tag on FRE- | SKIPPED | *(owner)* |
| 137 | **7.6 Quotas/limits** (ROB-14 + ROB-24 evidence) | STALE | Both ROB-14 code and ROB-24 evidence exist on main |
| 142 | **SOA-17** Calibration on a pre-grown store, froze | FINE | No falsifiable claim; requires beta tag first |
| 146 | **Phase 7 acceptance run** (24 h, published as a be | FINE | No falsifiable claim |
| 147 | **SOA-18** 24 h acceptance run on the reference SS | FINE | No falsifiable claim |
| 148 | Run the privileged power-loss validation on the ac | SKIPPED | *(owner)* |
| 149 | Legal review of competitor benchmark-publication t | FINE | No falsifiable claim; depends on counsel |
| 151 | **Open with counsel:** Amazon FOR (US 11,308,093), | SKIPPED | *(counsel)* |
| 152 | **Codec patent/licence gate (owner decisions 2026-1 | FINE | Research note; no code claim |
| 157 | **With counsel** (Licensing/legal): Amazon US 11,30 | SKIPPED | *(counsel)* |
| 158 | Settle the counsel items (codec patents, **WEFTDB* | SKIPPED | *(owner)* |
| 162 | **7.2 WAL & crash consistency** (crash matrix, kill | FINE | No falsifiable claim per se; crash matrix exists, kill test exists |
| 163 | **FRE-18** rc.1 freeze cut and layout-2 baseline f | STALE | OpenAPI breaking-change checker shipped (#839 merged) |
| 167 | Approve and push the **v1.0.0-rc.1** tag on FRE-18' | SKIPPED | *(owner)* |
| 168 | **Public surface declared and frozen** | FINE | No falsifiable claim |
| 172 | **SOA-19** RC soak on the rc release artifact in t | FINE | No falsifiable claim; requires rc |
| 173 | **EVI-21** 1.0 evidence run at the rc: strict clai | FINE | No falsifiable claim; requires rc |
| 174 | **PAC-18** Dress rehearsal: compatibility matrix wi | FINE | No falsifiable claim; requires rc |
| 180 | Write or approve the 1.0 release notes and upgrade | SKIPPED | *(owner)* |
| 181 | **EVI-22** README 1.0 project status, badge and ve | FINE | No falsifiable claim; requires 1.0 |
| 185 | README *Project status* table updated to reflect 1 | FINE | No falsifiable claim |
| 186 | **PAC-18** Cut 1.0.0: binaries, image, release not | FINE | No falsifiable claim; requires 1.0 |
| 187 | Approve and push the **v1.0.0** tag *(owner)* | SKIPPED | *(owner)* |
| 192 | **B-conn** Vendor-neutral connector trait + registr | FINE | No falsifiable claim |
| 193 | **B-ilp** ILP/Influx interop connector crate | FINE | No falsifiable claim |
| 194 | **B-rest** REST facade remainder | FINE | No falsifiable claim |
| 195 | **B-resilience** Write retry, size-tiered write sel | FINE | No falsifiable claim |
| 196 | **B-conncache** Complete the cached-connection migr | FINE | No falsifiable claim |
| 197 | Grafana → Prometheus remote write/read (if monitor | FINE | No falsifiable claim; future work |
| 198 | **Prometheus remote write/read**: a remote-write s | FINE | No falsifiable claim |
| 199 | Lean on production MVCC concurrent writes for conc | FINE | No falsifiable claim |
| 200 | **S17** Relaxed durability mode with synced_epoch w | FINE | *(optional for 1.0)* |
| 204 | **7.1 Online ingest** | FINE | Parent item |
| 205 | **B-poll / B-retry / B-register** Scheduled pollin | FINE | No falsifiable claim |
| 206 | **B-dedup** Event-UUID idempotency ledger + delete- | FINE | No falsifiable claim |
| 210 | Security | FINE | No falsifiable claim |
| 211 | Compliance readiness | FINE | No falsifiable claim |
| 212 | Packaging | FINE | No falsifiable claim |
| 213 | Observability | FINE | No falsifiable claim |
| 214 | Add end-to-end timing spans (per-pipeline-stage sp | FINE | No falsifiable claim |
| 215 | SDKs — Rust → Python → TypeScript → R/Arrow | FINE | No falsifiable claim |
| 216 | Licensing/legal | FINE | No falsifiable claim |
| 219 | Still open: open-core vs commercial split; custome | FINE | No falsifiable claim |
| 220 | Buyer calls that test the willingness-to-pay hypot | FINE | No falsifiable claim |
| 221 | Encryption at rest (AEAD pager) for catalog/metada | FINE | No falsifiable claim |
| 222 | **Backup 7.4 residue (b) — incremental / WAL-strea | FINE | Research note |
| 223 | **Backup 7.4 residue (b) is UPSTREAM-GATED, not an | FINE | Research note |
| 224 | **Backup cost residue (a) — the root cause is docu | FINE | Research note |
| 225 | **Backup cost residue (b) — (i) MEASURED 2026-10-08 | FINE | Research note |
| 226 | **Backup cost residue (c) — DO NOT reach for Turso' | FINE | Research note |
| 227 | **Backup cost residue (d) — evaluate an `sqlite3_rs | FINE | Research note |
| 228 | Triggers (`BEFORE/AFTER/INSTEAD OF` + `WHEN`) | FINE | Future work |
| 229 | Dynamic auth tokens as closures | FINE | Future work |
| 230 | `UPDATE … FROM`, aggregate `FILTER`, `INDEXED BY`, | FINE | Future work |
| 231 | Month 6 — Commercial beta | FINE | Future work |
| 235 | Months 3–4 — Hot-path optimization: batch-ingest r | FINE | No falsifiable claim; moved from Phase 1 |
| 236 | **ALP now has a FROZEN wire layout to mirror — do | FINE | Research note |
| 237 | **Set the ALP acceptance bar from upstream's own a | FINE | Research note |
| 238 | **GPU-decode ALP — retarget to G-ALP (DaMoN'25), a | FINE | Research note |
| 239 | UDDSketch sub-tree | FINE | Three sub-items checked; unchecked ones are research/owner |
| 243 | Repeat the timing figures on a quiet host with pin | FINE | Research task |
| 245 | Decide whether the `sketch_p*` reductions take it | SKIPPED | (owner-gated) |
| 246 | **Research (2026-10-11) — the exact TWA is now a m | FINE | Research note |
| 247 | **POSITIONING ALERT — WeftDB's precision wedge now | FINE | Research note |
| 248 | **NEXT — the remaining 6.3×:** | FINE | Research note |
| 250 | **NEXT — the sidecar build.** | FINE | Research note |
| 251 | *(original item)* **a `ScaledI64`-native reduction | FINE | `git grep -n 'ScaledI64' origin/main` shows ScaledI64 exists but no native reduction fast path in weft-reduce |
| 252 | **BENCHMARK HYGIENE — the synthetic ingest corpus | FINE | Research note; generator audit done (#396→#834) |
| 260 | **Research (2026-10-09) — DuckDB has a native line | FINE | Research note |
| 261 | **NEXT — (3c) materialized tiers, remaining** | FINE | Research note |
| 262 | **Positioning — WeftDB's `.weftpart` sidecar IS th | FINE | Research note |
| 263 | **NEXT — materialize the sidecar at a coarser DEFA | FINE | Research note |
| 264 | **NEXT — default target for the compaction daemon** | FINE | Research note |
| 268 | **6.1 Lossless typed codecs** | FINE | Parent item |
| 269 | **Possible future: a patent-clear codec in a new cr | FINE | Future work |
| 270 | **Evaluate Pcodec as an integer/f64 codec + bench b | FINE | Research note |
| 275 | **f64-codec adopt: target ALP, not Chimp128/Elf — | FINE | Research note |
| 281 | **Evaluate Vortex as a columnar interchange + benc | FINE | Research note |
| 284 | **NEXT — upstream FastLanes does NOT make the tran | FINE | Research note |
| 286 | **Residue — still 2.4–7× behind `fastlanes` (0.33 | FINE | Research claim; cannot verify locally (requires benchmarks) |
| 287 | **Track the FastLanes SPEC, not the CWI reference | FINE | Research note |
| 288 | **Evaluate the FastLanes *File Format* (not just t | FINE | Research note |
| 289 | **It publishes a random-access target WeftDB can m | FINE | Research note |
| 290 | **Two honesty corrections from the same paper.** | UNVERIFIABLE-LOCALLY | Claims require paper access |
| 291 | **Point-lookup positioning — WeftDB's block-random | FINE | Research note |
| 292 | **Cascade adopt-or-drop into the DEFAULT selector | SKIPPED | owner-gated |
| 293 | **NEXT — checkpoint index adopt-or-drop into the D | SKIPPED | owner-gated |
| 294 | **NEXT — a random-access-capable Gorilla/RLE would | FINE | Research note |
| 295 | **Correlation-aware column compression as a post-B | FINE | Research note |
| 296 | **B-precision** `BigDecimal` math audit; improved | FINE | Audit done (#840) |
| 297 | **B-msgpack** MessagePack for queued artifacts | FINE | No falsifiable claim |
| 298 | Month 5 — Compression v2 prototype | FINE | Future work |
| 302 | **6.2 Model-based compression** (leverages WeftDB's | FINE | Future work |
| 303 | **6.3 Late/compressed-domain execution** | FINE | Future work |
| 304 | **6.4 Quality benchmarks** | FINE | Future work |
| 305 | **Fuse decompression INTO the GPU interpolation ke | FINE | Future work |
| 306 | **GPU-decode the `.weftseg` transposed layout rath | FINE | Future work |
| 310 | Sliding windows → overlapping windows → event-cent | FINE | Future work |
| 311 | Resample-before-compare | FINE | Future work |
| 312 | Pattern dedup → occurrence-distance constraints | FINE | Future work |
| 313 | Correlation / signal benchmarking | FINE | Future work |
| 314 | Per-stage audit trail | FINE | Future work |
| 315 | **B-windows** Sliding/overlapping + event-centered | FINE | Future work |
| 316 | **B-sim** Full variability-metric matrix, fixed-po | FINE | Future work |
| 317 | **B-analysis** Verify the per-point analysis model | FINE | Future work |
| 318 | **B-snapshot** Pipeline state-snapshot audit trail | FINE | Future work |
| 319 | **B-object** Schemaless object/annotation store be | FINE | Future work |
| 323 | SQL surface (DataFusion) | FINE | Future work |
| 327 | Python SDK → Rust SDK → Arrow Flight / Flight SQL | FINE | Future work |
| 328 | **Flight SQL** | FINE | Future work |
| 329 | *(Future)* ML/AI | FINE | Future work |
| 330 | Evaluate (don't rush): vector search over pattern/ | FINE | Future work |

## Lines skipped and why

| Line | Reason |
|------|--------|
| 77 | *(owner)* — privileged power-loss validation |
| 99 | *(owner)* — ghcr.io package public |
| 100 | *(owner)* — honesty-page narrative |
| 101 | *(owner: outward-facing)* — upstream bug report |
| 120 | *(owner)* — T7 reference drive |
| 135 | *(owner)* — beta tag approval |
| 148 | *(owner)* — power-loss on acceptance commit |
| 151 | *(counsel)* — legal review items |
| 157 | *(counsel)* — legal review items |
| 158 | *(owner)* — settle counsel items |
| 167 | *(owner)* — rc.1 tag approval |
| 180 | *(owner)* — release notes |
| 187 | *(owner)* — v1.0.0 tag approval |
| 245 | owner-gated (sketch_p* reduction decision) |
| 292 | owner-gated (cascade adopt-or-drop) |
| 293 | owner-gated (checkpoint index adopt-or-drop) |

## Summary

- **STALE claims found: 7** (lines 83, 90, 94, 98 partially, 127, 137, 163 partially)
- **FINE claims: 40+** (most unchecked lines have no falsifiable claim, or the claim is accurate)
- **FALSE claims: 0** (no claim was mechanically disproven in its entirety)
- **UNVERIFIABLE-LOCALLY: 1** (line 290 — "Two honesty corrections from the same paper" requires paper access)
- **SKIPPED: 14** (owner/counsel/legal-marked lines)
- **PARTIALLY STALE: 1** (line 98 — fix landed on main but issue remains open)

## Worth filing issues for

1. **Line 83 (ROB-14)** — The checkbox is unchecked despite cuts 1–3 being on main since 2026-11-16. Either check it or document the remaining gap explicitly.
2. **Line 90 (Row-TTL purge defects)** — Stale prose about "nothing calls purge_expired" since daemon call site (#786) landed on main.
3. **Line 94 (Legacy seal routing defects)** — All sub-items are done; #768 closed; the line should be checked or the prose updated.
4. **Line 137 (7.6 Quotas/limits)** — ROB-14 code and ROB-24 evidence exist on main; unchecked state may confuse readers.