# CHRONO Inventory: `chrono` v0.4 Dependency Assessment

**Issue:** #791  
**Date:** 2026-11-22  
**Commit audited:** `3ff1796`

## 1. Dependency Graph

### 1.1 Workspace definition

```toml
# Cargo.toml:54
chrono = { version = "0.4", features = ["serde"] }
```

The workspace enables the `serde` feature unconditionally, making it available to every member that uses the workspace `chrono`.

### 1.2 Workspace members that depend on `chrono`

| Crate              | Declared dependency                                                          |
|--------------------|------------------------------------------------------------------------------|
| weftdb             | `{ workspace = true, features = ["serde"] }`                                |
| weft-bench         | `{ workspace = true, features = ["serde"] }`                                |
| weft-orchestration | `{ workspace = true, features = ["serde"] }`                                |
| weft-server        | `{ workspace = true, features = ["serde"] }`                                |
| weft-reduce        | `{ workspace = true }` (no feature overrides, inherits workspace `serde`)   |
| weft-client        | `{ workspace = true }` (published crate, serde available from workspace)    |
| weft-line-protocol | `{ workspace = true }` (published crate)                                    |
| weft-tui           | `{ workspace = true }` (published crate)                                    |
| weft-xbench        | `{ version = "0.4", features = ["serde"] }` (non-workspace, standalone)     |

**9 crates** declare a `chrono` dependency. `weft-xbench` does not use the workspace definition and repeats the version and features.

### 1.3 Crates that do NOT depend on `chrono`

- **`weft-physical-type`** — the in-repo integer-timestamp layer. Defines `TimeUnit`, `DeltaColumn`, `DeltaOfDeltaColumn`, all codecs (delta, delta-of-delta, RLE, bitpack, gorilla, FOR, blocked bitpack, DFOR, transpose bitpack), checkpointed index, and arithmetic-stride detection. All arithmetic is pure `i64` over epoch ticks in the declared unit. No chrono dependency at all.

- `compat` — has a single file (`compat/v010/weftdb/examples/gen_legacy_fixture.rs`) that uses chrono, but only as a transitive dev-dependency of the old crate path; not a direct consumer.

### 1.4 Transitiveness

No crate re-exports chrono types transitively — every direct consumer lists it in its own `[dependencies]`. `weft-client` (published as `weftdb_client`) exposes `chrono::DateTime<chrono::Utc>` in its public `types.rs`, making chrono a *de facto* public dependency of that crate even though it is not `pub use`'d separately.

---

## 2. Public API Surface

Chrono types appear in the public signatures of **three published-crate boundaries** and **two internal-crate boundaries**.

### 2.1 `weft-client` (published) — `weft-client/src/types.rs`

```rust
ComputePoint          { pub timestamp: DateTime<Utc>, pub value: f64 }
Interpolate           { pub start: Option<DateTime<Utc>>, pub end: Option<DateTime<Utc>> }
OutputPoint           { pub timestamp: DateTime<Utc> }
Interpolated          { /* answer structs with Option<DateTime<Utc>> */ }
InterpolatedPoint     { pub instant: DateTime<Utc> }
Downsample            { pub start: Option<DateTime<Utc>>, pub end: Option<DateTime<Utc>> }
Bucket                { pub timestamp: DateTime<Utc> }
InterpolatePointBody<'a> { pub instant: DateTime<Utc> }
```

`weft-client/src/client.rs:424`:
```rust
pub async fn interpolate_point(
    &self, points: &[ComputePoint], instant: chrono::DateTime<chrono::Utc>, spline: Spline
) -> Result<InterpolatedPoint>
```

Any replacement of chrono in `DateTime<Utc>` would break this crate's API. The compute endpoints map directly to HTTP/JSON serialization (RFC 3339 strings).

### 2.2 `weftdb` (published as `weftdb`) — internal types re-exported via `pub use types::*`

```rust
InputMeasurement  { pub timestamp: chrono::DateTime<Utc> }    // weftdb/src/types/input_measurement.rs
Measurement       { pub timestamp: chrono::DateTime<Utc> }    // weftdb/src/types/measurement.rs
Event             { pub fn duration(&self) -> chrono::Duration }
```

