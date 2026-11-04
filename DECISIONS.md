# Decisions

Judgement calls made while working the 1.0 buildout, where the instruction set was silent or
self-conflicting and the choice is not visible in the diff. Recorded so the next session does not
re-litigate them, and so a reader of a ticked roadmap item can see what it was decided to mean.

## 2026-11-03 — SOA-10: derived-read checks judge the stored downsample's buckets against the model, exactly (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6, W6) lists SOA-10 by name only — "Derived-read
checks" — and the queue's acceptance fixes one outcome: derived-read checks run
in the soak harness. Neither source says which derived reads, what the expected
values derive from, or where the verdict lives. What the sources do fix:
SOA-5's DECISIONS entry assigns reads to "the oracle and the derived-read
checks (SOA-6, SOA-10), which are what make a read result verifiable at all";
SOA-6's entry and module doc reserve the *endpoint-level* derived-read checks
to SOA-10 (the oracle core judges raw read-backs the caller supplies, and
performs no read itself); the stored downsample read
(`GET /api/v1/storage/{aspect}/downsample`) is the server's derived read over
stored data — its buckets are computed from the sealed segments, not stored —
with the streaming reductions `count`/`min`/`max`/`sum`/`avg` (weft-reduce's
`Aggregation` set); SOA-5 records that the ingest route seals only the numeric
subset of the named field into the aspect's `f64` schema; and SOA's standing
rules govern the shape (exact judgement, no invented tolerance or threshold; a
field added to `SoakRun` bumps `SOAK_SCHEMA_VERSION` on SOA's own line; records
carry no free text).

**Chosen (grounded as above, under the owner's best-guess authorization of
2026-10-26; if merge review disagrees, this decision is revisited there):**

1. **Library module, no read client, no CLI flag**: `weft-bench/src/soak_read.rs`,
   exported from the crate, on the SOA-6 precedent. The caller performs the
   downsample read against its server and maps each returned bucket into a
   `DerivedBucket`; the slices that own how a soak is launched are later ones.
2. **One derived read, at one-second buckets.** Grid rows are one second apart
   on whole seconds, so each row's bucket is its own timestamp and the expected
   bucket aggregates, across series, the values sealed at that timestamp. The
   expected side counts only the numeric cells (float/integer/unsigned — the
   subset SOA-5 records the server seals); boolean and text cells never reach
   the store and never reach an expected bucket. Sums accumulate in seal order
   (ticks ascending, series in grid order) so exact `f64` comparison is
   attainable, and `avg` is judged as the server's own reported value, not
   recomputed from the observed sum, so a server that sums right but averages
   wrong is still caught.
3. **Pass is zero mismatches, not a threshold.** Differences are classified
   `MissingBucket`, `UnexpectedBucket`, `DuplicateBucket`, or the first
   differing reduction (`WrongCount`, `WrongMin`, `WrongMax`, `WrongSum`,
   `WrongAvg`, checked in that order — one mismatch per bucket, the oracle's
   one-per-cell rule at bucket granularity). The mismatch list is capped at
   1024 (`MAX_REPORTED_DERIVED_MISMATCHES`); the count is not. A verdict that
   judged nothing passes vacuously, so publishers check `ticks_judged` beside
   it (the SOA-12 skipped-check rule).
4. **The verdict is recorded on the run record, and the soak schema takes v5.**
   `SoakRun.derived: Option<DerivedReadVerdict>` (`#[serde(default)]`, skipped
   when absent, so v1–v4 artifacts still deserialize) is set exactly once by
   `SoakHarness::record_derived_verdict`, only while `Running`, mirroring
   `record_oracle_verdict`. The verdict carries no free text, so redacted
   output emits it unchanged.

**Rejected:** judging the stateless compute endpoints (`POST /api/v1/downsample`,
`/api/v1/interpolate`) — those derive from a request body, not from what the
server stored, so they verify the reducer, not the soak's server; a tolerance-
based float comparison (the sealed values and their seal-order sums are exact,
so a tolerance would be an invented threshold); and folding the point-at or
range raw read-backs in here (those are the oracle's raw judgements, SOA-6 —
the derived surface is what was missing).

## 2026-11-03 — FRE-12b: what the scope pin pins, where the check lives, and what `/api/v1/version` may say (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the plan gives FRE-12b one line — "Scope pin, `GET /api/v1/version`,
incompatible-store mapping and exit code" (`ROADMAP.md:343`). It does not say what the pin
pins, which lane owns the check, whether a disagreement refuses a start or only reports it,
or what the version route must answer. The exit table (decision 4, C7) has named "scope
mismatch" under 65 ever since PAC-7 wrote that table, and nothing on main ever produced one:
the sentence described a class the server could not emit.

**Chosen (best guess, grounded in the code on main, flagged for merge review):**

- "Scope pin" pins the scope the process is configured to serve — `WEFT_STORE_DATABASE` and
  `WEFT_STORE_SUBJECT`, each defaulting to `default` — against the scope the store root's own
  `STORE_FORMAT` marker records. That is the only statement a store makes about itself:
  `StoreFormat::scope` (`weftdb/src/types/store_format.rs:126`) is written by
  `StoreFormat::new_store` at creation and carried unchanged through every later rewrite by
  `settled_format` (`weftdb/src/store/open.rs:1813`), which spreads the marker read off disk
  and overrides only the layout numbers — so it is the store's founding scope, not the last
  writer's. It is also the pair `import-rows-mode` tells the operator to serve the migrated
  root under (`weft-server/src/bin/weft-server.rs:431` prints
  `WEFT_STORE_DATABASE=... WEFT_STORE_SUBJECT=...`), so the pin measures the thing an operator
  is already told to set, and a wrong one is exactly the mistake that message invites.
- The check lives in the server, not in the store. A Storage-v2 root is a container of
  `(database, subject)` namespaces, and opening one root under two subjects is a tested store
  feature (`weftdb/src/store/segment_store.rs:4468`); a refusal inside the open path would
  undo designed behaviour that other code and other tests depend on. What is actually broken
  is a *server* serving a namespace of its own store that holds nothing, which from outside
  the process is indistinguishable from a lost database — the one failure mode an operator
  must not have to diagnose from query results. So the pin is
  `weft-server/src/store_open.rs::pin_store_scope`, and the store lane is untouched.
