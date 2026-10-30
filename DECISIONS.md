# Decisions

Judgement calls made while working the 1.0 buildout, where the instruction set was silent or
self-conflicting and the choice is not visible in the diff. Recorded so the next session does not
re-litigate them, and so a reader of a ticked roadmap item can see what it was decided to mean.

## 2026-10-29 — EVI-15: where the first cross-engine report ran, and what it may claim

**Ambiguous:** EVI-15 (#244) is "First cross-engine report". Decision 18 fixes the matrix (WeftDB
stored, in-memory and `f64` against DuckDB `DOUBLE` and `DECIMAL`, on MCAR, block missingness and
UCI, at three scales), and decision 19 recommends a priced cloud NVMe instance for the report.
Nothing says what to do when a run is disturbed, how many runs make a report, or where the report
lives.

**Chosen:**

- **The reference SSD, not cloud.** At the operator's direction the report ran on `/mnt/weftbench`
  on the project machine. It has no hourly-cost field, so the report makes no cost-per-query claim;
  a priced rerun is a later report, not a condition of this one.
- **The report is committed** at `bench/reports/evi-15/`: `xbench.json`, the rendered `report.md`
  and `report.html`, a README with host, commit, input checksum and reproduction, and
  `scripts/xbench/fetch-uci.py` to fetch the UCI zip against its SHA-256. The UCI data is not
  redistributed.
- **A disturbed run is re-run whole and disclosed, not patched.** The first run's
  `mcar-medium` / `weftdb-stored-f64` load took 5.3, 61 and 140 s for a load that took 5.2 s
  ± 0.1 on the identical arm beside it; with three load reps the stall was the median. The whole
  matrix was re-run once with 5 load reps, and the first run is kept beside the report as
  `discarded-run-1.json`.
- **What may be claimed is what two runs agree on.** Comparing the runs showed that the within-run
  bootstrap intervals understate run-to-run variation: 53 of 144 rows had disjoint p50 intervals
  between runs. 79 of 90 verdicts agreed, none crossed between win and loss, and the 11 that changed
  all had margins of 1.4× or less. `HONESTY.md` quotes only verdicts that held in both runs, and says
  that WeftDB's wins hold only at `tiny` and `small` scale.
- **Renderer fix.** The "not tested" list still named downsampling as WeftDB-only; EVI-14 added a
  cross-engine downsample, so it now names gap fill and point and range reads only.

**Rejected:**

- Reporting the first run with the stalled row dropped or replaced: choosing which reps count after
  seeing them is the cherry-picking the honesty section exists to prevent.
- Widening the renderer's intervals to cover between-run variation in this slice: that needs
  several independent runs per report, which is fair-protocol depth (roadmap 1.1), not a renderer
  change.
- Waiting for cloud hardware: the operator chose this host, and the report says what it lacks.

## 2026-10-29 — EVI-14: what the second DuckDB slice adds, and the arms it changes

**Ambiguous:** EVI-14 (#293) is "DuckDB adapter II: downsample, storage bytes, DECIMAL arm, nightly
smoke". EVI-13's entry left it a like-for-like load arm. Nothing says which reductions the
downsample covers, what "storage bytes" measures, or what a `DECIMAL` arm computes in.

**Chosen:**

1. **Downsample is count, min, max and avg per epoch-aligned bucket:** minute buckets for the
   per-second synthetic data, hour buckets for UCI's per-minute data. That is `weft_reduce`'s grid.
   WeftDB reduces from the store (`SegmentStore::downsample_range`) and in memory
   (`weft_reduce::reduce`, the reference). DuckDB runs a `GROUP BY (t // width) * width`. Every row
   counts, duplicates included, in both engines. The gate needs identical buckets with identical
   counts, and min, max and avg within the 1e-9 relative tolerance.
2. **Storage bytes are whole files after a load:** the store directory for WeftDB (segments, index,
   catalog) and the database file plus WAL for DuckDB. They are reported per load as bytes per
   point. This is what a disk holds, not a codec estimate. `weft-bench` already reports the codec
   figure.
3. **`duckdb-decimal` stores `DECIMAL(18, s)` exactly,** with `s` the dataset's most fractional
   digits. A value that would round is refused, so the knots are the same exact numbers WeftDB
   stores. Its interpolation line is computed in `DOUBLE` (DuckDB's `DECIMAL × DOUBLE` is
   `DOUBLE`), and so is WeftDB's (splimes computes in `f64`). The report says neither engine
   interpolates in exact arithmetic. `min` and `max` stay exact in both, and `avg` is each engine's
   own, read as `f64` for the gate.
4. **`weftdb-stored-bulk` seals one segment per load,** the shape of DuckDB's single bulk
   transaction. It is the like-for-like load comparison EVI-13 promised. The 1,024-row arms stay,
   because they are the layout the stored read path is measured on.
5. **The stored arms now own their `SegmentStore`** (`WeftStore`), so they can downsample and
   report bytes. The store is configured as `StoredWeftAdapter`'s is (narrowest lossless type,
   nanosecond timestamps, a margin that reads every row), and the in-memory gate still checks they
   agree. `weftdb-stored-f64` reports no downsample of its own, since it would repeat
   `weftdb-stored`'s over the same store.
6. **The nightly smoke run is a separate workflow** (`xbench-smoke.yml`, 06:30 UTC and on
   dispatch). It checks the seeded lock, builds with `--locked`, runs every arm at `tiny`, puts the
   rendered report in the job summary, and uploads it. It fails on any gate. Its timings are not
   cited, because hosted runners are not reference hardware (decision 19).

**First run** (2026-10-29, `/mnt/weftbench`, 10 reps, 3 load reps; every arm and phase passed its
gate at all three scales). At `medium` (604,689 MCAR / 481,252 block rows), p50:

| | load | B/pt | interpolate | downsample |
| --- | --- | --- | --- | --- |
| `weftdb-memory` | — | — | 139.0 / 110.7 ms | 7.5 / 5.9 ms |
| `weftdb-stored` (1,024-row) | 5,136 / 4,088 ms | 7.17 / 7.13 | 173.0 / 137.5 ms | 13.5 / 10.2 ms |
| `weftdb-stored-f64` | 5,154 / 4,073 ms | 7.17 / 7.13 | 52.0 / 37.8 ms | — |
| `weftdb-stored-bulk` (one segment) | 959 / 806 ms | 6.66 / 6.62 | 172.4 / 130.2 ms | 28.9 / 21.0 ms |
| `duckdb-double` | 202 / 165 ms | 8.26 / 8.74 | 46.4 / 38.8 ms | 4.5 / 3.8 ms |
| `duckdb-decimal` | 253 / 193 ms | 8.26 / 8.20 | 48.1 / 40.9 ms | 4.4 / 3.4 ms |

What it says:
- **Storage:** WeftDB's store is about 15–25% smaller than DuckDB's file.
- **Load:** like for like, WeftDB is still about 5× slower to load (`stored-bulk` against DuckDB).
- **Downsample:** DuckDB is 2–3× faster, and a single large segment downsamples about 2× slower
  than 1,024-row segments. That last one is worth a look in the store lane.

**Rejected:** percentiles in the downsample (DuckDB's `quantile_disc` and WeftDB's nearest-rank need
a rank-convention argument first, which is a separate comparison); a DuckDB `DECIMAL(38)` column
(it would leave the `i64` fast path that `DECIMAL(18)` keeps); adding the smoke run to
`bench-regression.yml` (a C++ build failure would then read as a performance regression).

## 2026-10-29 — EVI-16: one honesty document at the root, and what it may claim

