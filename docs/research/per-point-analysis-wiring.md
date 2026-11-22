# Is the per-point analysis model wired end-to-end? (B-analysis, #362)

Audited 2026-11-22 against `origin/main`. Read-only: no build, no behaviour change. Every
claim below is a call-site count or a definition search, and each carries the file and line.

## Verdict

**No.** The model exists in three pieces that do not connect: a per-point `Analysis` payload
carried on batched measurements, a batch transform that fills part of it, and a query-time
`analyze_point`/`analyze_range` pair that ignores both. Four separate breaks are each
sufficient to make "wired end-to-end" false, and the roadmap line stays unchecked for that
reason: the verification is finished, the answer is negative.

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

`Batch::process()` has exactly four call sites in the workspace:

- `weft-orchestration/src/lib.rs:795` — the analytics pipeline, in memory, on batches it
  fetched for one pass (`// Process batches in parallel (batch.process() is synchronous)`);
- `weft-orchestration/src/debug_batch_test.rs:62`;
- `weftdb/tests/db_tests.rs:1133,1177,1230`.

`BatchCollection::process_batches()` has **zero** call sites. No ingest path, no server
handler and no client call reaches either.

## Break 2 — it is never stored, and every read clears it

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

- #862: per-point `Analysis` is not persisted and reads clear it (`mod.rs:22-24`,
  `batches/mod.rs:82,138`), so `weft-orchestration`'s `relative()` reads are `Some` only
  inside the pass that computed them.
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
