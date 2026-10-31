# EVI-15: first cross-engine report (WeftDB vs DuckDB)

The run behind [`report.md`](report.md) (and its self-contained [`report.html`](report.html)):
decision 18's matrix of WeftDB stored, in-memory and `f64` against DuckDB `DOUBLE` and `DECIMAL`,
on MCAR gaps, block missingness and the UCI household-power corpus, at three scales. All 99
correctness gates passed. The honesty section in the report (wins, losses, noise, what was not
tested) is generated from [`xbench.json`](xbench.json), not written by hand.

## Where and how it ran

- **Host:** the reference SSD on the project machine (`/mnt/weftbench`, an NVMe-class disk of its
  own; decision 19). AMD Ryzen 9 7950X3D, 16 cores with SMT off, 125 GiB. Both engines' stores and
  temp files were on that disk (`TMPDIR=/mnt/weftbench/tmp`); the report's "working directory"
  caveat names the checkout's disk, which held no data. This is not priced cloud hardware, so the
  report has no hourly-cost field and no cost-per-query figure.
- **Commit:** `9903465` (main after EVI-14), release build, `--locked`.
- **Reps:** 10 per interpolation and downsample row, 5 per load row (each load builds a store from
  scratch).
- **UCI input:** `household_power_consumption.txt` from the UCI zip, SHA-256
  `9f84b46ade8a2d8e1286ec4b2b6c2987a45a755c59f263be3b3b3d10dfbda3ff`, CC BY 4.0 (Hébrail and
  Bérard, UCI Machine Learning Repository, dataset 235). `scripts/xbench/fetch-uci.py` downloads and
  checks it.

## Reproduce

```bash
scripts/xbench/fetch-uci.py /path/to/data
cargo build --locked --release --manifest-path weft-xbench/Cargo.toml
TMPDIR=/path/on/disk/under/test target/release/weft-xbench --scales tiny,small,medium --reps 10 --load-reps 5 --uci /path/to/data/household_power_consumption.txt --out OUT
```

The synthetic datasets are regenerated from the seed in the report.

## Known defect in the load rows

Every load rep after the first in this report also timed the teardown of the rep before it:
dropping the previous WeftDB store removed its directory, and the previous `DuckDB` file was
deleted, inside the load's clock. Found on 2026-10-31 while adding the IoTDB arm, whose
`DROP DATABASE` takes a second; the runner now releases the previous load before starting the
clock (DECISIONS.md, "Load timing excludes the previous load's teardown"). The interpolation and
downsample rows are unaffected. The load rows here are left as published; the combined
cross-engine rerun supersedes them.

## The discarded first run

A first run with 3 load reps ([`discarded-run-1.json`](discarded-run-1.json)) also passed every
gate, but one row was broken by something outside the benchmark: `mcar-medium` load for
`weftdb-stored-f64` took 5.3 s, 61 s and 140 s across its three reps, against 5.2 s ± 0.1 for the
identical load on `weftdb-stored` in the same run. Nothing else in the run moved. With three reps,
one stall is the median, so the row could not be reported. The whole matrix was re-run once, with 5
load reps; that run is the report. The discarded run is kept so the choice can be checked.

## How far the verdicts hold across runs

Comparing the two runs row by row says the report's intervals are too narrow to be read as the
uncertainty of a figure. The p50 intervals are bootstrapped from the reps *within* one run, and
53 of the 144 rows have intervals that do not overlap between the two runs, with shifts of up to
about 40% (most of them on sub-10 ms rows and on the UCI rows). Run-to-run variation on this host
is larger than within-run variation, so a single run's interval understates it.

The verdicts are steadier than the figures. Of the 90 win, loss and noise verdicts, **79 are the
same in both runs, and none goes from a win to a loss or back**. The 11 that differ move between a
decisive verdict and "within noise", and each has a margin of 1.4× or less:

| dataset | phase | arm | discarded run | this report |
| --- | --- | --- | --- | --- |
| block-medium | downsample | weftdb-memory | noise | loses |
| block-small | interpolate | weftdb-stored | noise | loses |
| block-tiny | downsample | weftdb-stored | loses | noise |
| block-tiny | downsample | weftdb-stored-bulk | noise | wins |
| mcar-medium | interpolate | weftdb-stored-f64 | noise | loses |
| mcar-small | downsample | weftdb-memory | noise | wins |
| mcar-small | interpolate | weftdb-stored-bulk | loses | noise |
| mcar-tiny | downsample | weftdb-stored | loses | noise |
| mcar-tiny | downsample | weftdb-stored-bulk | loses | noise |
| uci-medium | interpolate | weftdb-stored-f64 | noise | wins |
| uci-small | downsample | weftdb-memory | wins | noise |

So the verdicts that hold in both runs are **23 wins, 51 losses and 5 within noise**. Every one
of the 23 wins is at `tiny` or `small` scale. At `medium` scale WeftDB has no win that holds in
both runs: the `uci-medium` `f64` interpolation win (1.4×) was within noise in the discarded run.
Treat any margin under about 1.5× in `report.md` as unsettled until several independent runs
agree.
