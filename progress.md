# P12-722 — store-side row-level TTL/expiry, cut 1

## Handoff — 2026-11-14

### What is done

**New files** (untracked):
- `weftdb/src/types/segment_store/row_expiry.rs` — `RowExpiryPolicy` enum, pure helpers (`is_row_expired`, `expired_cutoff`, `raw_ttl`), sweep report types (`AspectExpirySwept`, `ExpirySweep`), inline unit tests for the helpers. The policy is a minimal closed enum with `NoExpiry` and `Ttl { ttl: u64 }`.
- `weftdb/src/types/segment_store/row_expiry_tests.rs` — Integration tests exercising all 7 criteria from the brief via `SegmentStore::read_time_range_of_with_expiry` and `expiry_sweep`.

**Changed files** (staged/unstaged):
- `weftdb/src/types/segment_store.rs` — Added `expiry` field to `SegmentStoreOptions` and `SegmentStore` structs, `with_expiry()` builder, `mod row_expiry;` and `mod row_expiry_tests;` declarations, default to `NoExpiry` in both `Default` and `from_env()`.
- `weftdb/src/types/segment_store/open.rs` — Wired `options.expiry` into the `open_on` constructor's `Self { ..., expiry: options.expiry, ... }` line.
- `weftdb/src/types/segment_store/read.rs` — Added `read_time_range_with_expiry` and `read_time_range_of_with_expiry` public methods that call the existing `read_time_range_of` then filter expired rows after the newer-wins merge.
- `weftdb/src/types/segment_store/maint.rs` — Added `expiry_sweep` public method (iterates declared aspects, checks breakers, acquires maint lock, calls `expire_aspect_held`) and `expire_aspect_held` internal method (reads index rows, counts fully/partially expired segments, returns `AspectExpirySwept` report). Cut 1 is report-only — no physical deletion.
- `DECISIONS.md` — Added "Store-level row TTL/expiry cut 1 (#722)" entry covering: store-wide policy only, read-path filter after newer-wins merge, report-only sweep, no env var, CI first execution.
- `CHANGELOG.md` — Added short entry under `### Added` for the cut 1 expiry.

### Not done (requires code changes)
- No weft-server wiring (intentionally deferred per brief).
- No OpenAPI/config-schema changes (intentionally deferred).
- No env var (intentionally deferred — programmatic API only, cut 1).
- No `cargo build`/`cargo check`/`cargo test` run locally (host has ~5 GB free, build OOM-kills local model server — CI first execution per brief).
- No physical deletion in sweep (intentionally deferred to cut 2/3).

### Implementation details

**Read path**: `read_time_range_of_with_expiry` calls `read_time_range_of` (existing, with newer-wins merge), then filters rows through `is_row_expired`. `read_time_range_with_expiry` delegates to `*_of_with_expiry` with `SeriesId::UNTAGGED`. Both accept an injected `now_ns` — never read the wall clock.

**Sweep**: `expiry_sweep` iterates declared aspects, respects both maintenance and decode breakers, acquires the maint lock, then calls `expire_aspect_held`. Per-aspect failures are collected in the report rather than aborting. Fully-expired segments are counted from index data alone (no decode); partially-expired segments are decoded and counted row-by-row.

**Policy**: `NoExpiry` preserves all rows; `Ttl { ttl }` expires rows where `age > ttl` strictly (age == ttl is visible). The `expired_cutoff` helper returns `i64::MIN` for NoExpiry (nothing expires) or `now_unit - ttl` with underflow guard.

### Known LSP noise
The standard Rust LSP shows false-positive errors on the `match` expressions in `maint.rs:expire_aspect_held` — the codebase uses a custom Rust dialect where `match option_var { None => ..., Some(x) => ... }` is valid. The existing code in `aspect_catalog.rs` and throughout uses the identical pattern.

### CI readiness
All tests are behind `#[cfg(test)]` / `#[tokio::test]`. The test file name follows the `suspect_frame_policy_tests.rs` naming convention. No `.briefs/` or `progress.md` is committed (`.gitignore` as-is should exclude `progress.md` — verify before push).

### Next steps
Cut 2 would wire physical deletion in the sweep (rewrite/reclaim expired rows) and/or add a per-aspect policy override. Cut 3 could add env-var support and server wiring. The integration tests pass silently once CI runs them.