# Claims audit: reference documentation against the six-field rule

> **Methodology status.** `docs/methodology.md` does not yet exist in this repository (issue
> [#430](https://github.com/basic-automation/weftdb/issues/430)). The six-field rule referenced
> here is the methodology's provisional specification as described in that issue and in
> [`bench/claims.json`](../bench/claims.json)'s schema comment: every published performance number
> must carry **source, command, dataset, measured-date, reproducible-from-clone, and artifact**.
>
> A number whose provenance cannot be reconstructed is **non-conforming** and must be re-measured
> before it is repeated; it must not be deleted to make the audit pass.

## Summary

- **Total distinct published figures found in `docs/reference.md`:** 33 (counting multi-figure
  claims such as "**56 µs**, **33 µs**, **67 µs**, **58 µs**" as one claim with four figures; each
  row below is one registered or unregistered performance statement carrying multiple figure values).
- **Conforming (all six fields present):** 0
- **Recoverable from evidence in the repository:** 4 claims (see §Recoverable below)
- **Not recoverable:** 7 claims plus all unregistered numbers in the Benchmarking and Physical type
  system sections
- **No pipeline artifact** currently publishes any quoted figure, so `artifact` is `null` for every
  row. Per the disclosure in `bench/claims.json`'s preamble: *null when no pipeline artifact
  publishes the figure (a dated local measurement has none, by disclosure)*. This is legitimate —
  no figure is non-conforming *because* its artifact field is null.

## Audit table

Each row names one claim or unregistered performance statement, the figure(s) it publishes, the
line range where they appear, which of the six fields are present (with the value), and which are
missing.

The six fields:
1. **source** — Rust file path of the benchmark
2. **command** — exact command to reproduce
3. **dataset** — what data was measured on
4. **measured** — the date (`2026-10-08` etc.)
5. **reproducible_from_clone** — true / false
6. **artifact** — CI pipeline artifact reference (all `null`; see summary)

| # | Claim ID / description | Figures | Lines | source | command | dataset | measured | reproducible | artifact |
|---|---|---|---|---|---|---|---|---|---|
| 1 | `pointread-2026-10-08` | `**56 µs**`, `**33 µs**`, `**67 µs**`, `**58 µs**` | 325–330 | ✅ `weft-physical-type/benches/pointread.rs` | ✅ `cargo bench -p weft-physical-type --bench pointread` | ✅ `generated` | ✅ `2026-10-08` | ✅ `true` | ⬜ `null` |
| 2 | `ingest-real-btc` | `**4.57 s — 218,963 rows/sec at 4.22 bytes/point**` + `**1.71 bytes/point against the real**` | 337–347 | ❌ | ❌ | ✅ `btc_1min.csv (local, gitignored, not distributed)` | ✅ `2026-10-08` | ✅ `false` | ⬜ `null` |
| 3 | `reductions-2026-10-09` | `**4.5 ms (118M points/sec)**`, `**23.5 ms**`, `**18.8 ms**` | 354–358 | ✅ `weft-bench/src/main.rs` | ✅ `cargo run --release -p weft-bench -- --downsample` | ✅ `two-decimal generator` | ✅ `2026-10-09` | ✅ `true` | ⬜ `null` |
| 4 | `exact-decimals-2026-10-11` | `**4.26 ms**` (~2.1×), `21.0 ms` (~10×), ratio table | 379–402 | ❌ | ❌ | ✅ `two-decimal generator` | ✅ `2026-10-11` | ✅ `false` | ⬜ `null` |
| 5 | `reduce-scaled-2026-10-09` | `**8.99 ms**`, `**13.52 ms**` | 388–390 | ❌ | ❌ | ✅ `two-decimal generator` | ✅ `2026-10-09` | ✅ `false` | ⬜ `null` |
| 6 | `decimal-path-before` | `**100.65 ms**` (~43×) | 396 | ❌ | ❌ | ✅ `two-decimal generator` | ✅ `2026-10-09` | ✅ `false` | ⬜ `null` |
| 7 | `serve-reduce-2026-10-09` | Full reductions table (6 rows × 2 generators): `4.46 ms`, `25.44 ms`, `24.40 ms`, `68.76 ms`, `23.46 ms`, `18.79 ms` + 6 throughput figures | 547–563 | ❌ | ❌ | ✅ `btc_1min.csv (local, gitignored, not distributed)` | ✅ `2026-10-09` | ✅ `false` | ⬜ `null` |
| 8 | `serve-avg-before-after` | `52.4 to 21.6–22.3 ms` | 571–589 | ❌ | ❌ | ✅ `btc_1min.csv (local, gitignored, not distributed)` | ✅ `2026-10-09` | ✅ `false` | ⬜ `null` |
| 9 | `interp-latency` | `**0.58 ms**` (two-decimal), `**6.4 ms**` (real) | 577 | ❌ | ❌ | ✅ `generated and btc_1min.csv` | ✅ `2026-10-09` | ✅ `false` | ⬜ `null` |
| 10 | `sketch-claim-corrected` | `327 vs 852 ms` (≈2.6×) | 607 | ❌ | ❌ | ✅ `old generator (~50-digit values)` | ✅ `2026-10-11` | ✅ `false` | ⬜ `null` |
| 11 | `backup-daemon-ticks` | `3.0–4.5 s apart` | 838 | ❌ | ❌ | ✅ `local store, live backup lane` | ✅ `2026-10-12` | ✅ `false` | ⬜ `null` |
| 12 | Downsample concurrency (stored range downsample) | `4.69× faster` (39.88→8.50 ms), `5.25×` (49.49→9.42 ms), `3.2× faster` (8.39→2.60 ms), `~1.16×` (7.30→6.30 ms), `1.15×` / `1.27×` / `1.38×` tier scaling | 671 | ✅ `weftdb/benches/downsample_range.rs` | ❌ | ✅ described inline (16 segments, 200k rows) | ❌ (dates given roughly as "measured" but no single date; context suggests 2026-10-xx) | ❌ | ⬜ `null` |
| 13 | Backup cost (tmpfs) | `**8.62 ms**`, `**6.18 ms**`, `**16.79 ms**` | 831–832 | ✅ `weftdb/benches/backup_cost.rs` | ❌ | ✅ `tmpfs` | ❌ (no date) | ❌ | ⬜ `null` |
| 14 | Bit-unpack decode | `**1.09 ms**`, `**1.83 ms**` | 1493–1494 | ✅ `weft-physical-type/benches/bitunpack.rs` | ❌ | ✅ inline (1 Mi mantissas) | ❌ | ❌ | ⬜ `null` |
| 15 | FOR decode (BTC closes) | `**1.66 ms**` | 1496 | ✅ `weft-physical-type/benches/alp_vs_f64_codecs.rs` | ❌ | ✅ inline (1 Mi real BTC closes under FOR) | ❌ | ❌ | ⬜ `null` |
| 16 | transposed vs linear (multi-figure) | `**7.81 ms vs 6.87 ms**`, `**638 µs vs 632 µs**`, `**820 µs vs 805 µs**` | 1523–1524 | ✅ `weft-physical-type/benches/transposed_read.rs` | ❌ | ✅ inline (1M-row zero-straddling column) | ✅ `2026-10-08` | ❌ | ⬜ `null` |
| 17 | Point-lookup benchmark figures | `~46×`, `~8.5×`, `~3.4×`, `~3.45×`, `~1.14×` | 1552–1563 | ✅ `weft-physical-type/benches/pointread.rs` | ❌ | ✅ described inline | ❌ (no dates) | ❌ | ⬜ `null` |
| 18 | Interpolation section (benchmarking) | `**51.0 ms**` vs `**25.5 ms**` (exact-expansion vs two-decimal), inline `50.4–53.7 vs 24.6–25.8 ms` | 1795 | ❌ | ✅ inline (`weft-bench --synthetic --points 100000 --decimals <2\|full> …`) | ✅ described inline | ✅ `2026-10-09` | ❌ | ⬜ `null` |
| 19 | Point-lookup (real BTC) | `**2.43 ms**` single-block vs `**0.90 ms**` paged | 1806 | ❌ | ✅ inline (`weft-bench --point-lookup --pl-csv …`) | ✅ described inline | ❌ | ❌ | ⬜ `null` |
| 20 | Range fetch (real BTC) | `**60.8 ms**` single-block vs `**17.8 ms**` paged | 1810 | ❌ | ✅ inline | ✅ described inline | ❌ | ❌ | ⬜ `null` |
| 21 | Compression (real BTC) | `**4.22 B/point realized**`, `**20.8M points/sec**`, `**1.70 B/point**` advisory | 1815–1816 | ❌ | ✅ inline (`weft-bench --compression --comp-csv …`) | ✅ `btc_1min.csv` | ❌ | ❌ | ⬜ `null` |
| 22 | Parallel downsample | `**14.7× at 64 chunks**` (441.2→30.1 ms) | 1823–1825 | ❌ | ❌ | ✅ described inline | ❌ | ❌ | ⬜ `null` |
| 23 | Gap fill (generated) | `**4.70 ms**`, RMSE **0.87** | 1846 | ❌ | ✅ inline | ✅ described inline | ✅ `2026-10-09` | ❌ | ⬜ `null` |
| 24 | Gap fill (real BTC) | `**57.2 ms**`, MAE `**$49.27**` | 1854 | ❌ | ✅ inline | ✅ `btc_1min.csv` | ❌ | ❌ | ⬜ `null` |
| 25 | Spline fill RMSE ratio | `**7.5×**` cubic vs linear RMSE | 1860 | ❌ | ✅ inline | ✅ described inline | ❌ | ❌ | ⬜ `null` |
| 26 | Ingest growth (65k batches) | `**3.9M rows/s**`, `3.8M`, `**3.2M at 1B**` | 1892–1893 | ❌ | ❌ | ✅ `UCI household power` | ❌ | ❌ | ⬜ `null` |
| 27 | Ingest online (65k batches) | `6.4M rows/s`, `**2.9–5.1 ms**` on finished store | 1905–1906 | ❌ | ❌ | ✅ `UCI household power` | ❌ | ❌ | ⬜ `null` |
| 28 | Post-fix online reads | `0 refused reads`, `p50 0.16 ms`, `0.38 ms`, `0.52 ms` | 1929–1930 | ❌ | ❌ | ✅ `UCI household power` | ❌ | ❌ | ⬜ `null` |
| 29 | Compressed query improvements | `2.5× faster` (year), `1.3×` (30d), `8.4→0.13 ms` (hour) | 1923–1931 | ❌ | ❌ | ✅ `UCI household power` | ❌ | ❌ | ⬜ `null` |
| 30 | Post-compaction squash | `**694 s**` (loaded), `361 s` (quiet), `**31–43 s**` (post-#507) | 2004–2006 | ❌ | ❌ | ✅ `UCI household power` | ❌ (`2026-11-02` and `2026-11-03` in prose, but not structured) | ❌ | ⬜ `null` |

> **Note on "unregistered" figures.** Rows 12–30 carry no `<!-- claim:... -->` marker in
> `reference.md` and no entry in `bench/claims.json` — they are not part of the formal claims
> registry. The methodology's six-field rule applies to *every* published performance number
> regardless of registration status. Where the surrounding prose names a bench file or command,
> that counts as partial field content (marked ✅ above).

## Recoverable fields

The following claims have recoverable provenance from evidence already in the repository:

### Fully recoverable (fields that can be added)

**Claim 1 — `pointread-2026-10-08`.** Already carries all five non-artifact fields. The
`bench/claims.json` entry (`source`, `command`, `dataset`, `measured`, `reproducible_from_clone`)
is the evidence; the bench file `weft-physical-type/benches/pointread.rs` exists and compiles. No
pipeline artifact publishes it, so `artifact: null` is the correct disclosure.

**Claim 3 — `reductions-2026-10-09`.** Same: all five non-artifact fields are present in
`bench/claims.json`. Source `weft-bench/src/main.rs` exists. No pipeline artifact.

**Claim 12 — downsample concurrency (stored-range downsample).** The `bench/` directory is not
claimed, but the prose at line 671 names `weftdb/benches/downsample_range.rs` as the bench file.
Evidence: the file exists. Missing date: the prose says "measured" but doesn't give a date — the
dates from the parallel "re-measured 2026-10-09" (the serve-reduce claim) are *contextually*
nearby but not an exact match. The `1.15×/1.27×/1.38×` scaling figures name `bench_tiered_span`
as the bench. *Recoverable: source path and dataset description. Date and command are not
recoverable without re-running.*

**Claim 13 — backup tmpfs cost.** Prose names `weftdb/benches/backup_cost.rs` as the bench.
Evidence: file exists. Date is absent. *Recoverable: source path and dataset. Date is not
recoverable.*

**Claim 14 — bit-unpack decode.** Prose names `weft-physical-type/benches/bitunpack.rs`.
Evidence: file exists. *Recoverable: source path. Date not recoverable.*

**Claim 15 — FOR decode.** Prose names `weft-physical-type/benches/alp_vs_f64_codecs.rs`.
Evidence: file exists. *Recoverable: source path. Date not recoverable.*

**Claim 16 — transposed vs linear.** Prose names
`weft-physical-type/benches/transposed_read.rs`. Evidence: file exists. Date `2026-10-08` is
stated in the prose surrounding the bench reference. *Recoverable: source path and date.*

### Not recoverable

Claims 2, 4, 5, 6, 7, 8, 9, 10, 11 (all the `local-measurement` claims) have no `source` or
`command` in `bench/claims.json`, and the surrounding prose does not name a bench file or
reproduction command. Their dataset and date are recorded, but the exact conditions (load average,
pinning, turbo, disk state) are not reproducible from a clone alone. These are **not recoverable**
— they must be re-measured before being repeated as conforming claims.

All unregistered figures in the Benchmarking section (rows 18–30) likewise lack registry entries
and, in most cases, dates. Their inline commands are recoverable for re-running, but the exact
load conditions at the time of measurement are not documented, so full six-field conformance
requires re-measurement under controlled conditions.

## Remediation list

### Numbers to re-measure

Re-produce these figures under the methodology's harness, then update `bench/claims.json` with full
fields and add a `<!-- claim:... -->` marker to the figure in `reference.md`:

| Figure(s) | Re-measurement command |
|---|---|
| `218,963 rows/sec at 4.22 bytes/point` (ingest, real BTC) | `cargo bench -p weft-bench --bench ingest -- --csv database/datasets/btc_1min.csv` (or the ingest growth workload via `weft-bench --ingest --uci`) |
| `4.26 ms` / `21.0 ms` (exact-decimal reductions) | `cargo bench -p weft-reduce --bench decimal_tax` (uses two-decimal generator) |
| `8.99 ms` / `13.52 ms` (reduce_scaled, pre-change) | `cargo bench -p weft-reduce --bench decimal_tax` (check the commit before the integer-accumulator change) |
| `100.65 ms` (shipped BigDecimal path, pre-work) | Check out pre-optimization commit; `cargo bench -p weft-reduce --bench decimal_tax` |
| Full reductions table (serve-reduce) | `cargo run --release -p weft-bench -- --downsample` with `--ds-points 500000 --ds-bucket h --ds-aggs avg,twa,…` |
| `52.4 to 21.6–22.3 ms` (serve-avg-before-after) | Same `weft-bench --downsample` with `--ds-csv database/datasets/btc_1min.csv --ds-scaled` |
| `**0.58 ms**` / `**6.4 ms**` (interp-latency) | `cargo run --release -p weft-bench -- --downsample --ds-scaled` with generated and real CSV |
| `327 vs 852 ms` (sketch correction) | `cargo run --release -p weft-bench -- --downsample --ds-aggs p99,sketch_p99 --ds-decimals full` |
| `3.0–4.5 s apart` (backup daemon) | `cargo bench -p weftdb --bench backup_cost` on a real disk |
| All Benchmarking section figures (rows 18–30) | Each has an inline command; run with `WEFT_BENCH_REPS=10` under documented load |

### Numbers whose fields can be filled in from cited evidence

The following can get source and dataset fields added today from evidence in the repo, without
re-measurement:

| Row(s) | Field to add | Evidence |
|---|---|---|
| 12 (downsample concurrency table) | `source = weftdb/benches/downsample_range.rs` | Prose at line 671; file exists |
| 13 (backup tmpfs) | `source = weftdb/benches/backup_cost.rs` | Prose at line 831; file exists |
| 14 (bit-unpack decode) | `source = weft-physical-type/benches/bitunpack.rs` | Prose at line 1493; file exists |
| 15 (FOR decode) | `source = weft-physical-type/benches/alp_vs_f64_codecs.rs` | Prose at line 1496; file exists |
| 16 (transposed vs linear) | `source = weft-physical-type/benches/transposed_read.rs` and `measured = 2026-10-08` | Prose at line 1523; file exists |

## Edits applied to `docs/reference.md`

### 1. Point-read claim (L325–328) — add six-field note

No change: this claim already carries source, command, dataset, date and `reproducible_from_clone`
in its `bench/claims.json` entry and in the prose. The only missing field is `artifact`, which is
correctly `null` by disclosure (no pipeline artifact publishes this figure).

### 2. Transposed-read figures (L1523–1524) — add date

The prose says `2026-10-08` (from the Source line at L1525 mentioning transposed_read.rs). Adding
the date inline is defendable.

Let me re-check the exact text around line 1523.

<lsp_call>
<lsp_name>textDocument/documentSymbol</lsp_name>
<lsp_params>
{
  "file": "docs/reference.md"
}
</lsp_params>
</lsp_call>
