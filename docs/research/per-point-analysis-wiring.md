# Is the per-point analysis model wired end-to-end? (B-analysis, #362)

Audited 2026-11-22 against `origin/main`. Read-only: no build, no behaviour change. Every
claim below is a call-site count or a definition search, and each carries the file and line.

## Verdict

**No** — but the reason is narrower than this audit first said, and one of its four breaks
has been withdrawn. A correction pass (2026-11-22, after the follow-up issues were filed)
found that **Break 2 is false**: the `analysis` field *is* persisted and read back, and the
producer *does* run on a live path. What survives are three real breaks — the batch-collection
producer is dead and computes less than the live path, the trend half has no producer at all,
and neither half is reachable from the shipped server. The roadmap line stays unchecked: the
verification is finished and the answer is still negative, for fewer reasons than first
claimed. See "Correction (2026-11-22)" below, which supersedes Break 1's call-site list and
all of Break 2.

## The three pieces

1. **The payload.** `weftdb/src/types/batches/batched_measurements/analysis.rs:5-9` —
   `pub struct Analysis { trend: Option<Vec<Trend>>, relative: Option<Relative> }`, held as
   `analysis: Option<Analysis>` on `BatchedMeasurement`
   (`weftdb/src/types/batches/batched_measurements/mod.rs:13-19`).
2. **The producer.** `Batch::relative_analysis()`
   (`weftdb/src/types/batches/batch.rs:355`) sets it per measurement at `batch.rs:400-402`,
   called from `Batch::process()` (`weftdb/src/types/batches/batch.rs:447-453`), which also
   runs `level_transform`, `transpose_origin` and `simplify_transformation`. The batch
   collection offers `BatchCollection::process_batches()`
   (`weftdb/src/types/batches/mod.rs:176`).
3. **The query-time pair.** `DatabaseOutputs::analyze_point` and `analyze_range`
   (`weftdb/src/types/database/traits/outputs.rs:39,41`, implemented at
   `weftdb/src/types/database/outputs.rs:293,433`) interpolate a point or a stream of points
   at a resolution, caching an `AnalysisResult` (`weftdb/src/types/cache.rs:14`, key built at
   `outputs.rs:298-299`, stored at `outputs.rs:422-423`). It is called "point analysis" in
   the source, but it never touches `Analysis`: it returns `splimes::Point` values.

`weft-orchestration` is the only consumer that reads `Analysis`: the pipeline pulls relatives
back out of the measurements it is working on at
`weft-orchestration/src/lib.rs:872,887`
(`measurement.analysis()?.relative()`), and its detectors
(`weft-orchestration/src/detectors.rs`) turn those into events.

## Break 1 — nothing in the product produces it

<!-- PARTLY WITHDRAWN 2026-11-22: the entry point `build_processed_batch_queue` is called
from the production `Pipeline` at `weft-orchestration/src/pipeline.rs:747,849`, so the call
site at `weft-orchestration/src/lib.rs:795` is not test-only. What remains true is that the
*batch-collection* entry point, `BatchCollection::process_batches()`, is dead. -->

<!-- SUPERSEDED: the list below was produced from a `head -20`-truncated grep and missed
call sites; the corrected list is in "Correction (2026-11-22)". Kept as written because the
follow-up issues quote it. -->
`Batch::process()` has exactly four call sites in the workspace:

- `weft-orchestration/src/lib.rs:795` — the analytics pipeline, in memory, on batches it
  fetched for one pass (`// Process batches in parallel (batch.process() is synchronous)`);
- `weft-orchestration/src/debug_batch_test.rs:62`;
- `weftdb/tests/db_tests.rs:1133,1177,1230`.

`BatchCollection::process_batches()` has **zero** call sites. No ingest path, no server
handler and no client call reaches either.

## Break 2 — it is never stored, and every read clears it

<!-- WITHDRAWN 2026-11-22: false. `analysis` is inside the `measurements` JSON that
`weftdb/src/types/database/inputs.rs` writes and `outputs.rs` reads back with
`serde_json::from_str::<Vec<BatchedMeasurement>>`, and `BatchedMeasurement` has no
`#[serde(skip)]`. Read on; the correction section states the verified facts. -->
`BatchedMeasurement::new` fixes `analysis: None`
(`weftdb/src/types/batches/batched_measurements/mod.rs:22-24`), and both read-side batch
constructors build every measurement through it:
`weftdb/src/types/batches/mod.rs:82` (`BatchCollection::new`) and `:138`
(`new_with_limited_data`) — `window.iter().map(|p| BatchedMeasurement::new(p.clone()))`.

