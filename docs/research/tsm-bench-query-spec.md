# TSM-Bench Query Spec — Weft-Bench adoption

## 1. The spec as we will use it

### Source of the spec

The TSM-Bench benchmark is defined in:

> Abdelouahab Khelifati, Mourad Khayati, Anton Dignös, Djellel Difallah, and Philippe Cudré-Mauroux. "TSM-Bench: Benchmarking Time Series Database Systems for Monitoring Applications." *PVLDB*, 16(11): 3363–3376, 2023. doi:10.14778/3611479.3611532.

The paper defines seven queries (Q1–Q7) over a wide-format sensor table (`ts_table` with columns `time`, `st_id`, and sensor columns `s_1`…`s_N`), two datasets (D-LONG, D-MULTI), and three workload tiers (bulk-loading, offline query, online query). Section 3.3 of the paper carries the canonical SQL for each query; the source repository at `eXascaleInfolab/TSM-Bench` carries system-specific translations under `systems/{system}/queries.sql`.

**Citation note.** The brief for this slice references "Garcia, Seltzer, VLDB 2015" as the TSM-Bench paper. The paper that the community and this codebase (see `gap_fill.rs:4–5`) cite is the Khelifati et al. PVLDB 2023 paper above. The Garcia/Seltzer PDF at `vldb.org/pvldb/vol8/p2030-garcia.pdf` is behind Cloudflare protection and its content could not be verified by this author. All query definitions below are extracted from the PVLDB 2023 paper text, which was obtained and read. If any Garcia/Seltzer-specific definition exists that differs from what is recorded here, it is **unverified — needs the paper text before implementation**.

### Adopted queries

We adopt **Q1–Q7** as defined in the paper. Each subsection gives the canonical SQL from the paper (§3.3), notes what varies, and states the Weft-Bench shape.

---

#### Q1 — Data Fetching

**Paper SQL (Khelifati et al., §3.3, p. 3367):**
```sql
SELECT time, st_id, <s_list>
FROM ts_table
WHERE st_id in <st_list>
  AND time < ?timestamp
  AND time >= ?timestamp - ?range;
```

**What varies:** `?timestamp`, `?range` (time-range offset, default 1 day), `<st_list>` (number of stations, default 1), `<s_list>` (list of sensor columns, default 3).

**Weft-Bench shape:** A `--range-fetch` windowed read (see §2) from a prepared segment, but with the time window expressed concretely and the row count recorded. No `st_id`/sensor filtering — the segment is single-valued. The query is storage-backed, not the `range_fetch` default of 32 windows per rep; a single `[t0 - range, t0)` window per rep.

---

#### Q2 — Data Fetching with Filter

**Paper SQL (Khelifati et al., §3.3, p. 3367):**
```sql
SELECT time, st_id, <s_list>
FROM ts_table
WHERE st_id in <st_list>
  AND time < ?timestamp
  AND time >= ?timestamp - ?range
  AND s_k > ?value; /* s_k in <s_list> */
```

**What varies:** As Q1, plus `?value` (a per-sensor threshold that selects ~5% of values).

**Weft-Bench shape:** Same range-window as Q1 but with a post-read value filter. No existing arm supports a filter predicate in the query (the `gap_fill` farmland and `query_mix` write-side are insert-time transforms); this is a **new arm** or a parameter flag on Q1's arm.

**Unverified:** The paper text says the filter condition outputs "5% from the sensor" (p. 3367). The mechanism for sampling that threshold is described at the prose level — "the condition value is sampled from the data distribution to output 5% from the sensor" — which is guidance, not a precise algorithm. The implementation slice will need to decide the sampling method (quantile look-up, distribution assumption, or ad-hoc). This section is **unverified — needs the paper text before implementation** for the exact filter-threshold derivation.

---

#### Q3 — Data Aggregation (GROUP BY station)

**Paper SQL (Khelifati et al., §3.3, p. 3367):**
```sql
SELECT st_id, AVG(s_i) ... AVG(s_j)
FROM ts_table
WHERE st_id in <st_list>
  AND time < ?timestamp
  AND time >= ?timestamp - ?range
GROUP BY st_id;
```

**What varies:** As Q1; the aggregation is `AVG` applied to each sensor column in `<s_list>`.

