# WeftDB performance methodology

The rules by which a performance number may be published in this repository, and the
evidence a published number must carry so that a reader can reproduce it, audit it, or
reject it.

## What counts as a performance claim

A **performance claim** is any number in a file tracked by this repository that would
move a reader's evaluation of WeftDB's speed, efficiency, resource consumption, or
scaling behaviour. This includes, without limitation:

- A latency percentile (p50, p95, p99, mean) for any database operation, measured
  end-to-end or at any internal stage.
- A throughput figure expressed in points per second, points per millisecond, bytes
  per point, operations per second, or any equivalent unit.
- A compression ratio or realised bytes-per-point for stored data.
- A speed ratio between two engines (e.g. "engine X is Y× faster than engine Z"),
  or between two versions of WeftDB itself, or between two code paths
  (e.g. "the sketch path is Y× faster than the exact path").
- A resource footprint such as peak memory, GPU memory use, or disk space for a
  given dataset.
- Any claim of the form "we chose X because it was faster", stated or implied.
- A scaling factor (e.g. "throughput grows linearly with core count up to N
  cores") offered as a generality rather than a narrow observation.

The following are **not** performance claims and may be stated without the evidence
requirements of this document:

- A correctness property (e.g. "the reduction preserves every input value within
  tolerance"), unless it uses speed or throughput to argue for its own significance.
- A measurement stated in the methodology document itself (this document's numbers
  describe the methodology's own bounds, not the product's).
- A number quoted from a peer-reviewed paper or a standards body, with a citation,
  where the number is not attributed to WeftDB.

## What a claim must carry

Every performance claim published in this repository must, in the same document or
in a directly linked manifest that the claim names, carry each of the following.
A claim that omits any one of these is incomplete and must not be presented as an
achievable result.

### Crate version and commit

The version of every crate whose code contributed to the measured operation, or
`(checkout)` if the measured binary was built from a checkout rather than a tagged
release, and the full Git commit SHA from which the binary was built. A dirty checkout
must be recorded as the commit SHA with a `+dirty` suffix; a claim from a dirty
checkout must explain which uncommitted changes are present.

### Target triple and CPU model

The full Rust target triple (e.g. `x86_64-linux-gnu`, `aarch64-darwin`) and the CPU
model string the operating system reports (e.g. `AMD Ryzen 9 7950X`, `Apple M3 Pro`).
Platform features that affect the measurement (SIMD level, core count, hyper-threading
state, frequency governor, thermal throttling state when it was measured) must be
stated; a claim whose platform is only described as "a Linux machine" is incomplete.

### Release vs dev profile

Whether the binary was built with `--release` (optimised) or `--debug`/`--dev`
(unoptimised). A number obtained from a dev-profile binary must be labelled **dev
profile** every time it appears, and must never be presented beside a release-profile
number without the profile difference being called out in the same sentence.

### Dataset identity and provenance

The dataset the claim was measured against, in enough detail that a reader can obtain
or reproduce it:

- For a **synthetic dataset**: the generator seed, the generator shape parameters
  (point count, timestamps, value distribution parameters, decimal places), and the
  random number generator algorithm and seed. The published seed must produce the same
  byte-for-byte dataset when the same tool and flags are used.
- For a **real dataset**: the file name, the row range or filter applied, the column
  selected, and a link to where the dataset is obtainable. If the dataset is not
  publicly available, the claim must say so and state why the reader should still
  credit the number (e.g. "the corpus is proprietary, but the methodology requires
  the same harness and tolerances to be used on a public dataset before the number
  may be cited").

### Run count

How many timed repetitions the reported latency or throughput is derived from, as a
whole number. A single run (reps = 1) must be labelled **single run** and may not be
presented as typical. The minimum for presenting a number as typical is five runs.

### Tolerance and how it was chosen

For any measurement that compares against a reference (correctness gate, accuracy
score, or cross-engine comparison), the tolerance value expressed as a decimal string
and a prose justification of how that tolerance was arrived at. The justification must
be specific to the measurement, not a generic "engineering tolerance". For example:
"tolerance 1e-9 was chosen because the values are two-decimal USD prices sealed as
`ScaledI64` with scale 0.01, so the maximum representable error between two
reconstructions is one hundredth of a cent, which at these magnitudes is 5e-11 of
the value; 1e-9 is two orders of magnitude above that floor and covers any
interpolation-grid edge effect." A tolerance chosen after seeing the data is a
**calibrated tolerance** and must be labelled as such; a tolerance set before the
run was run is a **committed tolerance**.

### Warm-up policy

Whether each timed repetition started from a cold state (process restart or OS cache
drop), a warm state (previous rep left the process resident and the page cache warm),
or a steady state (the process had handled enough requests that JIT compilation and
allocation patterns had stabilised). The number of warm-up rounds before the first
timed round, and the criterion for deciding that warm-up is complete, must be stated.

### Tool and exact command line

The exact executable name (e.g. `weft-bench`, `weft-xbench`) and the complete command
line, including every flag and its value, that produced the reported measurement. The
command line must be one that the tool currently accepts — a reader must be able to
paste it and, on a comparable machine, obtain a result of the same order. If the
command reads a configuration file or environment variable that changes the behaviour,
those must be stated too.

### Manifest (when available)

When a run was performed by a tool that writes a run manifest (`weft-bench
--manifest`, `weft-xbench`), the published claim should link to or name the manifest
file, and the manifest must be the one the tool wrote — not a hand-edited copy. The
manifest records the provenance facts (binary version, source commit, environment,
run metadata) that this section describes, and verifying a manifest against the report
it accompanies is itself a checked operation.

## What disqualifies a claim

A number that meets any of the following criteria is **disqualified** and must not be
stated, cited, or implied as a WeftDB performance claim anywhere in this repository.

### Dev-profile numbers presented as achievable

A number obtained from a dev (unoptimised) build that is presented without the
**dev profile** label, or that is presented beside a release-profile number without
an explicit call-out that the comparison spans profiles, is disqualified.

### Single runs presented as typical

A number from a single timed repetition (reps = 1) that is presented without the
**single run** label, or that forms the sole basis of a comparison or a ratio, is
disqualified.

### Cross-engine comparisons through a non-adapter workload

A performance ratio or ranking that compares WeftDB with another engine where the
competitor was driven through a workload not built for it, specifically:

- A workload that does not set `adapter_info` for the competitor engine.
- A workload whose `adapter_info` records `null` for the competitor.
- A workload that uses WeftDB's own ingestion path or storage format and then
  compares read performance against an engine that never received the same data
  through its own ingestion.

These cases are recorded as `adapter_info: null` in the schema and measure only
WeftDB's own paths against an external engine running a generic benchmark harness,
not a fair comparison. They may be stated as "WeftDB vs [engine] on the WeftDB
workload" but must not be stated as "[engine] is Y× slower than WeftDB for [task]"
or any equivalent framing. Issue #452 discusses the rationale.

### Indeterminate runs

Any number from a run whose correctness gate or combined gate reported
`indeterminate` — meaning each repetition passed its own tolerance but the
repetitions produced different answer digests. Per the decision recorded for
issue #516, a run that passes its tolerance every time yet produces different
digests is reported as `indeterminate` / "Non-deterministic within tolerance",
not as a pass or a failure. A claim drawn from such a run cannot be said to be
reproducible and must not be published. Until the indeterminacy is resolved
(address the root cause, tighten the tolerance, or increase the run count so that
the bootstrap intervals bound the spread), the number may not appear in a claim.

### Missing-tolerance comparisons

A cross-engine or cross-version comparison where the tolerance for the correctness
gate is not stated. If the reader cannot tell what difference between engines is
considered significant by the claim's author, the claim is unverifiable and is
disqualified.

## How to reproduce a published number

To reproduce a published performance claim, a reader should follow the steps below
in order, using the versions and commands recorded with the claim.

### 1. Obtain the same machine profile

Confirm that the CPU model, core count, memory, and storage are comparable to the
one named in the claim's platform description. The relevant hardware dimensions are
the ones the claim's harness measures — a compression claim depends chiefly on main
memory and CPU, a throughput claim on the number of cores and the storage bandwidth.
If the claim names a GPU, confirm that the GPU model, driver version, and available
memory are the same or comparable.

### 2. Check out the same commit

```bash
git checkout <COMMIT_SHA>
```

If the claim records `+dirty`, apply the described uncommitted changes before
building. Confirm the checkout is clean or dirty as described with
`git describe --always --dirty`.

### 3. Build the same profile

```bash
cargo build --release
```

If the claim is from a dev-profile run, use `cargo build` without `--release`. If
the claim names a specific build flag or cargo feature, add those.

### 4. Set the environment

Set every environment variable the claim's command line reads that is not the
default. For most `weft-bench` runs the defaults suffice. For a `weft-xbench`
cross-engine run, set the engine endpoint URLs (`--clickhouse`, `--timescale`,
`--questdb`, `--influxdb3`, `--iotdb`) that the claim used.

### 5. Run the benchmark

Run the exact command line the claim records. Example from the reference document:

```bash
cargo run -p weft-bench --bin weft-bench -- --downsample --ds-bucket m \
  --ds-aggs min,max,p99,twa --reps 20
```

For a cross-engine run:

```bash
TMPDIR=/mnt/weftbench weft-xbench \
  --scales tiny,small,medium --reps 10 --load-reps 3 \
  --seed 7 --uci ./UCI_Household_Power.csv
```

### 6. Verify the output

Confirm that the run produced a JSON report whose content matches the manifest
(if a manifest was written) by running:

```bash
weft-bench verify PATH/TO/manifest.json
```

Or, for `weft-xbench` combined reports, confirm that every arm's correctness gate
passed (or is `indeterminate` where the claim acknowledges it).

### 7. Compare

Compare the reproduced latency or throughput against the published figure.
Because all measurements are statistical, a reproduced p50 within 20 % of the
published figure on comparable hardware is considered a successful reproduction,
provided the machine's load average during the run was stated with the claim.
A reproduced figure that falls outside this band may indicate a hardware or
configuration difference, and the reproduction report should name what differs.

## Where numbers may be stated

Performance numbers may appear in the following files, under the conditions stated.

### `docs/methodology.md` (this file)

This document may contain numbers that describe the methodology's own bounds and
definitions (e.g. "a reproduced p50 within 20 % of the published figure"), but must
not carry performance claims about WeftDB itself. It is not a venue for benchmarks.

### `docs/reference.md`

This document is the primary venue for performance numbers that are maintained
as part of the reference. Every number in the [Performance](reference.md#performance)
and [Benchmarking](reference.md#benchmarking) sections must cite an exact command line
that
produced it (or a directly linked run manifest), and every such command line
must be one that the current `weft-bench` or `weft-xbench` accepts. The reference
document may contain numbers that were obtained on hardware not available to the
reader; in that case the hardware description must be complete enough (see
"What a claim must carry" above) that the reader can judge comparability.

### `README.md`

The README may carry **headline numbers only** — a small set of representative
figures that give a new reader an immediate sense of performance. Every headline
number must directly cite the section and command line in `docs/reference.md`
where it is backed by a full claim. The README may not invent a number; every
performance figure in it must trace to a full claim in the reference document.

### `DECISIONS.md`

Decision records may state performance observations as part of their rationale,
but those observations are not maintained claims and must not be advertised or
repeated outside the context of the decision. A decision record that states a
performance number should preferably link to the reference-document section that
contains the full claim.

### `CHANGELOG.md`

The changelog may state performance improvements in prose ("downsampling is
approximately 1.3× faster on the integer path"), but not precise numbers
(percentiles, throughputs, ratios) without citing the reference-document claim.
Improvements stated in the changelog must name the relevant benchmark command
or manifest.

### All other files

Performance numbers must not appear in any other file tracked by this repository.
This includes, without limitation, issue comments, pull request descriptions, CI
configuration files, build scripts, test fixtures, and documentation that is not
one of the files named above. The rationale is traceability: a number in an
unexpected file is not maintained and a reader cannot tell whether it is current
or outdated.

Numbers already present in Rust doc comments and test fixtures predate this
document and are not held to it today. They are tolerated, not endorsed: no new
number may be added to such a file without a matching claim in the reference
document, and an audit that moves or deletes the existing ones is tracked
separately (#430).