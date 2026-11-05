# Phase 1: the pooled cross-engine report (#508)

Two separate full runs of `weft-xbench`, pooled with `weft-xbench combine` into
[`report.md`](report.md) (and its self-contained [`report.html`](report.html)). WeftDB is measured
against DuckDB, ClickHouse, TimescaleDB, QuestDB, InfluxDB 3 Core and IoTDB, on MCAR gaps, block
missingness, the UCI household-power corpus and one TSBS `cpu-only` host, at `small` and `medium`
scale. Every engine is asked for the grid in each interpolation class it supports: native fill (A),
portable SQL (B) and client-side (C). The report's honesty section (wins, losses, noise, what was
not tested) is generated from [`xbench.json`](xbench.json), not written by hand. The two runs it
pools are [`run-1.json`](run-1.json) and [`run-2.json`](run-2.json).

## Where and how it ran

- **Host:** the reference SSD on the project machine (`/mnt/weftbench`). AMD Ryzen 9 7950X3D,
  16 cores, 125 GiB. The engines' stores and temp files were on that disk
  (`TMPDIR=/mnt/weftbench/tmp`). The five servers ran in local containers whose image digests,
  commands and environment (secrets redacted) are in the report's run manifest. A local model
  server that had held 82 GB of memory, and made ClickHouse refuse queries in the first attempt,
  was stopped for both runs. This is not priced cloud hardware, so the report has no cost figure.
- **Commit:** `4531d48` (main after #511, plus the `weft-xbench` lockfile fix in #513), release
  build, `--locked`, clean tree.
- **Reps:** 10 per interpolation and downsample row, 3 per load row, in each run; pooled, 20 and 6.
- **Inputs:** the UCI file (SHA-256 `4259c9d7…94b7`) and TSBS's `cpu-only` data (SHA-256
  `30cd624e…e659`), both recorded in full in the run manifest with their sizes.

## Reproduce

```bash
cargo build --locked --release --manifest-path weft-xbench/Cargo.toml
for n in 1 2; do
  TMPDIR=/path/on/disk/under/test target/release/weft-xbench --scales small,medium --reps 10 --load-reps 3 \
    --uci household_power_consumption.txt --tsbs cpu-only.lp \
    --clickhouse URL --timescale URL --questdb HOST:PORT --influxdb3 URL --iotdb URL --out run-$n
done
target/release/weft-xbench combine run-1/xbench.json run-2/xbench.json --out pooled
```

The report's reproduction section gives each server's container and the TSBS generator's pinned
commit; the synthetic datasets are regenerated from the seed in the report.

## How far the verdicts hold

**319 of the 320 correctness gates pass.** The one that fails is not WeftDB's:
`timescaledb-double` downsample at `mcar-medium`. Each run passed it on its own, 7.1e-14 and
1.4e-13 from the reference against a tolerance of 9.2e-8, but the two runs' answers differ in the
last bits, so their digests disagree, and `combine` refuses to pool answers that are not the same
answer. TimescaleDB's `avg` over `double precision` does not add in a fixed order, so two runs of
the same query can differ in the last bits. The gate is doing what it was built to do, and it is
left failing rather than loosened after the fact. The row has no verdict.

Of the 112 win, loss and noise verdicts, **105 are the same in both runs, and none goes from a win
to a loss or back**. The verdicts that hold in both runs are **17 wins, 85 losses and 3 within
noise**:

- **Every WeftDB win is against class B, portable SQL** (DuckDB's hand-written `ASOF JOIN`
  reconstruction), on interpolation and on the in-memory downsample. Five hold at `medium`:
  TSBS interpolation, 2.2× to 6.1× faster on every WeftDB arm, and UCI interpolation on the `f64`
  stored path, 1.5× faster.
- **WeftDB has no win against class C.** Fetching the knots into a Rust client and interpolating
  there beats WeftDB's interpolation by 1.6× to 11.6×: 27 of those 30 losses are to ClickHouse's
  client arm and 3 to QuestDB's.
- **Every load loses**: the fastest engine on every dataset is ClickHouse, whose one-part
  `INSERT` is 7.8× to 47.4× faster than WeftDB's bulk and durable-segment loads.
- **The stored downsample loses** to DuckDB and QuestDB, by 1.5× to 5.8×; only the in-memory
  downsample wins.

Every rep was warm (no `--cold-hook`), only `small` and `medium` ran, and the widest p50 spread
between the two runs was 200%. Treat a margin under about 1.5× as unsettled.
