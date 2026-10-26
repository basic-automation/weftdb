# Decisions

Judgement calls made while working the 1.0 buildout, where the instruction set was silent or
self-conflicting and the choice is not visible in the diff. Recorded so the next session does not
re-litigate them, and so a reader of a ticked roadmap item can see what it was decided to mean.

## 2026-10-26 — SOA-2 "preflight, device resolution, run metadata and secret redaction" (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6, W2) lists SOA-2 by name only, and §3 says
only that "EVI-3 owns the shared run metadata and redaction; SOA-2 reuses
it" and that SOA keeps its own `soak_schema_version`. The concrete
preflight gates, the device-resolution mechanism, and what "secrets are
redacted in soak outputs" means beyond SOA-1's `redacted_label` are not
specified anywhere. SOA-1's module doc supplies the one seam: preflight
"validates a config before a harness starts".

**Chosen (grounded in SOA-1's seam plus the plan's §3 ownership rules; if
merge review disagrees, this decision is revisited there):**

1. Preflight is a report, not a boolean: `SoakHarness::preflight(store_dir)`
   returns a `SoakPreflight` carrying named gates even when gates fail, and
   `start_checked(preflight)` is the gate that refuses to start. The SOA-1
   `start()` transition is kept ungated for compatibility; the scheduler
   path (SOA-3) must use `start_checked`.
2. Gates: `config_valid` (name non-blank, duration positive — re-checked
   because `SoakConfig` fields are public), `store_dir_exists`,
   `device_resolved`, `store_dir_writable`. Preflight never creates the
   store directory: creating a store is not a check's side effect.
3. Device resolution reuses EVI-3's disk probe (`report::disk_for`, factored
   out of `work_disk` with identical behaviour) for the store directory.
   The resolved kind/filesystem/mount are the same values `RunMetadata`
   records as `work_disk_*`; they live only in the preflight report, never
   as a field on `SoakRun`, per §3's "do not add a second disk field".
4. No free-space threshold: preflight reports `available_bytes` and leaves
   the judgement to the operator, because a soak's footprint depends on
   SOA-4's workload, which does not exist yet. A threshold invented now
   would be a number with no grounding.
5. Secret redaction in soak outputs means whole-artifact redaction, not
   just the label accessor: `SoakRun::redacted_json` serializes a copy
   with the label redacted, and `SoakPreflight::redacted_json` redacts the
   store path and every check detail (paths can carry a home directory or
   a credential-like directory name). Both go through EVI-3's `redact`;
   no new secret list exists.
6. No schema bump: SOA-2 adds no field to `SoakRun` or `SoakConfig`, so
   `SOAK_SCHEMA_VERSION` stays at v1 per SOA-1's bump rule.

**Rejected:** a second disk/driver field on `SoakRun`; a preflight that
creates the store directory; a free-space pass/fail threshold; and a new
redaction routine in the soak module (all contradict §3's ownership rules
or invent an ungrounded number).

## 2026-10-26 — EVI-8A "adapter v2" and its schema number (FLAGGED FOR MERGE REVIEW)