**Weft-Bench shape:** The `--downsample` workload computes per-bucket reductions but has no `GROUP BY` equivalent — WeftDB's aspect/series model is single-valued within a bucket, with no secondary grouping key. This query requires a **new arm** that aggregates over a stored segment's time window and reports the reduced value. Without station multi-tenancy, the "GROUP BY st_id" collapses; the weft-bench arm would omit the `GROUP BY` clause and reduce the whole selected range into one aggregate value, which is a meaningful subset of Q3.

---

#### Q4 — Downsampling (SAMPLE BY 1H)

**Paper SQL (Khelifati et al., §3.3, p. 3367):**
```sql
SELECT time, st_id, AVG(s_i) ... AVG(s_j)
FROM ts_table
WHERE st_id in (<st_list>)
  AND time < ?timestamp
  AND time >= ?timestamp - ?range
GROUP BY st_id, time
SAMPLE BY 1H;
```

**What varies:** As Q1; the downsampling granularity is 1 hour (the paper also evaluates by varying granularity).

**Weft-Bench shape:** The `--downsample` workload (see §2) is the closest existing arm: it reduces a seeded dense series into grid-aligned buckets. It is equivalent to the "SAMPLE BY 1H" in-memory half, but missing: (a) a WHERE time-range clause (the workload generates its own full series), (b) GROUP BY st_id, (c) a stored-data path. The gap is that the workload's corpus is generated in-memory rather than read from storage. A new arm or mode that applies `weft_reduce::reduce` to a stored segment's window would match Q4 exactly.

---

#### Q5 — Upsampling (SAMPLE BY 5s FILL(LINEAR))

**Paper SQL (Khelifati et al., §3.3, p. 3367–3368):**
```sql
SELECT time, st_id, <s_list>
FROM ts_table
WHERE st_id in <st_list>
  AND time < ?timestamp
  AND time >= ?timestamp - ?range
SAMPLE BY 5s
FILL(LINEAR);
```

**What varies:** As Q1; the upsampling step is 5 seconds.

**Weft-Bench shape:** The `--gap-fill` workload (`weft-bench/src/gap_fill.rs:4–18`) explicitly identifies itself as TSM-Bench Q5's shape: "reduce a series into grid-aligned buckets, then synthesize the empty ones … `SELECT … SAMPLE BY 5s FILL(LINEAR)`". It differs in:
- **Bucket width:** WeftDB's grid only has unit widths (`s`/`m`/`h`/…), so Q5's literal 5-second buckets are expressed as `Resolution::Seconds` with stride-based approximation (see `gap_fill.rs:14–16`).
- **No WHERE time-range clause:** The `run_gap_fill` generator produces a synthetic corpus for the full generated span; no caller-specified `[t0 - range, t0)` window exists yet.
- **No station/sensor filtering:** The gap-fill arm works on a single generated series.

This is the **mature** arm — it has a correctness gate with accuracy scoring (`gap_fill.rs:12–13`, `:238`), and the codebase already calls it "TSM-Bench Q5's shape" (`gap_fill.rs:4`, `main.rs:2529`, `CHANGELOG.md:1948`).

---

#### Q6 — Cross Average

**Paper SQL (Khelifati et al., §3.3, p. 3368):**
```sql
SELECT time, s_i, s_j, AVG(s_i, s_j)
FROM ts_table
WHERE st_id = st_k
  AND time < ?timestamp
  AND time >= ?timestamp - ?range;
```

**What varies:** `st_k` selects a single station; `s_i` and `s_j` are two sensor columns from within it.

**Weft-Bench shape:** No existing arm computes cross-sensor arithmetic over stored data. This is a **new arm** requiring multi-column context (or two separate range-fetches followed by client-side averaging). The output size is the same as the input range (both sensor columns plus the average per row).

---

#### Q7 — Correlation

**Paper SQL (Khelifati et al., §3.3, p. 3368):**
```sql
SELECT CORR(s_i, s_j)
FROM ts_table
WHERE st_id = st_k
  AND time < ?timestamp
  AND time >= ?timestamp - ?range;
```

**What varies:** As Q6; the output is a single scalar (Pearson correlation coefficient).

**Weft-Bench shape:** No existing arm computes a cross-sensor correlation. The `analytics.rs` pipeline includes a correlation stage (`weft-bench/src/analytics.rs:6`, `:187`) but it is a multi-stage pipeline over a stored corpus, not a standalone one-shot query. A **new arm** that fetches two value ranges and computes a Pearson correlation coefficient would match Q7.