`BatchedMeasurement` appears in five files under `weftdb/src`: `types/batches/mod.rs`,
`types/batches/batched_measurements/mod.rs`, `types/batches/batch.rs`,
`types/database/outputs.rs`, `types/mod.rs`. None of them is on the write path, and the
segment layout has no trend or relative column, so a re-read cannot restore what a previous
pass computed. The practical consequence is that
`weft-orchestration/src/lib.rs:872,887` sees `Some` only for batches that
`batch.process()` enriched earlier in the same pass: an analysis run that reads a batch back,
or a second pass over stored data, silently sees `None` and falls through the `?`.

## Break 3 — there is no product surface for either half

`weft-server` contains no occurrence of "analysis" anywhere under `src`, so neither the
per-point `Analysis` nor `analyze_point`/`analyze_range` is reachable over HTTP. Those two
methods have no caller outside `weftdb`: `weftdb/tests/new_api_tests.rs`
(`:60,68,147,150,153,217,219,221,244,276,281,864,899`),
`weftdb/tests/new_api_stress_tests.rs` (`:49,93,160,245,296,341,360`) and
`weftdb/benches/new_api_benchmarks.rs:118,140`. The TUI's single "analysis" token is an
unrelated field name (`weft-tui/src/app.rs:193`, `view_resolution`).