`weftdb/src/types/aspect.rs` public methods:
```rust
pub async fn mark_dirty_region(        &mut self, region_start: chrono::DateTime<chrono::Utc>, ...)
pub async fn clear_dirty_regions(      &mut self, before: chrono::DateTime<chrono::Utc>)
pub async fn get_compressed_range_containing(&mut self, timestamp: chrono::DateTime<chrono::Utc>)
```

`weftdb/src/types/compression/mod.rs:257`:
```rust
pub fn calculate_tier_aggressiveness(data_age: chrono::Duration, ...) -> f64
```

### 2.3 `weft-orchestration` (not published)

`weft-orchestration/src/lib.rs:1389`:
```rust
pub async fn filter_expired_signals(
    database: &Database, aspect_id: &AspectId, current_time: Option<chrono::DateTime<chrono::Utc>>
) -> Result<()>
```

---

## 3. Usage Classification

All source files counted from `rg -cn 'chrono::' --type rust`. Total: **~272 lines** across **~100 source files** (excluding generated/compat).

### 3.1 By type imported

| Chrono type            | Occurrences | Purpose                                                    |
|------------------------|-------------|------------------------------------------------------------|
| `Utc`                  | ~90         | Timezone marker; `Utc::now()` for clock reads (37 sites)  |
| `Duration`             | ~78         | Window arithmetic, tier thresholds, time-step calculations |
| `DateTime`             | ~68         | Public API struct fields, internal metadata timestamps     |
| `TimeZone`             | ~12         | `.with_ymd_and_hms()`, `.from_utc_datetime()` in tests    |
| `TimeDelta`            | ~5          | Width arithmetic in server interpolation, reduce scaling   |
| `SecondsFormat`        | ~2          | RFC 3339 formatting options (`Nanos`, `Millis`, `Secs`)   |
| `Datelike`             | ~2          | Date-component extraction (TUI, orchestration)             |
| `NaiveDateTime`        | ~1          | TUI display                                                |
| `Timelike`             | (implicit)  | TUI clock display                                          |

### 3.2 By operation category

| Category                       | Count | Key crates                          |
|--------------------------------|-------|-------------------------------------|
| **Clock reads (`Utc::now`)**   | 37    | weftdb (25), weft-bench (10), rest |
| **Duration arithmetic**        | ~77   | weftdb (compression, outputs), orchestration, detectors |
| **Instant timestamp**          | ~68   | weftdb (input_measurement, measurement, aspect, database/inputs) |
| **RFC 3339 formatting**        | ~40   | weft-bench (main, manifest, e2e), weft-reduce, weft-server, weft-xbench |
| **RFC 3339 parsing**           | ~4    | weft-client types, weft-reduce, live tests |
| **Timezone conversion**        | ~12   | Tests, interpolation endpoints   |
| **Date component extraction**  | ~2    | TUI, orchestration Datelike      |

### 3.3 By crate

| Crate             | Source files | Total `chrono::` lines | Primary use                     |
|-------------------|-------------|------------------------|---------------------------------|
| weftdb            | ~46         | ~150                   | `Utc::now()`, Duration, DateTime |
| weft-orchestration| ~12         | ~54                    | Duration, DateTime, utc clocks  |
| weft-bench        | ~20         | ~26                    | Metadata timestamps, formatting |
| weft-server       | ~9          | ~17                    | Interpolation/reduce arithmetic |
| weft-reduce       | ~6          | ~9                     | Bucket arithmetic, tests        |
| weft-xbench       | ~6          | ~6                     | Metadata, formatting            |
| weft-client       | ~3          | ~3                     | Public type definitions         |
| weft-tui          | ~2          | ~5                     | Display formatting              |
| weft-line-protocol| ~1          | ~1                     | Timestamp offset (minor)        |
| compat/v010       | ~1          | ~1                     | Legacy fixture generation       |

### 3.4 Serde feature usage

The `serde` feature is enabled in the workspace default and explicitly by 4 crates (weftdb, weft-bench, weft-orchestration, weft-server). However, a `grep` for `chrono.*serde` / `serde.*chrono` across all `.rs` files returns zero direct derive or attribute usage — chrono's serde feature is used *indirectly* when `DateTime<Utc>` fields are `#[derive(Serialize, Deserialize)]` on enclosing structs (e.g., `ComputePoint`, `OutputPoint` in `weft-client/src/types.rs`). The feature enables chrono's built-in `Serialize`/`Deserialize` impls so that `#[serde(flatten)]`, `#[serde(skip_serializing_if)]`, and `#[serde(default)]` on DateTime fields compile without manual impls.