---

### Queries we deliberately do NOT run

The paper defines seven numbered queries (Q1–Q7). The brief for this slice mentions "ten query numbers, Q1–Q10"; no Q8–Q10 appear in the paper or the reference repository's `queries.sql` files. No Q8–Q10 are adopted because no canonical definition exists in the published spec.

The following queries from the TSM-Bench framework are deliberately excluded:

| Query / Workload | Exclusion reason |
|---|---|
| **Bulk-loading workload** (not a numbered query) | Evaluates system-specific loading tools with no uniform harness interface. Weft-Bench's `--ingest` workload (`weft-bench/src/ingest.rs:1–4`) times WeftDB's own `SegmentStore::seal_declared` path, not a multi-system bulk-loader; a cross-engine comparison of bulk loading is a separate policy question. |
| **Online workload** (concurrent insert + query) | Weft-Bench's `--query-mix` (`weft-bench/src/query_mix.rs:1–13`) replays a seeded mix of operations but uses a different concurrency model (single-store lane replay, not streaming ingest with concurrent queries). An online TSM-Bench workload would need a streaming data source. |
| **TSM-Bench's own data generation (TS-LSH / GAN)** | Weft-Bench uses seeded synthetic generators and real corpora (UCI, TSBS); adopting TS-LSH is a separate data-gen evaluation, not a query-spec adoption. |

### Citation inventory

| Statement | Source | Verifiable? |
|---|---|---|
| Q1–Q7 canonical SQL | Khelifati et al., §3.3, pp. 3367–3368 | Yes — paper PDF obtained and read |
| Q1: default 1 station, 3 sensors, 1 day range | Khelifati et al., §3.3 & experiments §5.2 | Yes — paper README (`--nb_st 1 --nb_sr 3 --range 1 day`) |
| Q2: 5% threshold filter | Khelifati et al., §3.3 p. 3367 | Partial — prose guidance, not algorithm |
| Q4: 1-hour downsampling | Khelifati et al., §3.3 p. 3367 | Yes |
| Q5: 5-second upsampling, FILL(LINEAR) | Khelifati et al., §3.3 pp. 3367–3368 | Yes |
| Q6: cross average over two sensors | Khelifati et al., §3.3 p. 3368 | Yes |
| Q7: Pearson correlation | Khelifati et al., §3.3 p. 3368 | Yes |
| Garcia/Seltzer, VLDB 2015 as TSM-Bench paper | Brief for P394 | **Cannot verify** — PDF behind Cloudflare; community/codebase cites Khelifati et al. |

---

## 2. Mapping to our existing arms

### Existing Weft-Bench arms (verified against `origin/main`)

| Arm | File | TSM Query | Gap |
|---|---|---|---|
| `--range-fetch` | `weft-bench/src/range_fetch.rs:1–6` | Q1 (Data Fetching) | Storage-backed, but: no `st_id`/sensor filtering, no `WHERE` time-range expression (uses pre-seeded window offsets), single value column, 32 windows per rep vs one window per rep |
| *(none)* | — | Q2 (Filter) | No arm supports a value-filter predicate on the query path |
| `--downsample` | `weft-bench/src/downsample.rs:1–20` | Q3 (GROUP BY station), Q4 (SAMPLE BY 1H) | In-memory only (no stored segment), no `GROUP BY st_id` (single-valued series), no `WHERE` time-range clause; `Resolution` enum maps to `s`/`m`/`h` but cannot express literal 1-hour bucket semantics without the WHERE clause |
| `--gap-fill` | `weft-bench/src/gap_fill.rs:4–18`, `main.rs:2529–2531` | Q5 (SAMPLE BY 5s FILL(LINEAR)) | No `WHERE` time-range clause, no station/sensor filtering, bucket widths limited to `s`/`m`/`h` (5s approximated via stride); otherwise the most mature mapping |
| *(none)* | — | Q6 (Cross Average) | No multi-sensor arithmetic arm exists |
| *(none)* | — | Q7 (Correlation) | `analytics.rs` has a correlation stage but it is a pipeline stage, not a standalone query — not a direct mapping |
| `--query-mix` | `weft-bench/src/query_mix.rs:1–13` | Online workload (conceptual) | Uses single-store lane replay model, not streaming ingest + concurrent querying. Not a direct query mapping. |
| `--ingest` | `weft-bench/src/ingest.rs:1–13` | Bulk-loading workload (conceptual) | Times WeftDB's `SegmentStore::seal_declared` path, not a multi-system bulk-loader. Not a direct query mapping. |