`weft-orchestration` itself is depended on by `weft-bench` only
(`weft-bench/Cargo.toml:36`, the analytics-pipeline workload of #145). `weft-server`'s
`Cargo.toml` names it only inside a feature-unification comment
(`weft-server/Cargo.toml:25`), and `weftdb/Cargo.toml` does not name it at all: the two
`weft_orchestration` mentions in `weftdb/tests` are prose in doc comments
(`new_api_tests.rs:372,520`, `db_tests.rs:469`).

## Break 4 — the trend half is dead code

There is no `trend_analysis` definition in the workspace. The only trace is a commented-out
call, `weftdb/src/types/batches/batch.rs:303-304`:

```
// Skip the expensive O(n²) trend_analysis since we only need consecutive slopes
// self.trend_analysis()?;
```

So `Analysis::trend` is never written by anything, and `trend()`, `get_trend`, `get_slope`
and `set_trend` (`analysis.rs:17-33`) have no callers outside `analysis.rs` itself. The
`Vec<Trend>` half of the model is API surface with no producer and no consumer.

## Correction (2026-11-22)

Three claims above are wrong, and two of them were carried into #862 and #863. What a
re-check against `origin/main` (`fb881ee`) shows, with the command that shows it:

1. **`analysis` is persisted, and a re-read restores it.** `BatchedMeasurement` is
   `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]` over five private fields,
   the last of which is `analysis: Option<Analysis>`
   (`weftdb/src/types/batches/batched_measurements/mod.rs:11-19`), reached through
   `analysis()`/`set_analysis()`. There is **no serde attribute anywhere in
   `weftdb/src/types/batches/` that would drop it**: `rg -n '#\[serde' weftdb/src/types/batches/`
   returns exactly one line, `weftdb/src/types/batches/batch.rs:65`, a
   `skip_serializing_if = "Option::is_none"`. The original audit's "`#[serde(skip)]`" at
   `mod.rs:14-15` does not exist in any revision. The processed-batch write is
   `serde_json::to_string(&batch.measurements)` at
   `weftdb/src/types/database/inputs.rs:993` (inside `insert_processed_batch`, `inputs.rs:990`,
   whose result is written to the `measurements` column of the aspect's processed-batches
   `batches` table at `inputs.rs:1001-1008`; `move_batches_to_processed` at `inputs.rs:1274`
   and `bulk_insert_processed_batches` at `inputs.rs:1193` route through it), and the read is
   `serde_json::from_str::<Vec<BatchedMeasurement>>(&measurements_json_str)` at
   `weftdb/src/types/database/outputs.rs:1721`, reached from `get_processed_batch`
   (`outputs.rs:709`) and `get_processed_batches` (`outputs.rs:753`). So Break 2's "never stored" and "every read
   clears it" are both false. The `measurements` column *is* where the numbers live.
2. **The producer runs on a live path.** `build_processed_batch_queue`
   (`weft-orchestration/src/lib.rs:773`, which is where the `batch.process()` call at
   `lib.rs:795` lives) is called from the production `Pipeline` at
   `weft-orchestration/src/pipeline.rs:747` and `:849`; `Pipeline` is re-exported at
   `weft-orchestration/src/lib.rs:457` and `weft-bench` depends on the crate
   (`weft-bench/Cargo.toml:36`), as does the crate's own doctest (`lib.rs:267,279`). So the
   call site this document listed as "the analytics pipeline, in memory" is not test-only —
   it is the product's analytics path. What *is* dead is `BatchCollection::process_batches()`.
   The first version also under-counted: the grep was piped through `head -20`.
3. **`trend_analysis` does not exist**, so the commented-out call at
   `weftdb/src/types/batches/batch.rs:303-304` is a comment above an unrelated function, not
   a disabled step. Break 4's conclusion stands; its wording did not.

What survives, restated as the real defect list (and what #862 now says):

- `BatchCollection::process_batches()` (`weftdb/src/types/batches/mod.rs:176`) has zero call
  sites, and the body it would run omits `relative_analysis()` — so the dead entry point
  computes *less* than the live one, and a caller switching to it would silently lose
  relatives. That is #862.
- `weft-orchestration/src/lib.rs:795` discards the result: `let _ = batch.process();`, so a
  failed transform is thrown away, not logged. The list of steps it can fail through is
  `level_transform` → `relative_analysis` → `transpose_origin` → `simplify_transformation`
  (`weftdb/src/types/batches/batch.rs:447-453`) — #862.
- `weft-orchestration/src/lib.rs:872,887` read `measurement.analysis()?.relative()`, so the
  detectors do depend on the field being carried through, and on the same pass having
  produced it: a persisted `analysis` is only as fresh as the run that wrote it.
- `Analysis::trend` still has no producer anywhere: `set_trend`/`set_trend_from`
  (`weftdb/src/types/batches/batched_measurements/analysis.rs`) are not called by production
  code, so persisted `trend` is always absent. That is #863, unchanged.
- The pipeline's pattern-detection fallback builds relatives from **raw maxima** rather than
  the range-normalised maths `relative_analysis` uses, on points the producer has not seen.
  Filed as #893.
- Neither half reaches the shipped product: `weft-server` has no `weft-orchestration`
  dependency, and the public `analyze_point`/`analyze_range` pair is not HTTP-reachable —
  Break 3 is correct as written.

## What "wired" would take, in rough cost order


1. **Decide which half survives.** Deleting the trend half is a breaking public-API change
   and the freeze (#238) is open, so it needs the same owner sign-off as adding one. Keeping
   it means restoring `trend_analysis` behind a cost gate — the comment says it was dropped
   for `O(n²)`, which is a claim worth re-measuring rather than accepting.
2. **Make the producer reachable.** Call `process()` (or the relative step alone) on the
   path that serves analysis, or in `BatchCollection::new`, and decide the cost: relative
   analysis is one pass over the batch plus a max/min scan, so recomputing on read is
   cheaper than storing it would be.
3. **Then persist, or recompute.** Persisting means a column or batch-metadata field in the
   segment layout, which is a format change and a `layout_version` question; recomputing
   means no new format and no stale values. The audit's lean is recompute, because the
   numbers are a pure function of the points in the batch.
4. **Then give it a surface**, if the product wants one: `analyze_point`/`analyze_range` are
   already public `weftdb` API, so the open question is whether the HTTP API grows a route —
   a public-surface decision that belongs to #238, not to this audit.

## Follow-ups filed

- #862: first written from the withdrawn Break 2 ("not persisted, reads clear it"), now
  re-titled to what was actually verified: `BatchCollection::process_batches` is dead and
  skips the relative step the live path performs.
- #893: `build_patterns_queue`'s fallback relatives use raw maxima where
  `relative_analysis` normalises by the batch range.
- #863: the `Analysis::trend` half has no producer; `trend_analysis` exists only as the
  commented-out call at `batch.rs:303-304`.

The roadmap line `B-analysis` stays unchecked and now states this verdict inline.

## How this was verified

`rg` call-site and definition sweeps over `**/*.rs` with `--glob '!target'` (call sites of
`Batch::process`, `process_batches`, `relative_analysis`, `trend_analysis`, `set_analysis`,
`set_trend`, `analyze_point`, `analyze_range`, `AnalysisResult`, `BatchedMeasurement`);
per-crate `Cargo.toml` dependency scan for `weft-orchestration`; a whole-tree search for
"analysis" under `weft-server/src`. No compilation was run, so this records structure, not
behaviour: nothing here shows a wrong answer from `analyze_point`, only that no product path
calls it.
