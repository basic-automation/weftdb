# SOA-15: Nightly soak comparison gate

The nightly soak is a scheduled (`0 7 * * *`) and
[dispatchable](https://github.com/weftdb/weftdb/actions/workflows/soak.yml)
workflow defined in `.github/workflows/soak.yml`. It builds the soak driver,
runs a time-boxed soak exercise against a null server (no live store or HTTP
endpoints), evaluates the result against a skeleton rule set, and compares
the outcome with the previous night's successful artifact.

## What the gate checks

| Step | Produces |
|------|----------|
| Build | `weft-bench` release binary |
| Soak run | `soak-report.json` (SoakReport), `honesty.md` |
| Comparison with previous night | `verdict.json` (SoakComparison), `verdict.md` |

The comparison reports three classes of finding:

1. **Pass→Fail regression** — a rule that passed the previous night now
   fails. This turns the workflow red and fails the job.
2. **Metric drift past tolerance** — a documented metric (e.g. backup RPO
   or RTO) changed more than its allowed threshold. The drift annotator
   (`scripts/annotate-soak-drift.py`) prints GitHub Actions `::warning::`
   annotations. The job is not red, but the warnings appear in the
   workflow summary.
3. **First night / no baseline** — when no prior successful artifact
   exists, the comparison step succeeds with an explicit "first nightly no
   baseline" note. The current run's artifact becomes the baseline for
   the next night.
4. **A refused comparison** — when the two reports' schema versions differ,
   or the two soak runs' schema versions differ, no metric drift figure is
   computed at all. That is the absence of a check rather than a clean pass,
   so `soak-compare` names the cause and exits non-zero, which turns the
   nightly red through the comparison step's `exit "$compare_status"`. Rule
   outcomes are still matched by id across a mismatch, so a genuine
   Pass→Fail regression keeps its own, sharper message and outranks this
   one. A run stamped `soak_schema_version = 0` counts as unstamped and is
   compared, not refused.

## What is NOT tested: TTL lane

The soak workflow's honesty section reports SOA-15's TTL lane as
**not tested**. The store has no row-level TTL or expiry mechanism today:
`purge_expired_quarantine` (`weftdb/src/types/segment_store.rs:1982-1992`)
only purges expired corrupt (quarantined) records, not user data. The soak
driver (`weft-bench/src/main.rs:966`) uses `NullServer` — no live server is
launched — so no server endpoint (`POST .../quarantine/purge-expired` or
otherwise) can be exercised at all. Adding a TTL lane requires a store-side
row-level TTL feature first, and wiring it into the soak driver.

## What is NOT tested: remaining soak fleet (upgrade, read, maint lanes)

The soak-run driver records only harness steps; it does not wire the
SOA-4 workload model, SOA-5 ingest client, SOA-6 oracle, SOA-8 failure
deck, SOA-10 derived-read checker, or SOA-11 backup lane. These are listed
as untested surfaces in the honesty section. SOA-16 (upgrade lane) is also
untested.

All soak lanes beyond the skeleton are pre-1.0 scope; the nightly
workflow exists to validate the comparison machinery and catch regressions
in the soak framework itself.

## The upgrade lane (SOA-16)

The upgrade lane runs an upgrade inside a soak and judges the store it
leaves behind. While ingest is running, the driver stops the old server
binary and starts the new one against the same store root, following
the procedure `UPGRADING.md` (PAC-14a) pins: copy the whole store
directory first, start the new binary, let the first open run the
layout migration, then read every acknowledged point back out of the
upgraded store. The lane (`weft-bench/src/soak_upgrade.rs`) judges
that read-back against the run's whole acknowledged history — every
acknowledged tick, complete and exact, and nothing else — and records
the upgrade step (the versions on either side, the store layouts, and
the pass's duration) on the run record
(`SoakHarness::record_upgrade_verdict`, soak schema v7).

The lane performs no upgrade, stop, start or read itself: the caller
runs the binaries and hands the read-back to the checker, on the
SOA-11 backup lane precedent. The nightly skeleton driver performs no
upgrade, so its honesty section still lists the lane as not tested
until a caller drives it; a run that records an upgrade verdict drops
the line. The upgrade duration is recorded, never gated, and the lane
adds no SLO rule (R1–R8 are fixed). The nightly comparison tracks
three upgrade metrics: `upgrade_ticks_judged` (0 %), 
`upgrade_mismatch_count` (0 % — any mismatch is a regression), and
`upgrade_duration_ms` (10 % — the pass depends on host load).

## Metrics and tolerances

The comparison uses a fixed set of named tolerances defined in
`weft-bench/src/soak_compare.rs` (`DEFAULT_METRIC_TOLERANCES`):

| Metric | Tolerance | Rationale |
|--------|-----------|-----------|
| `steps` | 0 % | deterministic soak-step budget |
| `oracle_ticks_judged` | 0 % | deterministic tick budget |
| `oracle_mismatch_count` | 0 % | any mismatch is a regression |
| `derived_ticks_judged` | 0 % | deterministic tick budget |
| `derived_mismatch_count` | 0 % | any mismatch is a regression |
| `backup_acknowledged_ticks` | 0 % | deterministic tick budget |
| `backup_rpo_ms` | 10 % | backup interval depends on wall clock |
| `backup_rto_ms` | 10 % | restore time depends on host load |
| `backup_restore_duration_ms` | 10 % | restore time depends on host load |
| `upgrade_ticks_judged` | 0 % | deterministic tick budget |
| `upgrade_mismatch_count` | 0 % | any mismatch is a regression |
| `upgrade_duration_ms` | 10 % | upgrade pass depends on host load |
| `contamination_flags_count` | 0 % | same hardware profile each night |

To update tolerances, edit the constant in `soak_compare.rs` and update
this table.

## How to regenerate a baseline

A baseline is a successful nightly run whose `soak-report.json` artifact
is stored for 90 days. To force a new baseline:

1. Go to
   [soak.yml actions](https://github.com/weftdb/weftdb/actions/workflows/soak.yml)
   and click **Run workflow**.
2. Wait for the run to finish — the first successful run after a gap
   becomes the new baseline.
3. The next scheduled or dispatched run will compare against it.

There is no manual "promote this run to baseline" button. Deleting all
soak artifacts from past runs and re-running the workflow creates a
fresh baseline (the comparison step finds no prior artifact and
succeeds with the "first nightly" note).

## Comparison library

The pure comparison function lives in
`weft-bench/src/soak_compare.rs`:

```rust
pub fn compare_soak_reports(
    previous: &SoakReport,
    current: &SoakReport,
    tolerances: &[MetricTolerance],
) -> SoakComparison
```

`compare_soak_reports` is deterministic and stateless. It compares:

- Schema version — a mismatch causes metric drift to be skipped.
- Rule outcomes — matched by rule `id` across the two reports.
- Coverage status — whether coverage met/insufficient changed.
- Metric drift — every named tolerance is checked; a metric absent
  from both reports is skipped.

The returned `SoakComparison` is serialised as JSON for the annotator
script and as Markdown for the job summary.