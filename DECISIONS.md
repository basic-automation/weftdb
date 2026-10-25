# Decisions

Judgement calls made while working the 1.0 buildout, where the instruction set was silent or
self-conflicting and the choice is not visible in the diff. Recorded so the next session does not
re-litigate them, and so a reader of a ticked roadmap item can see what it was decided to mean.

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

## Needs human

- Anything that creates a git tag or GitHub release, or publishes to crates.io (PAC-19, FRE-18,
  W11/W13/W15 tag approvals, PAC-4 follow-ups): prepared, not executed.
- Steps the roadmap marks *(owner)*: private vulnerability reporting, prerelease deletion, the 10 Gbps
  port move, privileged power-loss validation, narrative approvals.
- `~/.cargo/config.toml` on this machine sets nightly-only `rustflags = ["-Z","threads=8"]`, so every
  `+stable` invocation needs `RUSTFLAGS=""`; a human should decide whether that stays.
