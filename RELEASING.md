# Releasing WeftDB

Two artifacts come out of a release: the library crates on crates.io, and the
cross-platform binaries on the GitHub release. They are separate steps on purpose. In
order: prepare, dry-run the binaries, tag (which drafts the GitHub release), publish
the crates, then publish the GitHub release.

## 0. Before the first release

The release gates trust some repository settings that no file here can enforce. The
repository owner sets these up once, and checks they still hold before each release:

- **A branch ruleset on `main` and the maintenance branches `release/[0-9]*`**: changes
  only through pull requests, no force pushes, no deletion. `scripts/release/verify.sh`
  accepts any commit on `main` or on a `release/<major>.<minor>` branch, so whoever can
  push to them can make a commit releasable. Other branches never count, even one
  named `release/<something>`.
- **A tag ruleset on `refs/tags/v*`**: only maintainers may create, update or delete
  release tags. The workflow builds the commit a tag points at, and publishing a draft
  release attaches it to wherever the tag points at that moment.
- **The `crates-io` environment**: required reviewers, deployments limited to `v*` tags,
  and the `CARGO_REGISTRY_TOKEN` secret. Create it before the first dispatch of the
  **Publish to crates.io** workflow, even a dry run: GitHub creates a missing
  environment, unprotected, the first time a job uses it, and it does not document
  whether a job its `if:` skips counts.

## 1. Prepare

```bash
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets -- -D clippy::correctness -D clippy::suspicious -D clippy::perf
SKIP_SLOW_TESTS=1 cargo test --workspace -- --test-threads=4
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo package --workspace --no-verify
```

Then:

1. Bump `version` in `[workspace.package]` of the root `Cargo.toml`. Every crate
   inherits it with `version.workspace = true`, so they release in lockstep.
2. Bump the `version = "x.y.z"` on the internal `[workspace.dependencies]` path
   entries in the same file — they carry an explicit version for crates.io.
3. Move the `## [Unreleased]` items in `CHANGELOG.md` under a new `## [x.y.z] - DATE`
   heading, and update the link definitions at the bottom. The release workflow reads
   this section verbatim for the GitHub release notes and **fails if it is missing**.
4. Commit, open a PR, merge once CI is green.

The release compiler is pinned in `.github/release-toolchain`. CI's stable jobs read the
same file, so the compiler that builds a release is the one CI tested (the build is not
the same: CI unifies features across the workspace and uses the dev profile). Move it in
a PR of its own. The MSRV job keeps testing the oldest supported compiler (1.95)
separately.

## 2. Dry run the binaries

`.github/workflows/release.yml` builds `weft-server`, `weft-tui` and `weft-bench`
natively on five targets — no cross toolchains — with the compiler from
`.github/release-toolchain`:

| Target | Runner |
|--------|--------|
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `x86_64-apple-darwin` | `macos-15-intel` |
| `aarch64-apple-darwin` | `macos-15` |
| `x86_64-pc-windows-msvc` | `windows-2025` |

The images are pinned rather than `*-latest`, so a release is reproducible and the glibc
floor the README states (from Ubuntu 24.04: 2.39 for `weft-tui`, 2.34 for `weft-server`
and `weft-bench`) cannot rise silently when GitHub moves `ubuntu-latest`. Moving an
image is a deliberate change: update this table and the README with it. Intel macOS
used to build on `macos-13`, which GitHub has retired; `macos-15-intel` is its hosted
Intel replacement.

Each binary is built by its own `cargo build --locked --profile dist`, so no package's
features leak into another's binary, and nothing is restored from a build cache. The
`dist` profile, in the root `Cargo.toml`, is the `release` profile (fat LTO, one codegen
unit) without debug info and with symbols stripped. `release` keeps full debug info for
profiling, which made the Linux `weft-server` about 400 MB. A stripped binary's panic
backtrace names no functions: to investigate a crash, reproduce it on a
`cargo build --release` of the same commit. Each leg lists the binaries' sizes, runs
`weft-bench --help` from what it built, then packs the target as a `.tar.gz` (a `.zip`
on Windows) with a `.sha256` next to it, and a last job checks every archive and writes
one `SHA256SUMS` over all five.

Every archive holds the three binaries, `README.md`, both license files (`LICENSE-MIT`,
`LICENSE-APACHE`), `NOTICE`, and the `THIRD-PARTY-NOTICES` of `weft-physical-type` and
`weft-reduce` (as `THIRD-PARTY-NOTICES-weft-physical-type` and
`THIRD-PARTY-NOTICES-weft-reduce`), whose Apache-2.0 portions are compiled into the
binaries. `scripts/release/stage.sh` lists them.

Dispatching the workflow is a dry run unless you say otherwise:

```bash
gh workflow run release.yml --ref main                   # build main's head
gh workflow run release.yml --ref main -f ref=<commit>   # build another commit
gh workflow run release.yml --ref <branch> -f fast=true  # dev profile, to iterate on staging
```