### Summary

- **Q5** is the only query with an existing arm that explicitly identifies itself as a TSM-Bench query. The `--gap-fill` workload can be adapted with a CSV-style time-range flag and optionally a multi-series generator.
- **Q1** is the next closest: `--range-fetch` provides storage-backed windowed reads. Adding a single-window mode and a `--tsmb-q1` flag that reports row count and wall time would match.
- **Q4** maps to `--downsample` but needs a stored-data path and the WHERE time-range to match the paper's shape.
- **Q2, Q3, Q6, Q7** require **new arms** or substantial extensions: filter predicates, GROUP BY secondary key, multi-sensor arithmetic, and Pearson correlation respectively.
- **Bulk-loading and online workloads** are not direct query mappings and are deliberately excluded from the spec adoption.

---

## 3. The honesty rules that bind it

The adoption of the TSM-Bench query spec is governed by the following sections of `docs/benchmark-policy.md`.

### Section 2 — "What we race, and on what terms"

> **Self-hosted open-source servers only.** We do not benchmark a vendor's hosted service, and we do not use a hosted service to do benchmarking: a terms-of-use clause barring use of the service for developing competing products … is a reason not to sign up, independent of anything about publication.

**How this spec honours it.** Adopting the TSM-Bench *query spec* does not race any vendor on its own harness. We run the spec against WeftDB (our own engine) using Weft-Bench, and we run the spec against self-hosted open-source engines listed in the policy's table (ClickHouse, TimescaleDB, QuestDB, DuckDB, etc.) through `weft-xbench` — each of which the policy has already cleared for self-hosted publication. This is the distinction the issue title makes: "the spec, not the repo." The TSM-Bench repository is a Python benchmark harness that launches each system's client driver; we do not run that harness. We adopt *the query shapes* the paper defines, expressed in Weft-Bench's own arm system and in Weft-XBench's adapter interface.

### Section 3 — "Every published number is ours, and reproducible"

> A comparison may be published only if, for every engine row, we can point at the run that produced it. Required … our commit, and each engine's exact version; the workload and its seeds, scale factors, and cardinalities; the hardware and storage configuration the run used; the harness and flags, at the commit the run used.

**How this spec honours it.** Every adopted TSM-Bench query arm in Weft-Bench produces a `BenchResult` with workload label, seed, profile parameters, and latency distribution — the same reproducibility machinery every existing arm uses. A published comparison under the TSM-Bench spec will list the Weft-Bench commit (or the `weft-bench` crate version), the arm flags, the seed, and the per-engine adapter version. The policy's §3 requirements are already satisfied by the Weft-Bench result schema (`BenchResult`, `CorrectnessReport`, `DatasetMeta` in `weft-bench/src/schema/`).

### Section 6 — "Naming"

> Use the vendor's own spelling of its mark … No logos, no trade-dress, nothing that reads as endorsement or affiliation.

**How this spec honours it.** Publications refer to "the TSM-Bench query spec (Khelifati et al., PVLDB 16(11), 2023)." We do not use the TSM-Bench name as a product endorsement, and we do not claim affiliation with the TSM-Bench project or its authors. The arms are named `--tsmb-q1` through `--tsmb-q7` in Weft-Bench (or `--tsmb` with a query selector), which clearly indicates the TSM-Bench specification as the workload definition while distinguishing it from the benchmark repository.

### Distinction made explicit

> **Racing a vendor on its harness:** Running the TSM-Bench repository's Python scripts (`python3 tsm_eval.py --systems clickhouse --queries all …`), which launch each system's client driver with the benchmark's own measurement collection and result format. This is what `docs/benchmark-policy.md` §2's table refers to when it restricts publication terms — we avoid it because it means delegating measurement to the benchmark's controller rather than our own harness.

> **Adopting a published query spec:** Implementing the same query shapes (Q1–Q7) in our own harness (Weft-Bench arms), with our own measurement, our own reproducibility controls, and our own correctness gates. The numbers are ours, the methodology is ours, and the policy's publication obligations are met by our existing machinery. The query spec is a public scientific artifact (the PVLDB paper), not a vendor tool.