- Refused, and refused by default, with `WEFT_STORE_SCOPE_PIN=off` as the way out. Report and
  serve anyway leaves the operator discovering the empty namespace by querying it, which is
  the failure this slice exists to remove; the workspace's 1.0 posture elsewhere is the same
  refusal-first choice — strict configuration (PAC-5b) and SEC-8's non-loopback-without-keys.
  The refusal names both scopes, the root, and the three ways out (set both variables to the
  store's scope, point `WEFT_SEGMENT_STORE_ROOT` at the root you mean, or turn the pin off),
  and startup exits 65 — the operator-action class, which is what `exit_code_for_store_error`
  has always said a scope mismatch is; this slice is the first to produce one.
- Checked *before* the open, through the public `weftdb::store_format::read_marker`
  (`weftdb/src/types/store_format.rs:715`), so a refusal leaves the root exactly as it found
  it, takes no root lock, and cannot be raced by whoever holds it. A root with no marker
  records no scope and is accepted: a fresh root has nothing to disagree with.
- One narrow edge is accepted rather than fixed, and recorded here because it is real. A root
  whose marker was lost while `store_meta` survived has no scope on disk; the open rebuilds
  the marker from `store_meta`, which mirrors no scope, and records the scope that open came
  with. The first start after such a loss therefore blesses whatever scope it ran under, and
  every pin afterwards measures against that. Refusing it would mean refusing every
  unmatched-marker recovery, a worse trade than the typo it would catch.
- `GET /api/v1/version` takes `Scope::Read`, not `Scope::Probe`. It names layout numbers, the
  store UUID and the scope being served, which is more than the anonymous probe surface should
  carry; `/health` and `/ready` stay keyless, and `/ready` keeps reporting service, version
  and store UUID, so a monitor needs no key and an operator with one gets the rest. The route
  table in `docs/reference.md` records the choice beside the route.
- The answer is built from process state alone — `Store::store()` and the scope startup
  captured — and never from a store read. The questions this route exists to answer (which
  build, what layout, which scope, why will it not start) are exactly the ones asked while a
  store is being refused, and a handler that read the store would fail on the very state it is
  being asked about. `/ready` reports a poisoned store for the same reason rather than failing.
- The served scope and the pin mode are captured once into `AppState`'s `ServingScope`, on the
  way to the open that consumes the environment, rather than re-read from the environment per
  request: a request must report what the process *is* serving, which is what the open did,
  not what a shell has exported since.

**Rejected:** the pin inside `weftdb`'s open path, for the tested multi-scope reason above; a
`scopes` list added to `STORE_FORMAT` so a root could record every scope it has held, which is
a layout-format change on a slice whose subject is reporting a format that must not move (the
release-pin job re-hashes those fixture bytes); `Scope::Probe` on the new route, which would
widen the anonymous surface for no operator benefit; and answering the route from a fresh
marker read, which would make it lie, or 500, precisely when it is most needed.

**Not in this slice:** any change to the `STORE_FORMAT` bytes or the migration gate (FRE-12a's);
the CLI's `--version` output, which PAC-7 settled; an HTTP status or flag for "the pin is off"
beyond the field the version route reports; a store-lane scope registry; and pin handling in
`import-rows-mode`, `restore`, `verify` or `admin`, which take `--database`/`--subject` or
their own flags and resolve no serving scope at all.

## 2026-11-02 — SOA-8: the failure deck is a validated card deck, recovery is exact, and the maintenance driver is a pure state machine (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6, W5) lists SOA-8 by name only — "Failure deck,
phases, recovery windows, SIGTERM verdict and the maintenance driver" — and
neither the plan nor the queue fixes the deck's contents, the phase set, what
a recovery window measures, what the SIGTERM verdict judges, or what the
maintenance driver drives. What the sources do fix: SOA-1's harness doc says
SOA-8's phases are a finer breakdown *within* `Running` and do not replace
the lifecycle state; the queue's acceptance fixes three outcomes — a failure
deck with phases and recovery windows exists, a SIGTERM verdict is produced,
and a maintenance driver drives the deck; the server's own code fixes the
failures and maintenance that exist (SEC-4/D-S14's graceful drain with the
`WEFT_SHUTDOWN_GRACE_SECS` grace and exit 0, a signal before serving exits 0
per plan C9, `/ready`'s `previous_clean_shutdown`, the store's maintenance
locks and busy/skipped sweeps, and ROB-3's daemons resuming reconcile,
squash and backup); SIGKILL is SOA-9's `crash_kill9` (W6); and SOA's standing
rules govern the shape (exact judgement, no invented tolerance or threshold;
a field added to `SoakRun` bumps `SOAK_SCHEMA_VERSION` on SOA's own line;
records carry no free text).

**Chosen (grounded as above, under the owner's best-guess authorization of
2026-10-26; if merge review disagrees, this decision is revisited there):**

1. **New pure module `weft-bench/src/soak_failure.rs`; no live server.**
   Like the oracle core (SOA-6), the deck, driver and verdict perform no
   signal, read or maintenance themselves: the caller applies what the
   driver's `DeckAction` names and supplies what it observed. The same
   deck and observations replay identically in tests.
2. **The deck is validated on construction.** Card ids are slugs (never
   free text, so no record needs redaction), ids are unique, every
   recovery window is non-empty, cards are in injection order, and a
   SIGTERM card is at most one and last (it ends the server). Kinds are
   `MaintenanceContention`, `ServerRestart` and `Sigterm` — the failures
   the shipped server has. `FailureDeck::standard()` deals contention,
   restart, SIGTERM, each recovering under a different sweep.
3. **Phases are per-card and strictly ordered:**
   `Baseline → Injection → Recovery → Verification → Complete`, entered
   through the driver, which refuses gated transitions (recovery must be
   observed and the card's maintenance run before verification; the
   SIGTERM verdict must be judged before the SIGTERM card completes).
   Phase records are recorded on the run record in order; the harness
   refuses out-of-order, interleaved or tick-regressing phases.
4. **Recovery windows are tick ranges judged exactly.** A card's window
   is `inject_tick..inject_tick + recovery_ticks` — the deck author's
   schedule, not a threshold this slice invents. Recovery observed
   inside the window recovered; at/after its end, or never, it did not.
   A card that calls for maintenance counts as recovered only when that
   maintenance ran during recovery: the driver runs the card's
   `MaintenanceKind` (reconcile/squash/backup/none) in the recovery
   phase, because a recovery in which the daemons never resumed is not
   the recovery a deployment performs.
5. **The SIGTERM verdict is the exact conjunction** of its observation:
   signal sent, exit code 0, drained within the grace the *caller*
   configured (the serve path's grace is an input, never a constant
   here), and the reopened store reporting a clean previous shutdown.
   Any missing observation fails — no vacuous pass.
6. **Records on the run record; soak schema v4.** `SoakRun` gains
   `phases` and `recoveries` (`#[serde(default)]`, skipped when empty)
   and `sigterm` (`#[serde(default)]`, skipped when absent), recorded
   set-once/in-order through the harness while `Running`;
   `SOAK_SCHEMA_VERSION` 3 → 4 on SOA's own line. v1–v3 artifacts still
   deserialize.

**Rejected:** a SIGKILL card (SOA-9 owns it, W6); recovery judged by a
rate or a deadline this slice picks (the window is the deck's, the
judgement exact); a driver that signals or restarts a real server (no
slice names a process client, and a pure driver is replayable); phases
as new `SoakState` variants (SOA-1 fixed them as a breakdown within
`Running`); and free-text card names or failure descriptions (slugs
keep every record redaction-free, as SOA-6/SOA-7's records are).

## 2026-11-02 — SOA-7: window digests hash the oracle's own encoding, and the self-test judges the judge (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6, W5) lists SOA-7 by name only — "Window digests
and the oracle self-test" — and neither the plan nor the queue fixes the digest
algorithm, what a window is, where digests live, or what the self-test runs
against. What the sources do fix: SOA-6's DECISIONS entry scopes this slice
explicitly ("Out of scope … window digests and the oracle self-test (SOA-7)",
"digests after the core they digest"); the oracle core's doc calls itself "the
pure core SOA-7's self-test exercises"; the queue's acceptance fixes two
outcomes — window digests exist in the soak harness, and the oracle self-test
runs against SOA-6's oracle core; and SOA-6's own rules govern the shape (exact
comparison, no invented tolerance or threshold; a field added to `SoakRun`
bumps `SOAK_SCHEMA_VERSION` on SOA's own line; verdicts and records carry no
free text, so no new redaction path).

**Chosen (grounded in SOA-6's core and rules, under the owner's best-guess
authorization of 2026-10-26; if merge review disagrees, this decision is
revisited there):**

1. **A window is a contiguous tick range; its digest is SHA-256 over a
   canonical point encoding.** New module `weft-bench/src/soak_digest.rs`.
   Each point is hashed as one line — tick, series, row, timestamp, and the
   value's format-exact wire form (`GridValue::encode_field`, SOA-4) — so the
   digest is over exactly what the workload wrote and the oracle compares.
   SHA-256 and lowercase hex match the manifest's existing digest form
   (the run manifest already carries `report_sha256`). The first line
   hashed is a domain separator naming the algorithm
   (`weftdb-soak-window-digest/1`): an encoding change is a new algorithm,
   never a silent change to what an old digest means.
2. **Two computations, one comparison.** `expected_window_digest` regenerates
   a window from the model alone (also exposed as
   `SoakOracle::window_digest`); `observed_window_digest` hashes a read-back
   in the oracle's own input shape (`&[(u64, Vec<ObservedPoint>)]`).
   The observed side is canonicalized (tick, series, row, timestamp, encoded
   value) before hashing, so read-back order is irrelevant; duplicates are
   hashed as many times as they occur, so a duplicated seal changes the
   digest — the same failure the oracle classifies as `Duplicate`.
   `WindowDigest::matches` requires the same range, point count, and hash.
3. **Digests localize; the oracle still judges.** A matching window states
   exactly what the oracle's per-cell judgement states for those ticks, at
   hash-comparison cost; a differing window is re-judged tick by tick with
   SOA-6's core to classify the damage. No tolerance, rate, or threshold is
   introduced — equality is the only comparison, per SOA-2/SOA-3/SOA-6's
   refusal to invent pass/fail numbers.
4. **Digests are recorded on the run record, in window order, and the soak
   schema takes v3.** `SoakRun.windows: Vec<WindowDigest>`
   (`#[serde(default)]`, skipped when empty, so v1/v2 artifacts still
   deserialize) is appended by `SoakHarness::record_window_digest`, only
   while `Running`, and a window that overlaps or precedes the last recorded
   one is refused — a window recorded twice would double-count its evidence.
   Adding the field bumps `SOAK_SCHEMA_VERSION` 2 → 3 per SOA-1's bump rule,
   on SOA's own line (§3). A digest carries no free text, so
   `SoakRun::redacted_json` emits it unchanged.
5. **The self-test is a pure associated function on the core it tests.**
   `SoakOracle::self_test()` builds its own small deterministic model (never
   the caller's — a misconfigured run cannot make it vacuous) and runs nine
   checks whose correct outcomes are known by construction: a faithful
   read-back passes exactly; each classified corruption (missing, wrong
   value, wrong timestamp, unexpected, out-of-grid, duplicate) is caught as
   exactly that kind, once; and the window digests agree with a faithful
   read-back and disagree after a dropped point. The report
   (`OracleSelfTestReport`) is `passed()` only when every check passed and
   at least one ran (the SOA-12 skipped-check rule). It performs no read and
   touches no server: a driver runs it before a soak, a publisher before
   trusting a recorded verdict. Check names are fixed constants of the
   self-test, so the report needs no redaction.

**Rejected:** a verdict field per window on the run record (the verdict is
the oracle's, recorded once by SOA-6; windows are evidence, not verdicts);
a non-cryptographic hash (the digest is published evidence — a collision
must not be able to hide corruption, and sha2 is already a weft-bench
dependency); hashing the serialized JSON of points (the wire encoding is
the format-exact form SOA-4 already pins, and JSON field order would make
the digest depend on serde rather than on the workload); and a self-test
that runs against a live server or the caller's run model (no slice names
a read client, and a self-test that inherits the run's configuration can
pass vacuously against the wrong expectations).
## 2026-11-02 — Gap fill and bucket widths on the downsample endpoints (#142, #143) (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** #142 names `?fill=` on endpoints of which the compute ones take a JSON body, not a
query string, and #143 says "freeze-track API decision first", because splimes' `Resolution` is
one step of a unit and every reducing surface is keyed by it. Chosen:

1. **The compute endpoint's `fill` is a body field**, like every other knob there; the ILP and
   stored-range endpoints take `?fill=`. The vocabulary is `weft_reduce::Fill::from_token`'s.
2. **The fill grid is bounded by the interpolation endpoint's output cap** (10M), since a filled
   grid is an output grid of the same kind; past it the request is a `400` that says why.
3. **A width is additive, not a new `Resolution`.** `BucketWidth` is a unit and a count;
   reducing at a width is reducing at the unit and merging each run of `count` partial buckets
   (`PartialReduction::coarsen`), exact because merging is. No existing signature changed: the
   `Resolution` APIs delegate to the width ones with a count of one, so a freeze decision about
   replacing `Resolution` is not needed and stays open. `.weftpart` sidecars still serve the
   unit, and the width coarsens what they return, so `grids_nest` is untouched.
4. **The JSON body names a width in a new `width` field** (overriding `resolution`), since its
   `resolution` is a typed enum; the query endpoints accept a width as their `resolution` token.
   A response's `resolution` is `"15 minutes"` for a width and unchanged for a unit.

## 2026-11-02 — What closes Phase 1's open bench lines (#140, #147, #148, #149, #150, #393) (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the five Phase 1 trackers are one-line issues, and the plan text behind them was
dropped from `ROADMAP.md` in 1228723 (recovered from 1228723^). Several of its phrases name work
another track owns, or describe the engine as it was in July. The owner asked for all of Phase 1
(2026-11-02, "Complete all of phase 1"). What each line was taken to mean, and where it stops:

1. **Failure tests (1.1) are the bench's, the crash matrix is not.** `weft-bench --failure` kills a
   child process (this binary) with `SIGKILL` while it seals batches (restart during ingest) or
   compacts (crash during compaction), and flips a byte of a closed store's segment (partial or
   corrupt segment), then gates the reopened store on exact read-back. That is the plan's 1.1 list
   less one item. D-S21's crash-matrix CI and SOA-9's `crash_kill9` soak stay W6's: this is a
   bench workload, not a CI matrix or a durability claim. **Network retry** is SOA-8's failure
   deck, claimed by the other loop's open PR (#502), and is left to it.
2. **Post-compaction reads measure the compaction the engine has.** §7.5 (query-consistent,
   cancellable compaction) is still open, but `SegmentStore::squash_aspect_to_target_rows` exists
   and is what `POST …/compact` runs, so `--compressed-query-compact` measures reads after it. On
   10M UCI rows it took 694 s to go from 9,769 to 153 segments, answered every window identically,
   and made small windows ~36× slower (1h p50 0.16 → 5.75 ms) while the year window held
   (30.1 → 29.5 ms): the read decodes whole 65k-row segments. Recorded as a result, not acted on.
3. **A native fill that answers another question is gated on shape, not values (1.2).** The plan
   asks for class A "where possible". Four engines' native fills (TimescaleDB, QuestDB, InfluxDB 3,
   IoTDB) average the knots per grid bucket and draw lines between the averages, without
   extrapolating; ClickHouse's `INTERPOLATE` carries the last value forward and puts the column's
   default (`0`) before the first knot. Holding them to WeftDB's line would fail them for not being
   WeftDB; racing them against it would compare different answers. So their gate is the grid's
   shape (`Correctness::check_shape`), and the report shows their unfilled instants, distance from
   the line and RMSE against the truth, never a win or a loss. DuckDB has no native interpolation,
   so no class A arm. Class C fetches every knot into the client and runs an `f64` line with the
   SQL arms' arithmetic (no fused multiply-add), gated like class B.
4. **#393's rules.** (a) The QuestDB arms already speak QWP both ways and say so; WeftDB's own
   Arrow egress is Phase 2's (Flight), not a bench change. (b) Each ASOF strategy is run and
   published: ClickHouse `hash` (default) and `full_sorting_merge`; QuestDB's optimizer default
   (`AsOf Join Fast` in `EXPLAIN`) and the `asof_linear` hint (`AsOf Join`). The memoized and
   dense strategies need a symbol key this keyless join does not have; `EXPLAIN` showed every
   other hint name leaving the plan unchanged. (c) is a precedent, not work.
5. **"Each engine's live settings" are the container's.** The run manifest records each engine
   container's image digest, command line and environment via `--container ENGINE=NAME`, with
   variables whose names look secret redacted. That is where the reference host's settings live
   (QuestDB and IoTDB take them from the environment, InfluxDB 3 and TimescaleDB from the
   command). Querying each engine's own settings table is five dialects for the same facts and was
   not done.
6. **Report surfaces (#149) are Markdown, CSV and Prometheus text, plus a Grafana dashboard.**
   The plan's `reports/` list was JSON, Parquet, HTML and Grafana; the dashboard needs a metrics
   source, which the Prometheus text is (textfile collector or Pushgateway).
7. **Anti-Goodhart (#150) is enforced where it can be checked.** `scripts/bench/check-separation.py`
   (in CI) fails on an engine crate depending on a benchmark crate or reading a `BENCH` variable.
   A rule flagging any string that names the harness was tried and dropped: weft-server's settings
   registry documents the bench's test-only variables without reading them. The other rules
   (publish losses, methodology first, customer workloads, correctness gates) already had a
   mechanism; HONESTY.md now maps each rule to it.
8. **External-engine adapters (#140) are `weft-xbench`'s six.** The plan's minimum set (ClickHouse,
   InfluxDB 3, QuestDB, TimescaleDB, DuckDB) and IoTDB are measured. VictoriaMetrics was "if
   monitoring matters", and WeftDB does not target monitoring, so it is not built; kdb+ stays out
   on licence grounds.

## 2026-11-01 — FRE-7b: immutable declarations, scale rules and schema read-back — the shapes the plan left open (FLAGGED FOR MERGE REVIEW)

**Grounded:** the 1.0 plan names the slice (§6 W5), fixes that widening goes through
FRE-7b's `widen_value_encoding` (§5 D-S19/D-S20 — the helper itself landed inside D-S19,
which recorded that this slice's other parts were not built there), lists re-declare
overwrite among the live defects (§1), and reserves `409 schema_conflict` in the §4 code
table. It does not pin the exact re-declaration semantics, the scale rules' shape, or the
read-back's fields. Chosen here under the owner's best-guess authorization (2026-10-26),
recorded for merge review and reversible there:

1. **Immutability with exactly one exception.** `AspectCatalog::declare` decides inside
   its transaction: no declaration → insert; the identical schema → no-op (the call
   stays idempotent, which rows-mode import and the legacy seal adapter rely on); a
   value-encoding widening with tolerance and timestamp unit unchanged → update of the
   encoding alone; anything else → the new typed `StoreError::DeclarationConflict`,
   nothing written. The widening predicate is `weft_physical_type::is_value_widening`:
   a change widens exactly when it moves to `BigDecimalText`, because that is the only
   move `widen_value_encoding` can produce (its other outcome is the current encoding).
   `ScaledI64 → ScaledI128` and `F32 → F64` are therefore refused as sideways moves,
   not widenings — no caller performs them, and D-S19's `ensure_declared` (the one
   widening caller) only ever widens to text. The HTTP surface maps the typed error
   through the existing `classify_typed` to the §4 table's `409 schema_conflict`.
2. **Scale rules at the declare surface.** A fixed-point encoding named without a
   `scale` stays a `400` (pre-existing); a `scale` named for an encoding that carries
   none (`f64`, `f32`, `decimal128`, `bigdecimal_text`) is now also a `400` instead of
   being silently dropped — a declaration must read back as it was asked for.
3. **Read-back returns the whole declared schema.** `AspectInfo` gains `scale`
   (omitted when the encoding carries none), built by one constructor
   (`AspectInfo::declared`) from the stored schema at all three sites: the aspects
   list, `GET …/{aspect}/schema`, and the declare response — which is now the schema
   read back from the store after the write, not an echo of the request, so a widening
   re-declaration answers with the widened schema. `PhysicalType::scale()` is the new
   accessor beside `name()`. Accepted declares keep returning `201`, including the
   identical re-declaration and the widening one: the status contract pre-dates this
   slice and nothing in the plan asks to split it.

## 2026-11-01 — SEC-8: the exposure matrix's exact cells, the proxied-header set, and how startup order is enforced (FLAGGED FOR MERGE REVIEW)

**Grounded:** the plan fixes SEC-8's four parts — startup ordering through the `startup.rs`
phase list (§2 L-MAIN, §6 W5), the exposure matrix (decision 7, absorbing PAC-5b's bind guard
per §3, with C8 naming the Docker shape), the auth-off guard, and metrics auth (decision 17:
"`/metrics` needs the metrics scope when auth is on") — and SEC-5/SEC-6 each recorded the
remainder as this slice's. The points the sources do not pin, chosen here under the owner's
best-guess authorization (2026-10-26), recorded for merge review and reversible there:

1. **The matrix refuses keyless non-loopback binds over TLS too.** SEC-5's gate refused only
   *plaintext* non-loopback binds and let TLS through ungated; decision 7 says "Non-loopback
   with no keys: refuse to start" with no transport exception, and encryption without
   authentication still exposes every route. So `PlaintextGate::check` (kept name, widened
   rule) now reads: loopback → serve; `WEFT_AUTH=disabled` → serve; no mounted keys file →
   refuse (exit 78, via startup's config-error path); keys + TLS → serve; keys + plaintext →
   serve only with `WEFT_ALLOW_PLAINTEXT=1`. SEC-5's integration test that started a keyless
   non-loopback TLS bind is inverted to a refusal, with a keys-mounted twin that starts.
2. **`WEFT_ALLOW_INSECURE_BIND` is dropped by never existing.** No code ever read it; SEC-8
   makes the drop explicit rather than implementing a reader: the name is not in the VarSpec
   registry, so PAC-5b strict configuration refuses it as an unknown `WEFT_` name (a settings
   test pins that), and `WEFT_AUTH=disabled` is the only spelling of the opt-out.
3. **Proxied = `Forwarded` or any `X-Forwarded-*` of the common set** (`for`, `host`,
   `proto`), presence only, values never read. While auth is off *implicitly* such a request
   is refused `403 proxied_request_requires_auth` (the §4 code X-ERR reserved; this slice is
   its first producer) on every route, probes included: a proxy in the path voids the
   loopback assumption decision 7's keyless mode rests on. That required telling the two
   offs apart, so `AuthConfig` gains a `Disabled` variant (SEC-6 had folded both into `Off`
   and startup re-read the env var to log the difference; it now matches the variant). The
   gateway deployment's proxied requests are served; with keys mounted, the key check is
   the guard and proxied requests authenticate normally.
4. **Metrics auth needed no new mechanism — so this slice verifies and pins it instead.**
   `/metrics` already carries the registry's `metrics` scope (SEC-3) and SEC-6's gate
   enforces scopes; SEC-8 adds the decision-17 test stated as such (anonymous 401, reader
   403, admin 200; auth off serves), rather than bolting a second check onto the route.
5. **Startup order is enforced by a source-pinning test.** `startup::PHASES` names the seven
   phases (`strict-config`, `hooks`, `signals`, `config`, `state`, `daemons`, `serve`); a
   test in `startup/mod.rs` reads the file's own source after `run`'s signature and asserts
   each phase's entry-point call appears in that order. Reordering a phase now fails CI
   unless `PHASES` and the module docs move with it. (The probe-only admin listener of
   ROB-23r, which §3 says also goes through this matrix, is W7 work and does not exist yet;
   when it lands it binds through the same gate.)

**Rejected:** a separate `exposure.rs` module duplicating the gate SEC-5 already owned (the
matrix is the gate's rule, widened in place); refusing proxied requests under
`WEFT_AUTH=disabled` too (decision 7 assigns that deployment a gateway, whose requests are
proxied by construction); and treating `X-Real-IP` as a proxy header (it is set by one
proxy family for its own use and is not part of the de-facto forwarded set).
## 2026-11-01 — ROB-10 "Admission I": per-class budgets the gate enforces (FLAGGED FOR MERGE REVIEW)

**Grounded:** the 1.0 plan names ROB-10 only as "Admission I" (§6 W5) and absorbs its parts
elsewhere: its per-route class table into SEC-3's registry (§3 SEC-3/SEC-6 row — landed as
`RouteSpec.admission_class` with classes `probe`/`query`/`write`/`maintenance`, the write class
charged byte-weighted inside the gate by D-S14, plan C10) and its knob registry into the VarSpec
registry (§3 PAC-5a/PAC-6 row). Issue #270 repeats the one-liner. What remained for this slice —
and what its queue acceptance states — is that admission *exists on the serve path* driven by the
registry's `admission_class`, with its knobs in the VarSpec registry. The points the sources do
not pin, chosen here under the owner's best-guess authorization (2026-10-26), recorded for merge
review and reversible there:

1. **The gate dispatches on `admission_class`, for every class.** Before this slice the gate
   charged only `Scope::Ingest` routes against the byte budget. Now: `probe` is never shed (its
   class rationale: a shed probe reads as a dead process); `query` is counted against an
   in-flight cap and shed first (`503 overloaded` + `Retry-After`, §4); `write` keeps D-S14's
   byte-weighted charge, keyed on the class so the declare route — write class, declare scope —
   is charged exactly as an ingest route is; `maintenance` is admitted one at a time, a second
   admission refused `409 busy` + `Retry-After` (the answer the class's own doc comment has
   always promised; the store's per-aspect maintenance locks still answer `409` for conflicts
   below the whole class). The permit is held by the gate future itself across the handler
   call — not, as D-S14 left it, parked in the request's extensions: axum rebuilds the
   request as it routes, the extensions drop before the handler finishes, and this slice's
   gate tests caught the write charge being released early (a latent D-S14 defect, fixed
   here for every class by the same move).
2. **One new knob: `WEFT_QUERY_MAX_INFLIGHT` (Tuning, default 128).** It joins the VarSpec
   registry (72 → 73 entries) and the strict-config positive-integer list, is read once at
   startup through `AdmissionConfig` (the `IngestConfig` pattern) and travels on `AppState`
   beside it. The write budget stays D-S14's `WEFT_INGEST_MAX_INFLIGHT_BYTES`; maintenance
   exclusivity is the class's definition, not a knob, so it takes no variable. 128 is a
   judgement number: far under SEC-4's connection cap, far above one client's paging load.
3. **Admission II (ROB-11, W6) is not pre-empted.** It arrives with PAC-7's runtime builder
   (§2 L-MAIN) and owns later refinement; this slice adds no queueing, no priorities and no
   per-route overrides — refusal is immediate, which is what "shed" means in the class table.

**Rejected:** a ROB-10-owned knob registry (the §3 row absorbs it into VarSpec); keying the
write charge on scope as D-S14 left it (the acceptance names the registry's `admission_class`
as the input); queueing shed queries (an unbounded queue is the failure the `503` exists to
avoid); and blocking the slice until the query cap's default is pinned by the plan (the queue
records no open question for ROB-10, and the number is a Tuning-tier default, reversible at
merge review).

## 2026-11-01 — ROB-4: the lint's scope, and the Result API's two new errors (FLAGGED FOR MERGE REVIEW)

**Grounded:** the 1.0 plan names ROB-4 only as "Enforced no-panic lint (including asserts) and the
weft-arrow Result API" (§6 W5) and lists ROB-2/3/4 panic handling as required (§9); issue #269
repeats the one-liner. The points the sources do not pin, chosen here under the owner's
best-guess authorization (2026-10-26), recorded for merge review and reversible there:

1. **Scope is `weft-arrow`.** It is the only crate the slice names, and the only one whose public
   API panicked: its batch builders asserted on mismatched column lengths and expected
   `RecordBatch::try_new` to succeed. A workspace-wide ban is hundreds of sites across crates
   other slices own (ROB-5 owns the legacy queue-consumer panics, for one), so the lint lands
   where the Result API makes it satisfiable, and other crates opt in as their APIs go fallible.
2. **Every batch builder now returns `Result<_, ConvertError>`** (`ConvertResult<T>` is the
   alias): `segment_to_record_batch`, `segment_to_record_batch_typed`, `columns_to_record_batch`,
   `columns_to_record_batch_typed`, `paged_segment_to_record_batch(s)`,
   `reconstructed_series_to_record_batch`, `reduction_table_to_record_batch`. The IPC/Parquet
   convenience wrappers already returned `Result` and now propagate the builders' errors
   instead of panicking inside them. Callers in `weft-arrow-store` (anyhow `?`),
   `weft-server` tests and `weft-arrow`'s own tests were updated; no other production caller
   used the panicking forms.
3. **Two new `ConvertError` variants** (the enum is `#[non_exhaustive]`, so this is additive):
   `LengthMismatch { column, expected, actual }` for the former assertions, and `Batch(String)`
   for an Arrow assembly refusal after the lengths were checked. `weft-server`'s ingest
   classifier already falls through to `400` for unlisted variants, so a mismatched-columns
   body is a client error there, as before in spirit.
4. **Enforcement is two-layered.** The crate denies the panic-family clippy lints
   (`panic`, `unwrap_used`, `expect_used`, `unreachable`, `todo`, `unimplemented`,
   `panic_in_result_fn`) at its root — restriction lints, so the workspace clippy gate's
   denied groups are unchanged and the denial travels with the crate. Clippy has no lint for
   the assertion macros, so "including asserts" is enforced by a source-scan test over the
   production half of `src/lib.rs` (the `weftdb` backup.rs precedent), banning `assert!` /
   `assert_eq!` / `assert_ne!` / `debug_assert` alongside the panic tokens. Test code is
   exempt in both layers: an assertion failure there is the point.
5. **The three `#[should_panic]` tests became error assertions** on `LengthMismatch`, pinning
   the exact column/expected/actual values the assertions used to hide.

## 2026-11-01 — D-S14: how the HTTP surface wires onto the ingest entry point (FLAGGED FOR MERGE REVIEW)

**Grounded:** the 1.0 plan's §5 D-S14 amendment and resolutions C9–C11, plus the crash-consistency
design §8/§5.2/§2(g): routes through SEC-3's `Routes`; the one body cap is
`WEFT_MAX_INGEST_BYTES` (256 MiB default) on the ingest routes only; admission sits inside the
gate; the idempotency key (header or `batch_id`, visible ASCII ≤128 bytes, per aspect, shared by
all principals) is handled behind the gate; all four formats commit through
`SegmentStore::ingest` from a detached task whose JoinHandle the handler awaits; a panic is
mapped by `IngestPhase` (D21); the response gains `segment_ids` and `replayed`, a replay
answering 200 with `Idempotency-Replayed: true`; a `weft_audit` ingest event follows COMMIT;
the shutdown future goes to `serve::run` with handlers installed first; the drain is
`WEFT_SHUTDOWN_GRACE_SECS` (C-2); the shutdown watch is ROB-3's Daemons registry. The points the
sources do not pin, chosen here:

1. **Keyless requests commit under an ephemeral key.** The one entry point requires a key, so a
   request that supplies none gets a fresh unique key (`http-ephemeral:{pid}:{counter}:{nanos}`)
   minted per request. A keyless retry therefore stays the documented at-least-once — it cannot
   replay, because it cannot name its batch — and no format keeps a private seal path.
2. **Paged batches ride the entry point.** `IngestBatch` gains `rows_per_page`; the store's
   ingest seals a paged (v6) frame for a paged batch instead of the HTTP layer refusing the
   combination or bypassing the ledger. The fingerprint feeds the page height only when one is
   set, so an unpaged batch fingerprints byte-identically to before this slice and pre-D-S14
   ledger rows still replay.
3. **The in-flight budget defaults to the body cap.** `WEFT_INGEST_MAX_INFLIGHT_BYTES` is named
   by crash §8 with no default; it defaults to 256 MiB — one maximum body in flight. The gate
   charges `Content-Length` when present (a chunked body is charged 1 byte; the body cap still
   bounds it) and refuses with 503 `overloaded` + `Retry-After: 1`. The permit rides in the
   request extensions and is released when the request drops.
4. **The audit event is a tracing event, for now.** SEC-9's fsynced audit file is the consumer
   the design names, but SEC-9 is a W6 slice; the event is emitted on the reserved
   `weft_audit` target with the §5 fields (`aspect`, `format`, `rows`, `segments`,
   `commit_epoch`, `keyed`) so SEC-9 can subscribe without the emitters changing. It fires only
   on a fresh COMMIT — a replay commits nothing and emits none.
5. **`SegmentStore::shutdown()`** is the C9/§5.5 closing sequence as one call: a best-effort
   reaper pass (`MaintenanceWait::Skip`; busy/failed aspects are logged, never propagated — the
   shutdown must still land) and then the clean-shutdown commit. Startup calls it after the
   Daemons watch is raised, in place of the bare `mark_clean_shutdown()`.
6. **Parquet keeps only its decode in `weft-arrow-store`.** `decode_parquet_columns` is the
   decode half of `ingest_parquet_into_aspect` (which keeps its own seal behaviour for its
   library callers); the HTTP handler decodes, then commits like the other three formats.
7. **Response descriptor fields** come from the first committed segment's index row when it
   reads back; `null_count` uses the descriptor only for a single-segment commit (the receipt
   carries no null count), falling back to the request's own columns otherwise.
8. **Not in this slice, deliberately.** SEAL-2's `spawn_blocking` threshold
   (`WEFT_SEAL_BLOCKING_ROWS`, `WEFT_INGEST_MAX_SEGMENT_ROWS`) belongs to the seal engine's own
   amendment line, not D-S14's; and no `WEFT_IDEMPOTENCY_TTL_SECS` is registered, because
   nothing would read it yet — the ledger TTL/prune policy (crash §10.2's open default) lands
   with the slice that implements pruning.
9. **Fix-forward, not this slice's work:** D-S19 landed `WEFT_LEGACY_DEFAULT_STORAGE`
   (weftdb/types/legacy_seal.rs) without a registry entry, so
   `every_environment_name_the_workspace_reads_is_registered` was already red on main before
   this slice. The entry is registered here (and the asserted registry size moves 68 → 72: this
   slice's three, plus that one) because the C-2 registry is the surface this slice extends and
   the wave's end-of-wave CI would otherwise inherit the failure. Revert this entry with D-S19's
   own follow-up if its wording should differ.

## 2026-11-01 — D-S15: what the v4 sidecar reader checks, and what went with the v3 reader (FLAGGED FOR MERGE REVIEW)

**Grounded:** the crash-consistency design (§4) fixes the v4 shape — magic last byte `0x03`, a
CRC-32 trailer over magic and body, the stamp widened to `(row_count, byte_len, frame_crc)`, v3
files failing the magic check and treated as absent — and the 1.0 plan's §5 amendment adds only
that the reader sits under `DecodeLimits` and that v4 goldens join FRE-3's set. Three points the
sources do not pin, chosen here:

1. **Where `DecodeLimits` bites.** The trailer is verified before the postcard decode, so a torn
   or zero-filled file never reaches the decoder. The decoded claims are then refused past the
   limits: the stamp's row count past `max_rows`, any materialized grid holding more buckets than
   the segment has rows (a bucket is a non-empty cell, so the segment itself refutes the claim)
   or past `max_rows`, and a rollup chain past `MAX_SIDECAR_TIERS`, the most a writer produces.
   `PartialSidecar::from_bytes` uses `DecodeLimits::DEFAULT`; `from_bytes_with_limits` is the
   override form ROB-8's entry anticipated. Rejected: parsing postcard's varint length prefixes
   by hand to refuse before allocation — the format's own structure (each grid entry consumes
   body bytes) plus the CRC-first order bounds what a damaged file can make the reader do, and a
   hand-rolled pre-parser would be a second decoder to keep in sync.
2. **An unbound CRC does not refuse a sidecar.** `matches` takes the index row's `frame_crc` as
   an `Option`: legacy rows carry NULL until a backup/scrub binds it (design §4), and for those
   the `(row_count, byte_len)` pair alone decides, exactly as v3 did. Every frame this build
   writes has its CRC bound at commit, so the widened stamp guards every sidecar v4 writes.
3. **The v3 zero-rescale goes with the v3 reader.** `PartialSidecar::rescale_zeros_to` and
   `weft_reduce`'s `rescale_zeros` existed only to read 0.1.0-era v3 files whose zeros lost the
   column's scale; the weft-reduce doc already said the helper "goes with the v3 reader". Both
   are removed, and the scale test that simulated a pre-fix file by hand-rewriting one
   (`sidecars_with_unscaled_zeros_are_served_at_the_column_scale`) is retired with them: its
   subject is a file v4 treats as absent. Scale preservation for v4-written sidecars is still
   pinned by the remaining scale tests, whose sidecars the current serializer writes.

**Goldens:** `sidecar_v4_hours.bin` and `sidecar_v4_minutes_tiered.bin` join FRE-3's set in
`weftdb/tests/golden/`; the v3 pair stays committed as negative fixtures the v4 reader must
refuse.

## 2026-10-31 — What "OS-cold and post-restart runs" measure, and how (#146)

**Ambiguous:** the roadmap line is a heading with nothing under it. `weft-bench` already splits the
first timed rep from the rest (`cold_warm`, schema v21), but that first rep is cold only to the
process: the OS page cache still holds what the load just wrote. Nothing said which benchmark the
cold runs belong to, what "restart" means for an engine that runs in the harness's process, or how
to drop the page cache without the harness running as root.

**Chosen:**

- **In `weft-xbench`, beside the warm reps**, because the claim a cold read supports is a
  comparison: WeftDB's first read after a restart against the other engines' first read after
  theirs. `--cold-hook CMD` adds two more conditions per read phase, `--cold-reps` (3) reps each:
  - `os-cold`: the hook drops the page cache before each rep; the engine keeps running with its
    own caches (buffer pools, mark caches, decoded segments) intact.
  - `restarted`: the hook restarts the engine, the arm reconnects and waits for an answer, and then
    the page cache drops, so not even the reconnection's reads stay cached.
  Every cold rep is gated against the reference like a warm one: a restart that loses data fails
  the gate instead of timing an empty answer. Verdicts compare like with like (a cold WeftDB read
  against the fastest cold read of another engine).
- **The hook is outside the binary** (`sh -c 'HOOK "$@"' sh ACTION ENGINE`), so the harness needs
  no root and no docker. `scripts/xbench/cold-hook.sh` is the reference host's: `docker restart`
  of the engine's container, and `echo 3 > /proc/sys/vm/drop_caches` from a throwaway privileged
  container, the route approved for this host (no sudo password is typed).
- **In-process engines reopen rather than restart.** WeftDB's stored arms drop their
  `SegmentStore` and open the directory again (a root opens once per process, so the old handle
  goes first); DuckDB closes its connection and opens the file again. The harness's own memory
  (allocator, loaded code) stays warm; the report says so.
- **Reconnection is outside the clock**, except TimescaleDB's re-prepare of its two statements,
  which belong to the old connection: a reconnected client's first query pays it too.
- Load reps stay warm only: a load writes fresh files, so the page cache holds nothing of them
  before it starts.

**Rejected:**

- Rebooting the host between reps: the most honest cold, but the harness cannot survive it and it
  would turn a run into days.
- `posix_fadvise(DONTNEED)` on the stores' files instead of dropping the whole cache: it reaches
  only the files the harness can name, not a server container's data directory.
- Running the harness as root to write `drop_caches` itself: every engine's files would then be
  root's, and nothing else in the harness needs it.
- Cold reps in `weft-bench`'s single-engine workloads: without another engine beside them they show
  WeftDB's own cold cost, which `cold_warm` already approximates; the cross-engine run is where a
  cold-read claim is made.

## 2026-10-31 — What "TSBS-compatible comparison harness" means before tagged ingest (#141)

**Ambiguous:** the roadmap's Phase 1 line names a "full TSBS-compatible comparison harness" and
nothing more. TSBS's benchmark is its generator, its per-database loaders and its query suite;
every `devops`/`iot` query (`cpu-max-all`, `high-cpu`, `groupby-orderby-limit`, `lastpoint`, …)
groups many hosts' series by tag, and WeftDB stores one untagged series per aspect until TAG-11.
TAG-12 already lists "TSBS benchmarks" after it.

**Chosen (the owner's call, 2026-10-31):** TSBS's *data* through this benchmark's own phases now,
TSBS's query suite with TAG-12.

- `weft-xbench --tsbs FILE` reads `tsbs_generate_data --use-case=cpu-only --format=influx` output
  with the workspace's ILP parser and keeps one host's field, `host_0`'s `usage_user` (the field
  TSBS's own CPU queries lead with). A second tag set for the host, readings that do not strictly
  increase, or a line that does not parse are refused, not skipped.
- **Sized by span, not rows.** `tsbs-<scale>` is the readings within `scale.point_count()` seconds
  of the first, reconstructed per second: the synthetic datasets' grids (3,601, 86,401 and
  604,801 instants) from a knot every ten seconds, so nine in ten instants are interpolated. Taking
  `point_count()` readings instead would have made the medium grid six million instants.
- **The reference data** is generated by TSBS at commit `8323e59` with `--seed=123 --scale=10
  --log-interval=10s` over 2016-01-01T00:00:00Z to 2016-01-08T00:00:10Z (the end is exclusive;
  `00:00:01` stopped at 23:59:50 and left the medium grid's last ten seconds extrapolated). The
  report's reproduction section carries the commands.
- The report names TSBS's query suite under "Not tested" in every run, and the `tsbs` limitation
  says what the series is and that only the reference gate applies (no ground truth).

**Rejected:**

- TSBS's queries over one series each: a `groupby` over one host measures nothing TSBS measures.
- A WeftDB TSBS loader and query generator in Go now: nothing to load tagged series into until
  TAG-11, and TAG-12 owns it.
- Every host as its own dataset: ten times the run for ten draws of one random walk.
- The nightly smoke does not run `tsbs`: it would need a Go toolchain and TSBS's module graph in
  CI for data the unit tests already cover in shape.

## 2026-10-31 — Load timing excludes the previous load's teardown

**Ambiguous:** `weft-xbench` times each load rep from the start of `Arm::load`, and every arm's
`load` began by dropping what the previous rep stored (`self.db = None` / `self.stored = None`),
whose `Drop` removes the server-side database or the store directory. Whether that teardown
belongs to a load's time was never decided; it was simply inside the clock.

**Chosen:** it does not. `Arm` has a required `unload`, which releases the previous load, and
the runner calls it before starting each load's clock; a test with a 200 ms teardown pins it.
Found when IoTDB's loads measured about 1.2 s on every dataset, tiny ones included, against
0.2 s for the same load on a fresh server: its `DROP DATABASE` answers in 1.0 s. InfluxDB 3's
1 s loads are its own (its write waits for the WAL flush; a drop takes under 1 ms), so #471's
figures stand.

**Consequence:** every load row measured before this, in the committed EVI-15 report and in the
ClickHouse, TimescaleDB and QuestDB runs, includes the previous rep's teardown in reps 2 to 5,
so in its median. Interpolation and downsample rows are unaffected. The EVI-15 report keeps its
numbers with a note in its README; the combined cross-engine rerun supersedes them.

**Rejected:** dropping at the end of each rep instead: the last load's data must stay loaded for
the interpolation and downsample phases that follow it.

## 2026-10-31 — External engines V: how the Apache IoTDB arm is measured

**Ambiguous:** IoTDB 2.0 has two data models (tree and table), native clients over Thrift only,
no decimal type, no series generator, millisecond timestamps and an asynchronous WAL by default,
and a key of device plus instant that replaces duplicate writes. Which model, protocol, settings
and arms the benchmark uses were not settled.

**Chosen:**

- **One arm, `iotdb-double`**, in `weft-xbench/src/iotdb.rs`, run only with `--iotdb URL`, against
  IoTDB 2.0.11 standalone. No decimal arm: the engine has no decimal type.
- **The table model over its REST API** (`/rest/table/v1`, JSON, Basic auth as `root`). The table
  model is IoTDB 2.0's SQL surface, with window functions and `ASOF JOIN`; the tree model's
  path-based queries have no joins. Thrift has no Rust client, so the REST service is the protocol
  available; the report names the JSON encoding as a cost the binary paths of the other servers do
  not carry.
- **Server settings:** `timestamp_precision=ns` (fixed at first start; the datasets are
  nanosecond), `wal_mode=SYNC` so a write answers once its WAL entry is on disk, as the other
  durable arms do (IoTDB's default `ASYNC` would not), and `rest_query_default_row_size_limit`
  raised above the largest grid. Set by environment on the reference host and in the nightly smoke.
- **Writes:** `insertTablet` requests of 100,000 rows sent in time order, one table
  `knots (v DOUBLE FIELD)` with no tags, so one device. Not concurrently: IoTDB keeps rows that
  arrive behind ones already written in separate unsequence files that every query merges.
- **Interpolation:** `lead` pairs each knot with the next into segments; `date_bin_gapfill` with
  the grid's start as origin emits the grid (there is no `generate_series` and no `VALUES`); a native
  `ASOF LEFT JOIN` finds each instant's segment, the first and last segments standing in past the
  data. `date_bin_gapfill` left out the bucket at the grid's end when no knot fell in it, under
  `<= end` and `< end + step` alike, and emitted one past it under `<= end + step`; the grid is
  filled over the span widened by a step each way and cut back, which the `mcar-small` gate caught.
  **Downsample:** native `date_bin`, epoch-aligned.
- **Bytes on disk:** a `FLUSH` once the timed loads are done, then the table's TsFile bytes from
  `information_schema.table_disk_usage`.
- `WEFT_XBENCH_IOTDB` is registered in the Test tier.

**Rejected:**

- The tree model with `FILL(LINEAR)`: it fills `NULL`s between bucket aggregates of a `GROUP BY`
  time window, not at instants between jittered knots.
- Writing the grid into a second table to join against: a write the other engines do not make,
  inside the timed query.
- Concurrent writes, as the InfluxDB arm sends: out-of-order arrival lands in unsequence files.

## 2026-10-30 — External engines IV: how the InfluxDB 3 Core arm is measured

**Ambiguous:** InfluxDB 3 Core has no decimal type, no `ASOF JOIN` in its SQL engine (DataFusion),
a key of series plus instant that replaces duplicate writes, and rows that live in a WAL until the
server snapshots them to Parquet. Which of the decision-18 arms it can have, and how to read its
storage, were not settled.

**Chosen:**

- **One arm, `influxdb3-double`**, in `weft-xbench/src/influxdb3.rs`, run only with
  `--influxdb3 URL`, against InfluxDB 3 Core 3.12.2. No decimal arm: the engine has no decimal type,
  and the report says so rather than storing scaled integers no InfluxDB user would.
- **Writes:** line protocol over HTTP at nanosecond precision, `no_sync` at its default `false`, so
  the server answers once the WAL file holding the rows is persisted. **Reads:** SQL over HTTP with
  `format=parquet`, decoded with the workspace's own `parquet` 60 reader: columnar, like the other
  servers' binary paths, without Flight's gRPC stack.
- **Interpolation:** the grid merged with the knots; each grid row's neighbouring knot instants are
  a running `max` and a reversed running `min` (knot instants only increase), their values joined
  back by instant; the first and last segments stand in past the data. `last_value`/`first_value …
  IGNORE NULLS` gave the same answers but DataFusion re-scanned their frames: 6 min 25 s at the
  `medium` scale against 0.79 s. The one-row edge table is the build (left) side of its `CROSS
  JOIN`: with the grid on the left, DataFusion emitted one batch per grid row, 5.7 s against 0.08 s
  for 86,401 instants with identical answers, and run 4's numbers were discarded for it. A load is sent as concurrent writes of at most 8 MiB, under the
  server's 10 MiB request cap, so they share one WAL flush rather than waiting for one each. That flush runs every `--wal-flush-interval` (default `1s`, left at its default), so every load measures about one second; the report says so. `date_bin_gapfill` with `interpolate()` fills between bucket aggregates and does not answer
  jittered knots. **Downsample:** native `date_bin`.
- **Duplicates:** a second write at an instant replaces the first. Interpolation matches splimes
  (the last value wins); bucket counts would differ from `weft_reduce`, which counts every row. The
  benchmark datasets have no duplicate instants, the live test pins both behaviours, and the report
  states it.
- **No bytes on disk.** The rows sit in the WAL until a snapshot on the server's own schedule, and
  the server reports neither size per database; the load row shows no storage figure rather than a
  WAL size that would mislead.
- **`--query-file-limit 300000`.** InfluxDB 3 Core refuses a query that would read more than 432
  Parquet files. It writes one file per ten minutes of data time (`--gen1-duration` accepts only
  1m, 5m or 10m; compaction is an Enterprise feature), so the four-year UCI series becomes about
  200,000 files. 5,000 still failed on the third run. The cap is a documented server setting;
  raising it, and saying so in the report, keeps InfluxDB on the UCI rows instead of dropping it
  from them. The cost shows in the numbers: on run 5 a medium UCI interpolation took 15.5 s against
  0.3 s for the same-size synthetic series spanning a week. (The 295 s first measured for it was
  mostly the `CROSS JOIN` defect recorded above.)
- **Telemetry upload off** on the reference host and in the nightly smoke, which configures
  `influxdb3 serve` by environment because a service container takes no command.
  `WEFT_XBENCH_INFLUXDB3` is registered in the Test tier.

**Rejected:**

- Flight SQL: it adds `arrow-flight` and tonic to the crate for the same Arrow batches the Parquet
  answer carries.
- A scaled-integer "decimal" arm: not something an InfluxDB user stores.
## 2026-10-31 — SOA-6: the oracle judges read-backs exactly, and its verdict lives on the run record (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6, W4) lists SOA-6 by name only — "Soak oracle
core" — and the queue recorded its open question plainly: the oracle's verdict
semantics (what it checks; pass/fail thresholds — cf. SOA-2's refusal to invent
thresholds) are unspecified. What the sources do fix: SOA-4's workload is a
pure function of the config seed, so expected values are regenerable exactly;
SOA-5's DECISIONS entry assigns reads to "the oracle and the derived-read
checks (SOA-6, SOA-10), which are what make a read result verifiable at all",
and names the duplicated seal as the failure that "would surface much later
as an oracle mismatch"; SOA-7 (W5) is "Window digests and the oracle
self-test", so digests and the self-test follow this slice rather than being
part of it; and §3 says EVI-3 owns the shared run metadata while SOA keeps
its own `soak_schema_version`. The queue's acceptance fixes two outcomes: the
oracle judges a run's results against expected values, and its verdict is
recorded in run metadata.

**Chosen (grounded in SOA-4's model, SOA-5's seam, and §3's ownership rules;
if merge review disagrees, this decision is revisited there):**

1. **Library module, no read client, no CLI flag**: `weft-bench/src/soak_oracle.rs`,
   exported from the crate, on the SOA-5 precedent (the slices that own how a
   soak is launched are later ones). The caller supplies `ObservedPoint`s from
   whatever read path it used; the oracle performs no read itself. The
   endpoint-level derived-read checks are SOA-10's, not this core's.
2. **Exact comparison, classified.** Each observation names its tick's grid
   cell by (series, row); the oracle regenerates that tick's grid from the
   model and compares value, timestamp, and presence for equality. The
   workload is format-exact (SOA-4) and its numeric values are exact in the
   `f64` the server seals (SOA-5), so no tolerance exists to invent. A
   difference is one of `Missing`, `Unexpected`, `WrongValue`,
   `WrongTimestamp` (checked before the value: a point at the wrong instant
   is a different point), or `Duplicate` (a cell observed twice in one tick —
   the duplicated seal of SOA-5 point 6, surfaced here by construction).
3. **Pass is zero mismatches, not a threshold.** The verdict counts what was
   judged (ticks, cells checked, cells present, points observed) and lists
   mismatches with both sides of each comparison; `passed()` is
   `mismatch_count == 0`. No rate, tolerance, or latency bound is introduced
   — latency verdicts are SOA-13's SLO evaluator, and SOA-2/SOA-3's refusal
   to invent pass/fail numbers governs here. A verdict that judged nothing
   passes vacuously, so publishers check `ticks_judged` beside it (the
   SOA-12 skipped-check rule).
4. **The mismatch list is capped, the count is not.** `MAX_REPORTED_MISMATCHES
   = 1024` bounds the artifact a broken run produces (a lost segment in a
   24 h run could otherwise mint a mismatch per point for hours); the total
   always lands in `mismatch_count`, and `truncated()` says when the list
   was cut.
5. **The verdict is recorded on the run record, and the soak schema takes
   v2.** `SoakRun.oracle: Option<OracleVerdict>` (`#[serde(default)]`,
   skipped when absent, so v1 artifacts still deserialize) is set exactly
   once by `SoakHarness::record_oracle_verdict`, only while `Running` — a
   verdict that can be quietly replaced is not evidence. Adding a field to
   the record bumps `SOAK_SCHEMA_VERSION` 1 → 2 per SOA-1's bump rule, on
   SOA's own line (§3); the bench and manifest schema lines do not move.
   The verdict carries no free text, so `SoakRun::redacted_json` emits it
   unchanged and no new redaction path exists. One visible consequence:
   `SoakRun` drops its derived `Eq` — a record that can carry float grid
   values has no total equality; `PartialEq` is unchanged.
6. **Out of scope, recorded so it is not lost:** window digests and the
   oracle self-test (SOA-7), derived-read checks (SOA-10), SLO/latency
   verdicts (SOA-13), and any read client that produces observations from a
   live server (no slice names one yet; SOA-5's client owns the ingest lane
   only).

**Rejected:** a tolerance-based float comparison (the format-exact workload
makes exactness attainable, and a tolerance would be an invented threshold);
a separate verdict artifact beside the run record instead of a field on it
(the queue's acceptance says the verdict is recorded in run metadata, and
SOA-1's bump rule exists precisely for this change); judging latency or
scheduler warnings in the oracle (SOA-3's monitor and SOA-13 own those); and
folding SOA-7's window digests into this slice (the plan sequences them as
separate slices, digests after the core they digest).

## 2026-10-30 — ROB-8: the decode limit set, and why oversized is integrity (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the plan names ROB-8 only as "Frame decode hardening" (§6 W4,
L-FMT lane after X-FRAME) and "ROB-8 (decode limits)" in §9, and the queue
recorded its open question plainly: the exact limit set (sizes, extension
policy) is not specified anywhere. What the sources do fix is the shape:
§5's D-S15 amendment names a `DecodeLimits` the sidecar reader will sit
under, X-FRAME absorbed only ROB-8's `fault_class` naming, and C5 counts a
*length* failure as **Integrity**. The choices below are the best-guess set
the owner authorized for grounded slices (2026-10-26), recorded here for
merge review and reversible there.

**Chosen:**

1. **`DecodeLimits` lives in `weft-physical-type`'s frame registry**, beside
   the fault classes it feeds, with the defaults every frame reader in
   `weftseg` applies: `MAX_FRAME_ROWS = 16,777,216` per frame (or per page),
   `MAX_FRAME_PAGES = 1,048,576`, `MAX_FRAME_EXTENSIONS = 1,024`. The row
   bound sits above every row cap the plan names — decision 12's 5,000,000
   read/export rows and EVI-10A's 4 Mi `MAX_KNOTS` — and writers cut encoded
   frames to D-S13's 128 MiB target, so no produced frame comes near any of
   the three; they refuse claims, not data. D-S15's sidecar reader inherits
   the same type when it lands.
2. **A claim is checked before it allocates.** Each declared count is
   refused past its limit (`WeftSegError::DecodeLimit`, new) and, where the
   codec has a minimum bytes-per-element, against the bytes remaining
   (varint value/dod streams at one byte per element, checkpoint entries at
   three, RLE runs at two, paged index entries at eight) — a count the
   frame's own bytes refute is `UnexpectedEof`, never a `Vec` sized by the
   claim. The CRC is verified first, as before, but it is an integrity
   check, not a signature: a hostile file carries a correct one, so no
   count behind it was ever trustworthy.
3. **Expansions are totalled before they expand.** An RLE stream (timestamp
   dods, cascade deltas) whose runs sum past the declared count is refused
   (`ColumnLengthMismatch`, new) before `rle_decode` allocates on the sum;
   a short expansion stays the block layer's tolerated malformation, as the
   pre-existing short-block test pins — the frame layer is where short
   becomes refusal (point 4).
4. **A decoded frame must agree with itself.** `read_segment` /
   `read_paged_segment` cross-check the columns against the header: the
   value column holds exactly the present rows (`row_count - null_count`,
   with `null_count <= row_count` and `lossy_count <= count` checked at
   their own reads), the timestamp column reconstructs to exactly
   `row_count`, and a paged header is the rollup of its pages' row/null
   counts. A disagreement is `ColumnLengthMismatch` — damage, because no
   writer produces one and serving it would hand callers columns of
   different heights.
5. **Oversized is Integrity, never Undecodable.** The limits bound what any
   build may ask this one to materialise; they are not a format feature a
   newer build adds, so a frame past them is not "ahead of this build" —
   it is a length that does not add up, which is C5's own wording for
   Integrity. `frame_fault_of` maps both new variants there, so D-S12's
   quarantine treats an oversized frame as corrupt (TTL-purgeable) and
   never as suspect. X-FRAME's naming is untouched, as the queue required.

**Rejected:** a total frame-byte cap at the decode entry points (the byte
slice is already resident when a reader is called — the store layer, which
knows the file's size and its index `byte_len`, owns file-size policy; the
decoder's job is to stop a small resident frame from expanding into an
unbounded allocation, which points 2–3 do); per-call limit overrides in
this slice (no caller asks for one; D-S15 can add a `_with_limits` form
when it has a reader that needs it); and refusing short RLE expansions at
the block layer (a tested, deliberate tolerance — random-access readers
already agree on the short prefix, and point 4 refuses such a frame whole
at the segment layer).

## 2026-10-30 — TAG-0: what "fix ILP series merging" means (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the queue recorded TAG-0 as the weakest grounding in W3/W4 — no
source named the broken merge case, and no merge logic exists in
`interpolate.rs`/`downsample.rs`. The failing case is in the plan itself (§1,
"Live defects that the merged slices fix"): **"ILP silently pools series.
`parse_points` sorts every matching record together
(weft-line-protocol/src/lib.rs:208-212)."** Projecting field `load` from
`cpu,host=a` and `cpu,host=b` rows interleaved in one payload returned one
sorted stream alternating between the two hosts' samples — one fictitious
series fed to interpolation, downsampling and single-aspect ingest alike.

**Chosen (grounded in plan §1 + decision 15 + the §4 code table, flagged for
merge review):**

- **The fix lives in `parse_points`, where the defect is named.** The
  projection is a single series: every record that contributes a point must
  share one series identity — measurement plus tag set, tags canonicalized by
  sorting (`LineRecord::series_key`), so the same tag set in a different wire
  order is still the one series. A second contributing series is an error at
  that record's line, naming both series. Records that contribute no point
  (missing field/timestamp/non-numeric) do not count: they cannot pollute
  the stream, and interleaved foreign measurements remain skippable as
  before.
- **The refusal is typed, not a message convention.** `ParseError` gains a
  `kind: ParseErrorKind` (`Malformed` / `AmbiguousSeries`, `#[non_exhaustive]`)
  — this module's own doctrine is classification by type, and the old
  substring-matching style is what X-ERR removed. `weft-server` maps the kind
  through `error::from_line_protocol_error`: malformed stays `bad_request`,
  multi-series becomes the §4 table's existing `ambiguous_series` (400),
  which decision 15 already prescribes for multi-series reads. No envelope
  change — the code existed since X-ERR; TAG-0's envelope part was absorbed
  there, as the queue said.
- **All three single-series callers get the refusal**: `interpolate/ilp`,
  `downsample/ilp` (the L-COMPUTE lane, TAG-0's lane per §2) and
  `storage/{aspect}/ilp` ingest, which seals the projected points into one
  aspect and would otherwise store the pooled fiction. `weft-bench`'s
  line-protocol profile inherits the refusal through its existing
  `From<ParseError>`.

**Rejected:** returning grouped per-series points from `parse_points`
(changes every caller's shape and invents a multi-series contract no source
asks for — per-series reads are the TAG-5+ selector work), and fixing only
the compute handlers (leaves ingest pooling into the store, the worst of
the three).

## 2026-10-30 — PAC-5b: the strict knob set and the store filesystem check (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the plan names PAC-5b's two halves — strict server configuration and a
store filesystem check — and C7's exit table (65 for a store needing operator action,
78 for invalid configuration or an unsupported deployment, including filesystem
refusals), but no source enumerates the exact strict knob set (the queue recorded
this as PAC-5b's open question; decision 21's wording lives in the W8 docs). The
bind guard is explicitly NOT here: §3 moves it to SEC-8, with
`WEFT_ALLOW_INSECURE_BIND` dropped for `WEFT_AUTH=disabled`.

**Chosen (best guess, grounded in PAC-5a's code, flagged for merge review):**

- **Strict names.** PAC-5a's `settings::report` said it "never fails startup …
  until PAC-5b makes the surface strict", and its test file said refusing on an
  unknown variable is "PAC-5b's change to make". So the name half is exactly
  `warnings_for`'s list, now fatal: an unknown `WEFT_` name (with the nearest
  spelling) or a `Test`-tier channel set on a real server refuses the start,
  exit 78, before any phase reads a setting.
- **Strict values.** The value half is the registered variables whose `accepts`
  shape is closed and checkable without opening anything —
  `settings::value_errors_for`: the non-negative and positive whole-number
  knobs, the two finite-ratio knobs (`WEFT_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD`
  ≥ 1.0, `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` > 0), and
  `WEFT_ON_AMBIGUOUS_COMMIT` (`poison`/`exit`), the one enum whose reader used
  to warn and keep serving. Validation runs whether or not the daemon or policy
  the knob feeds is enabled — a malformed `WEFT_BACKUP_KEEP` refuses even with
  backups off. Open shapes (paths, endpoints, truthy flags where anything else
  is simply "off") are not judged centrally; their startup phases already
  refuse what they cannot use, and PAC-5b routes those phase errors
  (bind address, interpolation cap, serve limits, TLS, auth) to the same
  exit 78 instead of a generic failure.
- **Store filesystem check.** Before the open, `store_open::check_store_root`
  refuses a root that exists as a file, cannot be created/listed/written, or
  whose directory entry cannot be synced on Unix (the durability every store
  write assumes) — exit 78 as a C7 filesystem refusal, with a probe file that
  is always removed. The open's own refusals keep their classes through
  `exit_code_for_store_error`: `StoreError` → 65, `StoreLocked` → 75.
- **The library is unchanged in kind.** `weftdb` still never exits (C-1) and
  its `from_env` policies still default as before; strictness is the server's
  layer, checked before the library reads anything. `settings::report` remains
  as the warning-only printer for other callers.

**Rejected:** inventing a broader knob set (validating path existence for every
path variable, or resolution tokens for `WEFT_SEGMENT_PARTIAL_*`) — no source
grounds those shapes as startup refusals, and PAC-6/W8 own the configuration
documentation pass that can widen this list deliberately.

## 2026-10-30 — ROB-3: the panic mapping, the supervision policy, and where `weftdb::exec` grows

**Ambiguous:** plan C12 names the parts — ROB-2's hook aborts turso-origin panics, ROB-3's catch
is the only layer, ingest routes map by `IngestPhase` (D21) — and D-S14's amendment says its
`JoinError` is "mapped by `IngestPhase`" and its shutdown watch is "shared with ROB-3's Daemons
registry", but no source spells out the phase-to-envelope table, a restart policy for daemons, or
whether `weftdb::exec` (today only ROB-2's control-plane scope) is where the catch lives.

**Chosen:**

- **The mapping, from the §4 code table's own definitions.** Before `IngestPhase::Committing`
  the batch's one index COMMIT has not run, so a panic there is `500 commit_failed` with
  `committed: false` ("the write did not commit and the store was not modified" — frames already
  written are crash litter, as after a crash at the same point). At `Committing` the COMMIT may
  or may not have landed, which is exactly `503 outcome_unknown` ("the server cannot say whether
  the write committed"), with no `committed` field claimed either way. Every non-ingest panic is
  `500 internal`. The panic's message and phase are logged server-side only; the wire gets the
  code's redacted sentence, per the §4 5xx rule.
- **`weftdb::exec` is the entry point.** It gains `catch` (a future run under `catch_unwind`,
  returning a `CaughtPanic` with the safely-rendered message and the phase reached), the
  `INGEST_PHASE` task-local (`ingest_phase_scope` / `set_ingest_phase` / `current_ingest_phase`),
  and `SegmentStore::ingest` mirrors each of its phase advances into that task-local. The HTTP
  layer (`weft-server/src/panic_catch.rs`) is the single catch layer built on it — installed once
  in `app_with_state`, no `tower-http` CatchPanicLayer anywhere (X-ERR dropped FRE-6a's for this).
  ROB-2's abort is preserved mechanically: the hook fires during the unwind, before
  `catch_unwind` can observe the panic, so a control-plane panic never becomes a response.
- **Supervision restarts on panic only.** `weft-server/src/daemons.rs`' `Daemons` registry owns a
  `tokio::sync::watch` shutdown flag — the watch §5 D-S14 shares — and supervises each registered
  daemon: a `JoinError::is_panic` restarts it from its factory after 100 ms, with the restart
  counted in the registry snapshot; a clean finish is `Stopped`, never restarted; a one-shot
  (`spawn_once`, GPU calibration) is tracked but never restarted. Startup registers reconcile,
  backup and calibration through it and raises the watch when serving ends. SEC-9's audited
  routes catching `is_panic` inside their own tasks remains SEC-9's, a task boundary, not a
  second layer here.

**Rejected:** mapping a pre-commit ingest panic to `internal` (it would hide the one fact the
caller needs — that a retry is safe — which `commit_failed`'s envelope exists to state); and a
restart cap on supervised daemons (no source names one, and a capped-out daemon is the old
silent-death shape returning with extra steps — the 100 ms delay bounds a panic loop's spin, and
the snapshot's restart count makes it visible).

*Parent salvage note (2026-10-30): the loop run hit its implement cap with this tree
uncommitted; the parent verification bar caught one defect class the run's own checks
missed — nine call sites (the three startup registrations in `startup/daemons.rs` and
six in this module's own tests) ignored the `#[must_use]` supervisor handles, which
`-D warnings` rejects. Those sites now drop the handles explicitly (`drop(...)`,
which detaches without stopping the daemon): the registry's shutdown watch, not
per-daemon joining, is what stops these daemons, as this entry's supervision design
states.*

## 2026-10-30 — External engines III: how the QuestDB arm is measured

**Ambiguous:** #393's first rule says a QuestDB adapter must use QWP, QuestDB 10's columnar
WebSocket protocol, not PG Wire. Open-source QuestDB's acknowledgement also says less than the
other engines' commits do, and its native fills work on buckets. How to keep the load and the
answers comparable was not settled.

**Chosen:**

- **Two arms, `questdb-double` and `questdb-decimal` (`DECIMAL(18, s)`)**, in
  `weft-xbench/src/questdb.rs`, run only with `--questdb host:port`. Writes and reads go over
  **QWP** through `questdb-rs` 7.2, QuestDB's own client; DDL, the applied check and cleanup over
  HTTP `/exec` on the same port. The client's Arrow feature is off: its `arrow` range excludes the
  workspace's 60, and nothing here needs Arrow.
- **Load:** a WAL table partitioned by year (the coarsest a WAL table allows, after TimescaleDB's
  420-chunk lesson), rows sent over QWP, the server's acknowledgement awaited, then timed until
  `wal_tables()` shows every sequenced transaction applied, so the rows are visible. Open-source
  QuestDB acknowledges receipt, not durability (a durable ack is Enterprise-only), so the reference
  server runs with `cairo.commit.mode=sync`, and the engine line records the mode it found.
- **Interpolation** uses only QuestDB's native time joins: an `LT JOIN` of the deduplicated knots
  onto themselves makes one row per segment, keyed by its start, and an `ASOF JOIN` from the grid
  finds each instant's segment (past the last knot, the last segment, extended). 2,000,000 knots
  onto a 2,000,000-point grid took 3.5 s over HTTP. `SAMPLE BY … FILL(LINEAR)` fills between
  bucket aggregates and does not answer jittered knots. Two earlier shapes failed and are kept
  here: `lead`/`lag` on the right of an `ASOF JOIN` came back shifted on QuestDB 10.1 (a knot's own
  time read as the next knot's; every interior instant `NULL`, caught by the live test), and
  neighbours attached by `row_number()` self-joins on `rn ± 1` ran as a nested loop that hit the
  server's 60 s query timeout on the smallest dataset.
- **Downsample** is native `SAMPLE BY … ALIGN TO CALENDAR`, the width in its largest exact unit
  (QuestDB refuses `60000000000n`). The decimal arm's mean is `avg(v::double)`: QuestDB's `avg` of
  a `DECIMAL(18, s)` column rounds the mean to `s` places (2.804 for 2.80357… at `s = 3`), a real
  semantic the report names, not one an `f64` reference can gate.
- **Bytes on disk** are `table_partitions()`' `diskSize`, which counts the space QuestDB reserves
  ahead in the active partition; the report says small tables read far larger than they hold.
- **`WEFT_XBENCH_QUESTDB`** is registered in the Test tier; the nightly smoke starts a
  digest-pinned QuestDB 10.1.0 with telemetry off and sync commits.

**Rejected:**

- PG Wire: #393 rules it out as a deprecated path.
- Reporting either failed shape as QuestDB's result: an engine is measured on a correct query a
  QuestDB user would write, not on the first one tried. The owner decided (2026-10-30) not to report the `ASOF JOIN` behaviour upstream.

## 2026-10-30 — External engines II: how the TimescaleDB arm is measured

**Ambiguous:** TimescaleDB is the one engine in #413 with a native linear interpolation,
`time_bucket_gapfill(…)` with `interpolate(…)`. The fairness notes (#393, roadmap 1.2) prefer an
engine's native path where it exists. Whether that native fill answers this benchmark's question
was not settled.

**Chosen:**

- **Two arms, `timescaledb-double` and `timescaledb-numeric`**, in `weft-xbench/src/timescale.rs`,
  run only when `--timescale URL` gives a PostgreSQL connection string for a server with the
  extension. `tokio-postgres` over loopback, the async client a Rust user would use.
- **Not the native fill, for this phase.** `interpolate()` draws its line between *bucket
  aggregates* at bucket boundaries, and leaves the edges past the data empty. The datasets' knots
  are jittered off the grid (the synthetic generator's 0.6 jitter, UCI's own timestamps), so it
  answers a different question and would fail the gate on every row. The arm runs the SQL a
  PostgreSQL user writes without `ASOF JOIN`: a `generate_series` grid and two index-backed
  `LATERAL … LIMIT 1` lookups per instant, gated like every arm. The native fill belongs in roadmap
  1.2's native-fill comparison, scored as what it is.
- **Load:** one hypertable on `t` whose single chunk spans the dataset, filled by one text `COPY`
  committed with the default `synchronous_commit = on`. Each dataset fits in memory many times
  over, the size TimescaleDB's guidance picks a chunk for, and one chunk is the counterpart of
  `weftdb-stored-bulk`'s one segment. A one-day interval was tried first and is recorded as the
  wrong configuration: the four-year UCI series became 420 chunks, every per-instant index lookup
  walked all of them, and the same 20,001-point query took 6.6 s against 0.11 s with one chunk. The hypertable stays in the row store; the columnstore is a
  separate configuration, and the report says the bytes on disk are uncompressed.
- **Telemetry off.** The image ships with `timescaledb.telemetry_level = basic`, which reports to
  Timescale. The reference run starts the server with it off, and the nightly smoke sets
  `TIMESCALEDB_TELEMETRY=off`.
- **`WEFT_XBENCH_TIMESCALE`**, the live test's server, is registered in the Test tier of
  `weft-server`'s environment registry, beside `WEFT_XBENCH_CLICKHOUSE`.

**Rejected:**

- Aligning the knots to the grid so the native fill passes: it would change the dataset for one
  engine.
- Enabling the columnstore in this arm: a second TimescaleDB configuration is fair-protocol depth
  (roadmap 1.1), not this slice; one honest configuration first.

## 2026-10-30 — External engines I: how the ClickHouse arm is measured

**Ambiguous:** #413 / #140 ask for ClickHouse, InfluxDB 3, QuestDB and TimescaleDB adapters, and
the fairness notes (#393) ask that each engine be measured on the path its users actually take.
ClickHouse is the first engine that is a server rather than a library, and it has a native gap
fill. Nothing says which of its interfaces to time, whether its native fill counts as the
interpolation answer, or how durable its load must be to compare with a WeftDB seal.

**Chosen:**

- **Two arms, `clickhouse-double` (`Float64`) and `clickhouse-decimal` (`Decimal64(s)`)**, mirroring
  the DuckDB pair, in `weft-xbench/src/clickhouse.rs`, run only when `--clickhouse URL` names a
  server. A run without one lists both arms under "not tested" with the flag to pass.
- **HTTP on loopback, `RowBinary` both ways**, through a blocking client on tokio's blocking pool
  like the DuckDB calls. That is what ClickHouse's own drivers use; the report says the server's
  timings include the round trip the in-process engines do not pay.
- **The same `ASOF JOIN` interpolation SQL as DuckDB's.** `WITH FILL … INTERPOLATE` carries the
  previous row's expression forward; it does not draw a line between neighbours, so it does not
  compute this benchmark's answer and would fail the gate. The report says so.
- **Durable load:** one `INSERT` into a `MergeTree ORDER BY t` table with `fsync_after_insert` and
  `fsync_part_directory` on, the counterpart of a WeftDB seal and DuckDB's `CHECKPOINT`. Bytes on
  disk are `system.parts.bytes_on_disk` for the active parts.
- **The server is pinned** to `clickhouse/clickhouse-server:26.10.3.7` by digest in the nightly
  smoke run, which now starts it as a service container and runs the live arm test there; `ci.yml`
  is unchanged, so no pull request needs a database server.
- **No report yet.** One full run on the reference SSD passed all 135 gates. A published comparison
  waits for the second run EVI-15's decision requires, and for the other engines, so the next
  cross-engine report covers them together.

**Rejected:**

- The official `clickhouse` Rust crate: it pulls in hyper and its own TLS stack for what `ureq`,
  already in the lock through DuckDB, does in a few lines.
- Timing ClickHouse's native `WITH FILL` as a "class A" native arm: it computes a different answer.
  It belongs in the native-fill comparison (roadmap 1.2), labelled as LOCF, not here.
- Running the live ClickHouse test in `ci.yml`: a database service on three operating systems for
  every pull request, for a crate outside the workspace.
## 2026-10-30 — SEC-6: keys-file format, auth modes and the C10 namespace (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the plan (§6 W4, §3 SEC-3/SEC-6 row, §4 C10) fixes what SEC-6 must do — API keys
with Bearer auth enforced through SEC-3's gate, a keys file and a keygen tool, and a key
namespace that is per aspect and shared by all principals — but the queue records the
keys-file format as an open question: no source names a format, a hash, a token shape, or the
`WEFT_AUTH` values that turn auth on. SEC-3's decision entry likewise left "which API key
holds which role" to this slice. These choices are the best-guess set the owner authorized
for grounded slices (2026-10-26), recorded here for merge review and reversible there.

**Chosen:**

1. **Keys file: versioned JSON, hashes only.** `WEFT_KEYS_FILE` (the name SEC-5's gate
   already reads) names `{"version": 1, "keys": [{"id", "role", "key_hash"}]}`, where
   `key_hash` is `sha256:` plus the hex SHA-256 of the key. The file therefore holds no
   working credential. Entries are validated whole at startup — unknown fields, an unknown
   or `anonymous` role, a blank or duplicate id, a malformed hash, a hash shared by two
   ids, an empty key list, or a version other than 1 are startup errors, never a partially
   enforced table (the settings registry's fail-loud rule, applied to this file). A key's
   role is one of SEC-3's frozen six; per §10 per-aspect grants are deferred past 1.0, so a
   role grants its scopes on every aspect alike. Reload without restart is SEC-7's, not
   this slice's: the file is read once at startup.
2. **Tokens and keygen.** A minted key is `weft_` plus the hex of 32 bytes from the ring
   provider's secure random (SEC-5's provider; no new entropy source, no process-global
   provider). Lookup hashes the presented token and compares digests against every entry
   in constant time per entry, with no early exit. `weft-server keygen [--id] [--role]
   [--keys-file]` mints a key, prints it once, and appends only its hash (creating the
   file at mode 0600 on Unix); with no `--keys-file` it prints the entry to paste. Its
   defaults are the operator's own first key (`operator`/`admin`); a duplicate id or hash
   is refused rather than redefined.
3. **Modes.** `WEFT_AUTH=disabled` is off (SEC-5's value, and it wins over a mounted
   file, as in the plaintext gate); `WEFT_AUTH=keys` is on and *requires* the file;
   unset is on exactly when a file is mounted, off otherwise — so a keyless server behaves
   as it did before this slice, and the exposure matrix that refuses some keyless binds
   stays SEC-8's. Any other `WEFT_AUTH` value is a startup error: neither opt-in nor
   opt-out may be spelled by accident (decision 7).
4. **Enforcement through the gate, anonymous probes.** The check is a layer in
   `app_with_state` deciding from the sealed registry's own policy table (cached once;
   the registry is frozen), so the scope enforced is the scope the route registered.
   No header means the anonymous principal of SEC-3's role table — probes serve, every
   other scope is `401 unauthenticated` with `WWW-Authenticate: Bearer`. A malformed
   header or an unknown key is the same 401 on every route, probes included (a failed
   authentication is never silently demoted to anonymous); a known key whose role lacks
   the scope is `403 forbidden` naming the key id, its role and the needed scope. The
   gate's own refusals keep their shape: an unregistered path or wrong method defers to
   the gate/fallback, keyed or not. An authenticated request carries its `Principal`
   (id + role) in its extensions for SEC-9's audit.
5. **Credentials travel in the header only.** With auth on, a query parameter named
   `api_key`, `apikey`, `access_token` or `auth_token` is refused `400
   credentials_in_query` — the §4 code X-ERR reserved, produced here for the first time —
   because query strings are what proxies and logs record.
6. **C10's namespace is the idempotency namespace, and this slice keeps it that way.**
   Read in its §4 context, "the key namespace is per aspect and shared by all principals"
   governs the `Idempotency-Key` handling that sits behind this gate (landing with
   D-S14): D-S13's ledger is keyed by `(aspect, key)` with no principal component, and
   SEC-6 adds none — authentication answers who a key is and what its role may do, on
   every aspect alike, so a second principal replaying an ingest meets the first
   principal's ledger row. Partitioning that namespace by API key would break the rule;
   nothing here does.

**Rejected:** plaintext keys in the file (a mounted file must be safe to back up; the
keygen exists precisely so nobody hashes a self-chosen secret by hand); scopes listed per
key instead of a role (SEC-3 froze the role as the grant unit, and a per-key scope list
would be a seventh, unfrozen vocabulary); refusing non-probe routes for the anonymous
principal with 403 instead of 401 (the caller has not authenticated at all, and the §4
table pairs 401 with the `WWW-Authenticate` challenge that tells them how to); and blocking
the slice until the format is specified (the queue's open question is real, but the
owner's best-guess authorization covers exactly this class of decision when it is grounded
and recorded, as SEC-3's scope table and SEC-5's configuration shape were).

## 2026-10-30 — SEC-5: TLS configuration shape and the C8 gate's scope (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the plan (§6 W4, §4 C8, decision 7) fixes what SEC-5 must do — a TLS
listener (rustls with ring) on SEC-4's shared serve path, and plaintext only under an
explicit opt-in — but names no configuration surface and does not say which binds the
plaintext refusal applies to.

**Chosen:**

1. **Configuration by environment pair.** `WEFT_TLS_CERT_FILE` + `WEFT_TLS_KEY_FILE`
   name the PEM chain and key, read once at startup and registered in the settings
   registry alongside the gate's variables. Naming exactly one is a startup error:
   half a TLS configuration silently ignored would serve plaintext an operator
   believes is encrypted. The rustls provider is ring, built explicitly per config —
   no process-global provider is installed, so a library consumer's rustls choices
   cannot change what this server negotiates. ALPN offers `http/1.1` only (decision
   17; HTTP/2 is deferred past 1.0 by §10). The handshake runs inside the connection
   task on the shared serve path, under SEC-4's connection cap and keep-alive
   deadline, so a stalled handshake holds a cap slot for at most that deadline.
2. **The gate refuses only non-loopback plaintext.** Loopback binds are ungated
   (decision 7's protected mode serves loopback under the Host/Origin guard with no
   keys), and TLS binds are ungated (the traffic is encrypted whatever the auth
   story). A non-loopback plaintext bind starts only with a mounted keys file
   (`WEFT_KEYS_FILE` names it and the file exists) **plus** `WEFT_ALLOW_PLAINTEXT=1`,
   or with `WEFT_AUTH=disabled`. SEC-5 reads the keys file's *presence* only; SEC-6
   defines its format and enforces it. The full exposure matrix (proxied-request
   refusal, metrics scope, guard interplay) remains SEC-8's.

**Salvage note:** the implementing run hit its time cap before the ROADMAP tick and
this entry; the parent added both, plus one rustfmt import reformat in
`tests/tls_serve.rs`, and re-ran the full verification bar on the final tree.

## 2026-10-30 — EVI-15: the owner approved the report narrative

**Ambiguous:** ROADMAP W7 has an owner-only line, "Write or approve the cross-engine report
narrative". EVI-15 (#463) shipped the narrative in `bench/reports/evi-15/README.md` and the
EVI-15 figures in `HONESTY.md`, and left the line open.

**Chosen:** The owner approved the narrative as merged (2026-10-30), so the line is ticked. The
approval covers the EVI-15 report and its figures in `HONESTY.md`; EVI-17's pages (*When WeftDB
beats general TSDBs*, *GPU tuning and economics*) still need their own approval.

**Rejected:** Treating this approval as covering EVI-17: those pages do not exist yet.

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
## 2026-10-30 — D-S13: the ingest entry point's shapes (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§5 D-S13, §4 C1, §3 X-ERR) fixes what D-S13 must do — an
idempotent atomic library ingest, `IngestBatch.groups` accepting only the empty `TagSet`
before the gate flips, `ingest_ledger.series_ids`, `#[non_exhaustive]` typed errors
including `UnknownAspect`, a fingerprint over the bounded plain form,
`IngestPhase::Committing` set just before COMMIT, and a 128 MiB encoded-frame target
(ROB D12) — but names no Rust shapes: no batch/receipt types, no fingerprint algorithm,
no ledger encoding for `series_ids`, and no statement of how a multi-frame batch commits.

**Chosen:**

1. **One transaction, modelled on `publish_seal`.** `SegmentStore::ingest` validates,
   looks the key up, encodes, writes every frame durably, then — under the aspect's
   commit lock, with the ledger re-checked there — commits one `IndexTxn`: an
   `InsertNew` per segment, the `SeqBump`, the S11 rollup fold over all segments, and a
   new guarded `IndexOp::RecordIngest` (plain `INSERT`; a taken `(aspect, key)` is a
   conflict, never a replace). The ledger row therefore exists if and only if the
   segments do. A replay found by the second check returns the recorded receipt and
   leaves its just-written frames as crash litter for S12 recovery, exactly like a seal
   that crashes before its commit.
2. **The receipt is the ledger's payload.** `IngestReceipt` (aspect, key, fingerprint,
   row count, span, segment ids, series ids, commit epoch, `duplicate`) serialises into
   `receipt_json`; a replay — in-process or after a restart — returns it with
   `duplicate` set. `id_lo`/`id_hi` name the segment run (ids are consecutive under the
   commit lock).
3. **`series_ids` is a comma-joined text column** (migration `0005_d_s13`, layout 2),
   matching `store_meta`'s text convention; a pre-D-S13 row reads back with none. Until
   the tags gate flips it always records `0`, the empty group, and every segment row of
   the batch carries `series_id` 0.
4. **Fingerprint: FNV-1a over length-prefixed fields** — the groups' canonical series
   key, the row count, then per row the timestamp and either a null marker or the
   value's plain form. Plain form is ROB-6's bounded form (plan: "fingerprint over the
   bounded plain form"); the domain check runs first, arithmetically
   (`plain_digit_count`), so a hostile exponent is refused as the typed
   `StoreError::IngestValueOutOfRange` before any plain string is allocated. The
   idempotency key itself is not fingerprinted: the ledger keys on it.
5. **Typed errors land in `StoreError`** (already `#[non_exhaustive]`): `UnknownAspect`
   (now returned by `SegmentStore::require_schema`, replacing the anyhow sentence),
   `IdempotencyConflict`, `GroupsUnsupported`, `EmptyIngestBatch`,
   `IngestLengthMismatch`, `IngestValueOutOfRange`. In `weft-server`, `classify_typed`
   maps them to their §4 codes (`aspect_not_found` 404, `idempotency_conflict` 409,
   `tags_unsupported` 400, `decimal_out_of_range` 400, the rest `bad_request`) instead
   of the generic `incompatible_store`; the read path's message rule stays, now only
   for `weft-arrow-store`'s still-untyped lookup.
6. **The 128 MiB cut is by rows, proven per chunk.** A batch whose whole frame exceeds
   `MAX_ENCODED_FRAME_BYTES` is cut at timestamp boundaries (never inside a run of
   equal timestamps) by a conservative per-row estimate (24 bytes + the value's plain
   digit count, computed arithmetically); each chunk's real encoded frame is then
   checked against the target before any frame is written, and
   `FaultPoint::SBetweenFrames` fires between frame writes. A single timestamp run
   whose frame alone exceeds the target still seals — a frame is the smallest cut.
   Ingest seals nullable (the quality-column form), so dense and null rows share one
   entry point; D-S14's HTTP wiring decides how its formats map onto it.

**Rejected:** a second transaction for the ledger (a crash between them is precisely
the duplicate-commit window idempotency exists to close); hashing the encoded frame
bytes instead of the plain form (the plan names the plain form, and frame bytes would
change with codec options for the same batch); allocating series ids for non-empty
groups now (the gate has not flipped; TAG-5 owns the `series` table).

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

## 2026-10-31 — D-S16 whole-store backup shapes — FLAGGED FOR MERGE REVIEW

**Ambiguous:** plan §5's D-S16 amendment is seven clauses with no shapes: the
manifest's new fields, the `SERIES_KEY` extension's number (the frame
registry shipped empty), and the restore intent marker's name and contents
(the crash design calls the in-place marker `RESTORED`; plan C19/C-7 only
says "restore intent marker written first").

**Chosen:**
1. **Manifest.** `BackupManifest` gains `frames`, `store_format` and
   `allocators` (serde-defaulted, so control-plane manifests are unchanged),
   and whole-store backups write the new format string
   `weftdb-whole-store-backup/1` — the shape backup.rs already prefigured
   ("a backup a newer release writes (with frames, S16) is never restored as
   if it were control plane only"). A control-plane restore refuses the new
   format; a whole-store restore refuses the old.
2. **SERIES_KEY = extension 1**, registered in `KNOWN_EXTENSIONS` with the
   payload defined as the canonical series-key bytes. Frames carrying it are
   journaled `held` (a new `frame_journal` state and `IndexOp::JournalHeld`;
   the reaper and the open's replay skip held rows entirely) until TAG-9.
3. **Intent marker.** Named `RESTORE_INTENT`, JSON
   `{format, store_uuid, backup_created_ms, restore_started_ms}`: written
   first into the restore staging directory, published with the root,
   removed by the first open once restore-mode recovery completes.
4. **Link fallback.** B3 says copy on EXDEV; any `hard_link` error falls
   back to copy + `sync_all`, and the manifest records which happened per
   frame (`linked`), since a backup that silently copied is still correct
   and one that silently failed to link is not a space surprise either way.
5. **Headroom.** `check_backup_headroom(mode, …)`: Linked needs only the
   control plane's bytes, Copy needs the whole store. Free space is read by
   `statvfs` on Unix (`libc` promoted from a unix dev-dependency to a unix
   dependency); where it cannot be read, the check is skipped rather than
   faked.
6. **Adoption scope.** Restore-mode adoption (R8, via `describe_frame`)
   requires the aspect to have a declared schema in the restored store's
   scope; an orphan whose aspect is undeclared is left in place, neither
   adopted nor quarantined.

**Not in this slice:** `restore_in_place` (the design's second restore
shape) and the `weft-server restore` CLI — plan C19 assigns the one CLI to
PAC-13 (W7), and the queue's D-S16 acceptance names backup, adoption,
`held`, the intent marker, the embedded format, the floor refusal, the RAII
hold and the headroom check, not the in-place shape.

## 2026-11-01 — D-S19 seal-backed legacy aspects: the shapes the design left open — FOR MERGE REVIEW

**Ambiguous:** the crash-consistency design (§8, S19) and the 1.0 plan (§5
D-S19/D-S20 amendment) name what a seal-backed legacy aspect must do, but
leave several shapes open: where `widen_value_encoding` lives (FRE-7b is a
separate, still-pending W5 slice), which registry registers the legacy
DDL, what target the post-capture squash gate uses, and what identity a
measurement read back from a seal frame carries (frames hold no per-row id
or dataset id).

**Chosen:**
1. **Widening.** `widen_value_encoding(current, values)` is implemented in
   `weft-physical-type::schema` — the minimal FRE-7b helper this slice's
   widening needs: an encoding that encodes every offered value exactly is
   kept; anything lossy or unrepresentable widens to `BigDecimalText`, the
   one universally lossless encoding. FRE-7b's other parts (immutable
   declarations, scale rules, schema read-back) are not built here and its
   ROADMAP line stays unticked. The legacy adapter declares `Decimal128`
   at zero tolerance in milliseconds on first use and widens the
   declaration through this helper; frames self-describe, so earlier
   frames stay readable.
2. **Legacy DDL.** `aspects.storage_mode` follows the legacy metadata
   pattern the codebase already uses for `compression_config` (in the
   wireframe CREATE for new databases, plus an idempotent
   `ALTER … ADD COLUMN` for existing ones, its duplicate-column error
   ignored), because the legacy `metadata.db` is not one of the seal
   store's four control-plane databases. The seal store the aspect's
   frames live in (`{db}/segments_v2/`, scope `(db, 'legacy')`, keyed by
   the AspectId UUID) is a normal Storage-v2 root: all of its DDL is the
   registered migration registry (FRE-14's `0003` included), and no new
   DDL was added there.
3. **Mode choice.** Recorded once at `track_aspect`: a compression config
   forces rows mode (compression deletes ranges; the seal store cannot),
   otherwise `WEFT_LEGACY_DEFAULT_STORAGE=segments` makes the new aspect
   seal-backed, otherwise rows. Existing rows are never switched and there
   are no union reads; a pre-S19 row (`NULL`/no column) reads as rows.
4. **Writes and the queue.** `batch_capture_measurements` is one
   `SegmentStore::ingest` under the design's derived key
   (`legacy:{dataset_id}:{payload_crc:08x}:{n}`, CRC-32 over the canonical
   rows, `n` the row count); TxIds are synthetic UUIDv5 over key and row
   index, with SHA-1 computed in `legacy_seal` because this build of
   `uuid` does not enable its `v5` feature. `capture_measurement` is a
   one-row ingest followed by
   `squash_aspect_to_target_rows_if_fragmented` at a 100,000-row target.
   Until D-S20, seal-backed ingest still enqueues its timestamps in the
   per-timestamp queue with the S18 write-ahead shape, so the queue
   consumer works unchanged; D-S20 is the slice that drops the queue.
5. **Read-back identity.** The readers the design names (raw, chunk,
   range, count, boundary, earliest/latest) go through the seal store.
   A returned `Measurement` synthesises its id deterministically (UUIDv5
   over aspect, timestamp, value and occurrence — two reads of one stored
   row agree, which the boundary reader's de-duplication needs) and
   carries the nil dataset id, because a frame stores neither.

**Not in this slice:** D-S20's `segment_changes` queue replacement (its
own W7 seal-lane slice; the `:780` gate tick lands with D-S19 + D-S20
together, per plan §6 W7 — this slice ticks only the W5 D-S19 line), the
other legacy capture variants (`capture_new_measurement` and the chunk
helpers, which the design does not name), and compression on seal-backed
aspects (deferred by the design itself).

## 2026-11-01 — SEC-10 exposure and secrets hygiene: the shapes the plan left open — FOR MERGE REVIEW

The plan's whole SEC-10 spec is §3's dedup row ("absorbs SOA-14's OTLP
userinfo redaction", 0.5 h) plus the queue's two acceptance lines
(exposure hygiene across the server surface; secrets hygiene including
that redaction). An audit of the surface on main at 33d3353 found the
rest of the surface already hygienic — request spans record the path
only (no query, no headers), 5xx bodies are redacted sentences (X-ERR),
`/ready` carries counts only (D-S12), startup lines print key counts and
file paths but never key material, and the OpenAPI document is generated
but not served. Two gaps remained, and this slice closes exactly those:

1. **OTLP userinfo redaction (the absorbed SOA-14 part).**
   `startup/hooks.rs` printed `OTEL_EXPORTER_OTLP_ENDPOINT` as configured
   in both its startup lines (the enabled `println!` and the
   exporter-build-failure `eprintln!`). The endpoint is a URL and can
   carry userinfo (`https://user:password@collector:4317` — the shape
   collectors use for basic auth), so a configured password landed in
   the server's stdout. New `weft_server::redact::redact_url_userinfo`
   replaces every URL's userinfo with `[redacted]`, mirroring
   weft-bench's EVI-3/SOA-2 manifest redaction (same marker, same
   semantics) in a server-local module — weft-bench is not a dependency
   the server may take. **Redaction is display-only**: the exporter
   still receives the endpoint exactly as configured, because the
   collector's basic auth rides on the userinfo.
2. **Credentials in the query string are now refused in every auth
   mode.** SEC-6's `400 credentials_in_query` check ran only when a key
   store was mounted; with auth off (the loopback default) or explicitly
   disabled, `enforce` returned before the check, so a credential in a
   query string passed through untouched — into exactly the proxy and
   access logs the refusal exists to keep it out of. The check now runs
   first in `enforce`, before the mode branches. Two consequences,
   accepted deliberately: a proxied auth-off request carrying a
   credential parameter now gets the 400 instead of SEC-8's 403 (the
   request shape is refused before the deployment shape is considered),
   and a gateway deployment with `WEFT_AUTH=disabled` whose clients put
   tokens in query strings will see 400s — that is the hygiene working
   as intended, and the parameter-name list is unchanged
   (`api_key`, `apikey`, `access_token`, `auth_token`).

**Not in this slice:** SEC-9's audit (its own slice), SEC-7 key reload,
and any change to what `/ready` reports (its build info and store uuid
are deliberate, per plan §5 D-S12). The keygen subcommand still prints a
minted key once — that is its documented purpose, not a leak.
## 2026-11-01 — FRE-5 "no silent narrowing": the shape of the refusal — FLAGGED FOR MERGE REVIEW

**Grounded:** FRE-5's first two clauses landed earlier (spelling half 2026-10-28,
exact-case tokens 2026-10-29; ROADMAP line 90). The remaining clause names no
mechanism. In the L-COMPUTE lane the plan assigns to FRE-5
(`weft-server/src/{interpolate,downsample}.rs`, plan §2), the silent narrowing
was concrete: every wire conversion did `value.to_f64().unwrap_or_default()`,
so a computed `BigDecimal` outside the `f64` range (e.g. a `sum` of two 1e308
samples) came back as `0.0` — a plausible-looking answer the caller never
asked for, with no signal anywhere.

**Chosen:** one checked conversion (`downsample::wire_value`) at all three
L-COMPUTE wire sites — downsample bucket values (shared with the stored-range
endpoint via `buckets_to_response`, which now returns `Result`), interpolate
series points, and the interpolate single-point response. A value with no
finite `f64` representation fails the request with the taxonomy's existing
`not_finite` code (400), matching how produced non-finite values are already
classified (`from_reduce_error`'s SketchValue arm, splimes' NonFiniteResult).
`ValueNotRepresentable` was considered and rejected: its documented meaning is
the aspect's physical type, not the wire type.

**Unchanged:** the documented transport boundary itself. In-range values
still narrow to `f64` exactly as before (the module docs call the boundary
deliberate; the precision-preserving representation is Phase 4 work). Only
the silent substitution is gone. The claim adjudication behind taking this
slice (the peer's `compute/fre-5-*` branches treated as stale squash
leftovers) is recorded on board issue #125 and in the loop's queue notes.

## 2026-11-02 — PAC-13 offline restore CLI: the shapes the plan left open — FOR MERGE REVIEW

The plan gives PAC-13 one line ("Offline restore CLI over D-S16
(`weft-server restore`)", W7) and the C-7 table the exit statuses, so
every shape below is a judgement call this loop made. D-S16 landed the
library half on 2026-10-30 (`restore_store`, the whole-store manifest,
the restore-intent marker), which is the whole reason this slice is
startable now.

1. **Why this slice leads W7, with W6 items still open.** W7's other
   code items want W6 remainders — ROB-13 wants the bounded-read work,
   ROB-16 wants the GPU provenance lane, SOA-11 and EVI-19's RPO/RTO
   want SOA-10's checks and EVI-25's measurements — while PAC-13's only
   prerequisite is D-S16. PAC-9 (image) and PAC-11 (systemd unit) are
   listed ahead of it on the packaging lane but are siblings, not
   dependencies: the CLI ships inside whatever artifact packs it, so
   neither blocks the binary. Chose the unblocked one and left the W6
   remainder for the other lane rather than serialising behind it.
2. **The exit statuses are the ones the server already returns.** The
   command reuses `settings::CONFIG_EXIT_CODE` (78) and
   `settings::STORE_EXIT_CODE` (65) rather than minting restore-specific
   codes: a supervisor that already understands "78 = fix the command
   line or the destination, 65 = the store/backup needs a human" needs no
   new rule, and PAC-7's operator CLI folds this command into its table
   without changing a number. The 78/65 split is by *which side of the
   terminal* is wrong, not by severity.
3. **The command exits the process itself.** Handing a `Refused` back to
   `main` would flatten every status to 1 through the runtime, so
   `restore::run` prints the reason to stderr and calls
   `std::process::exit` with the code — the same shape
   `store_open::spawn_exit_on_poison` uses for 77, in the same crate, for
   the same reason. The library API stays `Result`-shaped
   (`Refused: Display + std::error::Error`) so a caller that wants its own
   exit policy can keep it; only the binary's path exits.
4. **There is no `--force`.** The destination must not exist, so the one
   thing this command cannot do is remove a live store. That is deliberate:
   the library already refuses to overwrite a control plane, and a restore
   that replaces a running root is a deployment decision (move the old root
   aside by hand, then restore) rather than a flag on the command that is
   also the last-resort recovery tool. The refusal is 78 and writes
   nothing, so a mistyped `--into` cannot damage anything.
5. **`--adopt` runs the adoption the marker defers, in the foreground.** A
   whole-store restore publishes `RESTORE_INTENT`, and the frames it
   carried are adopted into the index at whatever open first reads the
   root. Waiting for that is correct but invisible at 3 a.m., so `--adopt`
   opens the restored root once and reports adopted / held / quarantined /
   suspect counts and the scope it opened. `--database`/`--subject` name
   that open and are *refused* without `--adopt` rather than quietly
   ignored — a scope flag that changes nothing is worse than an error.
   Adoption is idempotent, so omitting `--adopt` is safe and the CLI says
   so in its output.
6. **One command, both backup kinds, chosen from the manifest.** The
   alternative was two subcommands. The backup already declares its kind
   (`weftdb-whole-store-backup/1` versus the control-plane manifest), and
   an operator at 3 a.m. does not know which kind they have, so the
   command reads `MANIFEST.json` and takes `restore_store` or
   `restore_control_plane` accordingly, printing which leg it took. A
   pre-manifest backup — the v0.1.0-era shape, four database files with no
   manifest and therefore no checksums to verify against — is refused at
   65 rather than copied on trust: a restore that cannot verify is exactly
   what D-S16 exists to prevent. A manifest naming a store format this
   build cannot honour (a newer WeftDB's write floor) is the same code.
7. **Hand-rolled parsing, no new dependency.** `main.rs`'s subcommand
   dispatch and `import-rows-mode`'s `--flag <value>` parsing are the
   precedent; adding `clap` for one more command would buy little and
   change how every other subcommand parses. `--help`/`-h` anywhere in the
   command line prints the module's own `USAGE` text (a bare `restore` word is
   treated as that question, not as an error), and an unknown option is a 78 that
   points at `--help`.

**Not in this slice:** PAC-7's operator CLI (it will absorb this command's
table), any HTTP route that restores over a live control plane (still a
deployment decision; the drill remains the only live-server restore), the
image and unit files (PAC-9, PAC-11), and any change to
`weftdb::restore_store` semantics — this is a binary over the library, and
`Cargo.lock` is untouched.
## 2026-11-02 — PAC-9 container image: the shapes the plan left open — FOR MERGE REVIEW

The plan gives PAC-9 one line ("Docker image and container smoke test (with a
mounted keys file)", W7) and no image, no base, no user, no registry. Each
choice below is recorded because a later reader cannot recover it from the
Dockerfile alone.

**Publishes nothing.** PAC-9 builds and proves an image; PAC-10 is the item that
says "Publish the multi-arch image (only after API auth)", and line 354 reserves
the ghcr.io package make-public for the owner. So the workflow builds the image,
runs the smoke test against it, and pushes nothing. The alternative — pushing
`ghcr.io/basic-automation/weftdb:latest` from CI now — would publish an image
ahead of the item that decided to publish it, under a name whose visibility is
an owner action. Cost: README cannot offer a `docker pull`, and says so.

**Builder is the release's manylinux_2_28 baseline, not `rust:`.** The image
then carries the same glibc floor as the release binaries (PAC-2's 2.28), which
`scripts/check-glibc.sh` re-reads out of the binary inside the finished image. A
`rust:1-slim` build would produce a binary needing Debian's glibc and put a
different promise behind the same name. The image tag and digest are copied from
the x86_64 leg of `release.yml`, and the same `.github/release-toolchain`, the
same `--profile dist` and the same `--locked` lockfile are used.

**Runtime installs no packages at all.** An earlier draft `apt-get install`ed
`curl` and `ca-certificates` for the health check and for operators. Cut: the
probe runs on bash's `/dev/tcp`, so the image ships no HTTP client, no
package-manager state, and depends on no mirror at build time. The published
cost is an operator inside a container who has to install what they need — which
is the correct posture for an image whose only job is to serve.

**The probe asks `/ready`, not `/health`.** `/health` says the process is up,
which a store that failed to open also satisfies briefly; `/ready` says the
store opened and requests are being answered, which is what should gate traffic,
and it is `Scope::Probe` so it answers with no key (SEC-6) and is never shed by
admission (ROB-10).

**Known gap: the probe cannot speak TLS.** With `WEFT_TLS_CERT_FILE` and
`WEFT_TLS_KEY_FILE` set the listener serves TLS only, so such a container reads
unhealthy while serving correctly. Fixing it in the image means a TLS-capable
client, which is the package class this slice just removed. The right fix is a
readiness check that needs no network at all — a `weft-server` subcommand that
opens the same state in-process — which is new CLI surface and a separate slice.
Filed as a follow-up against #305; documented in the health-check header, the
Dockerfile's `HEALTHCHECK` comment, and the README's container section, with the
`--no-healthcheck` / Compose `healthcheck: {disable: true}` workaround, because
a silent restart loop at 3 a.m. is worse than an honest limitation.

**Binds `0.0.0.0:8080`, so an unconfigured container refuses to start.**
Loopback inside a container is unreachable through a published port, and plan
C8/SEC-8 requires authentication on a non-loopback bind. The out-of-the-box
experience is therefore exit `78` with a message, not an open server: the smoke
test asserts both that a keys-less `0.0.0.0` bind refuses and that a
`WEFT_KEYS_FILE` pointing at an unmounted file refuses. A gateway deployment
opts into `WEFT_AUTH=disabled` itself; the image never defaults to it.

**uid and gid 10001**, above the range Debian's `useradd` hands out, so a bind
mount the container writes cannot collide with a host user's files the way uid
1000 does. The smoke test deliberately declares and ingests as this uid — file
ownership, locking and fsync on a mount the image did not create are the
container-specific part of the write path, and a root-only test path would prove
nothing about a non-root image.

**A separate `container.yml` with a path filter, not a job of `ci.yml`.**
This is the only gate that pays for a full workspace release build inside a
container. `paths:` cannot be scoped per job within one workflow. Push and
nightly triggers cover what a path filter cannot see — a base-image digest that
stops resolving, a toolchain component that vanishes from the baseline. To keep
the scripts gated on *every* pull request, `scripts/packaging/*.sh` joined
`ci.yml`'s shellcheck step; hadolint runs in the container workflow itself.

**Excluded from this slice:** the registry and multi-arch image (PAC-10), the
Helm chart, a Compose example, a `docker-compose`-managed keys workflow,
podman-specific code (the scripts take `--docker COMMAND` instead of assuming),
and any change to `weft-server`'s startup, routes or settings registry — the
image only sets names that `weft-server/src/settings.rs` already registers,
because PAC-5b's strict configuration would stop the container on an invented
one.

## 2026-11-02 — Needs human: an 80-column YAML rule and `ci.yml`'s pre-existing width

**Ambiguous:** the packaging slice added a `shellcheck` glob and a four-line comment to
`.github/workflows/ci.yml`, and the workspace's YAML lint reports 100 `line-length`
violations (max 80) in that file. Whether the repo wants an 80-column workflow style is a
maintainer policy call, not a packaging one.

**Chosen:** the slice's own added lines were wrapped to fit 80 columns, and the file's
pre-existing lines were left alone. **Why:** 101 of the file's 390 lines already exceed 80
on `main`, including the lines the report names first (`main` L5/L6/L7 are 87/87/85; the two
flagged as new, L373 and L382, are byte-identical to `main` L369/L378 and merely shifted by
the comment added here), no CI job lints YAML — `.github/workflows/*.yml` contains no
`yamllint` step and there is no `.yamllint` in the tree, so nothing in the merge contract
enforces the width — and reflowing a file that a dozen open pull requests touch would create
conflicts against every one of them to buy a style change nobody asked for. A tree-wide YAML
reflow, or a `.yamllint` that records the width the repo actually uses, is a fine commit; it
is not fine inside a packaging slice.
## 2026-11-02 — D-S21 crash matrix shape (loop best-guess, FLAGGED FOR MERGE REVIEW)

**Context:** the 1.0 plan names the crash matrix (D-S21, with SOA-9's
`crash_kill9`) as CI durability evidence but does not fix its exact shape, and
decision 20 leaves the privileged power-loss run owner-gated (issue #281).

**Chosen:** one judged matrix in `weft-bench` — every failure test
(kill-ingest, kill-compaction, corrupt-segment) at every matrix seed (3 by
default, `--crash-matrix-seeds`), one report holding every cell, the run
failing if any cell fails. `scripts/crash-matrix.sh` adds the fault-injection
matrix (the same three suites ci.yml's crash job runs) and stages the report
through EVI-4's artifact pipeline as the `bench-report` artifact;
`.github/workflows/crash-matrix.yml` runs it in CI.

**Explicit non-claim:** the matrix proves process-crash durability only. A
kill soak cannot see a missing fsync, so the privileged power-loss validation
(dm-log-writes / QEMU hard reset) remains the owner task (issue #281) and the
§9 durability tick waits for it. No nightly compare gate: a cell either holds
or it does not; the first successful workflow run is the hand-compared
baseline.

## 2026-11-02 — TAG-4 scope: the weftdb crate's read sites, audited (loop best-guess, FLAGGED FOR MERGE REVIEW)

**Context:** the 1.0 plan (§6 W6, "X-SPLIT-STORE → TAG-4") and ROADMAP line
307 name the slice in one line — "weftdb read sites on the frame-reader API
with typed errors" — without itemizing the sites. X-FRAME (W2) had already
built `weftdb::types::frame_read` and routed `SegmentStore`'s production
decode sites through it, which left the slice's remaining scope ambiguous.

**Audit (on main c766830, post X-SPLIT-STORE):** every frame decode in the
`weftdb` crate was inventoried. Production sites — `read_time_range`,
`read_point(s)`, `read_value_range`/`decode_all`, `downsample_partial`'s
`segment_partial`, maintenance's `read_bound_frame`/`merge_members`, and
fsck/adoption's `read_full` calls — already decode through `frame_read`
(`read_frame*` underneath, errors carrying the typed `FrameFaultError`).
The remaining direct legacy-reader uses in the crate were three test read
sites in `segment_store.rs`: the scaled-downsample helper dispatched on the
index row's `format_version` to `read_segment_range_physical` /
`read_paged_segment_range_physical` — the exact routing TAG-4 removes — and
two format checks used `Segment::read_from` / `PagedSegment::read_from`.

**Chosen:** TAG-4 completes the migration inside the `weftdb` crate. Those
three sites now read through `frame_read` (the paged-format check asserts
the frame reader's `rows_per_page()` instead of a layout-specific reader),
the `read.rs` doc references name the frame-reader entry points, and two
store-level tests pin the acceptance at the public API: an envelope-wrapped
(v7) frame reads through `read_time_range`/`read_point`, and a
newer-format frame fails a store read with a downcastable
`FrameFaultError { Undecodable { newer_format: true } }`. After this slice,
no code in `weftdb/src` names a legacy layout-specific reader.

**Explicitly out of scope, deliberately left as they are:** `weft-bench`'s
harnesses (`compression`, `point_lookup`, `range_fetch`) call the legacy
readers *by name* — they measure those readers, and their reports and the
README claims name them, so rerouting them through `read_frame*` would
falsify the measurements. `backup.rs`'s restore verification uses
`describe_frame`, a header/describability check (D-S16's A3), not a decode
read site. `quarantine.rs`'s classifier already takes the typed
`WeftSegError` source under `FrameFaultError`; frame error classes *in
recovery* are TAG-9 (W8), not this slice.
## 2026-11-02 — FRE-6b "strict request parsing": the shape of the refusal — FLAGGED FOR MERGE REVIEW

**Grounded:** FRE-6b's title names no mechanism. The plan assigns the slice to two
lanes (L-INGEST and L-COMPUTE, §2), lists it among the §9 contract slices required
for the semver promise, and its sibling half FRE-6a landed as the X-ERR error
envelope: `StrictJson`/`StrictQuery` already refuse a body or query the server
cannot *read*. What they still accepted was a request it could read but that named
something the endpoint does not have — every request type deserialized with serde's
default leniency, so an unknown JSON field or query parameter was silently ignored
and the caller got a successful answer to a different request than the one it sent
(the request-side twin of the silent narrowing FRE-5 removed from responses). The
codebase already holds the strict shape for a file it parses: SEC-6's keys file
refuses unknown fields (`auth.rs`, `#[serde(deny_unknown_fields)]`, with the
rationale spelled out there — a typo'd field silently ignored mints a key with
defaults the operator never chose).

**Chosen:** `#[serde(deny_unknown_fields)]` on every request type `weft-server`
extracts — 26 types across `backup_routes`, `declare`, `downsample`, `ingest`,
`interpolate` (including the `SplineSpec` enum, so the `polynomial` variant's
fields are strict too), `maintain` and `storage` — covering JSON bodies and query
parameters, top-level and nested. The refusal is the extractor's existing one, no
new vocabulary: `400 invalid_json` for a body, `400 bad_request` for a query, each
naming the unknown field. `weft-server/tests/strict_parsing.rs` pins it twice:
over HTTP on representative routes from each surface, and per type, enumerating
every request type so a future endpoint cannot reintroduce a lenient one without
extending the list.

**Callers:** the slice's "caller updates" half came out empty on evidence, not by
omission — no in-repo caller (weft-server's own suites, weft-bench's soak client,
the packaging/compat scripts) sends a field or parameter any endpoint does not
have; the full workspace bar passing unchanged is the check. The one deliberate
compatibility note: the B-rest aliases (`take`/`page`, `interpolation=`) are real
fields of their types, so alias-speaking clients are unaffected; only names no
endpoint has ever had are refused.
## 2026-11-02 — SEC-7: key reload mechanism (loop best-guess, FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the plan (§6 W6) and the ROADMAP name SEC-7 "Key reload" with the
acceptance "API keys can be reloaded without a restart", and SEC-6's entry reserved
the slice, but neither names the trigger, nor what a bad edit does.

**Chosen:** the store in force is a swappable snapshot (`auth::SharedKeyStore`
wrapping SEC-6's `KeyStore`); a reload re-reads and re-validates the whole keys
file and swaps it in only when the new file parses clean — a malformed edit is
refused and the keys in force stay, which is SEC-6's "never a partially enforced
table" carried into the edit path. The running server's trigger is `SIGHUP`,
handled by a watcher registered in the Daemons registry whenever authentication
is on, logging each outcome; the library also exposes `AuthConfig::reload_keys`
for embedders. **Why:** SIGHUP is the conventional Unix reload signal and needs
no new route, scope or setting — PAC-5b's strict configuration refuses invented
`WEFT_*` names, and a reload route would be API surface the plan never named —
while polling the file's mtime would silently pick up half-written edits, where
a signal is the operator saying the edit is finished. §10's deferral stands:
this is the API-key file only, not SEC-12's certificate hot reload.
## 2026-11-02 — SEC-9: the audit log's shape where decision 8 does not pin it (FLAGGED FOR MERGE REVIEW)

**Grounded:** plan §11 decision 8 (fsynced append-only JSONL, an attempt and an outcome
record per admin or mutating operation, fail-closed; ingest, auth denials and daemons emit
`weft_audit` events), C9 (shutdown drain: serve → SEC-9 TaskTracker → `store.shutdown()`),
C12 (audited routes catch `JoinError::is_panic` inside their own task), SEC-3's frozen
`RouteSpec.audit` table (every mutating route already carries `Audit::AttemptOutcome`), and
X-ERR's reserved `503 audit_unavailable`. The points the sources do not pin, chosen here:

1. **One middleware, the registry's own table.** `audit::audit_route` sits inside the auth
   layer and outside the gate, looks the matched route up in the sealed registry, and
   audits exactly the routes whose spec says `AttemptOutcome`. Reads/probes/compute
   (`Audit::None`) are never recorded. Because auth runs first, a denial is an *event*,
   never an attempt, and an attempt names the authenticated principal (key id + role —
   never the key).
2. **Fail-closed means both records.** A failed attempt write means the handler never
   runs (`503 audit_unavailable`). A failed *outcome* write also answers
   `503 audit_unavailable`: the operation may have completed, but the envelope claims
   nothing either way, and the latched failure refuses every later audited operation
   until restart. Events (committed ingest, auth denial, daemon panic) are best-effort
   at the emit site — none of them can be un-happened — but a failed event write still
   poisons the log, so the next audited operation refuses.
3. **Record shape.** `{"v":1,"seq","ts_ms","kind":attempt|outcome|event,...}`; an
   attempt's id is its own `seq`, named back by the outcome's `audit_id`. Wall-clock
   `ts_ms`, because a trail is correlated against other systems' clocks. Reopening a
   file continues its sequence by counting lines; a final line without a terminator is
   a torn record and refuses the open (it was never fsynced, so it was never reported
   recorded — the operator truncates it).
4. **The file's place.** `WEFT_AUDIT_FILE` names it; else `<store root>/audit.jsonl`;
   else `./weft-audit.jsonl`. The binary opens it in the state phase — after the store
   root's filesystem check, since its default home is inside that root — and refuses
   to start (exit 78) when it cannot (the keys file's rule). A router state with no log attached —
   the stateless test states — passes audited routes through, as a keyless state passes
   requests unauthenticated; the binary always attaches one.
5. **C12's task boundary is the audit layer's spawn.** Audited handlers run in a task
   drawn from the log's tracker; a panic returns as a `JoinError` the layer records
   (`result:"panic"`) and maps the way ROB-3's catch layer would have (ingest by
   D-S14's pre-commit arm, since no phase is readable at this boundary; everything else
   `500 internal`, redacted). ROB-3's layer remains the catch for non-audited routes,
   and ROB-2's abort still fires during the unwind, before any catch. `close_and_drain`
   is startup's C9 step between `daemons.shutdown()` and `store.shutdown()`.
6. **Daemon events come from the registry.** `Daemons` gains an optional audit handle;
   supervision emits `daemon_panic` (with `restarted` and the restart count) beside its
   stderr line. The backup/reconcile ticks themselves keep reporting through their own
   logs — their *failures to stay alive* are what the trail records.
## 2026-11-02 — PAC-7: the operator CLI's shape and the usage class (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the plan gives PAC-7 one line ("Operator CLI, exit-code contract and
store identity at startup", W6) plus the C7 table (§4) and the L-MAIN lane note
("PAC-7 then ROB-11: CLI then runtime builder", §2). No source enumerates the CLI's
commands, says which existing behaviours count as already-landed parts of it, or
reconciles C7's "2 = Usage" row with PAC-13's later, deliberate choice that
`restore`'s command-line refusals exit 78.

**Chosen (best guess, grounded in the code on main, flagged for merge review):**

- **The CLI is a dispatch, not a rewrite.** The three operator commands already
  exist as library modules with their own parsers, help text and tests
  (`keygen`, `restore`, `import-rows-mode`). PAC-7 adds one module,
  `weft_server::cli`, whose `parse` reads the first word into a `Command`
  (serve by default, `help`/`--help`, `--version`, or a subcommand carrying
  the exact argument shape that subcommand's parser already expects —
  restore's parser takes the words after `restore`; the other two take the
  subcommand word too, as before). `main.rs` becomes a pure dispatch over
  it, which is the L-MAIN shape ROB-11's runtime builder then builds on.
  A first word naming no command is refused by `cli::parse` itself rather
  than falling through to `import-rows-mode`'s parser as it did before.
- **Usage is exit 2, and only usage.** C7's row 2 had no owner before this
  slice: every subcommand parse error returned `anyhow::Result` and the
  runtime flattened it to 1, the row C7 reserves for an unexpected error.
  Now the CLI's own refusals exit 2 from `main`, and `keygen` /
  `import-rows-mode` exit 2 inside their `run` on a parse failure (the
  `restore::run` precedent of exiting inside the library, where the codes
  are defined). Their operational failures still return errors (exit 1),
  and the classified startup refusals keep their codes (78 config, 65
  store, 75 locked, 70 poison). `settings` gains the two C7 constants that
  had no home: `USAGE_EXIT_CODE` (2) and `UNEXPECTED_EXIT_CODE` (1).
- **`restore` keeps 78 for its command line — deliberately not renumbered.**
  PAC-13's entry records the split it chose (78 = fix the command line or
  the destination, 65 = the backup needs a human) and says PAC-7 "folds
  this command into its table without changing a number". Folding it in
  therefore means the CLI's help prints the exception next to the table,
  not that restore's refusals move to 2. A supervisor that learned
  restore's numbers keeps them.
- **The table is code, and the help prints it.** `cli::EXIT_CODES` holds
  the C7 rows; a unit test pins its codes to `[0, 1, 2, 65, 70, 75, 78]`
  and the constants the phases exit with, and `weft-server --help` prints
  the same table, so the contract an operator reads cannot drift from the
  statuses the process exits with.
- **Store identity at startup is one formatted line.** The state phase
  already printed the UUID and layout; PAC-7 moves the formatting into
  `store_open::store_identity_line` and completes the identity: root,
  `store_uuid`, the `(database, subject)` scope the store was opened
  under (from its `STORE_FORMAT`), layout, whether the previous shutdown
  was clean, and what the open did (created / opened / migrated /
  report-only is still the following line, as before). The scope half is
  the new information: one root can hold several scopes, and "which store
  came up" is not answered by a UUID alone. `/ready` is unchanged — it
  already carries the same UUID (D-S12), so log and probe can be checked
  against each other.

**Rejected:** renumbering `restore`'s usage refusals to 2 for uniformity
(breaks PAC-13's recorded promise to supervisors for no functional gain);
absorbing the subcommand parsers into `cli.rs` (their parsers, help text
and tests are theirs; a dispatch that rewrote them would be churn on three
landed slices in the L-MAIN lane PAC-7 shares with ROB-11); and a `clap`
dependency for the top level (the hand-rolled precedent PAC-13 recorded
still holds — one match is not a framework).

**Not in this slice:** ROB-11's runtime builder (next on L-MAIN, and the
reason the dispatch stays a thin match), any change to what the subcommands
do once parsed, and any new command — `PAC-9`'s follow-up (a readiness
subcommand that needs no network) is a separate slice, not this one.
## 2026-11-02 — ROB-11 "Admission II": the runtime-builder side (FLAGGED FOR MERGE REVIEW)

**Grounded:** the 1.0 plan names ROB-11 only as "Admission II" (§6 W6) and
places it on the L-MAIN lane as the wave's second main.rs edit — "PAC-7 →
ROB-11 (W6, CLI then runtime builder)" (§2). ROB-10's entry records the
split from the other side: Admission I is the registry gate's per-class
budgets, and "Admission II (ROB-11, W6) is not pre-empted. It arrives with
PAC-7's runtime builder (§2 L-MAIN) and owns later refinement". The queue
acceptance repeats the shape: "Admission II lands on the runtime-builder
side (after PAC-7's CLI side)". Plan C9 fixes one concrete runtime
behaviour: the drain order ends "serve → SEC-9 TaskTracker →
`store.shutdown()` → runtime `shutdown_timeout(5 s)`" — and the first
three steps already land inside startup, while the runtime itself was
still `#[tokio::main]`'s implicit one, whose shutdown has no such bound.
The points the sources do not pin, chosen here under the owner's
best-guess authorization (2026-10-26), recorded for merge review and
reversible there:

1. **The runtime is built explicitly, in a library module.** A new
   `weft_server::runtime` holds `RuntimeConfig`, `build_runtime` and
   `run` (build → `block_on` → `shutdown_timeout`); `main` reads the
   config, calls `runtime::run(config, dispatch())`, and the dispatch
   body is byte-for-byte the command dispatch `#[tokio::main]` used to
   run. The builder is multi-thread with every driver enabled — the
   attribute's own flavour — so no behaviour changes beyond what this
   entry names. Workers are named `weft-worker`.
2. **One new knob: `WEFT_WORKER_THREADS` (Tuning, unset = the runtime's
   machine-sized default).** It joins the VarSpec registry (74 → 75
   entries) and the strict-config positive-integer list. Sizing the
   pool beside Admission I's caps is the runtime side of admission:
   the gate decides how much work of each class may run at once, and
   this decides the capacity those permits spend. No default number is
   invented — unset keeps tokio's one-worker-per-core, exactly the
   pre-slice behaviour.
3. **`shutdown_timeout` is C9's 5 s, not a knob.**
   `RUNTIME_SHUTDOWN_TIMEOUT` is a constant in `runtime.rs`, applied
   after `block_on` returns — i.e. after serve, the daemons' drain and
   `store.shutdown()` inside startup have all finished, which is the
   order C9 names. The serve path's own connection grace stays
   `WEFT_SHUTDOWN_GRACE_SECS`; the two 5 s numbers are different steps
   of the same drain, not one setting read twice.
4. **A malformed `WEFT_WORKER_THREADS` exits 78 from `main`.** The
   runtime must exist before startup's strict-configuration phase can
   run, so that phase cannot be the first to refuse this one value;
   `main` refuses it itself with the C7 configuration status and the
   registry's own wording. Every other variable is still refused by
   the strict phase first, as before.

**Rejected:** admission policy changes on the gate side (queueing shed
queries, priorities, per-route overrides) — ROB-10's entry lists those
as not-Admission-I, but no source assigns them to ROB-11 either, and
the acceptance names the runtime-builder side, not the gate; a
`WEFT_RUNTIME_SHUTDOWN_SECS` knob (C9 fixes the number, and a second
shutdown knob beside `WEFT_SHUTDOWN_GRACE_SECS` invites confusing the
two steps); and building this slice on PAC-7's unmerged branch — the
PR is based on main like every W6 slice, so at the batch merge PAC-7's
CLI dispatch and this runtime builder combine in `main.rs` (the
dispatch body becomes the future `runtime::run` drives), which is the
L-MAIN order the plan lists.
## 2026-11-02 — ROB-19 maintenance scheduler shapes (loop best-guess, FLAGGED FOR MERGE REVIEW)

**Context:** decision 17 adopts the maintenance/compaction daemon "on by
default once D-S9 lands" (D-S9 has landed) and the ROADMAP names ROB-19
"Maintenance scheduler, compaction on by default, per-aspect circuit
breaker", but neither fixes the default interval, the default compaction
target, or the breaker's shape (threshold, cooldown, scope).

**Chosen:**

1. **On by default, `0` opts out.** An unset `WEFT_RECONCILE_INTERVAL_SECS`
   now resolves to 60 s (`DEFAULT_MAINTENANCE_INTERVAL_SECS`); the existing
   `0` spelling still disables the scheduler. All other daemon settings keep
   their meanings.
2. **Compaction on by default at 65,536 rows.** An unset
   `WEFT_COMPACT_TARGET_ROWS` now resolves to `DEFAULT_COMPACT_TARGET_ROWS`
   (65,536). The daemon's compaction sweep is fragmentation-gated, so a
   converged aspect costs one O(1) rollup read per tick and is never
   rewritten; 65,536 sits between the 8,192-row checkpoint floor and the
   ~200k-row single segment the `downsample_range` knee measured slowly.
3. **Breaker in the store's sweeps, not the daemon.** The per-aspect circuit
   breaker (`weftdb::MaintenanceBreaker`) lives in `SegmentStore`, consulted
   by every store-wide sweep (reconcile, hot/cold, overlaps, squash,
   compaction): it counts an aspect's *consecutive* failed sweep passes,
   trips at 3 (`WEFT_MAINTENANCE_BREAKER_FAILURES`; `0` disables), and sweeps
   then skip the aspect — listed in the sweep's new `tripped` field and in
   `POST /api/v1/storage/reconcile` responses — for a 300 s cooldown
   (`WEFT_MAINTENANCE_BREAKER_COOLDOWN_SECS`). The first pass after the
   cooldown is a half-open trial: success closes the breaker and clears the
   count, failure re-trips for a fresh cooldown. Any successful pass resets
   the count; a busy aspect records neither. Putting the state in the store
   means the HTTP store-wide sweep and the daemon share one breaker, exactly
   as they already share the sweeps. Per-aspect entry points are never
   gated, so an operator can always work on a tripped aspect directly.
   Breaker state is in-memory only (a restart starts every aspect closed —
   the first ticks re-trip a still-damaged aspect within the threshold).

**Alternatives considered:** a daemon-side breaker filtering per-aspect
calls (duplicates the sweeps' iteration and leaves the HTTP sweep
unprotected); breaker metrics counters (deferred — ROB-21 adds
cancellation/metrics on this scheduler, and the trip is already logged once
at `WARN` by the store plus per-tick `DEBUG`/span fields in the daemon).
## 2026-11-03 — SOA-14 adjudicated: its named elements landed under other slices; the residue is the /ready identity pin (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the plan lists SOA-14 by name only (§6, W6: "Server hooks"), and
the queue's acceptance — "Server hooks exist for the soak harness (startup
phases hooks.rs, plan §2 L-MAIN)" — names no deliverable that a diff could
obviously carry. An implementing agent returned blocked asking whether the
slice was already satisfied, and if not, whether "build info" requires fields
beyond the version string and whether "shutdown state" requires a live
shutting-down flag on `/ready`. Traced element by element, every concrete
thing the sources name for SOA-14 has already landed:

- **`hooks.rs` exists** as the §2 L-MAIN startup phase module
  (`weft-server/src/startup/hooks.rs`, phase 1: tracing subscriber, OTLP
  exporter, panic hook), landed with the startup split the lane describes.
- **The OTLP userinfo redaction is SEC-10's**, by the plan's own §3 table
  ("SOA-14's OTLP userinfo redaction" is SEC-10's row), and it landed in
  SEC-10 (W5); `hooks.rs` routes every endpoint it prints through it.
- **The `/ready` content is D-S12's**, by the same table: "/ready returns 503
  when not ready, carries counts only, and carries `store_uuid`, durability
  class, `previous_clean_shutdown` and build info (SOA-14/C-5)" — landed in
  D-S12 (W4), attribution tag and all.

**Decided:** (1) "Build info" is the build version the response already
carries (`weft_server::VERSION`, doc-commented as surfaced in probe responses
so a deployed instance is identifiable from a plain `curl`). No source —
plan, DECISIONS, or code — specifies a git SHA, build profile, or any further
field, and inventing a build-stamping pipeline for an unnamed field is scope
the spec rule forbids. (2) No source requires a live shutting-down flag on
`/ready`; the shutdown-related field the sources name is
`previous_clean_shutdown`, and it is carried. (3) The one residue that is
real: the C-5 identity fields had **no test pinning them** — the existing
`/ready` tests pin the counts, the poison report and the 503, so a refactor
could drop or miswire `store_uuid`, `durability_class`,
`previous_clean_shutdown`, or `version` unnoticed, while the soak harness
(SOA-10's checks read server state against a model) is the consumer that
needs the contract held. SOA-14 therefore lands as that pin:
`weft-server/tests/ready_identity.rs` asserts the fields against the served
store's own reported values (never restated constants), with and without a
store configured. No behaviour changes.

## 2026-11-03 — EVI-19 RPO/RTO: what this page may claim — FLAGGED FOR MERGE REVIEW

**Context:** issue #215 asks to document RPO/RTO "against the measured snapshot
cadence", and plan decision D-S21 corrects the durability design's RPO formula
to `max(I, d_max) + d_max`. The page had to be written now, but the slices that
measure recovery cost — EVI-25 (#295) and SOA-11 (#308) — are still open, so
the honesty of the page turns on what it refuses to state.

**Chosen:** (1) **`d_max` is defined from the write path, not invented.**
An ingest is acknowledged once its index COMMIT returns
(`weftdb/src/types/segment_store/seal.rs:729`), and off Windows the store's
durability class is `full` (`weftdb/src/types/segment_store.rs:1572-1576`), so
a returned COMMIT is durable. A snapshot is a `VACUUM INTO` point-in-time image
(`weftdb/src/types/backup.rs:300`), so writes that COMMIT while a pass runs are
absent from that snapshot: `d_max` is the pass's own duration, which the daemon
runs between ticks — not a setting. The worked example assumes `I = 60 s` and
takes `d_max` from the measured btrfs tick spacing already in reference.md,
labelling `I` as assumed because the daemon is disabled by default
(`weft-server/src/settings.rs:102`). (2) **Measured durations are deferred to
EVI-25/SOA-11.** Neither the offline restore `Report`
(`weft-server/src/restore.rs:184`) nor `RestoreDrillResponse`
(`weft-server/src/backup_routes.rs:36`) nor `BackupResponse` (`:175`) carries an
elapsed-time field — the only timestamp is the absolute `restore_started_ms`
(`weft-server/src/restore.rs:257`), and the only metrics are byte/snapshot
counters (`weft-server/src/metrics.rs:389`, `:422`). Quoting a duration would
mean inventing one, so the page says RTO is `unmeasured` until those two slices
land. (3) **Nothing is rounded into existence.** Every unmeasured figure is
listed with the command or job that would measure it.

**Alternatives considered:** stating a representative RTO (e.g. "minutes") from
the `tmpfs` benchmark numbers — rejected, because those are a tmpfs vacuum cost,
not a restore-time measurement, and the page's job is to be citable, not
reassuring. Treating `d_max` as `I` (so RPO = 2·I) — rejected as an
oversimplification that hides the pass-duration term the formula exists to
capture. Defining `d_max` as a setting — rejected: no such setting exists; it
is a measured property of the storage and the control-plane size.
## 2026-11-03 — EVI-18 backup health metrics and catch-up-free ticking (FLAGGED FOR MERGE REVIEW)

**Context:** the 1.0 plan (§6 W6) and the ROADMAP line name only the outcomes —
"backup health metrics exist" and "backup ticking is catch-up-free" — not the
metrics or the ticking rule. The code pins both gaps down: the daemon's ticker
is a bare `tokio::time::interval`, whose default `MissedTickBehavior::Burst`
runs missed ticks back to back (docs/reference.md already records the symptom:
ticks landing 3.0–4.5 s apart at a 2 s interval), and `/metrics` exposes only
`weft_backup_snapshots_total` / `weft_backup_bytes_written_total` — no failure
signal, no recency, so a daemon that fails every tick, or stops ticking at
all, is invisible to a scrape.

**Chosen:** two health signals, following the reconcile precedent
(`failed_passes` is how a failing sweep stays visible). `weft_backup_failures_total`
counts daemon ticks that failed to snapshot (the loop retries on the next tick,
so a transient failure is invisible without it); `weft_backup_last_success_seconds`
is a gauge of the most recent verified snapshot in unix seconds, `0` when none
has been recorded, stamped by every successful snapshot whichever path took it
(daemon tick or manual endpoint) and seeded at daemon start from the newest
snapshot already on disk, so a restart does not read as "no backup ever" until
the first tick. Ticking is `MissedTickBehavior::Skip`: a snapshot that
outlasts the interval delays the next tick to the following period boundary,
so the daemon takes at most one snapshot per interval however far behind it
fell. Scope is the backup daemon only; the reconcile daemon's ticker is
untouched (a different lane's slice).

**Explicit non-claim:** backup duration is NOT part of this slice. EVI-25 (same
wave, still pending) is the slice that records backup duration and RTO inputs;
putting duration here would duplicate it. The 3.0–4.5 s tick-spacing figure in
docs/reference.md is kept verbatim as a pre-EVI-18 observation under the old
`Burst` behaviour, not re-measured under `Skip`.
## 2026-11-03 — EVI-25 RTO inputs shape (FLAGGED FOR MERGE REVIEW)

**Context:** the 1.0 plan §6 lists EVI-25 in W6 and the ROADMAP line says only
"Backup, restore and recovery costs measured (RTO inputs and backup duration
recorded)"; neither fixes which quantities count as RTO inputs. EVI-19 (W7)
documents RPO/RTO from these inputs, so this slice must record them, not
document the RTO itself.

**Chosen:** the RTO inputs are four recorded durations plus the sizes already
reported: (1) backup duration — every completed, verified control-plane
snapshot observes into the `weft_backup_duration_seconds` histogram (manual
endpoint and daemon tick alike) and the manual response carries its own
`duration_ms`; (2) restore duration — the drill response and the offline
`weft-server restore` report both carry `duration_ms` for the copy plus
verification; (3) recovery duration — the restore report's
`adoption.duration_ms` times the `--adopt` open (frame adoption, quarantine,
clean shutdown) separately from the restore, so the two RTO components stay
distinguishable; a restore without `--adopt` reports no adoption duration,
because the recovery has not run yet. (4) The existing rows/bytes/frames let
a reader normalize a duration by what was moved. Deliberately not changed:
no restore semantics, no new bench (the existing `backup_cost` bench already
times the backup tick; this slice records production durations, which a bench
cannot), and no RPO/RTO document — that is EVI-19.

**Batch-merge note:** this slice deliberately does not touch
`Metrics::record_backup`, `BackupMetrics` or `Metrics::snapshot`, so it
applies cleanly over EVI-18's health-metrics branch (which owns those). The
one shared assertion to union at batch merge is the Prometheus `# TYPE`
count in `metrics.rs`: EVI-18's gauge and this slice's histogram take it
from 18 to 20.

## 2026-11-04 — TAG-5: the series catalog is a layout-2 migration, keyed on the series key (loop best-guess, FLAGGED FOR MERGE REVIEW)

**Conflict:** issue #437 and plan C1 ask for "the series table, the catalog and the
`layout_version=3` marker as a registered migration". The code and the rest of the plan say
otherwise: `store_format.rs` states that "every migration written before 1.0 belongs to
layout 2", the three migrations since FRE-12a (`0003_fre14_legacy_catalog`, `0004_d_s12`,
`0005_d_s13`) all followed that rule, and ROADMAP's FRE-13 ("Layout-1 to layout-2 upgrade
test") and FRE-18 ("layout-2 baseline fixtures") both name layout 2 as the 1.0 baseline.

**Chosen — deviates from C1 and from #437's second acceptance line:**

1. **Layout 2, no `SUPPORTED_LAYOUT` bump.** `0006_tag5_series` is registered at layout 2
   with floors 2/2, `older_writers_safe: false`, no precheck, no re-derive. The marker of a
   store that applies it keeps `layout_version` 2; only `applied_through` moves. So #437's
   "layout_version=3 marker written by that migration" is **not** met, on purpose.
   - Why: a layout-3 bump would raise the floors of every store, untagged ones included, and
     so remove rollback for everyone, to protect only unreleased layout-2 development builds
     (v0.1.0 is layout 1). It would also make FRE-13 and FRE-18 false as written.
   - What it costs: a layout-2 build from before TAG-5 still opens and writes a store that
     has the catalog (floors 2/2 let it). It never reads the table, and it cannot write a
     tagged series itself (the library and HTTP gates stay shut until TAG-8 and TAG-11).
     But once a tags-enabled build has written tagged rows, a pre-TAG-5 build that reopens
     the store breaks series isolation: its `segment_index` reads (`rows`, `all`) select by
     aspect with no `series_id` filter, so it returns tagged rows as untagged results, and
     its fsck R10 seeds `next_series_id` from the rows alone, ignoring the catalog.
   - **Accepted, not mitigated:** no released build is affected. v0.1.0 is layout 1 and
     refuses a layout-2 store, so only unreleased development builds between FRE-12a and
     TAG-5 could reopen a tagged store. Guarding against them needs a lazy floor raise to 3
     on the first tagged write, but `ensure_floor` cannot raise past `SUPPORTED_LAYOUT`, and
     that would bring back the layout-3 bump this entry rejects. If the owner wants that
     guard, it belongs to the layout decision at rc.1 (FRE-18), not to TAG-7.
2. **Catalog shape:** `series(aspect TEXT, series_key TEXT, series_id INTEGER,
   PRIMARY KEY(aspect, series_key))` in `segment_index.db`, so a registration commits in the
   same `IndexTxn` as the rows that use it. The primary key is the key, not the id: a plain
   `INSERT` of a taken key fails on the primary key, which is the conflict path proven
   under Turso's MVCC. No secondary `UNIQUE` index is relied on (every index in this code
   base is best effort). Ids are unique because only the allocator
   (`aspect_seq.next_series_id`) hands them out, under the aspect's commit lock, and
   because the registration's `INSERT ... SELECT ... WHERE NOT EXISTS` re-checks that no
   key of the aspect holds the id, so a retried transaction that read a stale allocator
   fails with `Conflict` rather than giving a second key the same id. The key is
   stored as its canonical text. Series 0 (the empty tag set) is implicit and has no row.
   The migration is pure DDL: no backfill.
3. **No composite `segment_index(aspect, series_id, min_ts, max_ts)` index here.** It would
   cost every `InsertNew` and every member delete on every workload, untagged included,
   long before anything reads by series. It is deferred to tag pruning.
4. **`IndexOp::RegisterSeries { aspect, series_id: SeriesId, series_key: SeriesKey }`** is a
   plain `INSERT`, classified as guarded (a retry re-checks both the key and the id and
   fails with `Conflict` on either), and is always paired with
   `SeedSeries { next_series_id: id + 1 }` in the same transaction. Series 0 and the empty
   key are refused as `Definite` before any statement runs, so the empty tag set never
   gets a second id and no key ever aliases series 0.
5. **Readers** on `SegmentIndexStore`: `series_id_for`, `series_key_of` (decoded through
   `SeriesKey::from_canonical`, the storage path that accepts reserved `__` keys),
   `series_of` and `next_series_id` (default 1). They are crate-private, so the public
   `SegmentIndexStore` methods stay aspect-wide, and they carry
   `cfg_attr(not(test), expect(dead_code))` until TAG-7 and TAG-8 call them.
6. **R10 covers the catalog.** fsck now walks the union of the aspects `segment_index` and
   `series` name, and seeds `next_series_id` past both the largest `series_id` on a row
   and the largest id the catalog registered. Without the catalog term, losing
   `aspect_seq` after every row of a series was quarantined would hand that id out again.
   The catalog maximum is read with `MAX(series_id)`, not by decoding keys, so the repair
   pass still seeds past a catalog whose keys this build cannot decode.
7. **`pub struct SeriesId(pub u64)`** in `weftdb`, with `SeriesId::UNTAGGED = SeriesId(0)`,
   `Default` (untagged) and `Display`. It is a newtype so that a series id cannot be passed
   where a segment id is meant. `IndexRow.series_id` stays a bare `u64` for now; moving the
   row type over is left to the slices that read by series.

**Rejected:** layout 3 with floors 3/3 (the design's B1), for the reasons in 1. Floors 2/2
with a re-derive hook (B2): it would be the first production use of `rederive` and still
protects only development stores. `PRIMARY KEY (aspect, series_id)` plus `UNIQUE (aspect,
series_key)`: the unique index is the part that cannot be relied on.

## 2026-11-04 — TAG-7: tagged frames are a critical series-key envelope, the id is resolved at commit, and each aspect has a series cap (loop best-guess, FLAGGED FOR MERGE REVIEW)

**Ambiguous:** issue #441 names the slice only ("seal-lane tags work"). The design note's PR 2
and its critique (B7-B9) fix the shape; where they disagree the critique wins. The library gate
stays shut (tags D5), so nothing outside the crate's tests writes a tagged batch yet.

**Chosen:**

1. **Frame identity (tags D3).** A tagged batch's frames are each wrapped in a v7 envelope with
   exactly one extension, `EXT_SERIES_KEY`, `critical: true`, whose payload is the canonical
   `SeriesKey` bytes. The frame never records the id. An untagged frame is not enveloped and is
   byte-identical to what `ingest` sealed before this slice; a test pins its CRC to the value
   measured on the pre-TAG-7 commit. `byte_len` and `frame_crc` are the wrapped frame's (the
   envelope ends in its own CRC, which is what the durable write reads off the trailer), so
   `read_bound_frame` and fsck's checks need no change. `critical: true` is kept although the
   critique calls the rationale weak (B10): it is harmless, and a build that cannot bind the key
   must not serve the rows as untagged.
2. **Chunking.** A tagged batch is cut to `MAX_ENCODED_FRAME_BYTES` less the envelope's worst
   case (16 + 6 + key bytes, at most 1,046), so the wrapped frame still fits the target. Only
   tagged batches pay it; an untagged batch is cut exactly as before.
3. **`format_version` (critique B7).** The descriptor, and so `IngestResponse.format_version`,
   records the **body's** layout: 5 single-block, 6 paged, never 7. Readers already choose
   their decoder from the frame's own header (X-FRAME), so the number is a description, and
   the body is what a caller can compare across tagged and untagged writes. The envelope is
   documented on `IngestResponse` and counted in `byte_len`. Pinned by
   `tagged_ingest_writes_a_critical_series_key_envelope`.
4. **Series resolution at commit.** `publish_ingest` resolves the key under the aspect's commit
   lock, in the same `IndexTxn` as the rows: the catalog's id when the key is registered;
   otherwise `aspect_seq.next_series_id`, pushed as `RegisterSeries` plus `SeedSeries`. Every
   row's `series_id`, the receipt's and the ledger's `series_ids` carry that id. A `Conflict`
   on the registration is an error, not retried (critique B8): only ingest registers series,
   and only under the commit lock. A crash before the commit leaves the frame as ordinary
   crash litter and leaks no id (`a_power_cut_between_tagged_frames_leaves_no_catalog_row_without_rows`).
5. **Gate split.** Public `ingest` is `ingest_inner(.., TagPolicy::Refuse)` and still answers
   `GroupsUnsupported`. `TagPolicy::Allow` is crate-private and, until TAG-8, constructed only
   by tests. Reserved `__` keys are not refused on that path yet; that check belongs to the
   gate flip (critique D17).
6. **`IngestBatch::series_ids()` is removed (critique B9).** Correction to B9's premise: the
   method did ship, in `weftdb` 0.2.0 on crates.io (v0.2.0 published the library crates). It is
   removed anyway, because it could only ever answer `[0]` and a tagged batch's id does not
   exist until commit; the pre-1.0 policy allows the break in the next minor release, and
   CHANGELOG marks it as breaking. Deprecating instead would leave a method that answers wrong
   for every tagged batch.
7. **Series cap.** `StoreError::SeriesLimit { aspect, limit }` and
   `SegmentStoreOptions::max_series_per_aspect` (`DEFAULT_MAX_SERIES_PER_ASPECT` = 100,000;
   the number is a guess, as the design says). It counts the catalog's rows for the aspect
   (series 0 never counts), is checked before any frame is written and again at commit under
   the lock, and refuses only a new key. `weft-server` maps it to the existing `400
   series_limit`. No operator setting is added; one Test-tier harness variable
   (`WEFT_TEST_TAG_SEAL_CRASH_ROOT`) is, so the registry holds 84. A refusal at commit
   (the race the pre-check cannot close) unlinks the frames that batch just wrote, so
   recovery never journals a refused key's frames as orphans for TAG-9 to adopt.
8. **Maintenance outputs keep their key.** `PlannedOutput` carries `Option<SeriesKey>`;
   `swap_plans` looks up `series_key_of` once per tagged plan, and `write_output` wraps the
   output in the same envelope. A plan whose series the catalog does not hold fails the swap.
   This lands here, not in TAG-6, so that no commit ever puts an untagged frame under a
   non-zero `series_id`.
9. **No floor raise.** Following the TAG-5 entry and D1, nothing calls `ensure_floor`; the store
   stays at layout 2.

**Rejected:** writing the id into the frame (meaningless after a restore); allocating the id
before the frame write (a catalog transaction outside the commit lock, and a crash would leak
ids); retrying once on a registration `Conflict` (a dead path under the commit lock);
reporting `format_version` 7 for a tagged frame (it would describe the wrapper rather than
the data, and differ between two writes of the same rows).

## 2026-11-04 — TAG-6: maintenance plans within one series; precedence, overlap stats and gates count per series; hot tail per series, aspect-wide threshold (loop best-guess, FLAGGED FOR MERGE REVIEW)

**Ambiguous:** issue #440 names the slice only ("maintenance lane"). The design note's PR 3 and
its critique (C11-C14) fix the shape; where they disagree the critique wins. Tagged rows still
come only from the crate's tests (the library gate opens in TAG-8), so nothing outside them
reaches the per-series paths yet; an untagged aspect is one series and plans exactly as before.

**Chosen:**

1. **Partition first.** `overlap_components` splits an aspect's rows by `series_id` (`by_series`)
   and sweeps each series on its own, so a component never mixes series (I12 is now guaranteed
   upstream of `swap_plans`' bail). The M2 closure property holds per series, which is sound
   only because a read targets one series (tags D4).
2. **No batching across components (critique C11).** `reconcile_overlaps_held` already makes one
   swap per component; it is unchanged. The design's batch test is dropped.
3. **One swap per series (critique C12).** `squash_aspect` folds each series into its own output,
   looping one series per `swap_plan` rather than putting a plan per series into one
   transaction, so no swap nears `MAX_PLANS_PER_TXN` (32) whatever the series count, and
   memory stays one series' merge. A failure leaves the series squashed before it squashed.
   `squash_aspect_to_target_rows` groups consecutive ids within a series; the P1 argument is
   restated per series (no other row *of the series* has an id between a group's members).
4. **Gates count per series (critique C13).**
   - `squash_aspect_if_exceeds`: the cap applies per series. The cheap aspect-wide count still
     settles most ticks (no series holds more than the aspect); past it, only the series over
     the cap are squashed.
   - Fragmentation gate: after the O(1) rollup check, an aspect with registered series computes
     `ideal = Σ max(1, ⌈rows_s / target⌉)` from its rows. That sum is never below the
     aspect-wide ideal, so an untagged aspect (no catalog rows) still costs one rollup read.
   - The overlap sweep's `had_overlap` is the per-series count, so a tagged aspect whose series
     merely share a window is not reported as reconciled.
5. **Precedence per series (critique C14, overriding the design's "leave it aspect-wide").**
   `refuse_outranking_outputs` compares only rows of the members' series. Left aspect-wide, a
   newer row of any other series in the window would refuse almost every `split_segment` and
   P2 output of a tagged aspect.
6. **Hot tail per series, aspect-wide threshold.** `reconcile_aspect_hot_cold` keeps each
   series' highest id hot; the backlog it compares with `threshold` stays the aspect's unsorted
   count, so a single knob stays and the untagged decision is unchanged.
7. **Overlap stats per series, no descriptor field.** `aspect_stats().overlapping_segments`
   and `store_overlapping_segments` sum `SegmentIndex::overlapping_count` over one index per
   series built from the index rows. `SegmentDescriptor` gets no series field (its serde shape
   is frozen).
8. **Deviation: the daemon test.** The design asks for weft-server's
   `every_tick_kind_isolates_a_truncated_frame` with a two-series aspect. weft-server cannot
   write a tagged batch until the gate opens (only the crate-private `TagPolicy::Allow` can),
   and a `fault-injection` hook would run only in a job that does not run the lib tests. The
   same coverage lands in weftdb as
   `every_sweep_kind_isolates_a_truncated_frame_beside_a_two_series_aspect`, which runs each
   store-wide sweep the daemon's six tick kinds call; the weft-server test is unchanged and
   gets its two-series variant when TAG-11 opens the HTTP gate.

**Rejected:** batching plans across components or series into one transaction (no saving the
swap needs, and it holds every merged series in memory at once); a series field on
`SegmentDescriptor`; per-series metrics and sweep counters (deferred by the plan); a per-series
backlog threshold for the hot tail (a second knob for no measured need).

## 2026-11-04 — TAG-9 (part of #310): recovery binds series keys — tagged orphans adopted into their series, held rows released, indexed rows checked against their frame's key (loop best-guess, FLAGGED FOR MERGE REVIEW)

Minimal TAG-9 per design PR 5 with critique E19-E20. The broader C5 frame-error-class work
of #310 is not done here.

1. **`adopt_orphan` takes `Option<SeriesKey>`.** A tagged orphan resolves its key in the
   frame's aspect — the catalog's id, or the aspect's next series id with `RegisterSeries` +
   `SeedSeries` — in the same transaction as its `InsertNew` and allocator bump (plus
   `ClearJournal` when it releases a held row). Adoption now runs under the aspect's commit
   lock and takes its id from `CommitGuard::allocate`, rather than computing it from
   `allocator_seed` beside the lock: a held row may be released by an fsck on a live store,
   where an id handed out outside the lock would collide with the lock's cached allocator.
   Ids may therefore start past a legacy-named file's id in `segments/`, as every seal's do.
2. **No series cap in recovery.** The new `resolve_series_key` (shared with ingest's
   `resolve_series`, which still checks `max_series_per_aspect` first) applies no cap: the
   data already exists, and refusing to bind it would leave it invisible again.
3. **Restore mode:** a decodable tagged orphan is adopted directly; `JournalHeld` only when
   its aspect is undeclared (no catalog to bind in). Held rows are released on every later
   pass (open or fsck) through the same `adopt_orphan` path once the aspect is declared; a
   still-undeclared one stays held. A held or orphan frame whose key cannot be bound is
   quarantined suspect (the held one's journal row cleared in the same transaction).
4. **Indexed rows:** `describe_frame` beside `read_full`. A `SERIES_KEY` payload that is not
   canonical, is empty, or appears twice, in a frame whose body decoded (so its CRC verified),
   is **suspect** under C5. A key that differs from `series_key_of(row.series_id)` is rebound
   with one `ReplaceExpected` (registration ops first in the same transaction) and counted in
   the new `RecoveryReport.series_rebound` as well as `rebound`. Critique E19: a row moved to
   the aspect its frame names gets its key resolved in the **target** aspect's catalog, never
   the source aspect's id.
5. **`report_only` everywhere:** it counts the suspects, series rebinds and held releases it
   would make, and registers, rewrites, quarantines and clears nothing.
6. **Rollup rebuild moved after the orphan pass**, so aspects that gained adopted rows get
   their rollup rebuilt in the same pass (before, adoptions were pushed onto
   `changed_aspects` after the rebuild loop had already run).
7. **Fixture and wording.** `restore_mode_adopts_orphans_and_holds_series_key_frames` used a
   non-canonical payload (`price|a=1`, critical false); it becomes
   `restore_mode_adopts_series_key_frames_into_their_series` with `a\x1f1`, critical true,
   expecting adopted = 2, held = 0. `restore_cli.rs:83` already asserted held = 0 (E20) and
   is unchanged; `restore.rs` wording no longer says frames are held "until TAG-9".
8. **Deviation (tests):** the inverted restore test cannot assert that untagged reads omit the
   tagged rows, because reads are still aspect-wide until series-scoped reads land (TAG-8);
   it asserts the tagged rows are readable and bound to their registered series instead.

**Rejected:** carrying the source aspect's `series_id` on an aspect move (E19); treating a
non-critical `SERIES_KEY` as malformed (writers always mark it critical, and the key is
still well-defined); a separate `released` counter (released held rows count as `adopted`).