A dry run checks only that the commit is on `main` (or a `release/<major>.<minor>`
maintenance branch of its version's series) and that CI passed for it. Only a CI run that tested the commit itself counts: a push,
dispatched or scheduled run of `ci.yml` in this repository. A pull request's run tested
the pull request merged into its base branch, not the commit, so it does not count. CI
runs on every push to `main` and `release/**`; for any other commit, dispatch it with
`gh workflow run ci.yml --ref <branch>`.

The dry run then builds all five targets and uploads the archives and `SHA256SUMS` as
workflow artifacts. It creates no tag and no release. Its archives are named
`weftdb-v<version>-dryrun-<commit>-<target>` (with `-dev` after the commit for a
`fast=true` build), so they can never pass for a release's. To check them:

```bash
gh run download <run-id> --dir dist --pattern 'weftdb-*'
gh run download <run-id> --dir dist --name SHA256SUMS
(cd dist && mv weftdb-*/* . && sha256sum -c SHA256SUMS)
```

A dry run builds the same `dist` profile as a release. `fast=true` builds with the dev
profile instead. Use it to test staging changes quickly, never to judge a release build.

The workflow file and `scripts/release/` come from the branch you dispatch from; the code
built comes from `ref`. A dry run of an unmerged branch's own head fails the ancestry
check, whatever the branch is called. To try a change to the release pipeline before it
merges, dispatch from your branch with `-f ref=main`.

## 3. Tag, and draft the GitHub release

Tag the commit on `main` whose CI is green, and push the tag:

```bash
git tag -a v0.1.0 -m "WeftDB v0.1.0"
git push origin v0.1.0
```

The push starts the workflow. Its `verify` job (`scripts/release/verify.sh`) runs before
anything is built, and fails the run unless:

- the run is on the tag itself, so this workflow and its scripts are the tagged
  commit's (a tag push is; a dispatch must use `--ref <tag>`);
- the tag is exactly `vX.Y.Z` or `vX.Y.Z-rc.N`;
- the tag already exists (the workflow never creates one);
- the tag is `v` followed by `weft-server`'s version at that commit;
- the commit is on `origin/main`, or, for a patch release, on the maintenance branch
  `origin/release/X.Y` of its own series (`v1.2.5` on `release/1.2`); no other branch
  counts;
- the latest completed CI run for that commit passed, counting push, dispatched and
  scheduled runs in this repository, as for a dry run;
- `CHANGELOG.md` has a non-empty section for the version, which becomes the release notes.

Every later job builds or releases exactly the commit `verify` resolved, with that
commit's scripts, and checks that it did. Only the last job can write to the
repository. Before it writes, it checks again that the tag still points at that commit,
that only the tag's own archives are about to be attached, that no release for the tag
is published already, and that any earlier draft for the tag has no asset this run
would not replace. It adds a last line to the notes naming the commit the archives were
built from.

The release is created as a **draft**, with the five archives, their `.sha256` files and
`SHA256SUMS`. A `-rc.N` tag makes a prerelease, which never becomes the Latest release.
Leave it as a draft until the crates are published.

## 4. Publish to crates.io

v0.1.0 is a binaries-only baseline release and publishes no crates: skip this section
for it and go on to section 5.

Order matters: crates.io resolves path dependencies by version, so nothing can be
published before its dependencies are on the registry. `scripts/publish.sh` encodes
the order.

```bash
./scripts/publish.sh              # dry run, uploads nothing
./scripts/publish.sh --execute    # publishes, in dependency order
```

Run `--execute` from a checkout of the release tag. A dry run cannot see crates
published earlier in the same run, so for a *first* release every crate with an internal
dependency will report an unresolved dependency. That is expected; the `--execute` path
resolves each one as it goes.

Alternatively run the **Publish to crates.io** workflow (manual only; see section 0 for
the environment it needs). A dry run (the default) runs `publish.sh` and uses no
environment. To publish, dispatch it from the tag with the dry run off:

```bash
gh workflow run publish.yml --ref v0.1.0 -f dry_run=false -f tag=v0.1.0
```

That job refuses to run from anywhere but the tag, checks out the tag and proves it did,
runs `verify.sh` on it (the same gate as the release, CI included), and only then
publishes, in the `crates-io` environment.

**Publishing is irreversible.** A bad version can be yanked but never replaced, and the
version number can never be reused. Do the dry run.

## 5. Publish the GitHub release

Review the draft's notes and attached files. Its last line names the commit the archives
were built from. Publishing attaches the release to wherever the tag points at that
moment, so first check that the tag still points at that commit:

```bash
git ls-remote origin 'refs/tags/v0.1.0^{}'   # the commit an annotated tag points at
```

If it prints a different commit, do not publish: find out why the tag moved. Otherwise
press Publish. The workflow refuses to change a release that is already published.

## Running it again

To run the release again for an existing tag, re-run the failed jobs, or dispatch it
from the tag with the dry run off:

```bash
gh workflow run release.yml --ref v0.1.0 -f dry_run=false -f tag=v0.1.0
```

A dispatch from any other ref fails in `verify`. If the draft left by an earlier run
carries an asset this run would not replace, the run stops: delete that asset, or the
draft, and run it again.

`scripts/release/test-verify.sh` and `scripts/release/test-stage.sh` test `verify.sh`
and `stage.sh` against a throwaway repository; CI runs them, with actionlint and
shellcheck.

## Which crates are published

Published: `weft-physical-type`, `weft-reduce`, `weft-line-protocol`,
`weft-arrow`, `weftdb`, `weft-arrow-store`, `weft-orchestration`.

`splimes` is released from its own repository
([basic-automation/splimes](https://github.com/basic-automation/splimes)) and consumed
here from crates.io. A WeftDB release that needs a splimes change waits for that
splimes release.

Not published (`publish = false`): `weft-server`, `weft-tui`, `weft-bench`. They are
binaries and ship as release artifacts instead.
