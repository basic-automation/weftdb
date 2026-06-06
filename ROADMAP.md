# DSP Roadmap

This roadmap captures feature ideas for DSP harvested from its **15 predecessor
repositories**, which have been consolidated (with full history) under
[`legacy/`](legacy) so the original repos can be retired. Several of these
predecessors were, in places, further along than the current crates — this
document is the distilled "what's worth carrying forward."

It is organized by theme, roughly in priority order. Each item notes its
**source** (the archived crate it came from), its **status** in current DSP, and a
rough **value / effort** read.

> **Porting principle.** The legacy crates are edition-2021, lean heavily on
> `.unwrap()`/`panic!`, hardcode credentials/URLs, and several are nightly-only
> microservices that spin a fresh Tokio runtime per work item. **Port the designs
> and algorithms, not the code.** Reimplement against DSP's current async + typed
> `Error` conventions, the libSQL storage layer, and the `splimes` engine. Do **not**
> reintroduce `sled` or `sqlx` (both were superseded by Turso/libSQL).

Status legend: 🔴 absent today · 🟡 partial / verify-and-complete · 🟢 exists, enhancement only.

---

## Theme 1 — External Data Sources & Ingestion 🔴 (largest gap)

Today DSP ingests via CSV import and direct API calls; it has **no connector
framework and no live data feeds**. The predecessors prove out a full connector
architecture, from a vendor-neutral abstraction down to concrete clients.

> **Design principle — vendor neutrality (hard constraint).** The DSP codebase
> defines **only** a vendor-neutral connector abstraction: ideally a small dedicated
> crate (e.g. `dsp-connector`) holding the `Source`/`Connector` trait, shared types,
> and a runtime registry. **Every concrete connector — Thorchain, InfluxDB, CSV, … —
> lives outside the DSP core as its own pluggable crate that depends on the
> abstraction, never the reverse.** No vendor-specific code, types, or dependencies
> (no Midgard/Thorchain client, no Influx client) may enter the core crates or the
> active workspace. Thorchain is just *one of many* sources and must not be coupled
> to DSP; connectors self-register into the runtime registry so DSP can drive any
> source it is handed without knowing what it is.
>
> The same boundary applies to storage: **Turso/libSQL is DSP's store.** External
> databases such as InfluxDB are *integrations* reached through connectors (as a
> source and/or a sink) — never replacement backends.

| # | Item | Status | Source | Value/Effort |
|---|------|--------|--------|--------------|
| 1.1 | **Connector abstraction (core, vendor-neutral).** A `Source`/`Connector` trait + runtime registry: each connector self-describes which `Subject`/`Aspect` it feeds and how to fetch/normalize into native `Measurement`s; string-keyed selection for config/CLI. **This trait is the only connector code that lives in the DSP codebase.** | 🔴 | `legacy/dsm-source`, `legacy/dsm-asset` | High / Med |
| 1.2 | **InfluxDB 2.x connector (separate crate, interop only).** Reference connector implementing the 1.1 trait against an *external* Influx instance — as a **source** (Flux query → ingest into DSP) and/or a **sink** (export DSP measurements via line protocol; predicate delete). **Not a storage swap — Turso/libSQL stays the store.** Lives outside the core with its own `reqwest`/Influx deps. | 🔴 | `legacy/dsm-influxdb`, `legacy/dsm-batch/src/influxdb2` | High / Med |
| 1.3 | **Thorchain / Midgard connector (separate crate).** One of many sources, kept entirely out of the DSP codebase: an external crate implementing the 1.1 trait to pull windowed pool price history (liveness-gated) and normalize it. Proves the abstraction supports arbitrary remote feeds without any vendor code in core. | 🔴 | `legacy/DSM-Thorchain` | Med / Low |
| 1.4 | **Scheduled polling daemon** — one task per source with a **per-source configurable interval**, continuously producing measurements + `measurement_add` events. | 🔴 | `legacy/DSM-Input-Module` | High / Med |
| 1.5 | **Ingestion retry buffer (at-least-once).** On write failure, push measurements back into an unprocessed buffer and retry next cycle instead of dropping; distinguish connection errors from others to gate retries. | 🔴 | `legacy/DSM-Input-Module` | Med / Low |
| 1.6 | **Runtime source registration** — add/remove feeds live (the old crate exposed `POST /sources/add` persisting to a sources bucket). Pairs with the service API in Theme 2. | 🔴 | `legacy/DSM-Input-Module` | Med / Med |
| 1.7 | **`MeasurementBatchLength → interpolation-resolution` auto-mapping** — derive sampling density from the requested horizon (e.g. 3h window → minute samples). | 🔴 | `legacy/dsm-batch`, `legacy/DSM-Batch-v2` | Med / Low |

