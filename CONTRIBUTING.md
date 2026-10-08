# Contributing to WeftDB

Thanks for taking a look. WeftDB is pre-beta, so the most useful contributions right
now are bug reports with a reproducer, and benchmark results from hardware we don't have.

## Getting set up

```bash
cargo build --workspace
SKIP_SLOW_TESTS=1 cargo test --workspace -- --test-threads=4
```

Cap `--test-threads` if your machine is memory-constrained: several suites stand up
their own segment stores, and at full parallelism the OOM killer takes a test binary
out, which shows up as a `SIGKILL` rather than a test failure.

You need **Rust 1.95 or newer** on stable. The one exception is formatting:
`rustfmt.toml` uses nightly-only options, so run

```bash
cargo +nightly fmt --all
```

A GPU is optional. The engine (splimes 1.0) runs on one CPU thread or the rayon pool,
by grid size, and uses the GPU only after `splimes::calibrate()` has started it and
measured where it is faster (`weft-server` calibrates at startup; the tests don't).
Without a `wgpu` backend everything runs on the CPU, and the test suite is expected to
pass either way — if a test only passes with a GPU, that's a bug in the test.

## What CI checks

| Job | Command |
|-----|---------|
| `fmt` | `cargo +nightly fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D clippy::correctness -D clippy::suspicious -D clippy::perf` |
| `msrv` | `cargo +1.95.0 check --workspace --all-targets` |
| `test` | `cargo test --workspace -- --test-threads=4` on Linux, macOS and Windows |
| `docs` | `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS=-D warnings` |
| `package` | `cargo package --workspace --no-verify` |
| `contrib` | `python3 -m unittest discover -s scripts/contrib -p 'test_*.py'` (tests for the contribution-terms checker) |
| `contribution-terms` | a DCO sign-off on every commit, or a signed CLA ([below](#contribution-terms)); a separate workflow, `contribution-terms.yml` |

Two notes on the gates:

- **rustc warnings are denied** (`RUSTFLAGS: -D warnings`). Keep the build clean.
- **clippy is gated on `correctness`, `suspicious` and `perf` only.** The `style`,
  `pedantic` and `nursery` categories have a large pre-existing backlog in this
  workspace; they are worth fixing, but they are not a merge gate yet. If you clean
  some up, do it in its own commit so the diff stays reviewable.

Bumping the MSRV means editing `rust-version` in the workspace `Cargo.toml` and the
`msrv` job — and saying so in `CHANGELOG.md`, because it is a breaking change for
downstream users.

## Design constraints

[`ROADMAP.md`](ROADMAP.md) is the single source of truth for the work queue and the
hard constraints. Two of those constraints shape most reviews:

- **No vendor connectors in the core.** Formats (Line Protocol, Arrow, Parquet) are
  fine; clients, SDKs and network calls to a vendor are not. That's why
  `weft-line-protocol` parses ILP but has no InfluxDB client.
- **The heavy dependency trees stay out of the hot path.** `arrow-*` lives in
  `weft-arrow`, and `weft-arrow-store` is the only crate allowed to depend on both
  `weftdb` and `weft-arrow`.

## Contribution terms

WeftDB is licensed under either of the Apache License 2.0 or the MIT license, at your
option. Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed
as above (`MIT OR Apache-2.0`), without any additional terms or conditions.

Every pull request also takes one of two routes. Pick whichever suits you; one is enough.

### Route 1: sign off every commit (DCO)

Commit with `git commit -s`, which adds a line like this to the message:

```text
Signed-off-by: Your Name <you@example.com>
```

By signing off you certify the [Developer Certificate of Origin 1.1](https://developercertificate.org):
that you wrote the change or otherwise have the right to submit it under the project's
open-source license (for example because it builds on suitably licensed earlier work, or
came to you from someone who certified the same), and that you understand the contribution
and the record of it, including the name and email in your sign-off, are public and kept
indefinitely. The full text is short; read it before you sign off for the first time.

The email in the sign-off must be the commit's author email (`git config user.email`);
case does not matter. Merge commits need no sign-off.

### Route 2: sign the CLA once

Sign the [WeftDB Individual Contributor License Agreement](CLA.md) by posting this comment
on your pull request:

```text
I have read the CLA Document and I hereby sign the CLA
```

The CLA is the Apache Software Foundation's individual CLA with Justin Icenhour as the
recipient. You grant him, and everyone who receives WeftDB from him, a copyright license
(including the right to sublicense) and a patent license for your contributions, and you
confirm that you have the right to make them. You sign once, and it covers your later pull
requests too. The signature is recorded against your GitHub account, so your commits must
be linked to it: add their author email to your GitHub account.

### How the check decides

The `contribution-terms` check passes when every commit is signed off by its author, and
otherwise when every commit author has signed the CLA. If some commits are signed off and
others come from CLA signers without a sign-off, neither route holds; sign off the rest.
Comment `recheck` on the pull request to run the check again.

### Fixing a missing sign-off

```bash
# Only the last commit:
git commit --amend --signoff --no-edit

# Every commit on your branch (use upstream/main if you work from a fork):
git rebase --signoff main
git push --force-with-lease
```

If the author email is wrong as well, set `git config user.email` first, then rewrite the
author and the sign-off together:
`git rebase --exec 'git commit --amend --no-edit --reset-author --signoff' main`.

### For maintainers

- Branch protection requires the **`contribution-terms`** job of
  `.github/workflows/contribution-terms.yml`, and only that job. `DCO sign-off` and `CLA`
  feed it; `CLA` is skipped whenever the DCO route holds.
- Before requiring it, create the `cla-signatures` branch holding
  `signatures/cla/v1.json` (the workflow's header has the commands) and leave the branch
  unprotected, since the workflow commits signatures to it.
- `physics515`, `dependabot[bot]` and `github-actions[bot]` are allowlisted: their pull
  requests pass without either route, and commits they author need neither. Whoever opens a
  pull request answers for the terms of any commits in it that are not their own.

## Pull requests

- Sign off your commits or sign the CLA; see [Contribution terms](#contribution-terms).
- One logical change per commit; keep mechanical churn (renames, reformatting) in
  commits of its own.
- Add a `CHANGELOG.md` entry under `## [Unreleased]` for anything user-visible.
- Performance claims need a benchmark in the repo that produces the number. This is a
  project rule, not a formality: the README links every figure to its harness.

## Releasing

See [`RELEASING.md`](RELEASING.md).
