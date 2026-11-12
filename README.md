<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/weftdb-logo-dark.svg">
    <img src="docs/assets/weftdb-logo-light.svg" alt="WeftDB" width="240">
  </picture>
</h1>

**The time-series database for data that wasn't sampled on a clean grid.**

[![Release](https://img.shields.io/github/v/release/basic-automation/weftdb?include_prereleases)](https://github.com/basic-automation/weftdb/releases)
[![CI](https://github.com/basic-automation/weftdb/actions/workflows/ci.yml/badge.svg)](https://github.com/basic-automation/weftdb/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue.svg)
![Status](https://img.shields.io/badge/status-pre--beta-orange.svg)

Sensors drop out. Markets trade in bursts. Devices upload in batches after hours offline.
Most time-series databases expect a steady cadence, so when you ask for a value between two
samples you get a gap, a `NULL` or a `0`, and rebuilding the signal is left to your code.

WeftDB does that rebuilding **inside the database**. Ask for any resolution and you get a
continuous series, filled with the method you choose, and **every point says where it came
from**: `raw`, `interpolated` or `extrapolated`. A made-up value is never mistaken for a
measured one.

```bash
curl -s -X POST http://127.0.0.1:8080/api/v1/interpolate \
  -H 'content-type: application/json' \
  -d '{"spline":"linear","resolution":"seconds",
       "points":[{"timestamp":"1970-01-01T00:00:00Z","value":0.0},
                 {"timestamp":"1970-01-01T00:01:00Z","value":60.0}]}'
```

Two samples a minute apart come back as 61 per-second points: the two you sent marked `raw`,
the 59 between them `interpolated`.

## Why WeftDB

- **Gap filling is a query, not a chore.** Resample, fill and upsample next to the data, with
  linear, quadratic or cubic splines, on the CPU or the GPU, instead of looping over raw rows
  in your application.
- **No silent rounding.** You declare each series' precision (`f64`, scaled integers or exact
  decimals). A value that doesn't fit is rejected, never quietly rounded, which matters for
  prices, billing and calibrated readings.
- **Real and synthetic points stay apart.** Every reconstructed point is labelled, so alerts,
  models and invoices can ignore or weight them on purpose.
- **Plays well with your tools.** Ranges and downsamples come back as CSV or Parquet as well
  as JSON (downsamples as Arrow too), and the endpoints accept InfluxDB Line Protocol, so data
  goes straight into pandas, Polars or DuckDB.
- **Honest numbers.** Every performance claim comes from a benchmark in this repository that
  you can re-run, and the results show where WeftDB loses as well as where it wins. The
  [rules behind every published number](docs/methodology.md) — the harness, the hardware class,
  the repetitions, and what disqualifies a measurement — are written down alongside them.

## Install

**Pre-built binaries.** Each [release](https://github.com/basic-automation/weftdb/releases)
has `weft-server`, `weft-tui` and `weft-bench` for Linux (x86_64, aarch64; glibc 2.28 or
newer), macOS (Intel, Apple silicon) and Windows (x86_64), with a `SHA256SUMS` file. The macOS
and Windows binaries are not signed yet, so Gatekeeper and SmartScreen will warn.

**From source.** Rust 1.95 or newer:

```bash
git clone https://github.com/basic-automation/weftdb && cd weftdb
cargo build --release -p weft-server
```

**As a Rust library.** The library crates are on crates.io:

```toml
[dependencies]
weftdb = "0.2"              # store, query and interpolate
weft-orchestration = "0.2"  # the analytics pipeline
splimes = "1"               # the interpolation engine on its own
```

**In a container.** `scripts/packaging/build-image.sh` builds the server image; there is no
published image yet. See [Running in a container](docs/reference.md#running-in-a-container).

## Quickstart

### 1. Start the server

```bash
WEFT_SEGMENT_STORE_ROOT=./store weft-server
# weft-server v0.3.5 listening on http://127.0.0.1:8080
```

It listens on localhost only by default. `WEFT_SEGMENT_STORE_ROOT` is where your data is
stored; without it, the compute endpoints below still work.

### 2. Fill gaps without storing anything

Send points in the request and get the filled series back. This is the quickest way to try
WeftDB on your own data:

```bash
curl -s -X POST http://127.0.0.1:8080/api/v1/interpolate \
  -H 'content-type: application/json' \
  -d '{"spline":"cubic","resolution":"seconds",
       "points":[{"timestamp":"1970-01-01T00:00:00Z","value":0.0},
                 {"timestamp":"1970-01-01T00:00:30Z","value":30.0},
                 {"timestamp":"1970-01-01T00:01:00Z","value":60.0}]}'
```

Already have InfluxDB Line Protocol? Send it as it is:

```bash
printf 'cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n' | \
  curl -s -X POST \
    'http://127.0.0.1:8080/api/v1/interpolate/ilp?field=load&precision=s&spline=linear&resolution=seconds' \
    -H 'content-type: text/plain' --data-binary @-
```

### 3. Store a series and query it

Declare the series once. This is where you choose its precision:

```bash
curl -s -X POST http://127.0.0.1:8080/api/v1/storage/aspects \
  -H 'content-type: application/json' \
  -d '{"name":"price","physical_type":"scaled_i64","scale":2,
       "value_tolerance":"0","timestamp_unit":"millis"}'
```

That means *exactly two decimal places*: a third is rejected rather than rounded. Then write
and read:

```bash
# write
curl -s -X POST http://127.0.0.1:8080/api/v1/storage/price/points \
  -H 'content-type: application/json' \
  -d '{"points":[{"timestamp":1000,"value":"1.25"},
                 {"timestamp":2000,"value":"2.50"},
                 {"timestamp":9000,"value":"3.75"}]}'

# a range, and a single instant
curl -s 'http://127.0.0.1:8080/api/v1/storage/price/points?start=0&end=9999&limit=10'
curl -s 'http://127.0.0.1:8080/api/v1/storage/price/at?t=2000'

# 2-second buckets, with the empty ones filled by a straight line
curl -s 'http://127.0.0.1:8080/api/v1/storage/price/downsample?resolution=2s&agg=avg,max&fill=linear'
```

For a file instead of JSON, ask for `…/range.csv` or `…/range.parquet`, or
`…/downsample.csv`, `.arrow` or `.parquet`:

```bash
curl -s 'http://127.0.0.1:8080/api/v1/storage/price/range.csv?start=0&end=9999'
```

The [HTTP API reference](docs/reference.md#http-api) lists every endpoint.

## Is it right for you?

**A good fit:**

- irregular or gappy series where you need values *between* the samples: sensor networks,
  market data, devices that upload in batches;
- resampling and gap filling that runs often enough to cost you real time in application code;
- values where precision is not negotiable, such as prices, quantities and scientific
  readings;
- workloads where it matters which points are real.

**Not a good fit, today:**

- **production systems that need a hardened, reviewed database.** WeftDB is pre-beta. It has
  TLS and API keys with roles and a fsynced audit log, but no security review yet;
- **generic analytics over wide tables:** use ClickHouse or DuckDB;
- **regular, gap-free data:** if you never need values between samples, a conventional
  time-series database is simpler;
- **tags and labels on measurements:** not supported yet;
- **managed hosting:** there is none; you run it yourself.

[HONESTY.md](HONESTY.md) has the longer version, including where WeftDB measurably loses.

## How fast is it?

The cross-engine benchmark compares WeftDB with DuckDB, ClickHouse, TimescaleDB, QuestDB,
InfluxDB 3 and IoTDB on the same data, the same machine and a correctness check for every
answer. The latest pooled run is in
[`bench/reports/phase1-pooled/`](bench/reports/phase1-pooled/README.md). In short:

- **WeftDB wins at interpolating inside the database:** it beats the same reconstruction
  written in SQL on all four datasets at the smaller scale, and on two of the four at the
  larger one.
- **It loses on bulk loading, and to fetching the raw points and interpolating in a fast
  client.** The report gives every figure, win and loss, with how far each one holds.

Run it yourself with `weft-xbench`; see [Benchmarking](docs/reference.md#benchmarking).

## Status

**v0.3.5 is pre-beta.** APIs, configuration and the on-disk format can still change between
0.x releases, and upgrading a store from v0.1.0 is one-way. What works today:

| | |
|---|---|
| Interpolation, gap fill, downsampling, HTTP API, Arrow/Parquet | Working, tested, benchmarked |
| Security: TLS, API keys and roles | Working, with a fsynced audit log; no security review yet |
| Crash safety | Durable writes and crash recovery; writes are refused (`507`) before they would fill the disk past a reserve; power-loss testing on real hardware still to do |
| Backup and restore | Working, including `weft-server restore` |
| Binaries, crates, container image | Binaries and crates released; container image built but not published |
| Client SDKs (Python, R, Rust) | Rust crate on crates.io; Python and R shipped in the tree; not on PyPI or CRAN yet |
| Tags (per-measurement series) | Working, tested (TAG-5..TAG-11); TAG-12 benchmarks still open |
| Helm chart | Not started |

The [changelog](CHANGELOG.md) lists every change and this release's known limitations;
[ROADMAP.md](ROADMAP.md) shows what's next.

## Learn more

- [**Reference**](docs/reference.md): every endpoint, configuration variable, the storage
  format, the terminal UI, the Rust API and how it all works.
- [**Operations**](docs/operations.md): backup and restore, systemd, container, upgrade,
  failure modes and what to do.
- [Migrating a 0.1.x data directory](docs/reference.md#migrating-a-01x-data-directory)
- [Configuration](docs/reference.md#configuration) and [backup and restore](docs/reference.md#backup--restore)
- [Stability and versioning policy](docs/stability.md): what you may rely on, what is
  experimental, and what a wire-format change counts as.
- [SECURITY.md](SECURITY.md), [HONESTY.md](HONESTY.md), [ROADMAP.md](ROADMAP.md),
  [CONTRIBUTING.md](CONTRIBUTING.md)

## License

Copyright (c) 2025-2026 Justin Icenhour.

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Two crates include code adapted from Apache-2.0 projects — the Chimp codecs in
`weft-physical-type` and the DDSketch quantile sketch in `weft-reduce`. Those portions
stay under the Apache License 2.0 whichever option you choose. [NOTICE](NOTICE) lists
them, and each crate's `THIRD-PARTY-NOTICES` file carries the attribution and the
license text.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

Every pull request also needs either a DCO sign-off on each commit or a signed CLA; see
[Contribution terms](CONTRIBUTING.md#contribution-terms).