---

## Theme 2 — Network / Service API 🔴

DSP is currently a library + TUI with no network surface. A predecessor exposed the
store as an HTTP service — the single highest-leverage way to make DSP consumable by
other processes.

| # | Item | Status | Source | Value/Effort |
|---|------|--------|--------|--------------|
| 2.1 | **REST facade over the libSQL store** (axum) — CRUD for subjects/aspects/measurements, plus range queries. | 🔴 | `legacy/DSM-Database`, `legacy/DSM-Thorchain` | High / Med |
| 2.2 | **Declarative query-params type** — `range`, `take` (step count), `count`, `page`, `interpolation`, `debug` as one deserializable struct shared by the TUI and the future REST layer. | 🟡 | `legacy/DSM-Database` (`params.rs`) | Med-High / Med |
| 2.3 | **Pagination** as a first-class query concern for large ranges. | 🔴 | `legacy/DSM-Database` | Med / Low |

---

## Theme 3 — Query-Layer Interpolation ✅ (already implemented)

**Correction:** an earlier draft listed query-time interpolation/extrapolation as a
gap. It is not — DSP already does everything the predecessors did here, in-process
via `splimes`:

- **Interpolate-on-read** and **single-instant lookup** — `Outputs::analyze_range`
  returns a series gap-filled/aligned to the requested `Resolution`, and
  `Outputs::analyze_point` returns the interpolated value at *any* instant. Both
  delegate to `splimes::auto_interpolate`, which *"handles both interpolation and
  extrapolation"* (`database/src/types/database/outputs.rs`).
- **Out-of-range extrapolation** — every method extrapolates beyond the data's
  edges (linear / quadratic / cubic / polynomial, on CPU, SIMD, **and** GPU;
  polynomial additionally supports `bounds_factor` damping). Exercised by
  `test_analyze_point_extrapolation_forward` / `_backward` in
  `database/tests/db_tests.rs`.

The one genuinely-open nuance — **marking interpolated/extrapolated points as
synthetic in the returned data** — is folded into item 4.1 (per-measurement tags):
`analyze_point` already *describes* the operation (e.g. "forward extrapolation") in
its returned strategy string, but the `Point` itself carries no synthetic flag.

---

## Theme 4 — Measurement Model & Metadata 🟡

The current `Measurement` is a clean `(timestamp, BigDecimal value)`. Predecessors
attached metadata and per-point analytics that DSP's analysis layer could exploit.

| # | Item | Status | Source | Value/Effort |
|---|------|--------|--------|--------------|
| 4.1 | **Per-measurement tags/labels** (`HashMap<String,String>`), including a conventional `interpolated=true`/provenance marker. Enables filtering, grouping, and synthetic-point tracking. `database/src/types/measurement.rs` has no tags today. | 🔴 | `legacy/DSM-Database`, `legacy/DSM-Measurement` | High / Low |
| 4.2 | **Complete the per-point analysis model.** DSP already has `Trend`, `Relative`, `MeasurementVector`, and `Analysis`/`BatchDistance` types — verify they capture signed positive/negative neighbor distance, slope-segmented trends, and max-normalized relative vectors end-to-end, and that detectors consume them. The legacy `dataset_management` stub sketched exactly this model; the legacy `DSM-Measurement` carried dual raw-vs-processed fields (`location`/`amplitude`). | 🟡 | `legacy/dataset_management`, `legacy/DSM-Measurement` | Med-High / Med |
| 4.3 | **Schemaless object/annotation store** beside numeric aspects — a place for config, metadata, or human annotations keyed by string. | 🔴 | `legacy/DSM-Database` (`BucketType::Object`) | Med / Med |

---

## Theme 5 — Batching & Windowing 🟡

