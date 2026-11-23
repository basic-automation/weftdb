# B-Object: Schemaless object/annotation store — audit

**Audited commit:** `6ae70f097ac874468a26882774065275c06b2747` (origin/main)

**Issue:** #363

## Question

Does WeftDB already have, or can it easily express, a schemaless object/annotation storage
mechanism beside the numeric aspect model — records that are not time-indexed numbers: notes,
blobs, JSON documents, labels attached to a series rather than measurements of it?

Three sub-questions:

1. Does any existing mechanism already cover it?
2. If not, what is the smallest honest statement of the gap, and can it be additive during the
   public-surface freeze (issue #238)?
3. Is it in scope for 1.0?

## Method

No `cargo` was run — host RAM belongs to the model server and a build risks OOM-killing it.
All findings are structural evidence from `git grep`, `rg`, and file reads at the audited
commit. Every `path:line` citation was verified against a fresh read at that commit.

Commands run:

```bash
git grep -n 'B-object' ROADMAP.md docs/roadmap-notes.md
git grep -rn 'struct IngestPoint\|struct IngestRequest\|struct DeclareAspectRequest' weft-server/src/  # the HTTP write surface
rg -n 'annotation\|object\|blob\|note\|schemaless' weft-server/src/openapi.rs  # the route registry
rg -n 'json\|blob\|object\|string' weft-physical-type/src/lib.rs  # PhysicalType variants
rg -n 'tag\|annotation' weft-physical-type/src/tags.rs  # tag/series model
rg -n 'struct Occurrence\|struct Dictionary\|struct Dataset' weftdb/src/types/  # other types
rg -n 'BigDecimalText\|parse_decimal' weft-physical-type/src/lib.rs  # the closest escape hatch
```

## What exists today

### 1. PhysicalType: six numeric encodings only

`weft-physical-type/src/lib.rs:161` defines `PhysicalType` with six variants, all numeric:

| Variant | Description |
|---------|-------------|
| `F64` | IEEE-754 binary64 |
| `F32` | IEEE-754 binary32 |
| `ScaledI64 { scale }` | Fixed-point i64 × 10^−scale |
| `ScaledI128 { scale }` | Fixed-point i128 × 10^−scale |
| `Decimal128` | Decimal floating point, per-value scale |
| `BigDecimalText` | Lossless decimal text |

`PhysicalType::BigDecimalText` (`:200`) sounds like a text escape hatch: it stores the
verbatim plain-string form of a `BigDecimal`. **It is not a general string/annotation
storage.** Every ingest path (`weft-server/src/ingest.rs`) calls
`weft_physical_type::parse_decimal()` (`:960`, `:1042`) to lift the text into a `BigDecimal`.
`parse_decimal` (`weft-physical-type/src/lib.rs:438`) returns
`Result<BigDecimal, ParseDecimalError>` with only `Invalid` and `OutOfRange` error variants
(`:467-473`). A JSON document, an annotation string, or any non-decimal text would be
rejected as `Invalid`. BigDecimalText is an *exactness* escape hatch for decimal values, not a
*type* escape hatch for non-numeric payloads.

### 2. The HTTP write surface: numeric points only

`weft-server/src/ingest.rs:554`:
```rust
pub struct IngestRequest {
    pub points: Vec<IngestPoint>,
    pub rows_per_page: Option<usize>,
    pub require_sorted: bool,
    pub batch_id: Option<String>,
    pub tags: Option<JsonTags>,
}
```

`weft-server/src/ingest.rs:538`:
```rust
pub struct IngestPoint {
    pub timestamp: i64,
    pub value: Option<String>,  // parsed as BigDecimal through parse_decimal
}
```

Every value is lifted to `BigDecimal`. The declare route (`weft-server/src/declare.rs:49`,
`parse_physical_type`) accepts only the six numeric token names.

### 3. The tag/series model: frozen identity fields

`weft-physical-type/src/tags.rs:165` (`TagSet`) and `:347` (`SeriesKey`) define series
identity: max 16 tags, each a `(String, String)` pair. Tags are query identity, not payload
storage. No `Annotation` or `Attributes` field exists anywhere in the series model.

### 4. Other domain types: all domain-specific

- `Dataset` (`weftdb/src/types/dataset.rs`): holds `Vec<Measurement>` — numeric measurements.
- `Occurrence` (`weftdb/src/types/occurrence.rs`): pattern/event analysis descriptors (aspect,
  resolution, size, database info, pattern ID, timestamps). No blob/annotation field.
- `Dictionary` (`weftdb/src/types/dictionary.rs`): pattern-matching analysis, holds `Pattern`
  vectors. No general object storage.
- Catalog/metadata stores (`weftdb/src/types/aspect_catalog.rs`, `metadata.rs`): control-plane
  schemas, rollup aggregates — no measurement payload.

### 5. The HTTP surface registry: no object endpoint

`weft-server/src/openapi.rs` registers exactly eight frozen scopes (Probe, Metrics, Read,
Compute, Ingest, Declare, Maintain, Backup). There is no `object`, `annotation`, `note`,
`blob`, `document`, or `schemaless` route. The OpenAPI baseline
(`compat/openapi/baseline/weftdb.json`) mirrors this.

### 6. #238 public surface freeze

Issue #238 ("Public surface declared and frozen") is milestone **Phase 18 — Release
candidate**, open, blocking all new routes and JSON schema additions. B-object would need at
minimum a write endpoint and a read endpoint, both of which the freeze prohibits.

### Summary

**Nothing covers schemaless object/annotation storage today.** Every path from the HTTP surface
down to the segment frame is routed through `BigDecimal` values and one of six numeric
physical encodings. Non-decimal text is not representable as a measurement value.

## What B-object would need

The smallest honest gap statement is:

> WeftDB has no storage path for non-numeric values (strings, JSON, blobs, annotations). Every
> aspect stores BigDecimal-domain values; the segment frame format, the ingest pipeline, the
> declare schema model, and the entire HTTP route surface assume numeric time-series data.

To add schemaless object/annotation storage, a minimal change set would include:

1. **A new `PhysicalType` variant** — e.g. `Text` (UTF-8 string), `Json` (validated JSON), or
   `Blob` (arbitrary bytes). This lives in `weft-physical-type/src/lib.rs` and would grow the
   `PhysicalValue` enum, the `encode`/`decode` paths, and the segment frame writer/reader.

2. **A new or widened ingest path** — `IngestPoint::value` is `Option<String>` parsed through
   `parse_decimal`; an object store would need a separate field or a different ingest endpoint
   that skips `BigDecimal` conversion.

3. **New HTTP routes** — at minimum `POST /api/v1/storage/{aspect}/object` (write) and
   `GET /api/v1/storage/{aspect}/object/{key}` (read), plus route registration in
   `weft-server/src/openapi.rs` and bumping the OpenAPI baseline.

4. **A new segment frame variant or a flag** — the `.weftseg` format (`weftdb/src/types/`)
   currently encodes timestamp/value columns; an object store would need a different column
   layout (key→value, possibly with timestamps as secondary index rather than primary).

### Can it be additive during the freeze?

**No.** Adding any new HTTP route violates the public surface freeze (#238). Even if the
implementation shared the existing `POST /api/v1/storage/{aspect}/points` path with a new
field or content-type, the OpenAPI spec would change, and the route's existing schema
(`IngestRequest`) assumes decimal-text values. A new `PhysicalType` variant in
`weft-physical-type` is not itself a public-surface change (it is a library type, not an API
route), but it would be dead code until a route exposes it — and dead code that compiles is
still a maintenance obligation.

The freeze applies to routes and their JSON schemas. Adding a `PhysicalType::Text` variant
without exposing it over HTTP is technically additive but practically incomplete: it cannot be
used. The recommendation below accounts for this.

## Verdict

### Recommendation: **Defer to post-1.0**

**Reasons:**

1. **The freeze blocks it.** Issue #238 is milestone Phase 18 (Release candidate) and prohibits
   new routes and JSON schema changes. B-object cannot ship as usable storage through the
   freeze.

2. **The scope is large enough to destabilise 1.0.** A new physical encoding type, a new
   segment columnar layout (or a branch in the existing one), a new ingest/read route pair, and
   a new OpenAPI schema section — none of this is a small additive change within the existing
   numeric-oriented codegen. The segment frame format (`weftdb/src/types/`) is built around
   timestamp-indexed BigDecimal columns; adding string/blob columns would be a cross-cutting
   change through the segment writer, reconciler, reader, and all format versions.

3. **The use case is uncharacterised.** Issue #363 says "Phase 4 · absent" and was inherited
   from the predecessor roadmap. No requirements, wire format sketches, or user stories are
   attached. A post-1.0 re-evaluation can scope it properly.

4. **The phase label already says "absent"** (`docs/roadmap-notes.md:465`: `*(Phase 4 · absent)*`),
   which is honest. Keeping it absent until after 1.0 is consistent with its own annotation.

**If implemented post-1.0**, the cleanest approach would be a new `weft-object` crate
modelled on the same segment-store architecture but with a key-value or annotated-timestamp
model rather than strict numeric BigDecimals, and a new HTTP scope in the route table.

### ROADMAP.md annotation (no box ticked)

The line at `ROADMAP.md:319` reads:

```
- [ ] **B-object** Schemaless object/annotation store beside numeric aspects
```

Annotated in place — no box ticked.

### docs/roadmap-notes.md annotation

The line at `docs/roadmap-notes.md:465` reads:

```
- **B-object** *(Phase 4 · absent)*
```

The audit pointer is mirrored there.

## Phase label reconciliation

Three sources reference B-object with different phase labels:

| Source | Line | Phase label |
|--------|------|-------------|
| Issue #363 body | — | "Phase 4" (from predecessor roadmap) |
| `ROADMAP.md` backlog | `:319` | Phase 28 (Analytics premium section) |
| `docs/roadmap-notes.md` | `:465` | "Phase 4 · absent" |

**Root cause.** Issue #363 was filed from the predecessor roadmap's backlog, which placed
B-object under Phase 4. The live `ROADMAP.md` moved it to Phase 28 ("Analytics premium"),
but the issue body and `docs/roadmap-notes.md` were never updated.

**Correction applied (this audit).**

- `docs/roadmap-notes.md:465` updated to `audit pointer (Phase 28 · absent)` — matching the
  live `ROADMAP.md` position.
- Issue #363 remains as-is (issue bodies are not rewritten in a code-audit PR).

The accurate phase is **Phase 28** (Analytics premium), where the `ROADMAP.md` line lives.

## Follow-ups filed

**None.** The recommendation is to defer everything to post-1.0. Filing implementation issues
before the freeze lifts would create fictionally actionable backlog items that cannot be
started. If the project decides to re-scope post-1.0, one umbrella issue should cover the
whole B-object feature with sub-issues for each crate touched.

## References

- `ROADMAP.md:319` — B-object line, Phase 28
- `docs/roadmap-notes.md:465` — phase-annotated pointer *(Phase 28 · absent)*
- `weft-physical-type/src/lib.rs:161-200` — PhysicalType enum (six numeric variants)
- `weft-physical-type/src/lib.rs:438` — `parse_decimal` (rejects non-decimal text)
- `weft-server/src/ingest.rs:538-554` — IngestPoint, IngestRequest (BigDecimal value only)
- `weft-server/src/declare.rs:49` — `parse_physical_type` (six token grammar)
- `weft-server/src/openapi.rs:285-370` — route annotations (no object scope)
- `weft-physical-type/src/tags.rs:165,347` — TagSet, SeriesKey (no annotation field)
- `compat/openapi/baseline/weftdb.json` — frozen API spec (no object models)
- Issue #238 — Public surface declared and frozen (Phase 18, open)