---

## 4. Existing In-Repo Time Types

### 4.1 `weft-physical-type` — the stored-timestamp layer

**File:** `weft-physical-type/src/timestamp.rs` (single file, ~2,800 lines of production + tests)

**Types defined:**

```rust
pub enum TimeUnit { Seconds, Millis, Micros, Nanos }
pub struct DeltaColumn { pub first: i64, pub unit: TimeUnit, pub deltas: Vec<i64> }
pub struct DeltaOfDeltaColumn { pub first: i64, pub first_delta: Option<i64>, pub dods: Vec<i64>, pub unit: TimeUnit }
pub struct DodCheckpoints { pub stride: usize, pub points: Vec<(usize, i64)>, pub rows: usize }
```

**Codecs implemented (all pure i64, no chrono dependency):**
- Delta (first + varint deltas)
- Delta-of-delta (first + first-delta + varint second-differences)
- RLE (run-length encoding over second-differences)
- Bitpack (fixed-width zigzag-bitpacked second-differences)
- Per-block adaptive bitpack (mixed-magnitude streams)
- FOR (Frame-Of-Reference, per-block min-subtract + bitpack)
- DFOR (Decimal-FOR, per-block exponent factoring + FOR)
- Gorilla-style (one-bit-per-zero, bounded bucket codes)
- Transpose bitpack (SIMD-friendly tiled transposition, behind `bitsliced-codec` feature)
- Delta-FOR (FOR over first-order deltas, wins on random intervals)
- Common-multiple factoring (reduced precision for regular sub-multiple ticks)

**Methods:**
- `encode_delta` / `decode_delta`
- `encode_delta_of_delta` / `decode_delta_of_delta`
- `best_estimated_bytes` / `best_encoding_name` (min-selector over the 5 shipped codecs)
- `first_order_deltas()`, `arithmetic_stride()`, `checkpoints()`, `search_sorted()`
- Per-codec byte estimators and round-trip encode/decode for advisory codecs (FOR, DFOR, Gorilla, transpose)

**Current usage:** `weft-physical-type` is used by `weftdb`'s segment store for on-disk timestamp encoding and by `weft-line-protocol` for type conversion. It is **never used** where `chrono::DateTime<Utc>` or `chrono::Duration` is consumed — those are in the in-memory API layer, the orchestration layer, and benchmarks.

### 4.2 Gap: midpoint, window, and layout arithmetic

`weft-physical-type` operates only on columns of stored epoch integers. It has no concept of:
- Duration algebra (add/subtract/compare durations to instants)
- Window/bucket midpoint computation
- Clock reading (`now()`)

These are the function groups that `chrono::Duration` and `DateTime<Utc>` provide in the current codebase and that no in-repo type replaces today.

---

## 5. Migration Feasibility Assessment

### 5.1 Replaceable with low effort

| Pattern | Occurrences | Replacement | Risk |
|---------|-------------|-------------|------|
| `Utc::now()` → `SystemTime::now()` | 37 | `std::time::SystemTime::now().duration_since(UNIX_EPOCH)` | Low — pure clock read, no semantic change |
| `Duration::seconds(n)` / `Duration::days(n)` | ~60% of Duration uses | Simple `i64` nanoseconds or a newtype `TimeDelta` | Low-Medium — no overflow risk but many call sites |
| `to_rfc3339()` | ~10 | `DateTime::<Utc>::from(instant).to_rfc3339()` still needs chrono |
| `NaiveDateTime`, `Datelike`, `Timelike` | ~4 | Replace with custom formatting or `chrono`-free rendering | Low — confined to TUI |

### 5.2 High effort — public API boundary

The major migration blockers:

1. **`DateTime<Utc>` in published API types** (weft-client, weftdb). Removing chrono from `ComputePoint`, `InputMeasurement`, `Measurement`, and the interpolate/downsample request structs would be a **semver-breaking change**. Any replacement type would need:
   - `Serialize` + `Deserialize` for RFC 3339
   - `Copy` (current `DateTime<Utc>` is not `Copy`; a newtype over `i64` + `TimeUnit` could be)
   - Interop with the existing integer-timestamp segment store