DSP's batch queue is non-overlapping and single-resolution. The batcher lineage had
richer windowing and policy.

| # | Item | Status | Source | Value/Effort |
|---|------|--------|--------|--------------|
| 5.1 | **Sliding / overlapping windows** (stride-1) — materially increases pattern yield vs. disjoint batches. Optionally **event-centered** windows (`[t−size, t+size]`). | 🔴 | `legacy/dsm-batch`, `legacy/DSM-Batcher` | High / Med |
| 5.2 | **Multi-resolution horizon fan-out** — one event emits batches at every horizon between a per-aspect `min`/`max` (`BatchLength::range`). | 🔴 | `legacy/DSM-Batcher` | Med-High / Med |
| 5.3 | **Event-UUID dedup / idempotency ledger** — track processed event IDs so already-batched data is never reprocessed; pairs with delete-after-success work-queue semantics for at-least-once guarantees. | 🟡 | `legacy/dsm-batch`, `legacy/DSM-Batcher` | Med / Med |
| 5.4 | **Minimum-density validation** — skip emitting a batch when `points < interpolation_steps`. | 🟡 | `legacy/DSM-Batcher` | Low / Low |
| 5.5 | **Config-driven batching policy** — declarative per-aspect min/max horizon and cadence. | 🟡 | `legacy/DSM-Batcher` | Med / Med |

---

## Theme 6 — Pattern Similarity & Dictionary 🟡

DSP already has `Pattern`, `Dictionary`, `Occurrence`, and `DictionaryConstraints`
with `Variability`/`VariablilityType`. The legacy `DSM-Pattern` had a fuller,
explicitly-named metric suite and a dedup engine worth reconciling against the
current implementation.

| # | Item | Status | Source | Value/Effort |
|---|------|--------|--------|--------------|
| 6.1 | **Full variability-metric matrix** — `static`, `absolute_static`, `percentage`, `absolute_percentage`, each with a `Max`/`Average`/`Sum` reduction. Confirm DSP's `VariablilityType` covers all combinations. | 🟡 | `legacy/DSM-Pattern` (`pattern.rs`) | Med-High / Med |
| 6.2 | **Fixed-point dictionary deduplication** — iterate merge passes until no further merges (pattern canonicalization/clustering). | 🟡 | `legacy/DSM-Pattern` (`dictionary.rs`) | Med / Med |
| 6.3 | **Occurrence-distance constraint** — treat two patterns whose occurrences fall within a configurable time distance as the same temporal event (avoids double-counting). | 🟡 | `legacy/DSM-Pattern` | Med / Low |
| 6.4 | **Resample-before-compare** — normalize unequal-length patterns to a fixed step count before comparison; wire this to `splimes` (the old crate called an external HTTP service for it). | 🟡 | `legacy/DSM-Pattern` (`enforce_steps`) | Med / Low |

---

## Theme 7 — Precision, Resilience & Correctness 🟡

Hardening details proven out in the predecessors; mostly audits to confirm they
survived the libSQL migration, plus a couple of genuine ports.

| # | Item | Status | Source | Value/Effort |
|---|------|--------|--------|--------------|
| 7.1 | **`BigDecimal` throughout the batch/pattern math.** v2 replaced v1's `f64` with `BigDecimal`/`BigUint` to eliminate float drift in pattern coordinates (which feed correlations/signals). Confirm current math is precision-clean. | 🟡 | `legacy/DSM-Batch-v2` | Med / Med |
| 7.2 | **Write resilience** — `add_dataset_with_retry` (exponential backoff + jitter) and a **size-tiered write selector** (individual / single-batch / chunked-txn / memory-buffer by row count). Verify equivalents exist on the libSQL path. | 🟡 | `legacy/database` | Med / Low |
| 7.3 | **Interpolation window extension** — pad the requested range by ±N resolution steps so edge points interpolate correctly. Confirm preserved in the current entrypoint. | 🟡 | `legacy/database` | Med / Low |
| 7.4 | **Improved `simplify` (local-extrema detection)** — v2's explicit prev/current/next minima/maxima retention was more correct than v1's in-place sign comparison. Relevant to the compression slope-simplification path. | 🟡 | `legacy/DSM-Batch-v2` | Med / Low |
| 7.5 | **Millisecond (and Week) interpolation/horizon tiers** — finer-grained options than currently exposed. | 🟢 | `legacy/DSM-Batch-v2` | Low / Low |
| 7.6 | **MessagePack** (`rmp-serde`) for queued artifacts vs JSON — smaller/faster; relevant to the compression work. | 🔴 | `legacy/dsm-batch` | Low / Low |

