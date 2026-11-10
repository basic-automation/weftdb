# Upgrade notes

## v0.3.0 → v0.4.0 (future)

### Breaking changes

1. **Range reads return each timestamp once, newest-wins across overlapping sealed
   segments (#443).** A time-range read no longer repeats timestamps that
   appeared in multiple sealed segments — the v0.1.0 release emitted duplicates where
   overlapping segments carried the same instant, and those windows now collapse.
   **Value-band** reads (`/value-range*`) remain a raw, unfolded scan in segment-seal
   then in-segment order and are unchanged by #443. Point-read semantics are unchanged.
   - v0.1.0 windows with duplicate rows now answer with fewer rows; compatibility
     tests updated for the corrected shape.

## v0.2.1 → v0.3.0

Released 2026-11-05. See the [CHANGELOG](../CHANGELOG.md) for the full list of changes.

### Migration

**No store migration.** v0.3.0 writes store layout 2 (the same as v0.2.0/v0.2.1). The
first open of a v0.2.x store applies additive migrations (`0006_tag5_series`,
`0007_series_time_index`, `0008_upsert_keys`) — these add tables and columns to
`segment_index.db` without rewriting existing data. The marker's layout and floors stay at
2 after these migrations.

**No breaking store changes.** A v0.3.0 store can be opened by v0.2.1 (the layout is the
same; v0.2.1 ignores unknown columns). Do not run v0.1.0 on it.

### Breaking changes

1. **HTTP: storage ILP ingest now requires `measurement=`** (TAG-11).
   `POST /api/v1/storage/{aspect}/ilp` without the `measurement=` query parameter is
   `400 bad_request`. Previously the parameter was optional and silently discarded tags on
   a tagged single-series payload — now tags name the series the rows land in.
   - The Rust SDK's `write_ilp` sends it from the new `WriteOptions::measurement`.
   - The Python client's `write_ilp` does not yet support it (design PR 7b).
   - The compute routes (`/api/v1/{interpolate,downsample}/ilp`) are unchanged.

2. **Library: `IngestBatch::series_ids()` removed** (TAG-7). It always answered `[0]`.
   Read `series_ids` from `IngestReceipt::series_ids` instead. This affects Rust library
   users only; the method never existed on the HTTP API.

3. **Reads without a series selector now return the untagged series only** (TAG-8).
   The existing read methods (those without an `_of` selector) answer series 0 (untagged)
   only. On an aspect holding tagged series, they no longer return the tagged rows — use
   the `_of` read forms that take a `tag.<key>=<value>` selector. A store with no tagged
   writes answers exactly as before.

4. **Stored reads that name no page size now return a page** (ROB-12). A read naming no
   `limit`/`take` gets 10,000 rows (or `WEFT_MAX_PAGE_ROWS`) and a `next_cursor`, instead
   of every row in the window. A client that wanted the whole window already had to page
   through it, but the first response body shape changes: it includes `limit` and
   `next_cursor`.

### What to update

| If you use | Action |
|-----------|--------|
| ILP ingest (HTTP) | Add `measurement=...` to every `POST /api/v1/storage/{aspect}/ilp` request. |
| Rust SDK `IngestBatch::series_ids()` | Replace with `IngestReceipt::series_ids` (the receipt is returned from `ingest`, the batch-level accessor is gone). |
| Stored reads without explicit `limit` | Expect a paged response with `limit` and `next_cursor`. To restore the old behaviour, set `limit=0` (which still pages with the default page size — there is no way to ask for "all rows at once" in v0.3.0). |
| Stored reads after tagged ingest | Reads without `tag.<key>=<value>` selectors return the untagged series only. Add selectors to read tagged series. |

### New variables

See [`docs/config-schema.md`](config-schema.md) for the full reference. New in v0.3.0:

| Variable | Tier | Default | What it controls |
|----------|------|---------|------------------|
| `WEFT_ADMIN_ADDR` | Tuning | Unset | Probe-only admin listener for `/health`/`/ready`. |
| `WEFT_AUDIT_FILE` | Stable | `<store>/audit.jsonl` | Audit log path (SEC-9). |
| `WEFT_BENCH_RUNS_DIR` | Experimental | Unset | Directory of `weft-bench` run manifests. |
| `WEFT_FLIGHT_ADDR` | Experimental | Unset | Arrow Flight gRPC listener address. |
| `WEFT_GPU_ADAPTER` | Experimental | Unset | Select the GPU adapter by index, API or name. |
| `WEFT_GPU_BACKEND` | Experimental | `auto` | Pin the interpolation compute backend. |
| `WEFT_GPU_CALIBRATE` | Experimental | Auto | Skip or force startup GPU calibration. |
| `WEFT_GPU_DEVICES` | Experimental | Unset (one) | Spread interpolation over several GPU devices. |
| `WEFT_MAX_INGEST_DECODED_BYTES` | Tuning | 1 GiB | Ceiling on decoded ingest body size (ROB-7). |
| `WEFT_MAX_INGEST_ROWS` | Tuning | 10,000,000 | Ceiling on rows per ingest body (ROB-7). |
| `WEFT_MAX_SERIES_PER_ASPECT` | Tuning | 100,000 | Max tagged series per aspect (TAG-7, TAG-11). |
| `WEFT_SEGMENT_MMAP_READS` | Experimental | Off | Memory-mapped frame reads (Phase 5). |
| `WEFT_STORE_SCOPE_PIN` | Tuning | `on` | Refuse start on scope mismatch (FRE-12b). |
| `WEFT_WORKER_THREADS` | Tuning | Per-core | tokio runtime worker count (ROB-11). |

### Deprecated

Nothing is deprecated in v0.3.0.

### Known limitations at v0.3.0

- **Audit log has no rotation.** The file grows unbounded; rotate it out-of-process
  (rename and `SIGHUP` to reopen). A torn record after a crash is detected at next
  startup and prevents the server from starting.
- **Key expiry not supported.** Keys have no `Expires` field. Tracked as SEC-11.
- **Admin access control.** There is no separate `admin` scope; backup and maintenance
  use the `write` scope. Tracked as SEC-12.
- **Certificate-based client auth.** Not supported. Tracked as SEC-13.
- **Arrow Flight is experimental and plaintext only.** The listener refuses a non-loopback
  bind because it has no TLS.
- **Container health check cannot speak TLS.** A TLS-only container disables the built-in
  health check.
- **Python SDK `write_ilp` does not yet send `measurement=`** (design PR 7b).

### Rollback

A v0.3.0 store can be read by v0.2.1 (same layout 2). **Do not run v0.1.0** on it.

To roll back `weft-server` itself:

1. Stop the v0.3.0 server.
2. Replace the binary (or revert the container image tag).
3. Remove or rename the audit log if it was at the default path (the older binary does not
   know to ignore it and would fail to open the store if the file cannot be truncated).
   With `WEFT_AUDIT_FILE` set explicitly, unset it or point it at a path the older binary
   won't check.
4. Start the older binary.

The store layout did not change, so no restore is needed for the data.

---

## v0.1.0 → v0.2.0

Released 2026-11-02. Copied from [CHANGELOG.md](../CHANGELOG.md) for reference.

⚠️ **Upgrading a segment store from v0.1.0 is one-way.** The first open by v0.2.0 migrates
a v0.1.0 store in place to store layout v2: it writes a `STORE_FORMAT` marker, adds columns
and tables to `segment_index.db`, and from then on segment ids come from a persistent
allocator and maintenance writes new, journaled frames instead of rewriting them in place.
v0.1.0 reads no marker, so it would not refuse a migrated store, but it knows none of this:
once v0.2.0 or later has opened a store, **do not run v0.1.0 on it again**.

Before upgrading, stop `weft-server` and **copy the whole store directory**
(`POST /api/v1/storage/backup` copies only the control plane, not the segment frames).