---

## 4. Staged adoption plan

### Stage 0 — Spec document published (this slice)

**Acceptance criteria:**
- [ ] `docs/research/tsm-bench-query-spec.md` exists with all four sections.
- [ ] Every `path:line` citation verified against `origin/main` by grep.
- [ ] `ROADMAP.md:246` ticked (checkbox filled in).
- [ ] `docs/roadmap-notes.md` updated with a dated adoption note.
- [ ] `DECISIONS.md` appended with the P394 decision section.
- [ ] PR `p394/tsm-query-spec` opened against `main` carrying `Closes #394`, not merged.

### Stage 1 — Q5 formalization (the existing gap-fill arm)

**Acceptance criteria:**
- [ ] `--gap-fill` arm accepts an explicit time-range window (`--time-start`, `--time-end` or `--window`, `--range`), defaulting to the full generated span.
- [ ] The arm labels its result with `tsmb_q5` in addition to `gap_fill`.
- [ ] Documentation at `docs/weft-bench/arms.md` states which TSM-Bench query this arm maps to, and what gaps remain.
- [ ] No published number claims TSM-Bench compliance for Q5 before this stage.

### Stage 2 — Q1 storage arm (range-fetch adaptation)

**Acceptance criteria:**
- [ ] `--range-fetch` gains a `--tsmb-q1` mode that runs a single window per rep (instead of the default 32) and reports the row count read and wall time.
- [ ] The arm labels its result with `tsmb_q1`.
- [ ] A CSV-style parameter file or flags allow setting the equivalent of `?timestamp`, `?range`, and the number of stations/sensors (the last two are no-ops for the single-valued segment and become informational).
- [ ] Documentation describes the omitted `st_id`/multi-sensor gap.

### Stage 3 — Q4 downsampling (stored-data path)

**Acceptance criteria:**
- [ ] `--downsample` or a sibling arm applies `weft_reduce::reduce` to a stored segment's time window (a `weftseg` range read piped into `reduce`), rather than an in-memory generated series.
- [ ] The window is specified by caller arguments matching the paper's WHERE clause shape.
- [ ] The arm labels its result with `tsmb_q4`.
- [ ] Documentation describes the omitted `GROUP BY st_id` gap and notes that WeftDB's Resolution unit widths (`s`/`m`/`h`) express the 1-hour SAMPLE BY with `--resolution Hours`.

### Stage 4 — Q2, Q3, Q6, Q7 new arms

**Acceptance criteria:**
- [ ] `--tsmb-q2` arm: a storage-backed range read with a value-filter predicate (reading a stored segment and filtering rows where `s_k > ?value`). Labels result `tsmb_q2`.
- [ ] `--tsmb-q3` arm: a stored-range reduction (AVG) over a time window without GROUP BY (the GROUP BY collapses in WeftDB's single-valued model). Labels result `tsmb_q3`.
- [ ] `--tsmb-q6` arm: fetches two stored ranges and computes per-row average of the two values. Labels result `tsmb_q6`.
- [ ] `--tsmb-q7` arm: fetches two stored ranges and computes the Pearson correlation coefficient. Labels result `tsmb_q7`.
- [ ] Each arm has a correctness gate (round-trip equality or analytic reference).
- [ ] Documentation for each arm names the remaining semantic gap versus the full TSM-Bench spec.

### Stage 5 — Cross-engine comparison gate

**Acceptance criteria:**
- [ ] Every adopted TSM-Bench query arm has an equivalent implementation in `weft-xbench/src/{engine}.rs` for at least the self-hosted open-source engines listed in `docs/benchmark-policy.md` §2.
- [ ] The `weft-xbench` renderer (`render.rs`) can produce a table row labelled "TSM-Bench Q{N}" for each query.
- [ ] The first published number under the TSM-Bench spec is reviewed against `docs/benchmark-policy.md` §3 and §7 (re-read cadence).

### Non-goals for any stage

- Running the TSM-Bench repository's Python harness (`tsm_eval.py`).
- Reimplementing TS-LSH / GAN data generation.
- Claiming TSM-Bench compliance without the paper text for Garcia/Seltzer 2015 being verified.
- Publishing a performance number (latency, throughput) during the spec-adoption slices — numbers belong in a separate `bench/claims.json` change under the claims policy.