---

## Theme 8 — Observability 🟡

DSP uses `tracing` (event/line logging). The predecessors complemented this with
**structured state-snapshot capture**.

| # | Item | Status | Source | Value/Effort |
|---|------|--------|--------|--------------|
| 8.1 | **Pipeline state-snapshot audit trail** — dump full before/after data payloads at each transform stage as queryable, order-preserving JSON keyed by stage + microsecond timestamp. Invaluable for debugging the merge/correlation pipeline and for reproducibility. Implement as a `tracing` layer or dedicated snapshot sink (the legacy file-rewrite-per-save impl is not production-grade). | 🔴 | `legacy/DSM-Log`, `legacy/DSM-Batch-v2` (per-stage toggle) | Med / Med |

---

## Suggested sequencing

1. **Foundation (model + query):** 4.1 tags (this also delivers the one open nuance from Theme 3 — marking synthetic points) → 2.2 query-params type. These are self-contained and unlock everything downstream.
2. **Connectors:** 1.1 vendor-neutral connector trait (in core) → 1.2 InfluxDB and 1.3 Thorchain as *separate, out-of-core* connector crates → 1.4/1.5 scheduled polling + retry buffer.
3. **Service:** 2.1 REST facade → 1.6 runtime source registration → 2.3 pagination.
4. **Analysis depth:** 6.1–6.4 pattern similarity/dedup → 4.2 per-point analysis completion → 5.1/5.2 windowing.
5. **Hardening & ops:** Theme 7 audits → 8.1 snapshot observability.

---

## Where the ideas live (archived source map)

| Legacy crate | What to mine it for |
|--------------|---------------------|
| [`legacy/dsm-source`](legacy/dsm-source) | Connector trait / source registry |
| [`legacy/dsm-asset`](legacy/dsm-asset) | `ToAsset` self-describing-target pattern |
| [`legacy/dsm-influxdb`](legacy/dsm-influxdb) | InfluxDB 2.x client, line protocol, Flux, `interpolate.linear` |
| [`legacy/DSM-Thorchain`](legacy/DSM-Thorchain) | Midgard price client (BigDecimal), windowed backfill, HTTP ingestion service |
| [`legacy/DSM-Input-Module`](legacy/DSM-Input-Module) | Per-source scheduling, retry buffer, runtime source registration |
| [`legacy/DSM-Database`](legacy/DSM-Database) | REST surface, tags, object buckets, query params, pagination |
| [`legacy/DSM-Measurement`](legacy/DSM-Measurement) | Enriched measurement model (raw-vs-processed, per-point analytics) |
| [`legacy/database`](legacy/database) | Retry+backoff writes, size-tiered batching, window extension, typed errors, TTL/LRU cache |
| [`legacy/dsm-batch`](legacy/dsm-batch) | Sliding windows, event-UUID dedup, horizon→interp mapping, MessagePack |
| [`legacy/DSM-Batch-v2`](legacy/DSM-Batch-v2) | BigDecimal DSP math, improved `simplify`, per-stage logging, `Result` stages |
| [`legacy/DSM-Batcher`](legacy/DSM-Batcher) | Multi-horizon fan-out, density validation, delete-after-success queue, backpressure |
| [`legacy/DSM-Pattern`](legacy/DSM-Pattern) | Variability metric matrix, fixed-point dedup, occurrence-distance constraint |
| [`legacy/DSM-Patterner`](legacy/DSM-Patterner) | Pipeline semantics: `dataset::length` bucketing, throttle/page, consume-then-delete |
| [`legacy/dataset_management`](legacy/dataset_management) | Design intent for the per-point `Analysis` (trend/distance/relative) model |
| [`legacy/DSM-Log`](legacy/DSM-Log) | Structured state-snapshot audit logging |

---

*Generated from a review of the archived predecessor repositories. The `legacy/`
crates are retained for reference and full git history; they are excluded from the
active Cargo workspace build.*