**Ambiguous:** EVI-16 (#277) is "Honesty pages I: not the right tool, benchmark and accuracy
methodology, precision". The pages were meant for basicautomation.io, which this repository cannot
write to. `docs/` was removed in #99. The ROADMAP marks the narrative as the owner's to "write or
approve" (beside EVI-17).

**Chosen:** one `HONESTY.md` at the root, as for WILLINGNESS-TO-PAY.md, linked from the README's
*Is WeftDB right for you?*. It has four sections, and EVI-17 adds the other two. Every claim links
to the code, benchmark or measurement behind it, and the page says that the link wins when they
disagree. The first cross-engine losses (EVI-13's run) are stated as losses, with the load layout
caveat beside them rather than in place of them. The precision page separates what is exact
(stored values, raw points, reductions) from what is not (interpolated values are computed in
`f64`), because "exact decimals" is the claim most easily over-read. The page says it awaits the
owner's approval before being published elsewhere.

**Rejected:** four separate files (they cross-reference constantly); re-creating `docs/`; quoting
cross-engine figures without the run date and caveat.

## 2026-10-29 — EVI-15R: how the honesty section decides a win

**Ambiguous:** EVI-15R (#276) is "Report renderer with honesty-section generation". The only text
for the honesty section is the pre-#99 ROADMAP's report requirements: "where WeftDB wins, where it
loses, workloads not tested, known limitations, config caveats, reproduction instructions". Nothing
says what counts as a win, which report is rendered, or in what format.

**Chosen:**

1. **It renders `weft-xbench`'s report,** the cross-engine one, in `weft-xbench/src/render.rs`.
   `weft-bench` already writes HTML for its single-engine runs, and only the cross-engine report can
   have a "where WeftDB loses". `weft-xbench render REPORT.json [--out DIR]` writes `report.md` and
   `report.html`, and every run writes both beside `xbench.json`.
2. **A win needs disjoint p50 bootstrap intervals,** WeftDB's wholly below the fastest other
   engine's. A loss is the mirror image, and overlap is reported as "within noise" rather than
   dropped. This is the same rule as EVI-5's regression gate, so a point estimate never decides.
   Every WeftDB arm is compared, not just the fastest one. Otherwise the exact-decimal arms' losses
   at `medium` would hide behind the `f64` arm's tie.
3. **A result that failed its correctness gate is never compared.** Failures are listed first, and
   a banner at the top of the report says the run has one.
4. **"Not tested" is computed, not written.** Missing dataset kinds, scales short of three, and
   decision 18 arms absent from the run (`duckdb-decimal` until EVI-14) are listed, followed by a
   fixed list of workloads and engines outside this report, including the licence exclusions from
   WILLINGNESS-TO-PAY.md.
5. **Caveats come from the run's metadata:** the hardware, the working directory's disk, where the
   engines wrote (`TMPDIR`), a debug build, and fewer than 10 reps.
6. **Markdown and HTML come from the same blocks,** so they cannot disagree. The HTML is
   self-contained: inline CSS, a dark-mode rule, no scripts and no external assets.
7. **Report schema v2** adds `invocation`, `scratch_dir` and `optimized`. A v1 report still renders,
   and says those facts were not recorded rather than guessing them.

**Rejected:** a template engine (one more dependency for six fixed sections); deciding wins on p50
alone; a hand-written honesty section beside a generated one.

**Noticed, not fixed here:** `RunMetadata` records the working directory's disk, not `TMPDIR`'s,
and reported the 7950X3D as 16 logical cores. The second belongs to `weft-bench`'s capture.

## 2026-10-29 — EVI-13: what "weft-xbench crate, seeded lock, load and interpolation arms" means

**Ambiguous:** EVI-13 (#275) survives as one line, "DuckDB adapter I: weft-xbench crate, seeded lock,
load and interpolation arms". The 1.0 plan's decision 18 (`f085f84:docs/release/1.0-plan.md:540`)
gives the report's scope (WeftDB stored, in-memory and `f64` against DuckDB `DOUBLE` and `DECIMAL`, on
MCAR, block-missingness and UCI household-power data at three scales), and the 2026-10-09 ROADMAP note
says the `duckdb` crate belongs behind an off-by-default switch because of its C++ build. Nothing says
what "seeded lock" is, how DuckDB should interpolate, or which arms are EVI-13's and which EVI-14's.

**Chosen:**

1. **A separate workspace, not a feature.** `weft-xbench/` has an empty `[workspace]` table and the
   root `Cargo.toml` excludes it. An off-by-default feature still compiles under CI's
   `clippy --workspace --all-features` on every push; a separate workspace never does.
2. **"Seeded lock" = its own `Cargo.lock`, copied from the workspace's.** Every dependency the two
   share resolves to the same version, so the WeftDB being benchmarked is the one the workspace
   ships. `scripts/xbench/check-lock.py` fails on drift (same name and semver-compatible class, at
   another version). A new major that only DuckDB pulls in is a separate package and is allowed. It
   runs in CI as `weft-xbench lock`. `duckdb` 1.20000.1 now uses `arrow` 60, the workspace's
   major, and a direct `arrow = "=60.0.0"` pin keeps the minor equal too.
3. **Arms.** EVI-13 has four: `weftdb-memory` (the reference), `weftdb-stored` (EVI-11's adapter),
   `weftdb-stored-f64` (`interpolate_range_f64`, decision 18's `f64` arm, which EVI-11 left here), and
   `duckdb-double`. `DECIMAL`, downsampling and storage bytes are EVI-14's. **Load** times getting the
   data into durable storage: a WeftDB seal against DuckDB's appender plus `CHECKPOINT`, both in
   `TMPDIR`. **Interpolation** is piecewise-linear, the one method all four implement.
4. **DuckDB interpolates in SQL, as a user would:** `range()` for the grid, two `ASOF JOIN`s for the
   knots either side, and the first or last segment's line extended past the data. That is splimes'
   `Linear` rule, so the two engines answer the same question. Duplicate instants keep the last row,
   as splimes does. Both engines' synchronous work runs on tokio's blocking pool, so the hand-off is in
   every arm's timing alike.
5. **Every arm is gated against the in-memory WeftDB grid** (relative tolerance 1e-9), and synthetic
   datasets are also scored against their analytic ground truth. A disagreement fails the gate, is
   reported beside its timing, and makes the binary exit 1.
6. **Datasets.** MCAR and block missingness come from the same seeded one-day profile (20% missing;
   blocks are 1/200 of the series), reconstructed per second, so input grows with scale and the grid
   stays at 86,401 points. UCI takes the first `point_count` rows, reconstructed per minute. Default
   scales are `tiny`, `small` and `medium`.

**Rejected:** a `duckdb` feature on `weft-bench` (still built by `--all-features`); a DuckDB arm that
pulls rows out and interpolates in Rust (that measures Rust, not DuckDB); comparing only interior grid
points (the edges are where a hand-written SQL fill usually goes wrong, so they are gated too).

**Load is not like-for-like yet, and the first run says so.** The WeftDB arms seal 1,024-row
segments, EVI-11's layout chosen for the read side, so `medium` is about 590 durable seals. DuckDB
does one bulk append and one `CHECKPOINT`. The first run (2026-10-29, `/mnt/weftbench`, 5 reps)
measured WeftDB load at 4–5 s against DuckDB's 150–175 ms at `medium`. The interpolation figures were:

| `medium` p50 | MCAR | block |
| --- | --- | --- |
| `weftdb-memory` | 139.7 ms | 113.2 ms |
| `weftdb-stored` | 174.6 ms | 138.9 ms |
| `weftdb-stored-f64` | 52.3 ms | 37.6 ms |
| `duckdb-double` | 45.8 ms | 39.2 ms |

At `small`, `weftdb-stored-f64` beat DuckDB (6.6 ms against 24.3 ms MCAR); at `medium` DuckDB is level
or ahead. These are recorded as found, not tuned. A load arm that seals one segment per load (the
same shape as DuckDB's single transaction) is EVI-14's to add beside this one, so the report can show
both layouts.

**Not run here:** the UCI datasets. The corpus is not on this machine, and `--uci FILE` adds them.
Building on the shared rotational target directory stalled at link time under I/O pressure;
`CARGO_TARGET_DIR` and `TMPDIR` on `/mnt/weftbench` (decision 19's SSD) build and run cleanly.

## 2026-10-29 — Phase 0: where the willingness-to-pay hypotheses live, and what ticks the box

**Ambiguous:** Phase 0 asks to "document willingness-to-pay hypotheses" and to "identify 3–5 design
partners", but `docs/` was removed in #99 with the prose moved to basicautomation.io, and the
2026-10-24 entry rejects re-adding it. The site is not writable from this repository.

**Chosen:**

1. Both documents sit at the repository root, beside `ROADMAP.md` and `DECISIONS.md` — the files
   #99 kept — and the ROADMAP's Phase 0 lines link to them. When the site has a page for them, they
   move there and the links follow.
2. **Willingness-to-pay is ticked:** the deliverable is the documented hypotheses, and
   `WILLINGNESS-TO-PAY.md` states six, each falsifiable, with a kill signal and a cheapest test,
   every one marked untested. Its price bands are placeholders an agent derived from incumbent
   categories, and it says so; the figures it leans on (spline vs linear on BTC, GPU vs rayon,
   exact-decimal cost) were checked against the README's measured claims.
3. **Design partners stays unticked.** `DESIGN-PARTNERS.md` is the profile, checklist and an
   archetype shortlist with public corpora as stand-ins; nobody has been contacted, and choosing
   and contacting partners is the operator's.

**Rejected:** re-creating `docs/`; ticking the design-partner line on a shortlist alone.

**Later the same day (operator):** the boundary and the opening prices in `WILLINGNESS-TO-PAY.md` are
adopted as written. The single-node engine stays MIT/Apache, TLS and basic auth included. HA and
replication, SSO/RBAC/audit, historian connectors and SLA support are paid, at $3k support / $12k
per node / $30k per site per year, with a $7.5k benchmark report and free pilots at 50% off for two
years. Contact is made as Basic Automation, discovery only until it is incorporated. A pilot
without TLS or auth is fine as an isolated test now, not for 1.0. Hosting is a viable paid offer, priced on use once
`weft-bench` publishes a cost figure. Benchmark publication: kdb+ and AVEVA PI are not benchmarked or
published (their licences bar it without written consent); open-source engines are, under six rules
(licence read, reproducible script, vendor-documented tuning, 14 days of vendor preview, losses
published, corrections logged); paid reports stay private and the buyer confirms their own licence. Licences read the same day: TimescaleDB's TSL has no
benchmark clause and is published as "TimescaleDB Community Edition" (its §2.4). QuestDB Enterprise's trial
terms bar direct competitors (§3.6), and InfluxDB 3 Enterprise's licences allow only personal (At-Home) or
internal (SLSA) use, so neither enterprise edition is benchmarked; their open-source editions are.

**Reframed the same day (operator):** the design-partner line becomes "identify 3–5 open datasets
with interpolation-heavy workloads" and is ticked on five selected in `DESIGN-PARTNERS.md`: UCI Beijing,
NOAA NDBC, PhysioNet 2012, Petrobras 3W, BDG2. This is because Basic Automation is not incorporated and
cannot sign partners yet. Recorded cost: datasets test the bench, not the market, so every
willingness-to-pay hypothesis stays untested until buyer calls happen.

## 2026-10-29 — EVI-11: what "stored WeftDB benchmark adapter" means

**Ambiguous:** EVI-11 (#266) survives only as its title, in `ROADMAP.md` and in the 1.0 plan
(`f085f84:docs/release/1.0-plan.md`); the evidence-track design that defined it is not in history. Two readings fit the title: an adapter that runs the bench's flagship workload against
data *read from a `SegmentStore`*, or one that drives a running `weft-server` over HTTP. And the
`SystemAdapter` trait hands an adapter its points on every rep, which a stored adapter can only use
by writing them — turning a read benchmark into a write benchmark.

**Chosen:**

1. **Stored means the library store, not the server.** `StoredWeftAdapter`
   (`weft-bench/src/stored_weft_adapter.rs`, adapter name `weftdb-stored`) seals the dataset into a
   temporary `SegmentStore` and times `SegmentStore::interpolate_range` — the EVI-10A API, which
   decision 18 ships as the 1.0 surface for stored interpolation with the HTTP route deferred past
   1.0. An HTTP adapter would measure a route that does not exist yet.
2. **Ingest is untimed, through a new `SystemAdapter::prepare` hook.** `prepare(&points)` has a
   no-op default, so the in-process adapters are unchanged; `run_profile` and `measure_accuracy` call
   it once before the timed reps, its span counting toward `end_to_end_ns` and never `measured_ns`.
   A call for points that were not prepared is *refused* (an O(1) signature: row count and both end
   rows), not quietly stored, so a missing `prepare` cannot put a write inside a timed rep.
3. **The stored knots are the request's knots.** The aspect is declared with the narrowest lossless
   physical type for the values (the type the run's storage estimate already costs) and
   nanosecond timestamps (the synthetic profiles jitter instants below the microsecond, and
   nanoseconds are chrono's own precision), and the read margin is widened to every stored row. The
   output therefore equals `WeftAdapter`'s point for point (tested for linear and cubic), and the
   latency gap between the two results is the storage read path alone. Input outside the `i64`
   nanosecond range (about 1677–2262), or unsorted input, is refused rather than clamped or reordered.
4. **Segments are sealed in 1024-row batches** (`DEFAULT_INGEST_BATCH_ROWS`, the ingest workload's),
   so a window spans many frames as it would in a store fed by ingest; `with_batch_rows` changes it.
5. **Opt-in on the CLI (`--stored`).** Adding a result to every default run would rename the nightly
   artifact (`<profile>__weftdb+weftdb-stored.json`) and add an unpaired workload to EVI-5's
   comparison; the nightly can opt in when a baseline exists.

**Rejected:** ingesting inside `interpolate_range` on the first rep and relying on `cold_warm` to
separate it (the first rep would carry a write, and the cold figure would stop meaning first-touch
read); a server-backed adapter; adding `--stored` to the nightly workflow in the same PR.

**Settled by the plan:** decision 18 (`f085f84:docs/release/1.0-plan.md:540`) scopes the first report
as "WeftDB (stored, in-memory, f64) against DuckDB `DOUBLE` and `DECIMAL`", with stored interpolation
shipping "as a library API (EVI-10A); the HTTP route is deferred". So the stored arm is the library
store, as built here. The `f64` arm (`interpolate_range_f64`) is not part of this adapter: the
`SystemAdapter` contract returns `BigDecimal` points, so converting back would put a per-point cost
into the timing that the arm exists to avoid. It belongs in EVI-13's cross-engine harness, which owns
its own arms.

## 2026-10-29 — SEC-4 "one HTTP/1.1 serve path with pre-auth listener limits" (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§3 SEC-4 row, §4 C9, §6 W3) fixes *what* SEC-4
must bound — a connection cap and a write-stall timeout absorbed from ROB-23,
a NOFILE-derived default for the cap, header and keep-alive deadlines, one
serve path (`serve::run`), signal handlers installed first, a signal before
serving exiting 0, and the drain order serve → SEC-9 → D-S14 → runtime
`shutdown_timeout(5 s)` — but names no numbers, no derivation formula, and no
mechanism. The queue records no open question for SEC-4, so these choices are
made here, grounded in the plan's constraints, and flagged for merge review.

**Chosen:**

1. **The serve path is a hand-rolled accept loop over hyper's HTTP/1.1
   server** (`weft_server::serve::run`), not `axum::serve`: the cap must be
   held *before* `accept` (a semaphore permit precedes each accept, so
   excess connections wait unread in the kernel backlog — the pre-auth
   property), and `axum::serve` exposes neither that hook nor per-connection
   IO. The router is the same `app_with_state` router, carried per
   connection by `TowerToHyperService`. HTTP/1.1 only, per decision 17.
2. **Cap derivation: half the `RLIMIT_NOFILE` soft limit**, clamped to
   16..=65,536, falling back to 1,024 when the limit cannot be read. Half,
   because connection sockets share the one descriptor budget with the
   store's segment/index files: a cap that could take every descriptor
   would let unauthenticated peers starve the store of file handles. The
   ceiling exists because a ~1M container soft limit halved is not a
   meaningful pre-auth bound. `WEFT_MAX_CONNECTIONS` (Tuning) overrides.
3. **Deadlines: header 30 s, keep-alive 30 s, write-stall 10 s**, each a
   Tuning `WEFT_*_SECS` variable, each refusing non-positive values at
   startup. 30 s is hyper's own header-read default, made explicit. The
   header deadline is hyper's `header_read_timeout` (tokio timer); the
   keep-alive and write-stall deadlines are progress deadlines on the
   connection's IO (available input is always taken first, so they fire
   only when no bytes move — between requests, no-progress *is* the
   keep-alive idle bound).
4. **Handlers first, literally:** `ShutdownSignals::install()` runs in
   startup immediately after the hooks phase and before config/state/bind,
   replacing terminate-on-signal from that point on. A signal delivered
   during store open is recorded by tokio's signal stream and observed on
   `serve::run`'s entry, which drains nothing and returns `Ok` — the
   process exits 0. While serving, a signal stops accepting and live
   connections drain for up to 5 s (C9's figure) before abort. The rest of
   C9's drain order (SEC-9's tracker, D-S14's `store.shutdown()`) belongs
   to those slices and slots in after `run` returns.

**Rejected:** capping by counting connections *after* accept inside a
middleware (post-read, so not pre-auth, and the accepted-but-unserved
sockets still consume descriptors); an overall per-connection lifetime
timeout instead of progress deadlines (it would kill long legitimate
downloads and idle-but-valid keep-alive alike on the same clock); and
leaving the second `axum::serve`-shaped path in the binary for tests
(the tests drive `serve::run` itself, over real TCP).

## 2026-10-29 — SOA-12: procfs over a crate, deltas over totals, and a check that cannot run

**Ambiguous:** the roadmap line and issue #258 name three things - Linux system
samplers, contamination flags, the strict-mode flush check - and say nothing about
what a sampler reads, which numbers are contamination, or what makes a run "flushed".

**Chosen:**

1. **`/proc` text readers, no `sysinfo`.** PSI (`/proc/pressure/*`), mount sources
   (`/proc/self/mountinfo`), device write counters (`/proc/diskstats`), `Dirty` /
   `SwapCached` / `oom_kill` (`/proc/meminfo`, `/proc/vmstat`) are half of what this
   item needs and no crate reports them. Reading the same kernel twice through two
   abstractions invites two answers to one question, so the module reads procfs
   through a `ProcRoot` whose path is a parameter: the tests point it at a fixture
   tree, and a host without a file gets `None` rather than a fabricated zero.
2. **Device counters are a window delta.** `diskstats` is cumulative since boot;
   reporting the total would credit a ten-minute run with every byte the machine
   ever wrote, which on this host reads as a huge foreign writer. The ledger keeps
   first and last and subtracts, and `None` (device unreadable, or a name that
   changed mid-run) stays unknown instead of becoming zero.
3. **Device naming prefers `/proc/partitions`.** `mountinfo` carries `major:minor`,
   which `partitions` names authoritatively whatever the mount source looked like
   (`/dev/mapper/vg-root`, a by-uuid link). The source basename is the fallback only.
   A multi-device filesystem (btrfs across disks) reports one source, so its
   counters cover that device alone - recorded in the doc comment, not silently
   averaged.
4. **Flags are recorded, and only strict mode screens.** Contamination is a property
   of the machine, not a verdict on the run: a `ContaminationVerdict` always comes
   back with what was seen, and the tolerance vector in `ContaminationPolicy` decides
   what fails, and only under `StrictMode::Strict`. That is also why the sampler
   never writes to the store directory: it resolves the device from the path and
   leaves the filesystem alone, so the harness cannot launder its own writes.
5. **The flush check measures, it does not force.** It compares what the store's
   device wrote during the run against what the server acknowledged, plus a slack
   for WAL and metadata, and screens residual `Dirty`. There is no `fsync` of the
   store here - that is the server's durability contract, not the harness's job, and
   forcing it would make an unflushed server look flushed. A run with no durability
   acknowledgment returns a skipped check with the reason; `blocks(strict)` fails it
   under strict mode, because a run that cannot show it flushed is exactly the run
   strict mode exists to reject.
6. **Clock honesty.** Offsets between `CLOCK_MONOTONIC` and wall clock cannot drift
   60 s, so a step in `wall - monotonic` (default 250 ms) flags `ClockStep` and
   invalidates the intervals across it; a sample gap over 30 s flags `SampleGap`.
   Both defaults live in `ContaminationPolicy::default`, so a soak config can move
   them.
7. **Artifacts carry numbers, not paths.** The verdict and flush JSON hold the device
   basename and the counters, never the store path or mount point: a published soak
   artifact should not leak a developer's directory layout. A test writes a store
   path containing a canary string and asserts the artifact does not contain it.

## 2026-10-29 — EVI-8B: what "retires v1" means, and why the report schema stays at v23

**Ambiguous:** EVI-8A's entry (2026-10-26, item 5) defines Part II as "ports the
remaining adapters to v2 and retires v1" without saying what retirement is: whether
`SystemAdapterV2` survives as the only interface, whether `run_profile_v2` stays as
the entry point, and whether retiring v1 bumps the report schema — §3 reserves a
number per schema change, and v23 exists precisely because of adapter v2.

**Chosen:**

1. One interface, not a rename: `adapter_info()` becomes a required method of
   `SystemAdapter` and `SystemAdapterV2` is deleted. A default implementation would
   have let a future adapter skip reporting silently, which is the exact gap v2
   exists to close; the compiler is now what makes an adapter say what it ran.
2. One entry point: `run_profile_v2` and the private `run_profile_with_info`
   collapse into `run_profile`, which always records the adapter's own report.
   Keeping both names would leave two run paths where the roadmap asks for one.
3. **No schema bump: the report stays v23.** The JSON shape is unchanged —
   `BenchResult.adapter_info` is still `Option` and still `skip_serializing_if`,
   because artifacts written while v1 existed, and the workloads that do not drive
   an adapter at all (ingest, compression, downsample, range fetch, point lookup,
   sparse, gpu_memory), still legitimately carry no report. §3 numbers a schema when
   its shape changes; nothing here changes shape, so v23 stands and
   `MANIFEST_SCHEMA_VERSION` does not move.
4. Provenance in `compare`: now that every adapter reports itself,
   `compare_reports` names both sides' engines in its notes (engine, method,
   native-vs-in-process, spline honoured or ignored), and calls a missing report
   `[no adapter report]`. This is a note, never a `hard:` refusal — pairing WeftDB
   with another engine is the point of a competitor comparison. What must not
   happen is a table that reads like a like-for-like race while one side ran a
   native engine and the other an in-process baseline.
5. The honesty check moves from comment to test:
   `reported_spline_honour_matches_the_output_the_adapter_produces` rebuilds the
   same dataset under `Spline::Linear` and `Spline::Cubic` and asserts the outputs
   differ exactly when the adapter claims `honours_requested_spline`. A report that
   flatters its adapter now fails CI instead of merely being untrue.
6. Out of scope, recorded so it is not lost: the non-adapter workloads still write
   `adapter_info: None`. Their `adapter` column is the literal `"weftdb"` and they
   measure WeftDB's own storage paths, not a foreign engine, so there is no second
   party to report; if a competitor is ever driven through one of them, that
   workload needs its own report before its numbers are publishable.

**Rejected:** keeping `SystemAdapterV2` as the sole trait with `SystemAdapter` as a
supertrait alias (two names for one interface, and the unreported one still
importable); a default `adapter_info()` returning a placeholder (silence would look
like compliance); bumping the report schema to 24 for the retirement (no shape
change, and it would refuse comparison against every v23 artifact already on main
for no information gain); and leaving `run_profile` unreported "for compatibility"
(its only callers are `main.rs` and this crate's tests).

## 2026-10-29 — SOA-5: the soak ingest client fills SOA-3's seam, and stops at the ingest lane

**Ambiguous:** ROADMAP line 274 names "Soak ingest client" with no acceptance list, and the plan
text does not say whether it is a CLI mode or a library component, whether it drives the read lanes
too, whether it may add an HTTP client dependency, or what it does when a POST fails. Each of those
changes what a published 24 h run (SOA-18) would mean.

**Chosen:**

1. **Library module, no CLI flag**: `weft-bench/src/soak_client.rs`, exported from the crate. The
   slices that own how a soak is launched are later ones — SOA-14 (server hooks), SOA-15 (`soak.yml`
   nightly workflow), SOA-17 (`scripts/crash-soak.sh`) — so a `--soak` flag added here would be a
   second entry point for those slices to remove.
2. **Fills the seam SOA-3 left**, implementing `SoakServer` rather than inventing a second trait:
   `OpenLoopScheduler::run_with_server` drives a live server exactly as it drives `NullServer`, so
   the difference between a null run and a live run is the server, not two harnesses.
3. **Ingest lane only, refused at construction.** `IngestClient::new` rejects a `WorkloadModel`
   whose lane weights can produce a `point_lookup` or `range_scan` tick, and `handle` errors on such
   a tick rather than doing nothing. The reason is the interval log: the scheduler times every tick
   it executes, so a silently skipped read would enter the histogram as a sub-microsecond step and a
   published run would claim read latency it never measured. Reads belong with the oracle and the
   derived-read checks (SOA-6, SOA-10), which are what make a read result verifiable at all.
4. **Its own HTTP/1.1 over a blocking `std::net::TcpStream`, no new dependency.**
   `SoakServer::handle` is a synchronous call inside the scheduler's tick loop, so an async runtime
   cannot be driven from a `&mut self` method; and the workspace gates build `--locked`, so hyper
   plus rustls plus a runtime is a large graph — and a feature-unification risk for a crate that
   otherwise never opens a socket — in exchange for one POST per tick to a loopback port. Connect and
   I/O timeouts are explicit (`DEFAULT_CONNECT_TIMEOUT` 2 s, `DEFAULT_IO_TIMEOUT` 30 s). Note for the
   next editor: a read timeout on a `TcpStream` surfaces as `EAGAIN`/`WouldBlock`, not as a timeout.
5. **The body is the model's, byte for byte.** The client regenerates the tick's `WorkloadStep` —
   pure in the seed and the tick, so any tick replays without its predecessors — and writes
   `WorkloadStep::body` unchanged: no tags added, no lines reordered, no cells filtered. The server
   seals the numeric subset of the named field and reports the count, recorded beside the count
   offered (`IngestFlow::lines_offered` against `IngestFlow::points_sealed`), so what the server
   declined to keep stays visible instead of being hidden by a client-side filter.
6. **No retry once a byte is written.** ILP ingest is not idempotent: a second POST of the same body
   seals those points again. Only a connect failure — nothing sent, no duplicate possible — is
   retried, and those are counted (`connect_retries` against `ticks_posted`). A failed step fails the
   tick, the scheduler aborts the harness, and the operator sees a short run; a silently duplicated
   seal would surface much later as an oracle mismatch with no trace of its cause here.
7. **One kept-alive connection, and a server that ends it is a failed tick.** A socket per tick would
   put the connect into every latency sample, which is the number the run is published for.
   `Connection: keep-alive` is not a transaction, so reconnect-and-retry after a partially written
   request would risk the duplicate seal of point 6. `declare_aspect` therefore runs on a connection
   of its own, so a schema write is not charged to the first tick.
8. **Bounded, framed, redacted.** The response is read under `MAX_RESPONSE_BYTES` (64 KiB): a server
   that answers more is refused rather than downloaded. Both `Content-Length` and chunked framing are
   decoded, and a body cut short by a closed connection is an error, never a short tick. Anything the
   server said that is quoted into an error passes through EVI-3's redaction first.
9. **Refused before tick zero.** `IngestTarget`'s defaults (aspect `soak`, field `value`, physical
   type `f64`, timestamp unit `ns`) are validated at construction, so a target the server would
   reject fails there instead of hours into a run.

**Rejected:** pulling `tokio`/`hyper` into `weft-bench` (point 4); a `--soak` CLI mode now (point 1);
retrying a POST after the request was written (points 6 and 7); filtering the model's body down to
the values the aspect can hold, which makes the offered/sealed delta invisible; and stubbing the read
lanes to succeed, which the interval log would report as excellent latency.

**Verification:** the crate's `--lib` tests, including a real listener on `127.0.0.1` that asserts the
request line, headers and body byte for byte, keep-alive reuse across ticks, chunked decoding, the
response cap, a body closed mid-answer, a refused declaration, and that a read-lane tick fails.

## 2026-10-29 — SEC-3 "sealed route registry, frozen scope table and fail-closed gate" (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§3 SEC-3/SEC-6 row, §2 L-ROUTER, C4, C10) fixes the
`RouteSpec` field set (`scope, audit, operator_detail, tier, admission_class,
operation_id`), says FRE's four roles are replaced by SEC's 8 scopes and 6 roles,
and says one registry produces the router, the policy table, the class table and
the OpenAPI document — but it never enumerates the eight scope names, the six
roles, or the admission classes. Two scope names are stated elsewhere in the
plan: `maintain` (§5, D-S12: fsck and quarantine routes register "with scope
`maintain`") and `metrics` (§11 decision 17: "`/metrics` needs the metrics scope
when auth is on").

**Chosen:**

1. The eight scopes are `probe, metrics, read, compute, ingest, declare,
   maintain, backup` — the two plan-named scopes plus one scope per route area
   the server already has (probes, storage reads, the stateless interpolate /
   downsample compute endpoints, point ingest, aspect declaration, backup).
   Declaration is its own scope, apart from ingest, so a pure ingest key cannot
   reshape the catalog; backup is its own scope, apart from maintenance,
   because a backup reads the whole store. The table is frozen in
   `weft-server/src/routes.rs` and pinned by the `scope_table_is_frozen` test.
2. The six roles are `anonymous, reader, writer, declarer, maintainer, admin`,
   with cumulative scope grants (each role holds everything the previous one
   holds; admin holds all eight). SEC-3 defines only the grants; SEC-6 decides
   which API key holds which role.
3. The admission classes are `probe` (never shed), `query` (reads and stateless
   compute), `write` (ingest and declaration, admitted against the body cap)
   and `maintenance` (maintenance and backup). This is ROB-10's per-route class
   table folded into the registry, per §3.
4. `tier` reuses the configuration registry's `Tier` vocabulary
   (`settings::Tier`); every route shipped today is `Stable`, because the whole
   surface is the frozen 1.0 surface.
5. The OpenAPI document is generated from the registry itself (one operation
   per route, keyed by `operation_id`, the rest of the spec as `x-weft-*`
   extensions). FRE-9a (W8) later feeds utoipa-axum's `routes!()` output *into*
   `Routes::route`, per §3 — the document's producer changes then, its shape
   does not.
6. The fail-closed gate is a layer on the registry-built router: a matched
   path with no (method, path) entry is refused `405 method_not_allowed` with
   `Allow` (FRE-6a's 405 fallback, absorbed by the §3 row), and an unmatched
   path is refused `404 route_not_found` by a fallback that decides from the
   same sealed table (a concrete path matching a template under another method
   also gets the 405).

**Rejected:** blocking the slice until the scope names are enumerated (the
queue records no open question for SEC-3, unlike its neighbours; the two
plan-named scopes anchor the table and the rest follow the server's existing
area split, and the whole table is reversible at merge review); keeping
`lib.rs`'s route chain and adding the tables beside it (that is exactly the
four-registries drift C4 resolves against); and adding a `utoipa` dependency
now (FRE-9a owns that integration in W8; the registry must exist first, in W3,
before D-S14 — C10).

## 2026-10-27 — D-S10 "write-once durable seal with id assigned at commit" (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6 W3, §5 D-S10 amendment) says a seal is
write-once with its id assigned at commit, and that `segment_path` /
`sidecar_path` are rewritten to use `aspect_name::encode` with a
containment check — but it does not say what the seal's frame is named,
what a sidecar is named by once the id stops naming the frame, or
whether a seal advances the commit epoch that swaps advance. The
crash-consistency design §4 (via `frame_name.rs`) says seals take
write-once names from S10, and `aspect_locks.rs` says seals advance the
epoch from S10, which grounds the first and third questions; the
sidecar question is genuinely open.

**Chosen:**

1. A seal frame is `{enc(aspect)}~g{gen}~p{gen}.weftseg`: the generation
   from the per-aspect counter that never hands a value out twice, with
   `prec` equal to the generation, as `frame_name`'s doc prescribes for
   a seal. The id is handed out by `publish_seal` under the commit
   lock, after the frame is written and fsynced with `segments/`
   fsynced, and the row carries `gen`, `prec`, the frame's trailer CRC
   and the commit epoch; the allocator is raised past both the id and
   the generation in the same transaction.
2. A sidecar is named by its frame, not by the segment id:
   `{stem}~g{gen}~p{prec}.weftpart`. An id is assigned at commit and a
   later output may take it, while a frame name is never reused, so an
   id-named sidecar would collide across a swap. A sidecar is loaded
   and removed by deriving it from the frame's own file name (the
   `.weftseg` extension exchanged for `.weftpart`), which resolves a
   legacy frame to its legacy sidecar and a write-once frame to its
   generation-named one without guessing the scheme. The reaper removes
   a write-once frame's sidecar by frame name once the frame is
   unlinked; the legacy id check stays for legacy frames.
3. A seal advances the commit epoch by one, like a swap, and records
   it as the row's `commit_epoch` — the epoch counter is shared, so a
   seal and a swap cannot share an epoch.
4. A seal that stops at a post-write fault point keeps its frame on
   disk, as before: the durable write runs with no fault points and
   the `S-frame-written` / `S-frame-synced` / `S-dir-synced` points
   fire after it, so the crash-matrix orphan (a frame with no row,
   which S12 recovery classifies) still exists. Routing the write
   through `write_new_durable`'s fault points instead would delete
   that frame on an injected error and erase the litter the recovery
   slice is specified to handle.

**Rejected:** keeping the legacy `{aspect}-{id}` frame name and only
encoding the aspect (the frame would still be named by an id assigned
before the write, which is what "id assigned at commit" forbids);
naming sidecars by segment id (collides when a swap output takes the
id); and leaving the epoch unadvanced by seals (two commits could
then share an epoch, which the swap epoch exists to order).

## 2026-10-27 — SOA-4 "deterministic workload model, lanes, format-exact value grids and body encoders" (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** as with SOA-2 and SOA-3, the 1.0 plan (§6, W2) lists SOA-4
by name only, and §3 says nothing about it beyond the SOA ownership rules
(EVI-3 owns run metadata and redaction; SOA keeps its own
`soak_schema_version`). What the workload model is, what a lane is, what
makes a value grid "format-exact", and what a body encoder encodes are
not specified anywhere. SOA-1's module doc supplies the seams: the model
derives its workload from the config's seed alone, and it supplies the
work each scheduler step performs; SOA-3's scheduler supplies the ticks.

**Chosen (grounded in SOA-1's seams, SOA-3's scheduler, and the bench
crate's existing operation families; if merge review disagrees, this
decision is revisited there):**

1. Deterministic means a pure function of seed, params and tick: the
   per-tick random stream is seeded from the seed and the tick (with a
   salt separating the lane stream from the grid stream), never from a
   running generator's state, so any tick replays identically and in any
   order. The generator is `ChaCha8`, as in the profile and sparse
   workloads, so the stream is portable across machines.
2. Lanes are the bench crate's three operation families — `ingest`,
   `point_lookup`, `range_scan` (the ingest, point-lookup and
   range-fetch workloads) — with a weighted mix: a tick belongs to
   exactly one lane, drawn against cumulative weights, so a run's mix is
   countable from its steps. A zero total weight is a construction
   error, not a run that silently does nothing.
3. A value grid is series × rows, walked series-major like the sparse
   grid, each cell present with the model's density or absent. Each
   column carries exactly one of the five line-protocol field formats,
   cycling float/integer/unsigned/boolean/text, so a column's values
   are always in that format's exact wire form.
4. Format-exact means the encoder and the parser agree by test:
   `GridValue::encode_field` writes the format's wire syntax (bare
   number, `i`/`u` suffix, `t`/`f`, quoted string with the protocol's
   escapes) and parsing the result through the line-protocol parser
   returns the same value. No coercion crosses a format boundary.
5. A body is line-protocol text: one line per present cell,
   `soak,lane=<lane>,series=s<series> value=<field> <timestamp_ns>`,
   timestamps advancing one second per row from the synthetic
   profiles' epoch anchor. The body carries no free text (generated
   series names, fixed lane names), so — like the scheduler's report —
   it needs no redaction pass.
6. No schema bump: the model adds no field to `SoakRun` or `SoakConfig`,
   so `SOAK_SCHEMA_VERSION` stays at v1 per SOA-1's bump rule; the
   model and its steps serialize on their own for artifacts that want
   them. The workload is not a server client: SOA-5 owns driving a live
   server, and this slice only produces the steps a client would submit.

**Rejected:** a stateful generator whose tick N depends on ticks before
it (a replay would have to regenerate the whole run); lanes invented
outside the crate's existing operation families (nothing would ground
what a lane means); encoding every value as a float (that is exactly
the format loss "format-exact" exists to forbid); and putting the model
on `SoakRun` (would bump the soak schema for no run-record change).

## 2026-10-26 — SOA-3 "open-loop scheduler, HDR interval logs, harness self-monitoring and null server" (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** as with SOA-2, the 1.0 plan (§6, W2) lists SOA-3 by name
only, and §3 says nothing about it beyond the SOA ownership rules (EVI-3
owns run metadata and redaction; SOA keeps its own `soak_schema_version`).
What "open-loop" scheduling means for the harness, what an HDR interval
log contains, what self-monitoring reports, and what the null server's
seam is are not specified anywhere. SOA-1's module doc supplies the seams:
the scheduler moves a harness through its states, the scheduler path must
use `start_checked`, and the scheduler and null server are SOA-3's, with
SOA-4 supplying the work each step performs.

**Chosen (grounded in SOA-1's seams plus the plan's §3 ownership rules; if
merge review disagrees, this decision is revisited there):**

1. Open-loop means ticks scheduled on the clock, not on completion: one
   tick per `interval_ms` for the run's `duration_secs`. A step that
   overruns its tick causes the *next* tick to be skipped and counted as
   an overrun, never executed late — executing it late would drift the
   schedule and silently turn the run closed-loop.
2. The schedule is computed, not materialized: `tick_count` plus an
   `index * interval_ms` offset, O(1) memory, so a 24 h run at 1 ms does
   not allocate ~86M offsets up front.
3. HDR interval logs group step durations by wall-clock window
   (`interval_log_secs`, from each tick's planned offset) into an
   HDR-style histogram with 1024 sub-buckets per power-of-two range
   (three significant figures, ~0.1% relative error). Each window closes
   as a record with count, exact min/max/mean, and p50/p90/p99/p999;
   empty windows still appear, with count zero and no percentiles, so a
   gap is visible, not missing.
4. The histogram is implemented in-crate (`soak_hdr`) rather than adding
   the `hdrhistogram` crate: that crate is not in the workspace lockfile
   and the gates build `--locked`, so a new dependency would be a
   lockfile change for a structure small enough to own. Its bucketing is
   the HDR scheme; only the storage (a growable counts vector) differs.
5. Self-monitoring reports counts and structurally bad conditions only:
   scheduled/executed/skipped/failed ticks and the slowest step, with
   warnings for a failed step, a skipped tick, or a step slower than the
   tick interval. No pass/fail threshold is invented (cf. SOA-2's refusal
   of a free-space threshold): each warning is a fact about the schedule.
6. The null server is the `SoakServer` trait plus a `NullServer` that
   accepts every step, does no work, and counts acceptances. The trait is
   the seam SOA-5's live-server client implements; the scheduler only
   ever sees the trait. `is_live` is `false` for the null server so a
   null run can never read as a live-server run.
7. No schema bump: the scheduler's report is its own artifact beside the
   run record, not a field on `SoakRun`, so `SOAK_SCHEMA_VERSION` stays
   at v1 per SOA-1's bump rule. The report carries no free text (no
   label, no path), so it needs no redaction pass.
8. Step durations on the deterministic path (`run_gated`) are
   caller-measured and returned by the step closure, so tests replay a
   schedule with fake durations and no wall clock; the real-time path
   (`run`) measures around the step itself. One loop serves both.

**Rejected:** a closed-loop scheduler that waits for each step before
scheduling the next (that is not open-loop); executing overrun ticks late
(it drifts the schedule); a second metadata/redaction path in the
scheduler (contradicts §3); putting the report on `SoakRun` (would bump
the soak schema for no run-record change); and adding an `hdrhistogram`
dependency (a lockfile change for a small owned structure).

## 2026-10-26 — SOA-2 "preflight, device resolution, run metadata and secret redaction" (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6, W2) lists SOA-2 by name only, and §3 says
only that "EVI-3 owns the shared run metadata and redaction; SOA-2 reuses
it" and that SOA keeps its own `soak_schema_version`. The concrete
preflight gates, the device-resolution mechanism, and what "secrets are
redacted in soak outputs" means beyond SOA-1's `redacted_label` are not
specified anywhere. SOA-1's module doc supplies the one seam: preflight
"validates a config before a harness starts".

**Chosen (grounded in SOA-1's seam plus the plan's §3 ownership rules; if
merge review disagrees, this decision is revisited there):**

1. Preflight is a report, not a boolean: `SoakHarness::preflight(store_dir)`
   returns a `SoakPreflight` carrying named gates even when gates fail, and
   `start_checked(preflight)` is the gate that refuses to start. The SOA-1
   `start()` transition is kept ungated for compatibility; the scheduler
   path (SOA-3) must use `start_checked`.
2. Gates: `config_valid` (name non-blank, duration positive — re-checked
   because `SoakConfig` fields are public), `store_dir_exists`,
   `device_resolved`, `store_dir_writable`. Preflight never creates the
   store directory: creating a store is not a check's side effect.
3. Device resolution reuses EVI-3's disk probe (`report::disk_for`, factored
   out of `work_disk` with identical behaviour) for the store directory.
   The resolved kind/filesystem/mount are the same values `RunMetadata`
   records as `work_disk_*`; they live only in the preflight report, never
   as a field on `SoakRun`, per §3's "do not add a second disk field".
4. No free-space threshold: preflight reports `available_bytes` and leaves
   the judgement to the operator, because a soak's footprint depends on
   SOA-4's workload, which does not exist yet. A threshold invented now
   would be a number with no grounding.
5. Secret redaction in soak outputs means whole-artifact redaction, not
   just the label accessor: `SoakRun::redacted_json` serializes a copy
   with the label redacted, and `SoakPreflight::redacted_json` redacts the
   store path and every check detail (paths can carry a home directory or
   a credential-like directory name). Both go through EVI-3's `redact`;
   no new secret list exists.
6. No schema bump: SOA-2 adds no field to `SoakRun` or `SoakConfig`, so
   `SOAK_SCHEMA_VERSION` stays at v1 per SOA-1's bump rule.

**Rejected:** a second disk/driver field on `SoakRun`; a preflight that
creates the store directory; a free-space pass/fail threshold; and a new
redaction routine in the soak module (all contradict §3's ownership rules
or invent an ungrounded number).

## 2026-10-26 — EVI-8A "adapter v2" and its schema number (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6, W2) lists EVI-8A with no definition of "adapter v2"
beyond the name, and §3 says "EVI-3 therefore sets it to 23 and EVI-8A to 24. If
another bump lands first, each takes the next free number and records it in
schema.rs's version history." Live state contradicts the assumed numbering:
the report schema (`weft-bench/src/schema.rs`, `SCHEMA_VERSION`) is at v22,
while v23 landed on a *different* line — the manifest schema
(`MANIFEST_SCHEMA_VERSION` = 23 in `manifest.rs`, EVI-3's bump).

**Chosen (owner-authorized best guess, 2026-10-26 — Justin: "use your best guess
based on the roadmap and docs"; if merge review disagrees, this decision is
revisited there):**

1. "Adapter v2" is the second version of the bench adapter interface
   (`SystemAdapter`): v2 adds the capability/provenance reporting the v1 trait
   lacks (`AdapterInfo`: engine, method, `native_engine`,
   `honours_requested_spline`). v2 is additive — no v1 method changes — so v1
   adapters keep working unchanged.
2. Part I ports only `WeftAdapter` to v2 (`run_profile_v2` records its report
   in the result); `BaselineLinearAdapter` and `ForwardFillAdapter` stay on v1
   via `run_profile` and carry no `adapter_info`.
3. Schema: the REPORT schema bumps 22 → 23, recorded in `schema.rs`'s version
   history, as the new optional `BenchResult.adapter_info` field. The manifest
   line is untouched (still 23); the two lines are now numerically equal by
   coincidence and are bumped independently. §3's own "next free number" rule
   is what licenses 23 on the report line, not the plan's assumed 24.
4. Compare: no change needed — `compare` already refuses any cross-schema
   comparison (v22 vs v23 included) with a clear error, and same-version
   compare is unchanged (existing test
   `reports_of_different_schema_versions_are_refused`).
5. Part II (EVI-8B, W3) ports the remaining adapters to v2 and retires v1;
   explicitly out of EVI-8A.

**Rejected:** bumping the report schema to 24 to match the plan's literal
number (it would skip the report line's free 23 and break §3's numbering rule);
putting the v2 report on the manifest line; and forcing the baselines onto v2
in part I (that is EVI-8B's scope, and their deliberately-ignored spline is
exactly what v2's `honours_requested_spline: false` would report — a claim
this slice does not need to make).

## 2026-10-24 — merging while a check is red

**Ambiguous:** the standing instruction says never merge with a red or pending check; the operator then
said "if CI is broken, then skip it and merge anyway".

These two are in direct conflict, so the resolution is stated explicitly rather than left to judgement
at merge time.

**Chosen:** the newer instruction governs, but only for breakage the PR did not cause. The test applied
before any red merge, in order:

1. Is the same check red on `main` at the merge base, or does the job log show an infrastructure flake
   (runner death, network/registry timeout, no compile or assertion failure)?
2. If neither, the failure is mine to fix and the PR stays unmerged.

Concretely: fix anything my change caused; re-run once where the log shows a flake and say so in the PR;
merge red only when (1) holds, recording in the merge comment the check name, the run that proves it red
on `main`, and the commit. `--admin` is the tool that makes a red merge possible, so it is used only
under (1) and never for a check the branch itself failed.

**Rejected:** treating "skip CI" as licence to edit, disable, or `--admin`-past a check the branch broke,
because every check in this repo exists because of a specific bug and the roadmap's release criteria are
expressed in them.

**Applied so far:** no red merge has been made. #100 (tokio), #101, #102 and #98 were each merged only
with `MERGE_STATE=CLEAN`. The two red states encountered (cargo-deny on #98, `compat_v010` on #98/#103)
were both pre-existing-on-`main` failures under test (1); neither was merged over — #100 and #102 were
landed first so the checks went green honestly instead.

## 2026-10-24 — one writer per worktree

**Ambiguous:** several agent sessions share `/mnt/deepmem` and the same clone, and branch switching in
the shared worktree races them. A peer's `checkout` at 20:40 silently changed the tree I was editing and
its commit swept two of my files in.

**Chosen:** never branch-switch or leave uncommitted work in `/mnt/deepmem/Development/DSP`. All slice
work happens in a dedicated worktree:
`git worktree add -b <slice>/<slug> /mnt/deepmem/Development/weftdb-wt-<slice> main`.

Also learned: `git status` can report phantom modifications after a foreign checkout (stale stat cache) —
`git diff` re-hashes and is authoritative.

## 2026-10-24 — fixture sets are byte-frozen; notes live outside them

`scripts/compat/releases.py` (the `release pins` job) requires every file inside a set to appear in that
set's `MANIFEST.sha256` with matching bytes — including the set's own `README.md`. Documenting something
about `compat/fixtures/v0.1.0/` by editing its README therefore fails CI.

**Chosen:** `compat/fixtures/README.md` (one directory up, outside the hashed set) carries notes about
the sets, and the rule is stated there so the next person does not rediscover it.

## 2026-10-24 — staging in a worktree that carries local tooling

**Ambiguous:** slice worktrees accumulate local helper files under `.pi/` (gate scripts, their logs) and
`git add -A` is otherwise convenient. On the compat branch `git add -A` staged 9.2k lines of
`.pi/logs/*.log` plus a copied gate script into the commit before it was pushed.

**Chosen:** stage named paths, never `-A`, in slice worktrees; and when auditing what a branch changes
use `git --no-pager diff --no-ext-diff --color=never --stat origin/main...HEAD`, because this clone sets
`diff.colormoved plain`, which can hide changed lines from a plain `git diff`.

## 2026-10-24 — proving a fix that a working tree cannot show

**Ambiguous:** the compat fixture bug only appears in a checkout where git itself materialised the tree,
so "it passes locally" is worthless for it, and the instruction to run CI's gates locally could not be
satisfied as stated.

**Chosen:** run the gates in a fresh `git worktree` checked out from the branch — that reproduces the
runner condition exactly (zero `dictionaries` directories) — and pair it with a counterfactual: disable
the fix, re-run, and keep the reproduced failure message in the PR and commit message. The pair is what
makes "CI is the experiment" unnecessary as a justification.

## 2026-10-24 — slice specs are read from git history, because `docs/` is gone from `main`

**Ambiguous:** W1/W2 steps are defined by `docs/release/1.0-plan.md` §6 and the design docs. PR #99
(`4de2c69`) deleted all of `docs/`, and #103 trimmed the ticked items, so `main` now carries one ROADMAP
line per slice — e.g. `- [ ] **FRE-3** Golden frames, golden sidecars and persisted-encoding goldens
(gated codecs included)` — with no acceptance criteria attached.

**Chosen:** treat the last commit that held the prose (`f085f84:docs/release/1.0-plan.md`,
`f085f84:docs/design/*.md`) as the authoritative spec and read it from history with `git show`, per
slice, before implementing. Rejected: implementing from the one-line ROADMAP text alone, because these
slices carry gates expressed as acceptance text, and inventing the criteria would make the tick
meaningless. Also rejected: re-adding `docs/` to the repository, because removing it was another
session's deliberate decision and re-adding it would fight that.

**Consequence worth recording:** if the prose has a real home (basicautomation.io), the ROADMAP's
one-liners should link to it; a checklist whose criteria live nowhere reconstructable is how a slice gets
ticked for less than it means.

## 2026-10-25 — what the nightly benchmark gate compares against, and when a move is a regression

**Ambiguous:** EVI-5 asks for a "nightly benchmark-regression workflow and `weft-bench compare`" and says
nothing about either of the two decisions that make it work: which run is the baseline, and what makes a
number "regress". A gate can be built to satisfy the sentence in several incompatible ways, and each of
them fails differently.

**Chosen.** The baseline is the newest *successful* run of the gate itself, fetched from its artifact; a
night that regresses goes red and stops being a baseline. Comparing against "yesterday" instead lets the
truth drift one small step per night — such a gate measures the slope, and a store can lose a third of its
speed over a quarter without any single night crossing anything. The first night has no baseline: it
uploads its report, says so in the summary, and passes; the gate starts comparing on the second night.

A regression requires two independent things: the move exceeds `--threshold` (10% by default) **and** the
bootstrap confidence intervals of the two runs do not overlap. Beyond-threshold-but-overlapping is
`suspicious`: printed, in the verdict, not gating unless `--strict`. The threshold alone would page on
hosted-runner shape changes; the interval test alone would call a 2% move on a tiny workload a crisis.
A measure with no interval (throughput, storage size, ratio, count) can only ever be suspicious, and the
verdict names that ceiling instead of quietly treating it as evidence of nothing.

**Rejected:** a fixed nanosecond or MB/s budget, because `docs/design/benchmarks.md` "How to cite a
number" already establishes that hosted runners are not the same machine twice, and a budget written
today is wrong the first time the runner fleet moves; and putting this job inside `ci.yml`, because
`scripts/release/verify.sh` reads the latest completed `ci.yml` run as release evidence, so a red night
caused by a noisy neighbour would gain the power to veto a release.

## 2026-10-25 — what claims hygiene meant once the README's commands checked out

**Ambiguous:** EVI-1 asks for "claims hygiene" in a README whose commands, endpoints and flags
were already believed correct. The open question was what is actually left to reconcile, and
whether a figure with no committed artifact is a false claim or merely an unverifiable one.

**Chosen:** treat the code as ground truth and check every falsifiable claim directly rather than
trusting prose, then classify what fails into *false* (fix now) and *unverifiable from a clone*
(disclose now, make checkable in EVI-2). Verified this way, on `main` at 977b812:

- all 26 `weft-bench` flags used in the README are declared in `weft-bench/src/*.rs` string
  literals — no invented switch;
- every `/api/v1/...` path the README calls is registered in `weft-server` (including the
  `/api/v1/storage/{aspect}/...` family, which the README only shows through the `price` aspect);
- every relative markdown link resolves against the tree, and the two anchors my slug check
  flagged (`#physical-types--columnar-storage`, `#where-it-doesnt-pay-off`) are artefacts of
  approximating GitHub's slug rule for `→` and `'`, not broken links;
- **no measurement artifact of any kind is committed** — grepping the tree for the quoted figures
  (`218,963`, `118,238,781`, `4.22`, `56 µs`) returns nothing outside `README.md` itself.

The fourth point is the only place the README over-claimed: its Performance preamble said every
number is "named so you can re-run it", which is true of the *bench code* and false of the
*figure*, because the schema-v23 manifest and result JSON behind it live on one workstation. The
preamble now says which figures carry a `Source:` bench path, which are dated local measurements,
and that the per-figure registry is EVI-2's job before 1.0. The numbers themselves stay: they were
measured, they are dated, and deleting them would trade an honest caveat for a lost fact.

**Method note:** four read-only sub-agents were dispatched to audit README ranges in parallel and
every one came back dead — two on the 30-minute hard limit, two on the five-minute no-output
watchdog, having written nothing. The checks above were run in-session as targeted greps in four
calls and cost a fraction of that. For a bounded claim-vs-code audit of one known file, do it in
the parent; sub-agents earn their keep on open-ended search, not on a checklist.

## 2026-10-25 — the Linux release baseline is a container image, not the runner

PAC-2 ("Portable Linux binaries on a glibc 2.28 baseline") is a title and a checkbox: its
slice body was deleted from the roadmap in the upstream restructure, so what "portable" was
owed had to be chosen. Two questions, both decided here.

**How old is old enough.** Settled on glibc 2.28 — RHEL 8, Debian 10, the 2018/2019 enterprise
host — rather than the 2.17 of the CentOS 7 era or the 2.34 that an Ubuntu 24.04 link happens
to produce. 2.17 would have meant a CentOS 7 image that has been off security support for
years, and its toolchain is older than the workspace's MSRV; 2.34 would have changed nothing,
because that is what v0.1.0 already shipped and what the README had to apologize for.

**Where the change lands.** Inside the existing `build` job's two Linux legs, with the asset
names unchanged, rather than a second matrix producing `*-portable` archives. Two artifacts
claiming one target is how a release ends up publishing whichever the checksums job happened
to sort first, and the `dist` profile is already LTO over one codegen unit, so doubling the
Linux builds would have been the expensive way to be ambiguous. The legs now run in
`quay.io/pypa/manylinux_2_28_{x86_64,aarch64}:2026.10.25-1`, arch-matched to the runner so the
no-cross-toolchain rule of this job still holds.

**Rejected:** shipping a `-portable` asset set alongside the current one (above); and a
musl-static build, which would answer the question with a bigger question, since a static
binary cannot dlopen and the store is not the only thing a deployer might want to load.

**The floor is asserted from the artifact.** `scripts/check-glibc.sh` parses the dynamic
symbol table (the `.gnu.version_d` section header, walked byte by byte with a struct unpack,
because no binutils version in CI is guaranteed to print symbol versions) and fails above the
floor; it runs against the built binaries in the same leg. Measured here before pushing: an
x86_64 dev-profile build of `weft-tui` and `weft-server` in the pinned image yields 2.28 for
both, interpreter `/lib64/ld-linux-x86-64.so.2` — the dev profile is what a container build
reachable from this machine costs, and the CI assertion is what governs the shipped
dist-profile artifact.

## 2026-10-25 — slice implementation comes back to the main session; delegates audit only

Seven delegated runs in a row died on this machine: three at a 30-minute wall (two after
1.5M and 2.5M tokens, one having produced 41 tool calls of a ten-file slice) and four killed
by a five-minute no-output watchdog at zero tokens, including a run whose task text was
deliberately cut to a tenth of the size to stay inside that watchdog.

**Chosen:** the main session implements slices, and delegates are used only for read-only
audits, with an explicit scope cap and an instruction to print a line between tool calls so
the watchdog sees activity. The EVI-1 claim audit went that way and came back with nothing:
every number in the README's performance section resolves to a named benchmark, which is what
the roadmap asked for — a finding that took reading, not compute.

**Rejected:** shrinking implementation tasks to satisfy the no-output watchdog. A slice that
must compile a fourteen-crate workspace cannot stay silent for less than five minutes, so the
limit does not fit the work; shrinking the task to fit it would only produce a smaller task
that still cannot be done in the time allowed.
## 2026-10-25 — what `--scale` was allowed to become

**Ambiguous:** EVI-9 asks for "block missingness and scale presets" and names the ladder
`--scale small|medium|large|huge` "for every workload". Written literally that is a fifth rung
nobody can run and a claim about workloads that have no size knob, so both halves had to be
narrowed, and the narrowing is a decision rather than an implementation detail.

**Chosen — the ladder.** Four rungs: `tiny|small|medium|large`, as value budgets 65 536 /
524 288 / 4 194 304 / 16 777 216. `Small` is the rung a caller gets for the word "default": it is
the same budget the flags described before presets existed (16 series × 256 blocks × 128 rows),
though not the same shape — a preset derives its series count from the cap, so `--scale small` is
64 × 64 × 128. The budget is what a rung promises; the shape is in the profile name either way. `huge` was dropped rather than implemented: the next rung is 67 108 864 values,
about 537 MiB of column data before the store's own overhead, which is not a size a benchmark gets
re-run casually and is past the point where the number still means "the machine", as opposed to
the page cache. The ladder already spans three orders of magnitude, and `tiny` was added because
the thing a preset is for — "give me the same shape, smaller" — had no rung below the old default.

**Chosen — which workloads.** A preset expands into the *size* knobs of the workloads that generate
their own data (`--sparse`, `--gap-fill`, `--downsample`). The interpolation profiles keep their
own shapes: their point counts are the experiment, not a knob to resize, and silently scaling them
would compare two different experiments under one name. `README.md` and `ROADMAP.md` say this in
those terms instead of claiming "every workload".

**Chosen — a preset fills, never overrides.** A size knob left at its default takes the preset's
value; a knob the caller typed is kept even where the preset would have chosen differently. The
cost is stated in `weft-bench/src/scale.rs`: a knob typed at exactly its default value is
indistinguishable from one left alone, so it gets overwritten. The alternative — refusing
`--scale` alongside any size flag — would make the common case ("large, but with 64-row blocks")
impossible.

**Chosen — grid shape, and what the profile name says.** `--sparse` derives series as
`sqrt(values / rows)` clamped to 64 and gives the remainder to blocks, so a bigger preset deepens
the table past the cap instead of transposing it; 64 is where a "wide table" stops being
distinguishable from a tall one. The preset's name is written into the profile even when it filled
nothing (`sparse-3x5x8-d20-large`), because the profile's job is to say what was asked for; the
grid dimensions are in the same name, so nothing is hidden.

**Chosen — what the preset's name is good for.** The preset is appended to the profile name of
every workload it sizes (`gap-fill-tiny`, `downsample-medium`, `sparse-64x4096x64-d20-large`),
not just the new one: `compare` matches runs by profile name, so before this a gap-fill at 3 300
points and one at 486 660 were the same measurement to it, and the second read as a 100×
regression. A workload a preset does not size gets no suffix, since there is nothing there to
record.

**Rejected:** publishing a measured figure for observed missingness in `README.md`. The default
grid's holes are a property of a seed and a density, and the number anyone re-runs differs the
moment either moves; the report already prints the observed value, which is the artifact a claim
should point at.

## Needs human

- Anything that creates a git tag or GitHub release, or publishes to crates.io (PAC-19, FRE-18,
  W11/W13/W15 tag approvals, PAC-4 follow-ups): prepared, not executed.
- Steps the roadmap marks *(owner)*: private vulnerability reporting, prerelease deletion, the 10 Gbps
  port move, privileged power-loss validation, narrative approvals.
- `~/.cargo/config.toml` on this machine sets nightly-only `rustflags = ["-Z","threads=8"]`, so every
  `+stable` invocation needs `RUSTFLAGS=""`; a human should decide whether that stays.
- Who owns **FRE-14c**. Two agent loops were told, separately, that the Legacy lane is theirs; the
  work is pushed but unopened. See "how two agent loops divide the remaining waves" and issue #125.
- Whether an agent merges a green 1.0-buildout PR, or stops at "open and green". The 2026-10-25
  entry below asks this and is still unanswered; issue #125 restates it with the merges taken since.

## 2026-10-25 — what a claims registry has to key on to survive an edit

**Ambiguous:** EVI-2 asks for a "claims registry and markdown claim/link lint", and the README
quotes figures in prose, in tables, and again in a later architecture section. What does one entry
describe — a number, a sentence, or a measurement?

**Chosen:** one entry per **measurement paragraph**, keyed by a `<!-- claim:id -->` marker placed
beside the paragraph in the README, with the entry listing the figure *text* it vouches for. Keying
on a number fails the first time someone reformats (`56 µs` → `56us`) and silently passes when a
figure is deleted, because nothing forces the entry to be removed. Keying on a paragraph makes the
pairing bidirectional: the lint fails if a marker has no entry, if an entry has no marker, and if
the figure text no longer appears within 12 lines of its marker — so a reworded sentence must
update the registry, which is the whole point. `*Source: [...](path)` lines are cross-checked
against the registry so a new bench-cited paragraph cannot ship unregistered, and every relative
link in the README resolves against the tree.

**Rejected:** a Rust subcommand in `weft-bench` — the check reads markdown and JSON, needs no type
information, and runs on a runner that already has `python3`, so a subcommand would have added a
compile step to a documentation gate. And machine-checking GitHub heading anchors: EVI-1 already
found that approximating GitHub's slug rules for `→` and `'` produces false positives, so anchors
are out of scope and the script says so rather than quietly skipping them.

**Not claimed:** eleven figures are registered, not every number in the repository — the registry
covers the README's Performance section plus one dated observation in the backup-daemon note. The
lint's `*Source:*` rule is what pulls future figures in; it is a ratchet, not a completed census.
## 2026-10-25 — who clicks "merge", and what happens when the rules are silent
**Ambiguous:** `AGENTS.md` tells this project's agents to "work on branches and open pull
requests; never commit directly to main", and also never to stop and wait for approval. It says
nothing about who merges. Slices #105-#112 were squash-merged by the agent on green CI, which made
an agent-side convention look like an authorised workflow.
**Chosen:** stop at "PR open, all checks green", report that state, and leave the merge to a human.
Merging is the one step that is awkward to undo, so the absence of an instruction is read as the
absence of authority rather than as permission by omission; the "don't wait for approval" rule
covers *ambiguity about how to do the work*, not permission to take an irreversible action nobody
asked for. Open PRs are not parked work: the review surface exists, CI has run, and a human can
merge in one click.
**Rejected:** continuing the #105-#112 practice. Repetition inside one buildout is not authorisation,
and it quietly made the agent both author and approver of every change to `main`.
**Needs human:** confirm which of the two this project wants for the rest of the 1.0 buildout.
Current queue: #113 (EVI-9 `--sparse` missingness, `--scale` presets) and #114 (EVI-2 claims
registry + claim/link lint), both waiting only on CI and a merge decision.

## 2026-10-26 — what the artifact pipeline is, and what it publishes (EVI-4) — FOR MERGE REVIEW

**Ambiguous:** the 1.0 plan says only that D-S21's crash matrices are "artifacts
published via EVI-4" (§5) and that W2 runs "EVI-2 → EVI-4" (§6); it never says
what the pipeline publishes, where it lives, or how it connects to the claims
registry EVI-2 built. EVI-4 on its own is a one-liner with no acceptance shape.

**Owner best-guess decision (2026-10-26, Justin: "use your best guess based on
the roadmap and docs").** This slice implements that decision; if merge review
disagrees, the decision is revisited here, not by re-blocking silently:
1. Scope is generalizing EVI-5's nightly bench-report publication to the CI
   bench gate: bench jobs upload BenchReport + manifest + compare verdict as a
   `bench-report` Actions artifact, same shape as nightly, 90-day retention.
2. Claims wiring: each bench/claims.json entry gains an artifact reference
   (artifact name + publishing workflow); check_claims.py validates the
   reference shape and that the named workflow publishes it. verify.sh
   unchanged.
3. D-S21 crash matrices are a FUTURE consumer of this pipeline, not part of
   EVI-4 (D-S21 does not exist yet).
4. Release binaries out of scope (release.yml already publishes them).
5. The README "run artifacts are not committed" STANDS — artifacts live in
   Actions storage, never committed files.

**Chosen, and one correction to the decision's wording.** ci.yml has no
bench-producing job today, and EVI-5 deliberately kept the regression gate out
of ci.yml (a red night must not veto a release through verify.sh's latest-ci
check), so there is no "CI bench job" to generalize onto yet. What this slice
generalizes is the publication itself: `scripts/bench/stage-artifacts.sh` is the
pipeline — one flat staging directory, report + manifest + verdict, artifact
name `bench-report`, 90-day retention — and bench-regression.yml now stages
through it instead of an inline loop. Any future bench-producing job (D-S21's
crash matrices, EVI-6's ingest workload reports) publishes by calling the same
script and uploading the same artifact name; the contract is in the script's
header. Two consequences fall out of the refactor: staging now runs on every
path, including a first run with no baseline — the old inline loop sat after
the no-baseline early exit, so a first run staged nothing and the upload
failed on an empty directory — and the script refuses to stage when the report
itself is missing, so an empty artifact can never become a future baseline.

**Claims wiring, honestly null.** No figure the README quotes is currently
produced by a pipeline artifact: the nightly gate publishes the
nightly-interpolation workload, which no registry entry quotes, and the
reproducible entries quote benches the nightly job does not run. So every entry
gains the field as null rather than inventing provenance, and the lint now
fails on a missing field, a malformed reference, or a reference to a workflow
that does not upload an artifact of that name. A future entry that does claim
a pipeline artifact is machine-checked against the publishing workflow.

**Rejected:** a parallel artifact store or manifest (the registry's artifact
field points at the workflow that already publishes, it does not duplicate
it); committing run artifacts to the tree (the README's disclosure stands);
adding a bench job to ci.yml (no bench gate exists there, and a gating bench
job would re-create the release veto EVI-5 rejected — a non-gating upload-only
job is a later slice's call, and it would stage through this same script).

**Not claimed:** the nightly workflow change itself only exercises on its next
scheduled run; the shape is unchanged (same artifact name, same flat files,
same retention), so the next night's baseline download is unaffected.

## 2026-10-26 — what testing a release-only script is allowed to assume (PAC-2 follow-up)

**Ambiguous:** the floor checker had no test and no other caller than the release workflow, and a
test cannot produce an artifact built against glibc 2.28 on a host that has 2.34. What should the
suite feed it?

**Chosen:** stub `objdump` and `readelf` on a controlled PATH, with artifacts that are the ELF magic
and nothing else. The checker's whole input is what binutils prints, so a fixture that says what a
given tool prints exercises exactly the logic under test — including the `readelf` fallback and the
case where neither tool exists, neither of which a real artifact could reach on this machine. Each
case gets its own PATH directory, because `/usr/bin` holds a real `objdump`: a case that wants a
tool missing cannot put `/usr/bin` on PATH at all, and `bash` is named by absolute path for the same
reason.

**Rejected:** building real binaries against several glibc versions (that is a cross-compile per
case, and it tests binutils rather than the checker); parsing an ELF version section in the test to
compare against the checker's parse (two implementations of one idea, both unverified).

**Two failures the suite found, both on paths no CI job runs.** The script had landed mode 644, and
`release.yml` invokes it by path — the step that asserts the floor was "Permission denied", exit
126, on the first real release. And when neither binutils is installed the symbol dump fails, which
fell through to "requires no glibc symbol (static or non-glibc libc)" and passed the artifact while
saying nothing useful on stderr; the dump's exit status now decides. The mode fix went on the script
rather than turning the workflow into `bash scripts/check-glibc.sh`, because every sibling release
script is executable and the workflow's existing style is a direct call.

**Not claimed:** this tests the checker, not the image. That the pinned manylinux image really
produces 2.28 binaries is a measurement the release run itself makes, and it is recorded in the
2026-10-25 entry above.

## 2026-10-27 — how two agent loops divide the remaining waves

**Ambiguous:** the operator asked a second agent loop to work the same waves at the same time, and
two relayed divisions of the work then disagreed about one slice: one gives the Legacy lane (FRE-14)
to this loop, the other reserves "FRE-14-remainder" to the Muse loop after SOA-2, SOA-3 and SOA-4.
The Muse loop's convention is that whoever opens the PR owns the slice, which makes opening a PR the
act that settles an ownership question — and this loop asked on #124 before an answer existed.
There was no shared place to ask: `list_peers` returns no peers, the repository had no issues until
this entry, and both loops commit and comment under the same GitHub account, so a comment's author
cannot tell one loop from the other.

**Chosen:** claims are recorded where both loops and a human can read them, and a branch is not a
claim. Issue #125 is that record; this entry is why it exists.
1. A slice is claimed by an **open PR whose title names it** (`[EVI-4] …`), not by a local branch, a
   pushed branch, or a line in a design note. A pushed branch with no PR is not a claim, and its
   owner says so on the board rather than letting silence read as availability.
2. A slice reserved to one loop stays unstarted by the other even where it looks unclaimed:
   a reservation outranks a race. A stale reservation is argued on the board, not by opening the PR.
3. Where two relayed divisions conflict, the work stops at "branch pushed, question asked" and the
   operator arbitrates. Neither loop resolves a conflict the operator created by telling both of us
   the lane was ours.
4. Long local builds are announced, because `/mnt/deepmem/Development/Cargo Target` is shared and a
   workspace `build`/`test`/`clippy` run holds cargo's lock for tens of minutes. Holding that lock
   is a way of claiming a slice without saying so, so a run that is no longer needed is stopped.

**Rejected:** opening the FRE-14c PR to win the race; rewriting or force-pushing the other loop's
branches; treating a relayed division as authority over the reservation it names; keeping this
loop's gate run going because its result would be useful — it blocked the shared target directory
for an hour for a slice this loop may not own.

**Applied so far:** `w2/fre-14c-legacy-route` (one commit on `e0de4e1`, `weft-server`
`import-rows-mode` plus two store-scope variables, +776/−8) is pushed, rebased onto `e0de4e1`, and
deliberately has no PR. Its body is drafted at `.pi/pr/fre-14c-import-route.md` with the gate
results left as a placeholder rather than filled with claims. SOA-3 (`w2/soa-3`) and SOA-4 are
untouched. Nothing in `weftdb` is affected by the parked branch.

**Needs human:** who owns FRE-14c, and who clicks merge (both restated on issue #125). This loop has
followed "make and merge PRs as you go" and merged #98, #100, #101, #102 and #110 on green CI; the
2026-10-25 entry above concludes the opposite and is marked Needs human. Both loops should end up
behaving the same way, and which way is not for them to pick.


## 2026-10-27 — the 0.1.x migration route is a server subcommand, and only half of FRE-14c

**Decision:** `weft-server import-rows-mode <tree> --subject <s>` reads one subject of a 0.1.x
data directory into the open store. It is a subcommand of the server binary, not an HTTP route:
the read takes a filesystem path chosen by the operator, and the server's API is unauthenticated
by default, so a route would turn any reachable client into one that can read arbitrary
directories off the host. `weftdb` gains nothing — `weftdb::rows_mode` already surveyed, read and
imported; only the entry point was missing.

**Decision:** the store's scope becomes operator-settable (`WEFT_STORE_DATABASE`,
`WEFT_STORE_SUBJECT`, both defaulting to `default`) because a migrated tree must read back under
the name the operator already types for it, and because the server opens exactly one scope at a
time — which is why `--subject` is required and a many-subject tree migrates as several runs.
Both halves are validated together before the first file opens, so a half-set scope is an error
naming both variables rather than a silent store in `default`.

**Coordination — asked, not yet answered.** Two relayed divisions conflict
about the Legacy lane. One, relayed 2026-10-26, assigns FRE-14 to this session and SOA-3/SOA-4 to
the other loop. The other, from that loop later, reserves "FRE-14-remainder" for itself and says
does-not-start applies, under the rule "whoever opens the PR owns the slice". Because opening a
PR would itself take the slice under that rule, this branch stays local and unopened until the
operator says who owns it; the entry above records how the work was built, not a claim on it.
Whichever loop finishes FRE-14c, two parts remain unbuilt either way: the streaming read (an
aspect is still buffered whole before sealing, so a large subject is bounded by RAM) and the
`metadata.db` non-row state (named as "stays behind" here, which is honest but is not a
migration). Both stay unticked in ROADMAP.md.

**Not claimed:** no unit conversion happens. 0.1.x's rows-mode path stamped milliseconds where
its batch path kept seconds, and an aspect already declared in the other unit is refused with
both schemas printed rather than rescaled, so nothing here says millis became seconds.

## 2026-10-27 — FRE-14c's remainder: the streaming read and the legacy catalog migration — FOR MERGE REVIEW

**Ambiguous:** the 1.0 plan names the Legacy lane only as "FRE-14" (§6, W2), and the
CHANGELOG's remainder sentence names three parts — the operator route, "the numbered
migrations that register `metadata.db`'s schema", and "a reading of a tree too big to
hold one aspect in memory" — without saying what the migrations migrate, or into what.
The operator route landed as PR #128; the other two are this slice, and the second one
needed a design before it could be a tick.

**Chosen — the streaming read.** `import_into` no longer holds an aspect: the encoding
survey streams the aspect's rows through a cursor (one pass for the scale and the
scale-free candidates, a second for the scaled candidates once the scale is known),
and the seal streams them again in chunks of `rows_per_segment`, extended past any
group of rows sharing the chunk's last instant. The decision is exactly
`recommend_encoding`'s at tolerance 0, because each candidate's exactness is a
per-value property — "every value exact" in a stream is the same fact as "the column
encodes exact" whole — and a test pins the survey against the whole-column call on
ten value sets, including the empty column and the overflows past `i64` and `i128`.
What one chunk holds is `rows_per_segment` rows plus one instant group, which is the
bound the instant-group rule forces: a group seals into one segment or not at all.

**Chosen — the catalog migration.** Numbered migration `0003_fre14_legacy_catalog`
(layout 2, so no layout bump) registers a `legacy_catalog` table in the store's own
`metadata.db`, and the import writes one row per aspect it carried: the catalog's
aspect row (legacy id, resolution, creation time, recorded span, compression config)
under the store's scope. The catalog is now read by `discover` — a `metadata.db`
without the `database`/`subjects`/`aspects` tables, or with a cell of the wrong type,
refuses as `NoCatalogSchema` before any store opens — because the migration cannot
carry state it cannot read. What stays behind, with reasons: the catalog's
transaction log (1.0 does not replay 0.1.x's log; the import record in `store_meta`
is the transaction this migration records), the catalog's database and subject rows
(they reduce to the scope the store already registers on open; their ids, creation
times and host path have no 1.0 consumer), and the per-aspect dictionaries and
pipeline/correlation/pattern databases, as before. A series directory with no catalog
row still reads — the directory names the series and `measurements.db` holds it — it
just gets no catalog row in the store: the rows are authoritative, so a missing
catalog row degrades provenance rather than stopping the read.

**Not claimed:** the operator route is untouched by this slice — it is PR #128's, on
main already — and no unit conversion happens, as the 2026-10-27 entry above says.

## 2026-10-27 — what quality pruning is allowed to skip, and where (ROADMAP 4.3)

**Ambiguous:** 4.3 asks for "all-null segment/page skipping" in the read path, and the
primitives for it already exist (`prune_present_by_time` in `weft-physical-type/src/segment.rs`
and `src/catalog.rs`, `prune_present_pages_by_time` in `src/page.rs`) — but with test callers
only. They cannot be called as they stand: they work on a resident `Segment` or page index,
while the store decides what to read from SQL over `segment_index.db` and then streams values
out of a frame's blocks. "Wire it in" therefore does not name a call site; it names a decision
about which readers may treat a null row as "nothing to return".

**Chosen (grounded in the doc comments' own rule — a null row must be part of the answer for
skipping to change results rather than cost — and in the three readers whose semantics differ
on exactly that point):**

* **Pruned:** `read_point` and `read_points` (single timestamp, timestamp set). Both already
  answer "no measurement" when the row is absent, so a null row and an absent row are the same
  answer. New `SegmentIndexStore::prune_present_descriptors` mirrors
  `SegmentIndex::prune_present_by_time` in SQL (`null_count < row_count` in the WHERE clause),
  and the readers additionally skip an `is_all_null()` descriptor — which also keeps them off
  `read_exact`'s hard error on a zero-row store, an error that is a bug at a point read, not a
  diagnostic anyone asked for.
* **Pruned, inside a frame:** fully null frames and fully null pages, tested on the persisted
  page stats rather than by decoding. The frame's single-block and paged forms stop asserting
  an exact row count, because not reading a page is the point of the change.
* **Not pruned:** `read_time_range`, `downsample_range`, the ingest-side bucketing callers, and
  `read_segment_instant`. The first three *return* null rows, so pruning would drop rows a
  caller is owed. The instant reader is last-writer-wins: skipping a segment can change which
  segment wins, which is 4.6's out-of-order reconciliation question, not 4.3's.

**Consequence:** the 4.3 box is ticked with that scope stated in `ROADMAP.md` rather than
silently, because the honest reading of "wire the test into the read path" is "into the readers
where it is sound", and a later reader should see that the range readers were considered and
declined, not missed. No format change: `STORE_FORMAT` stays 2, since `null_count` was already
in the persisted stats and the segment index.

## 2026-10-27 — what this loop did not build, after rebasing onto a moved main

**Ambiguous:** the operator's wave list gave this loop three items — quality pruning (4.3), the
seal index write's `Statement::n_change()` accounting (Phase 3), and `GET /bench/runs/:id`. By
the time the pruning slice was written, main had moved twice (#132, #133), and two of the three
items were no longer this loop's to do.

**Chosen:** the `n_change()` accounting was **not** rebuilt — #133 closed it (the seal's
`InsertNew` commit is wrapped in a `control_plane.index.insert` span whose `rows_changed` comes
from the `TxnApplied` the commit already returns; `weftdb/tests/seal_index_span.rs` asserts it,
`ROADMAP.md:53` records it). Re-implementing it would have collided with a shipped design over
metric naming for no behaviour change. `GET /bench/runs/:id` was **not** built either: main now
records it as deferred pending the exposure matrix and auth work (SEC-6, SEC-8), and because no
artefact on main carries a run id, building the route would force the run-naming decision that
main deliberately left open. That is logged as needing a human call on security ordering rather
than quietly implemented against a recorded decision. The 4.3 slice was kept, rebased, and is
the only thing this branch proposes. PR #128's red CI (`settings.rs` count and tier-list
assertions after two new store env vars) was diagnosed and reported on the board rather than
fixed on another loop's branch, per the one-writer-per-branch rule; it shipped as #133.

## 2026-10-27 — gating a Rust change while the runner is saturated

**Ambiguous:** AGENTS.md says run the project's tests and checks before claiming anything works,
but the CI-mirroring gate for this slice (`weft-physical-type` + `weftdb`, both feature
configurations, the denied clippy set, `nightly fmt`) did not finish: it timed out at 4200 s and
again at 9000 s, with only the `weft-physical-type` steps reaching an exit line.

**Chosen:** keep the gate's scope identical and change how it is run, rather than claim a pass
that has no run behind it. The runner is saturated by the operator's `strata` inference server
(264 % CPU, ~85 GB RSS, load average ~150 on 16 cores, running for hours), and both agent loops
build into one shared `CARGO_TARGET_DIR`, so builds queue behind each other. Every cargo now
runs `nice -n 19` with `-j 2` (after the subcommand — `cargo +1.99.0 -j 2 test …` is rejected by
the top-level parser, which cost a whole run), test threads drop to 2, and the script takes
`QPRUNE_FROM=<n>` so a resumed run keeps the steps already paid for. It also exits non-zero when
any step fails: a run whose every cargo step returned 1 had itself reported 0, which reads as a
pass in a completion notification. Three process lessons went into the same script:

* the log path was hardcoded, so a second run truncated the first run's log mid-analysis and its
  results looked lost — logs are now per-run (`QPRUNE_LOG`);
* **editing the gate script while it is running corrupted the run.** bash reads a script lazily,
  by byte offset, so a rewrite made the running process stop after 2 of 5 steps and exit 0. A
  resumed run now executes a snapshot copy under `/tmp`, never the live file.
* the runner's default nightly could not build the vendored patch lane at all, which the other
  loop has since closed by pinning `basic-automation/patches` at `c611862` and dropping
  `Patches` from `rust-toolchain.toml`, so the release toolchain `+1.99.0` is the one to gate with.

One correction to a claim made earlier in this entry's draft: the recurring `patches/branches`
cascade finding was **not** stale-and-gone. `patches/branches/src/lib.rs` is a real vendored crate
(`Cargo.toml:154` patches `branches = { path = "patches/branches" }`), and a fresh probe gives LSP
clean plus one ast-grep style finding at `patches/branches/src/lib.rs:65` (`redundant-unsafe-function`:
`unsafe fn` with no `unsafe` block inside). It is left alone: rustc does not flag it, both
workspaces' `cargo check` are clean under `-D warnings` on both toolchains, the file is vendored
upstream code whose whole purpose is to differ from `branches` by one line, and the patch lane is
the other loop's.

**Result:** the gate passed in full on the rebased branch — `weft-physical-type` 307+5+4+3 and
344+7+4+3 (`--all-features`), `weftdb` 423 lib plus 26+17+8+6+2+2+1+1+1 integration and 1 doc, the
`bitsliced-codec` configuration 424+1, clippy `-D correctness,suspicious,perf` over both crates'
`--all-targets`, and `cargo +nightly fmt --all -- --check` — every step `EXIT=0`, no error lines.
(The sentence above was first written while that run was still going, which is how it came to be
unverified at the time; the numbers are its terminal output, from the seventh run of the script,
background task `bd2e7b318`, against commit `40e61f7`.)


## 2026-10-28 — extend the open pruning PR rather than open a second store-lane slice

**Ambiguity.** ROADMAP 4.3's pruning item was ticked as landed for the point-read path, and 4.6's
"reconciliation — configurable upsert keys + skip-identical" is the last unchecked W3/W4 store item
whose table already exists (`ingest_ledger` in `segment_index.db`, `weftdb/src/types/migrations.rs:334`,
provisioned with a `fingerprint` column and touched by no code). The board also carries a store-lane
hold from `w2/fre-14c-legacy-seal`, whose work is in the seal path.

**Choice.** Wire `downsample_range` into the quality prune first, as a second commit on the open
pruning branch, and leave the ingest ledger for a slice of its own.

**Why.** The ledger's whole value is atomicity with the seal: the fingerprint check and the ledger
insert have to live inside the same commit transaction as `InsertNew` (`weftdb/src/types/index_txn.rs:135`),
which is exactly the region the legacy-seal lane is editing. Extending my own branch touches only
`downsample_range` and its tests — files no other lane is in — and it finishes the roadmap item
instead of leaving half of the read path on the table. Sequencing beats overlapping here: the ledger
slice starts after #134 merges.

**The boundary, which is the actual decision.** A null row is part of a range read's answer and not
of a bucket's. `read_time_range` returns rows, so pruning an all-null segment would delete timestamps
the caller asked for; `downsample_range` folds values, and a row carrying no value enters no sum,
count, min, max, first or last — and a partial holding nothing is dropped when buckets merge, so a
window covered only by nulls answers empty rather than emitting a run of empty buckets. That last
point is what makes the prune safe, and it is pinned empirically (`an_all_null_segment_changes_no_bucket_of_a_downsample`
compares against the single-pass `reduce` at two resolutions over seven aggregations, including a
window the null segment covers alone) rather than argued from the merge code.

**Also on the record.** `patches/branches` is tracked in this repository but is not a workspace
member, and its `src/lib.rs` does not compile on the current toolchain (`core::intrinsics::transmute`
is not a public path). Nothing in the workspace build touches it, so it fails silently for any tool
that scans the tree — pi-lens reports it as a blocker against any session in this directory. It
belongs to the patches lane; recorded here so the next agent does not spend a turn discovering it.

## 2026-10-28 — 4.1 time-zone and leap-second policy: POSIX fixed-day, stated rather than implemented

**Ambiguous:** roadmap 4.1's last open sub-item was the words "Explicit tz + leap-second policy" and
nothing else. No code, comment or test in the workspace said what an epoch column means when the
writer's offset is not UTC, or what happens on a calendar that inserts a second. The encodings
already behave in one specific way; the item asked for that behaviour to be *said*, which left open
which of several defensible policies to state.

**Chose:** UTC epochs under the POSIX rule — a fixed-length day of exactly `86_400` seconds, no zone
stored, no leap second modelled — documented on `TimeUnit` and pinned by three tests. A leap second
is a *counted-twice second*, not a discontinuity, a smear or a renumbering, and the doc states
plainly that the crate carries no table, cannot answer a TAI-UTC offset question, and will see a real
leap second as one second long when interpolating or downsampling across it.

**Why:** every alternative needs data or a mechanism the store does not have. A TZif-backed policy
cannot be evaluated without a zone database the encoding crate must not depend on, and a stored
*instant* needs no offset to be correct — only a display does, which belongs above the store.
UTC-TAI smoothing needs a table that changes by announcement. Storing local times would make a
segment's meaning depend on the writer, the one thing a columnar file that outlives its writer cannot
afford. And delta / delta-of-delta over integer epochs are exact precisely because the tick spacing
is uniform; a leap-aware clock stutters exactly where a monitored series would then show a
1-second spike.

**Also decided — two arithmetic rules worth naming.** The unit arithmetic moves from call sites onto
the type (`TimeUnit::{nanos_per_unit, to_nanos, from_nanos}`). `to_nanos` is checked and returns
`None` past the `i64` nanosecond range rather than wrapping: `Seconds` at `i64::MAX` is the year 292
billion, and a wrapped value sorts into the wrong segment. The store's point-read scaling path keeps
its own *saturating* form, because there the total order is what matters and saturation preserves it
while a wrap does not — a read that gives up on the far tail of the key space still answers
everywhere a segment can exist. `from_nanos` truncates toward minus infinity so `[t, t + unit)` means
the same thing on both sides of 1970; truncating toward zero would put `-0.5 s` and `+0.5 s` in one
bucket while a full second either side sits elsewhere.

**Rejected:** a `chrono::DateTime`-typed column (chrono is a dependency of `weftdb`, not of
`weft-physical-type`, and the encoding must not pay for a calendar); a per-column zone string
(display metadata presented as data); leap smearing (rewrites real timestamps and is only correct
with an announcement).

## 2026-10-28 — EVI-10A: `MAX_KNOTS` stays a `pub` const

**Ambiguous:** ROADMAP's review item ("Reconcile with EVI-10A", 2026-10-19) left
`interpolate_range::MAX_KNOTS` open: keep it a `pub` const, turn it into an option,
or drop it. Meanwhile the server's `POST /api/v1/interpolate` gained its own input
cap (`InterpolateConfig`, env-overridable via `WEFT_MAX_INTERPOLATE_POINTS`).

**Chosen:** keep `MAX_KNOTS` as a `pub` const (4 Mi rows). The two caps bound
different quantities, so there is nothing to unify: `MAX_KNOTS` bounds the stored
rows one reconstruction reads (the knots handed to splimes, and the second copy
built for them — a memory bound), while the server's cap and splimes'
`max_points` bound the output grid. Unifying them would cap the wrong thing on
one side or the other. Dropping the const would hide a public, CHANGELOG-
documented bound for no benefit, and turning it into a per-call option would
invent library API surface no caller has asked for: a caller that needs a
different bound already has the documented escape — narrow the window or the
margin, or interpolate in pieces, which the refusal message itself says. The
private `stored_window_capped` seam stays test-only.

**Applied:** EVI-10A's ROADMAP line is ticked on this decision plus the
verification that the two other acceptance criteria already hold on `main`:
`SegmentStore::interpolate_range` / `interpolate_range_f64` are reachable
through the store lane via the `types::interpolate_range` child module (one
`mod` line in `types/mod.rs`), and a window over the cap is refused
(`a_window_with_more_rows_than_the_cap_is_refused`, including the margin
counting toward the cap). The review item itself is left unticked here — it
names this slice, and this entry is its answer — so the loop can flip it
alongside the line it reviews.

## 2026-10-28 — D-S11: the rollup moves to `segment_index.db`

**Spec:** 1.0-plan.md §6 W3 store lane (D-S10 → EVI-10A hook → D-S11) plus
ROADMAP line 269, and the code's own anticipation of it: the layout-2 migration
already creates `aspect_metadata` in `segment_index.db` "with metadata.db's
columns, folded inside the seal transaction from S11", and `finish_swap` called
its post-swap rebuild "the interim until S11 folds it into the swap itself".
No §5 amendment names D-S11, and the queue's open question — the exact source
rows — resolves from `metadata.rs`: the rollup is `AspectMetadata`, derived
from the segment index (`from_index`) or folded one descriptor at a time
(`folded`).

**Chosen:** the `segment_index.db` `aspect_metadata` row is the rollup, and
both writers fold it in their own transaction. A seal reads that row, folds its
fresh descriptor, and commits `InsertNew` + `SeqBump` + `PutRollup` (a new
`IndexOp`, `INSERT OR REPLACE` on the aspect key) in one `IndexTxn`. A swap
derives the rollup from its post-swap rows — the current rows under the commit
lock, with each plan op applied in memory — and commits it with the swap's ops,
so a swap that deletes members without replacing them (a narrowing squash)
stays exact, where a per-output fold would not. Rebuilds write the same row
through a single-op `IndexTxn`. `PutRollup` counts as guarded for retries only
because its callers hold the aspect's commit lock across the whole transaction,
retries included, so no other writer of that row can commit between attempts.

**Not moved:** `metadata.db` stays a control-plane file — the FRE-14
`legacy_catalog` table lives there — and `AspectMetadataStore` stays a public
type with its own tests, but the store no longer writes its rollup table and
`SegmentStore::metadata()` now says so. Readers (`aspect_metadata`,
`store_stats`, the fragmentation guard) read `segment_index.db`; the journal
replay keeps its re-derive as a no-op reconciliation for pre-S11 or clobbered
rows.

## 2026-10-28 — D-S11 fix: open backfills the rollup rows the replay never writes

**Correction to the entry above:** the journal replay does NOT reconcile
pre-S11 rows in general — it re-derives only the aspects its journal names.
A clean pre-S11 store (no journal rows) therefore opens with no
`segment_index.db` rollup rows at all, and `aspect_metadata` reads default
zeros for every aspect. The v0.1.0 compat gate caught it: a migrated `price`
aspect read 0 nulls against the 14 v0.1.0 counted, while its segment count,
rows and bytes — which come from the live segment scan, not the rollup — still
matched. The re-derive itself is sound: `rebuild_aspect_metadata` on the
migrated store returns the true 14, so the descriptors carry the null counts
and only the rows are missing.

**Fix:** open backfills exactly the missing rows (`backfill_missing_rollups`,
step 13 of the open), after the journal replay and only for stores this build
may write. Rebuilding every row on every open would rescan the whole index
for stores that already have rows; deriving on read would change the
materialized-rollup contract and pay a full index scan per read. Missing-only
keeps an S11-native open at one `SELECT` per aspect and makes a pre-S11 store
read correctly from its first open; a later seal or swap keeps the row current
by folding, as usual.

## 2026-10-29 — FRE-5: reduction tokens are exact-case — FOR MERGE REVIEW

**Ambiguous:** ROADMAP's FRE-5 box reads "Compute vocabulary, exact-case tokens and no silent narrowing". `Aggregation::from_token` folded case, so `MEDIAN`, `Min` and `SKETCH_P99` parsed, and v0.1.0 shipped that on the query string. The spelling slice (#239, merged 2026-10-29) carried the folding into the JSON body and described it as part of the closed item, which the FRE-5 box does not say.

**Chosen:** exact-case. The roadmap names exact-case as the deliverable. The wire tokens are the compatibility surface, and the nearest precedent, `ValueKind` (`weftdb/src/types/value_kind.rs`), is case-sensitive and pins that by test. The defect the FOUND bullet names was the multi-word spellings, not case. Trimming surrounding whitespace stays, since it is not a case rule.

**Cost:** a v0.1.0 client that sends `?agg=Min` or `agg=MEDIAN` gets a `400`. Pre-1.0 minor versions may break, and the CHANGELOG records it under Changed with the lowercase spellings to send. A case-folding shim on the query string alone would keep v0.1.0 working, but it would leave the query string and the JSON body on two parsers, which the spelling fix removed.

**Not changed:** `Fill::from_token` (`weft-reduce/src/fill.rs`) still folds case. It is a separate vocabulary (`--gf-fill`, the interpolation fill), and the FRE-5 box does not name it. It needs its own decision.

**Applied:** `from_token` matches the trimmed token exactly. Tests refuse `Min`, `MEDIAN`, `SKETCH_P99` and `Twa_Linear` on both surfaces. The README and the CHANGELOG are updated, and the FOUND bullet no longer claims letter-case folding. FRE-5's "no silent narrowing" clause stays open.

## 2026-10-29 — ROB-6: the one decimal domain is the plain-form bound (1,024 digits)

**Spec:** 1.0 plan §6 W3 + §3 ROB-6 row (absorbs FRE-8a; one decimal domain,
decision 10) + §4 C13 + §9. Decision 10 fixes the bound and its enforcement
points — "plain (non-exponent) form at most 1,024 digits, enforced at every
parse site and in `PhysicalType::encode`" — so there is no open question to
guess at; the choices below are mechanism only.

**Chosen:** `weft-physical-type` owns the domain: `MAX_PLAIN_DIGITS = 1_024`,
`plain_digit_count` (computed from the `(mantissa, scale)` pair, never by
materialising the plain string, so `1e1000000000` is measured arithmetically),
`check_decimal_domain`, and `parse_decimal` (parse + check in one call, so a
parse site cannot forget the check). Digit count is the digit characters of
`to_plain_string()` — sign and point excluded, leading fractional zeros
included — which is the form every sink writes (C13). Enforcement points:
`PhysicalType::encode` (every encoding, first, as the new
`EncodeError::DecimalOutOfRange`; this is what every seal flows through via
`encode_column`), the `.weftseg` readers (`read_decimal` and the
`BigDecimalText` value arm, as `WeftSegError::DecimalOutOfRange`, an
integrity fault — no in-domain writer produces one), the server parse sites
(JSON/CSV ingest values, ILP values after the line-protocol lift, the declare
`value_tolerance`, and the `lo`/`hi` value-range bounds — all answering the
§4 table's existing `400 decimal_out_of_range` code), and
`weft-arrow::record_batch_to_columns` (the Parquet/Arrow ingest lift, as
`ConvertError::DecimalOutOfRange`, classified to the same code). A seal that
hits the bound classifies through `SealError::Encode` to
`decimal_out_of_range`, not `value_not_representable`: the value is outside
the input domain, not merely unrepresentable under one aspect's encoding.

**Superseded, not kept:** FRE-8a's ≤1,000-significant-digit / |exp| ≤ 6,144
rule exists nowhere in the tree (it never landed as code) and is not
implemented alongside; a 1,100-digit value with few significant digits is
refused, and `1e1023` (one significant digit, exponent under the old cap but
1,024 plain digits) is accepted — both pinned by tests.

**Full f64 range:** the bound covers it, as C13 asserts — `f64::MAX` needs
309 plain digits, the smallest subnormal's shortest round-trip form
(`5e-324`, the form decision 11 says float inputs enter as) needs 325, and
the exact expansion of `f64::MIN_POSITIVE` needs 1,023, all pinned by tests.
One consequence worth naming: an *exact-expansion* subnormal (e.g. a
`BigDecimal::from_f64(5e-324)`, 1,075 plain digits, or an ILP float field
that lifts to one) is outside the domain and refused — its plain form is
what a sink would have to write, and the bound is on that form.

**Not in this slice:** the legacy rows-mode `weftdb` parse sites
(`database/outputs.rs`, `dictionary.rs`) read the legacy store's own text,
not the Storage-v2 input domain, and stay as they are; query-time numeric
parameters that are not decimals (timestamps, limits) are untouched.

## 2026-10-29 — D-S12: startup recovery, quarantine with TTL, fsck endpoint, /ready report — FOR MERGE REVIEW

**Spec:** 1.0 plan §6 W4 store lane (first) + §5 D-S12 amendment + §4 C5 fault
policy + §4 C1 (`segment_quarantine.series_id`) + ROADMAP line 141 (S12).
C5 is fully specified; the choices below are mechanism and two parameters the
plan names but does not number.

**Chosen:**
- Migration `0004_d_s12` (layout 2) adds `segment_quarantine.class`,
  `min_ts`, `max_ts`, `series_id`; fresh stores create the table with them.
  A pre-D-S12 record reads back as class `corrupt`, no time span, series 0.
- Classification follows C5's text, not X-FRAME's mapping, in one named case:
  a failure on a frame whose trailing CRC-32 verifies is post-CRC, hence
  suspect, whatever the decoder said; otherwise the causes C5 names outright
  (newer version, codec this build lacks, unknown critical extension, unknown
  codec tag) are suspect and everything else is corrupt. X-FRAME counts an
  unknown tag as integrity; quarantine follows C5 here, and the read path
  keeps X-FRAME's mapping. `WeftSegError` variants are the evidence: the
  classifier takes the typed source under `FrameFaultError`, never the message.
- Quarantine is one transaction per segment (record insert + row delete), the
  file then moves to `segments/quarantine/`; reads lose the segment the moment
  the row is gone. Suspect and corrupt files both move there; only the purge
  differs. A missing indexed file quarantines with no move. A row whose path
  escapes `segments/` is skipped, never moved.
- Identity mismatch re-binds the row to the frame's key: the aspect the frame's
  file name names, its generation and adoption order, and the frame's CRC
  trailer when the row's `frame_crc` is unset or disagrees. A frame file
  carries no aspect id beyond its name, so there is no deeper key to re-bind to.
- R10 seeds `aspect_seq.next_series_id` past the largest series id the aspect's
  rows hold (at least 1), once per aspect per pass, via a monotonic upsert.
- **TTL: 30 days from `quarantined_ms`.** The plan names the TTL but no
  duration; thirty days is this slice's choice, long enough for an operator
  triaging a bad seal, and recorded here so merge review can change one
  constant (`QUARANTINE_TTL_MS`). Only corrupt records ever TTL-purge; the
  TTL purge runs at the end of every repair pass, and an operator purge of one
  record is the only purge a suspect record ever gets.
- **Durability class:** `full` where a seal fsyncs file and directory entry,
  `process-crash-only` on Windows, where `StoreFs::sync_dir` is a no-op — the
  distinction the `sync_dir` documentation already draws for `/ready`.
- **previous_clean_shutdown:** `store_meta.clean_shutdown`, read at open and
  reset to `0` by the same open (a crash must not inherit a clean verdict);
  the server sets it to `1` after serving stops cleanly. A store no process
  ever marked clean reads `false`.
- **/ready:** `503` exactly when the last recovery pass reports
  `incompatible_store` (a newer-format suspect cause); poisoned stores stay
  ready, as before, and a store-less server stays ready. `/ready` carries
  counts only — quarantined, suspect, incompatible — never records. The fsck
  and quarantine endpoints carry the records themselves (operator routes,
  `operator_detail: true`).
- **Weft-Incomplete:** a response header, not a body field, set by a layer on
  stored-data reads (`GET /api/v1/storage/...`, POST downsample reads) while
  the store holds any suspect record. The store tracks that as an atomic flag
  refreshed by every quarantine change, so reads pay no query for it.
- **Report-only:** the startup pass on a newer-layout store counts what it
  would do and changes nothing — no record, row delete, move, seed, purge or
  rollup rebuild. The fsck endpoint always repairs; an operator asking for
  fsck is asking for the repair.

**Not in this slice:** adopting a decodable orphan (no row, no id to adopt
under — a later slice's job); ledger invalidation beyond the row delete (the
ingest ledger is keyed by batch, not by segment, so there is no per-segment
ledger row to invalidate — the quarantine record is the invalidation trail);
C5 verification by TAG-9/ROB-22, which the plan assigns to later slices.

**Salvage fix (parent, loop run timed out):** two pre-existing tests encoded the
pre-D-S12 contract and failed on the salvaged tree. `frame_fault.rs` expected
read-time `500`/`503` for damaged/newer frames; D-S12 moves the fault to startup
recovery, so the tests now assert quarantine at open (read `200` empty,
`/ready` counts, `Weft-Incomplete` for suspect only). The registry-count tests
in `routes.rs` hard-coded 55 routes / 49 templates; D-S12 adds 5 routes (fsck +
quarantine list, purge-expired, release, purge), so the counts are now 60 / 54.
Both fixes are expectation updates for spec'd behaviour, not behaviour changes.

**Salvage fix 2 (parent):** the third bar's 6 `weftdb` lib failures share three
roots. (1) The fsck re-bind built `[DeleteExpected, ReplaceExpected]` for a
same-aspect row; the delete removes the row the replace must match, so every
re-bind failed with "matched 0" (phantom commit, ambiguous commit, and the
agent's own `fsck_binds_an_unset_frame_crc`). Same-aspect re-bind is now one
guarded `ReplaceExpected`; only a move to a different aspect deletes then
inserts. (2) Startup recovery moves an undecodable orphan to
`segments/quarantine/`, so `frame_gens_on_disk` no longer saw the stray gen-7
frame and the I6 floor fell to 3; it now scans `segments/quarantine/` too, so a
quarantined frame still floors its generation. (3) Two stale test details: the
agent's own classify test paired sound bytes with a checksum error (sound
bytes are post-CRC by construction — the test now damages the bytes per case),
and the pre-v2 test counted 3 migrations while asserting 4 (now 4).

**Post-publish fix (parent, Windows CI on PR #459):** `test
(windows-latest)` failed on exactly one test: the agent's
`ready_reports_incompatible_store_and_reads_carry_weft_incomplete` asserted
`durability_class == "full"`, but the class is platform-defined —
`process-crash-only` on Windows, where directory fsync is a no-op — so the
expectation is now `cfg!(windows)`-gated. No production code changed; this is
the Windows-only test-expectation class the Linux bar cannot catch.