2. **Duration arithmetic in orchestration** (weft-orchestration, 54 lines). The detector and batch-utility code chains `Duration::seconds()`, `.num_seconds()`, `.checked_add()`, `.max()`/`.min()` operations. A newtype wrapping `i64` nanoseconds would preserve most expressions but the transition touches ~77 call sites.

3. **Server-side TimeDelta** (weft-server, 5 sites). `chrono::TimeDelta::seconds()` is used for interpolation window expansion. Replaceable by a wrapper but breaks one function signature.

### 5.3 Recommended migration order

1. **Phase A — Clock read replacement (low-hanging fruit):** Replace all 37 `Utc::now()` calls with `std::time::SystemTime::now()`. This removes the largest single chrono feature-group without touching any public API. The database/inputs.rs uses `.timestamp_millis()` on the result, which is a direct `.duration_since()` call.

2. **Phase B — Duration newtype (internal crates):** Introduce a `weft-physical-type::TimeDelta` (or similar) that wraps `i64` nanoseconds and provides arithmetic ops. Migrate weftdb (non-public), weft-orchestration, weft-server, weft-reduce internal Duration uses. Keep `chrono::Duration` in published API structs (Event) until the next breaking release.

3. **Phase C — Instant newtype (internal + published boundary):** Define `weftdb::Instant` as a newtype over `i64` (epoch milliseconds). Replace `DateTime<Utc>` in weftdb's internal signatures and public API types (`InputMeasurement`, `Measurement`, aspect methods). This is semver-breaking and must be scheduled for the next major version of both `weftdb` and `weftdb-client`.

4. **Phase D — Purge `chrono` from workspaces:** After Phases A–C no crate uses chrono types in non-test code, remove the workspace dependency and each crate's individual dependency. The `chrono = "0.4"` workspace entry at `Cargo.toml:54` is deleted. The test-only uses in `weftdb/tests/`, `weft-bench/benches/`, etc. can either migrate to the new types or remain as `[dev-dependencies]`.

### 5.4 Estimated effort

| Phase | Files touched | New type surface | Breaking? | Effort |
|-------|---------------|------------------|-----------|--------|
| A | ~10 (database/inputs.rs, lib.rs, orchestration/src/lib.rs) | None | No | 1–2 days |
| B | ~25 (orchestration, weftdb compression/outputs/server) | `TimeDelta` in weft-physical-type | No (internal only) | 3–5 days |
| C | ~20 (weftdb types, weft-client types.rs) | `Instant` in weftdb or weft-physical-type | Yes (semver major) | 5–10 days |
| D | Workspace + 9 Cargo.toml | — | No | 1 hour |

---

## 6. Risks and Open Questions

1. **Serde round-trip contract.** The compute endpoints serialize `DateTime<Utc>` as RFC 3339 strings. A custom `Instant` newtype must produce byte-for-byte identical serialization (including sub-second precision, trailing `Z`, and zero-trim behaviour) to avoid breaking stored-query consumers and benchmark reproducibility. The `chrono` `serde` feature's default format is `"1970-01-01T00:00:00Z"` with whole seconds emitting no fractional part; an equivalent custom serializer is required.

2. **weft-client publish cadence.** `weftdb-client` is already published at v0.1.1. A pre-1.0 semver-compatible release can change API types as long as the version bump is minor. However, the interpolate/downsample endpoint bodies are defined by the server, not the client — a migration requires coordinated server+client changes.

3. **weftdb's public surface is large.** The `pub use types::*` in `weftdb/src/lib.rs` re-exports every public type from the `types` module that contains chrono. Replacing those types is a single coordinated change but must be tested for downstream consumers.

4. **`chrono::Duration` has no nanosecond overflow on hours/days** — the `i64` nanosecond replacement must guard against overflow in `.days()`, `.hours()`, etc. (currently chrono uses `i64::MAX` days of nanoseconds, i.e., ~292M years, which is safe in practice).

5. **TimeZone operations are test-only or rare.** The 12 `TimeZone` uses are primarily in test helpers (`.with_ymd_and_hms()`) and the interpolation/downsample endpoint's UTC construction. These can remain behind a `#[cfg(test)]` chrono dependency if desired.

6. **`compat` directory** — `compat/v010/weftdb` is a fixed snapshot of an old crate surface. It should keep its chrono dependency as-is and not be migrated (its purpose is to produce test fixtures for the legacy binary format).