# SOA-15: Nightly soak comparison gate

The nightly soak is a scheduled (`0 7 * * *`) and
[dispatchable](https://github.com/weftdb/weftdb/actions/workflows/soak.yml)
workflow defined in `.github/workflows/soak.yml`. It runs a time-boxed soak
exercise, evaluates the result against the eight SLO rules (R1–R8), and
compares the outcome with the previous night's successful artifact.

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

## What is NOT tested: TTL

The soak workflow's honesty section reports SOA-15's TTL lane as
**not tested**. The store has no row-level TTL or expiry mechanism today:
quarantined corrupted records use a TTL-based purge (`purge-expired`), but
there is no per-row TTL that the soak could exercise at the ingest or
serve layer. Adding a TTL lane would require a store-side TTL feature
first.

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