**Ambiguous:** the 1.0 plan (§6, W2) lists EVI-8A with no definition of "adapter v2"
beyond the name, and §3 says "EVI-3 therefore sets it to 23 and EVI-8A to 24. If
another bump lands first, each takes the next free number and records it in
schema.rs's version history." Live state contradicts the assumed numbering:
the report schema (`weft-bench/src/schema.rs`, `SCHEMA_VERSION`) is at v22,
while v23 landed on a *different* line — the manifest schema
(`MANIFEST_SCHEMA_VERSION` = 23 in `manifest.rs`, EVI-3's bump).

**Chosen (owner-authorized best guess, 2026-10-26 — Justin: "use your best guess
based on the roadmap and docs"; if merge review disagrees, this decision is
revisited there):**

1. "Adapter v2" is the second version of the bench adapter interface
   (`SystemAdapter`): v2 adds the capability/provenance reporting the v1 trait
   lacks (`AdapterInfo`: engine, method, `native_engine`,
   `honours_requested_spline`). v2 is additive — no v1 method changes — so v1
   adapters keep working unchanged.
2. Part I ports only `WeftAdapter` to v2 (`run_profile_v2` records its report
   in the result); `BaselineLinearAdapter` and `ForwardFillAdapter` stay on v1
   via `run_profile` and carry no `adapter_info`.
3. Schema: the REPORT schema bumps 22 → 23, recorded in `schema.rs`'s version
   history, as the new optional `BenchResult.adapter_info` field. The manifest
   line is untouched (still 23); the two lines are now numerically equal by
   coincidence and are bumped independently. §3's own "next free number" rule
   is what licenses 23 on the report line, not the plan's assumed 24.
4. Compare: no change needed — `compare` already refuses any cross-schema
   comparison (v22 vs v23 included) with a clear error, and same-version
   compare is unchanged (existing test
   `reports_of_different_schema_versions_are_refused`).
5. Part II (EVI-8B, W3) ports the remaining adapters to v2 and retires v1;
   explicitly out of EVI-8A.

**Rejected:** bumping the report schema to 24 to match the plan's literal
number (it would skip the report line's free 23 and break §3's numbering rule);
putting the v2 report on the manifest line; and forcing the baselines onto v2
in part I (that is EVI-8B's scope, and their deliberately-ignored spline is
exactly what v2's `honours_requested_spline: false` would report — a claim
this slice does not need to make).

## 2026-10-24 — merging while a check is red

**Ambiguous:** the standing instruction says never merge with a red or pending check; the operator then
said "if CI is broken, then skip it and merge anyway".

These two are in direct conflict, so the resolution is stated explicitly rather than left to judgement
at merge time.

**Chosen:** the newer instruction governs, but only for breakage the PR did not cause. The test applied
before any red merge, in order:

1. Is the same check red on `main` at the merge base, or does the job log show an infrastructure flake
   (runner death, network/registry timeout, no compile or assertion failure)?
2. If neither, the failure is mine to fix and the PR stays unmerged.

Concretely: fix anything my change caused; re-run once where the log shows a flake and say so in the PR;
merge red only when (1) holds, recording in the merge comment the check name, the run that proves it red
on `main`, and the commit. `--admin` is the tool that makes a red merge possible, so it is used only
under (1) and never for a check the branch itself failed.

**Rejected:** treating "skip CI" as licence to edit, disable, or `--admin`-past a check the branch broke,
because every check in this repo exists because of a specific bug and the roadmap's release criteria are
expressed in them.

**Applied so far:** no red merge has been made. #100 (tokio), #101, #102 and #98 were each merged only
with `MERGE_STATE=CLEAN`. The two red states encountered (cargo-deny on #98, `compat_v010` on #98/#103)
were both pre-existing-on-`main` failures under test (1); neither was merged over — #100 and #102 were
landed first so the checks went green honestly instead.

## 2026-10-24 — one writer per worktree

**Ambiguous:** several agent sessions share `/mnt/deepmem` and the same clone, and branch switching in
the shared worktree races them. A peer's `checkout` at 20:40 silently changed the tree I was editing and
its commit swept two of my files in.

**Chosen:** never branch-switch or leave uncommitted work in `/mnt/deepmem/Development/DSP`. All slice
work happens in a dedicated worktree:
`git worktree add -b <slice>/<slug> /mnt/deepmem/Development/weftdb-wt-<slice> main`.

Also learned: `git status` can report phantom modifications after a foreign checkout (stale stat cache) —
`git diff` re-hashes and is authoritative.

## 2026-10-24 — fixture sets are byte-frozen; notes live outside them

`scripts/compat/releases.py` (the `release pins` job) requires every file inside a set to appear in that
set's `MANIFEST.sha256` with matching bytes — including the set's own `README.md`. Documenting something
about `compat/fixtures/v0.1.0/` by editing its README therefore fails CI.

**Chosen:** `compat/fixtures/README.md` (one directory up, outside the hashed set) carries notes about
the sets, and the rule is stated there so the next person does not rediscover it.

## 2026-10-24 — staging in a worktree that carries local tooling

**Ambiguous:** slice worktrees accumulate local helper files under `.pi/` (gate scripts, their logs) and
`git add -A` is otherwise convenient. On the compat branch `git add -A` staged 9.2k lines of
`.pi/logs/*.log` plus a copied gate script into the commit before it was pushed.

**Chosen:** stage named paths, never `-A`, in slice worktrees; and when auditing what a branch changes
use `git --no-pager diff --no-ext-diff --color=never --stat origin/main...HEAD`, because this clone sets
`diff.colormoved plain`, which can hide changed lines from a plain `git diff`.

## 2026-10-24 — proving a fix that a working tree cannot show

**Ambiguous:** the compat fixture bug only appears in a checkout where git itself materialised the tree,
so "it passes locally" is worthless for it, and the instruction to run CI's gates locally could not be
satisfied as stated.

**Chosen:** run the gates in a fresh `git worktree` checked out from the branch — that reproduces the
runner condition exactly (zero `dictionaries` directories) — and pair it with a counterfactual: disable
the fix, re-run, and keep the reproduced failure message in the PR and commit message. The pair is what
makes "CI is the experiment" unnecessary as a justification.

## 2026-10-24 — slice specs are read from git history, because `docs/` is gone from `main`

**Ambiguous:** W1/W2 steps are defined by `docs/release/1.0-plan.md` §6 and the design docs. PR #99
(`4de2c69`) deleted all of `docs/`, and #103 trimmed the ticked items, so `main` now carries one ROADMAP
line per slice — e.g. `- [ ] **FRE-3** Golden frames, golden sidecars and persisted-encoding goldens
(gated codecs included)` — with no acceptance criteria attached.

**Chosen:** treat the last commit that held the prose (`f085f84:docs/release/1.0-plan.md`,
`f085f84:docs/design/*.md`) as the authoritative spec and read it from history with `git show`, per
slice, before implementing. Rejected: implementing from the one-line ROADMAP text alone, because these
slices carry gates expressed as acceptance text, and inventing the criteria would make the tick
meaningless. Also rejected: re-adding `docs/` to the repository, because removing it was another
session's deliberate decision and re-adding it would fight that.

**Consequence worth recording:** if the prose has a real home (basicautomation.io), the ROADMAP's
one-liners should link to it; a checklist whose criteria live nowhere reconstructable is how a slice gets
ticked for less than it means.

## 2026-10-25 — what the nightly benchmark gate compares against, and when a move is a regression

**Ambiguous:** EVI-5 asks for a "nightly benchmark-regression workflow and `weft-bench compare`" and says
nothing about either of the two decisions that make it work: which run is the baseline, and what makes a
number "regress". A gate can be built to satisfy the sentence in several incompatible ways, and each of
them fails differently.

**Chosen.** The baseline is the newest *successful* run of the gate itself, fetched from its artifact; a
night that regresses goes red and stops being a baseline. Comparing against "yesterday" instead lets the
truth drift one small step per night — such a gate measures the slope, and a store can lose a third of its
speed over a quarter without any single night crossing anything. The first night has no baseline: it
uploads its report, says so in the summary, and passes; the gate starts comparing on the second night.

A regression requires two independent things: the move exceeds `--threshold` (10% by default) **and** the
bootstrap confidence intervals of the two runs do not overlap. Beyond-threshold-but-overlapping is
`suspicious`: printed, in the verdict, not gating unless `--strict`. The threshold alone would page on
hosted-runner shape changes; the interval test alone would call a 2% move on a tiny workload a crisis.
A measure with no interval (throughput, storage size, ratio, count) can only ever be suspicious, and the
verdict names that ceiling instead of quietly treating it as evidence of nothing.

**Rejected:** a fixed nanosecond or MB/s budget, because `docs/design/benchmarks.md` "How to cite a
number" already establishes that hosted runners are not the same machine twice, and a budget written
today is wrong the first time the runner fleet moves; and putting this job inside `ci.yml`, because
`scripts/release/verify.sh` reads the latest completed `ci.yml` run as release evidence, so a red night
caused by a noisy neighbour would gain the power to veto a release.

## 2026-10-25 — what claims hygiene meant once the README's commands checked out

**Ambiguous:** EVI-1 asks for "claims hygiene" in a README whose commands, endpoints and flags
were already believed correct. The open question was what is actually left to reconcile, and
whether a figure with no committed artifact is a false claim or merely an unverifiable one.

**Chosen:** treat the code as ground truth and check every falsifiable claim directly rather than
trusting prose, then classify what fails into *false* (fix now) and *unverifiable from a clone*
(disclose now, make checkable in EVI-2). Verified this way, on `main` at 977b812:

- all 26 `weft-bench` flags used in the README are declared in `weft-bench/src/*.rs` string
  literals — no invented switch;
- every `/api/v1/...` path the README calls is registered in `weft-server` (including the
  `/api/v1/storage/{aspect}/...` family, which the README only shows through the `price` aspect);
- every relative markdown link resolves against the tree, and the two anchors my slug check
  flagged (`#physical-types--columnar-storage`, `#where-it-doesnt-pay-off`) are artefacts of
  approximating GitHub's slug rule for `→` and `'`, not broken links;
- **no measurement artifact of any kind is committed** — grepping the tree for the quoted figures
  (`218,963`, `118,238,781`, `4.22`, `56 µs`) returns nothing outside `README.md` itself.

The fourth point is the only place the README over-claimed: its Performance preamble said every
number is "named so you can re-run it", which is true of the *bench code* and false of the
*figure*, because the schema-v23 manifest and result JSON behind it live on one workstation. The
preamble now says which figures carry a `Source:` bench path, which are dated local measurements,
and that the per-figure registry is EVI-2's job before 1.0. The numbers themselves stay: they were
measured, they are dated, and deleting them would trade an honest caveat for a lost fact.

**Method note:** four read-only sub-agents were dispatched to audit README ranges in parallel and
every one came back dead — two on the 30-minute hard limit, two on the five-minute no-output
watchdog, having written nothing. The checks above were run in-session as targeted greps in four
calls and cost a fraction of that. For a bounded claim-vs-code audit of one known file, do it in
the parent; sub-agents earn their keep on open-ended search, not on a checklist.

## 2026-10-25 — the Linux release baseline is a container image, not the runner

PAC-2 ("Portable Linux binaries on a glibc 2.28 baseline") is a title and a checkbox: its
slice body was deleted from the roadmap in the upstream restructure, so what "portable" was
owed had to be chosen. Two questions, both decided here.

**How old is old enough.** Settled on glibc 2.28 — RHEL 8, Debian 10, the 2018/2019 enterprise
host — rather than the 2.17 of the CentOS 7 era or the 2.34 that an Ubuntu 24.04 link happens
to produce. 2.17 would have meant a CentOS 7 image that has been off security support for
years, and its toolchain is older than the workspace's MSRV; 2.34 would have changed nothing,
because that is what v0.1.0 already shipped and what the README had to apologize for.

**Where the change lands.** Inside the existing `build` job's two Linux legs, with the asset
names unchanged, rather than a second matrix producing `*-portable` archives. Two artifacts
claiming one target is how a release ends up publishing whichever the checksums job happened
to sort first, and the `dist` profile is already LTO over one codegen unit, so doubling the
Linux builds would have been the expensive way to be ambiguous. The legs now run in
`quay.io/pypa/manylinux_2_28_{x86_64,aarch64}:2026.10.25-1`, arch-matched to the runner so the
no-cross-toolchain rule of this job still holds.

**Rejected:** shipping a `-portable` asset set alongside the current one (above); and a
musl-static build, which would answer the question with a bigger question, since a static
binary cannot dlopen and the store is not the only thing a deployer might want to load.

**The floor is asserted from the artifact.** `scripts/check-glibc.sh` parses the dynamic
symbol table (the `.gnu.version_d` section header, walked byte by byte with a struct unpack,
because no binutils version in CI is guaranteed to print symbol versions) and fails above the
floor; it runs against the built binaries in the same leg. Measured here before pushing: an
x86_64 dev-profile build of `weft-tui` and `weft-server` in the pinned image yields 2.28 for
both, interpreter `/lib64/ld-linux-x86-64.so.2` — the dev profile is what a container build
reachable from this machine costs, and the CI assertion is what governs the shipped
dist-profile artifact.

## 2026-10-25 — slice implementation comes back to the main session; delegates audit only

Seven delegated runs in a row died on this machine: three at a 30-minute wall (two after
1.5M and 2.5M tokens, one having produced 41 tool calls of a ten-file slice) and four killed
by a five-minute no-output watchdog at zero tokens, including a run whose task text was
deliberately cut to a tenth of the size to stay inside that watchdog.

**Chosen:** the main session implements slices, and delegates are used only for read-only
audits, with an explicit scope cap and an instruction to print a line between tool calls so
the watchdog sees activity. The EVI-1 claim audit went that way and came back with nothing:
every number in the README's performance section resolves to a named benchmark, which is what
the roadmap asked for — a finding that took reading, not compute.

**Rejected:** shrinking implementation tasks to satisfy the no-output watchdog. A slice that
must compile a fourteen-crate workspace cannot stay silent for less than five minutes, so the
limit does not fit the work; shrinking the task to fit it would only produce a smaller task
that still cannot be done in the time allowed.
## 2026-10-25 — what `--scale` was allowed to become

**Ambiguous:** EVI-9 asks for "block missingness and scale presets" and names the ladder
`--scale small|medium|large|huge` "for every workload". Written literally that is a fifth rung
nobody can run and a claim about workloads that have no size knob, so both halves had to be
narrowed, and the narrowing is a decision rather than an implementation detail.

**Chosen — the ladder.** Four rungs: `tiny|small|medium|large`, as value budgets 65 536 /
524 288 / 4 194 304 / 16 777 216. `Small` is the rung a caller gets for the word "default": it is
the same budget the flags described before presets existed (16 series × 256 blocks × 128 rows),
though not the same shape — a preset derives its series count from the cap, so `--scale small` is
64 × 64 × 128. The budget is what a rung promises; the shape is in the profile name either way. `huge` was dropped rather than implemented: the next rung is 67 108 864 values,
about 537 MiB of column data before the store's own overhead, which is not a size a benchmark gets
re-run casually and is past the point where the number still means "the machine", as opposed to
the page cache. The ladder already spans three orders of magnitude, and `tiny` was added because
the thing a preset is for — "give me the same shape, smaller" — had no rung below the old default.

**Chosen — which workloads.** A preset expands into the *size* knobs of the workloads that generate
their own data (`--sparse`, `--gap-fill`, `--downsample`). The interpolation profiles keep their
own shapes: their point counts are the experiment, not a knob to resize, and silently scaling them
would compare two different experiments under one name. `README.md` and `ROADMAP.md` say this in
those terms instead of claiming "every workload".

**Chosen — a preset fills, never overrides.** A size knob left at its default takes the preset's
value; a knob the caller typed is kept even where the preset would have chosen differently. The
cost is stated in `weft-bench/src/scale.rs`: a knob typed at exactly its default value is
indistinguishable from one left alone, so it gets overwritten. The alternative — refusing
`--scale` alongside any size flag — would make the common case ("large, but with 64-row blocks")
impossible.

**Chosen — grid shape, and what the profile name says.** `--sparse` derives series as
`sqrt(values / rows)` clamped to 64 and gives the remainder to blocks, so a bigger preset deepens
the table past the cap instead of transposing it; 64 is where a "wide table" stops being
distinguishable from a tall one. The preset's name is written into the profile even when it filled
nothing (`sparse-3x5x8-d20-large`), because the profile's job is to say what was asked for; the
grid dimensions are in the same name, so nothing is hidden.

**Chosen — what the preset's name is good for.** The preset is appended to the profile name of
every workload it sizes (`gap-fill-tiny`, `downsample-medium`, `sparse-64x4096x64-d20-large`),
not just the new one: `compare` matches runs by profile name, so before this a gap-fill at 3 300
points and one at 486 660 were the same measurement to it, and the second read as a 100×
regression. A workload a preset does not size gets no suffix, since there is nothing there to
record.

**Rejected:** publishing a measured figure for observed missingness in `README.md`. The default
grid's holes are a property of a seed and a density, and the number anyone re-runs differs the
moment either moves; the report already prints the observed value, which is the artifact a claim
should point at.

## Needs human

- Anything that creates a git tag or GitHub release, or publishes to crates.io (PAC-19, FRE-18,
  W11/W13/W15 tag approvals, PAC-4 follow-ups): prepared, not executed.
- Steps the roadmap marks *(owner)*: private vulnerability reporting, prerelease deletion, the 10 Gbps
  port move, privileged power-loss validation, narrative approvals.
- `~/.cargo/config.toml` on this machine sets nightly-only `rustflags = ["-Z","threads=8"]`, so every
  `+stable` invocation needs `RUSTFLAGS=""`; a human should decide whether that stays.

## 2026-10-25 — what a claims registry has to key on to survive an edit

**Ambiguous:** EVI-2 asks for a "claims registry and markdown claim/link lint", and the README
quotes figures in prose, in tables, and again in a later architecture section. What does one entry
describe — a number, a sentence, or a measurement?

**Chosen:** one entry per **measurement paragraph**, keyed by a `<!-- claim:id -->` marker placed
beside the paragraph in the README, with the entry listing the figure *text* it vouches for. Keying
on a number fails the first time someone reformats (`56 µs` → `56us`) and silently passes when a
figure is deleted, because nothing forces the entry to be removed. Keying on a paragraph makes the
pairing bidirectional: the lint fails if a marker has no entry, if an entry has no marker, and if
the figure text no longer appears within 12 lines of its marker — so a reworded sentence must
update the registry, which is the whole point. `*Source: [...](path)` lines are cross-checked
against the registry so a new bench-cited paragraph cannot ship unregistered, and every relative
link in the README resolves against the tree.

**Rejected:** a Rust subcommand in `weft-bench` — the check reads markdown and JSON, needs no type
information, and runs on a runner that already has `python3`, so a subcommand would have added a
compile step to a documentation gate. And machine-checking GitHub heading anchors: EVI-1 already
found that approximating GitHub's slug rules for `→` and `'` produces false positives, so anchors
are out of scope and the script says so rather than quietly skipping them.

**Not claimed:** eleven figures are registered, not every number in the repository — the registry
covers the README's Performance section plus one dated observation in the backup-daemon note. The
lint's `*Source:*` rule is what pulls future figures in; it is a ratchet, not a completed census.
## 2026-10-25 — who clicks "merge", and what happens when the rules are silent
**Ambiguous:** `AGENTS.md` tells this project's agents to "work on branches and open pull
requests; never commit directly to main", and also never to stop and wait for approval. It says
nothing about who merges. Slices #105-#112 were squash-merged by the agent on green CI, which made
an agent-side convention look like an authorised workflow.
**Chosen:** stop at "PR open, all checks green", report that state, and leave the merge to a human.
Merging is the one step that is awkward to undo, so the absence of an instruction is read as the
absence of authority rather than as permission by omission; the "don't wait for approval" rule
covers *ambiguity about how to do the work*, not permission to take an irreversible action nobody
asked for. Open PRs are not parked work: the review surface exists, CI has run, and a human can
merge in one click.
**Rejected:** continuing the #105-#112 practice. Repetition inside one buildout is not authorisation,
and it quietly made the agent both author and approver of every change to `main`.
**Needs human:** confirm which of the two this project wants for the rest of the 1.0 buildout.
Current queue: #113 (EVI-9 `--sparse` missingness, `--scale` presets) and #114 (EVI-2 claims
registry + claim/link lint), both waiting only on CI and a merge decision.

## 2026-10-26 — what the artifact pipeline is, and what it publishes (EVI-4) — FOR MERGE REVIEW

**Ambiguous:** the 1.0 plan says only that D-S21's crash matrices are "artifacts
published via EVI-4" (§5) and that W2 runs "EVI-2 → EVI-4" (§6); it never says
what the pipeline publishes, where it lives, or how it connects to the claims
registry EVI-2 built. EVI-4 on its own is a one-liner with no acceptance shape.

**Owner best-guess decision (2026-10-26, Justin: "use your best guess based on
the roadmap and docs").** This slice implements that decision; if merge review
disagrees, the decision is revisited here, not by re-blocking silently:
1. Scope is generalizing EVI-5's nightly bench-report publication to the CI
   bench gate: bench jobs upload BenchReport + manifest + compare verdict as a
   `bench-report` Actions artifact, same shape as nightly, 90-day retention.
2. Claims wiring: each bench/claims.json entry gains an artifact reference
   (artifact name + publishing workflow); check_claims.py validates the
   reference shape and that the named workflow publishes it. verify.sh
   unchanged.
3. D-S21 crash matrices are a FUTURE consumer of this pipeline, not part of
   EVI-4 (D-S21 does not exist yet).
4. Release binaries out of scope (release.yml already publishes them).
5. The README "run artifacts are not committed" STANDS — artifacts live in
   Actions storage, never committed files.

**Chosen, and one correction to the decision's wording.** ci.yml has no
bench-producing job today, and EVI-5 deliberately kept the regression gate out
of ci.yml (a red night must not veto a release through verify.sh's latest-ci
check), so there is no "CI bench job" to generalize onto yet. What this slice
generalizes is the publication itself: `scripts/bench/stage-artifacts.sh` is the
pipeline — one flat staging directory, report + manifest + verdict, artifact
name `bench-report`, 90-day retention — and bench-regression.yml now stages
through it instead of an inline loop. Any future bench-producing job (D-S21's
crash matrices, EVI-6's ingest workload reports) publishes by calling the same
script and uploading the same artifact name; the contract is in the script's
header. Two consequences fall out of the refactor: staging now runs on every
path, including a first run with no baseline — the old inline loop sat after
the no-baseline early exit, so a first run staged nothing and the upload
failed on an empty directory — and the script refuses to stage when the report
itself is missing, so an empty artifact can never become a future baseline.

**Claims wiring, honestly null.** No figure the README quotes is currently
produced by a pipeline artifact: the nightly gate publishes the
nightly-interpolation workload, which no registry entry quotes, and the
reproducible entries quote benches the nightly job does not run. So every entry
gains the field as null rather than inventing provenance, and the lint now
fails on a missing field, a malformed reference, or a reference to a workflow
that does not upload an artifact of that name. A future entry that does claim
a pipeline artifact is machine-checked against the publishing workflow.

**Rejected:** a parallel artifact store or manifest (the registry's artifact
field points at the workflow that already publishes, it does not duplicate
it); committing run artifacts to the tree (the README's disclosure stands);
adding a bench job to ci.yml (no bench gate exists there, and a gating bench
job would re-create the release veto EVI-5 rejected — a non-gating upload-only
job is a later slice's call, and it would stage through this same script).

**Not claimed:** the nightly workflow change itself only exercises on its next
scheduled run; the shape is unchanged (same artifact name, same flat files,
same retention), so the next night's baseline download is unaffected.
