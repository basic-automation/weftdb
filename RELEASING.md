# Releasing WeftDB

Two artifacts come out of a release: the library crates on crates.io, and the
cross-platform binaries on the GitHub release. They are separate steps on purpose.

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
same file, so the compiler that builds a release is the one CI tested. Move it in a PR of
its own. The MSRV job keeps testing the oldest supported compiler (1.95) separately.

## 2. Publish to crates.io

Order matters: crates.io resolves path dependencies by version, so nothing can be
published before its dependencies are on the registry. `scripts/publish.sh` encodes
the order.

```bash
./scripts/publish.sh              # dry run, uploads nothing
./scripts/publish.sh --execute    # publishes, in dependency order
```

A dry run cannot see crates published earlier in the same run, so for a *first* release
every crate with an internal dependency will report an unresolved dependency. That is
expected; the `--execute` path resolves each one as it goes.

Alternatively run the **Publish to crates.io** workflow from the Actions tab. It is
manual-only. A dry run (the default) runs `publish.sh` and uses no environment. To
publish, turn `dry_run` off and give the release tag. That job checks out the tag rather
than the branch the workflow was dispatched from, proves it with
`git describe --exact-match`, and runs in the `crates-io` environment, which holds the
`CARGO_REGISTRY_TOKEN` secret and can require a reviewer. Create that environment, with
its protection rules, before the first real publish: GitHub creates a missing
environment, unprotected, the first time a job uses it.

**Publishing is irreversible.** A bad version can be yanked but never replaced, and the
version number can never be reused. Do the dry run.

## 3. Tag, and ship the binaries

`.github/workflows/release.yml` builds `weft-server`, `weft-tui` and `weft-bench`
natively on five targets — no cross toolchains — with the compiler from
`.github/release-toolchain`:

| Target | Runner |
|--------|--------|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `x86_64-apple-darwin` | `macos-15-intel` |
| `aarch64-apple-darwin` | `macos-latest` |
| `x86_64-pc-windows-msvc` | `windows-latest` |

Intel macOS used to build on `macos-13`, which GitHub has retired; `macos-15-intel` is
its hosted Intel replacement.

Each binary is built by its own `cargo build --locked`, so no package's features leak
into another's binary, and nothing is restored from a build cache. Each target is packed as a
`.tar.gz` (a `.zip` on Windows) with a `.sha256` next to it, and a last job checks every
archive and writes one `SHA256SUMS` over all five.

### Dry run first

Dispatching the workflow is a dry run unless you say otherwise:

```bash
gh workflow run release.yml --ref main                   # build main's head
gh workflow run release.yml --ref main -f ref=<commit>   # build another commit
gh workflow run release.yml --ref <branch> -f fast=true  # dev profile, to iterate on staging
```

A dry run checks only that the commit is on `main` (or a `release/*` branch) and that
CI passed for it. It then builds all five targets and uploads the archives and
`SHA256SUMS` as workflow artifacts. It creates no tag and no release. To check them:

```bash
gh run download <run-id> --dir dist --pattern 'weftdb-*'
gh run download <run-id> --dir dist --name SHA256SUMS
(cd dist && mv weftdb-*/* . && sha256sum -c SHA256SUMS)
```

`fast=true` builds with the dev profile. Use it to test staging changes quickly, never to
judge a release build.

The workflow file and `scripts/release/` come from the branch you dispatch from; the code
built comes from `ref`. A dry run of an unmerged branch's own head fails the ancestry
check, so to try a change to the release pipeline before it merges, dispatch from your
branch with `-f ref=main`.

### Release

Tag the commit on `main` whose CI is green, and push the tag:

```bash
git tag -a v0.1.0 -m "WeftDB v0.1.0"
git push origin v0.1.0
```

The push starts the workflow. Its `verify` job (`scripts/release/verify.sh`) runs before
anything is built, and fails the run unless:

- the tag is exactly `vX.Y.Z` or `vX.Y.Z-rc.N`;
- the tag already exists (the workflow never creates one);
- the tag is `v` followed by `weft-server`'s version at that commit;
- the commit is on `origin/main`, or on an `origin/release/*` branch for a patch release;
- the latest completed CI run for that commit passed;
- `CHANGELOG.md` has a non-empty section for the version, which becomes the release notes.

Every later job builds or releases exactly the commit `verify` resolved, and checks that
it did. Only the last job can write to the repository, and it checks again that the tag
still points at that commit.

The release is created as a **draft**, with the five archives, their `.sha256` files and
`SHA256SUMS`. A `-rc.N` tag makes a prerelease, which never becomes the Latest release.
Review the notes and the archives, then publish it. The workflow refuses to change a
release that is already published.

To run it again against an existing tag, re-run the failed jobs, or dispatch it with the
dry run off:

```bash
gh workflow run release.yml --ref v0.1.0 -f dry_run=false -f tag=v0.1.0
```

`scripts/release/test-verify.sh` tests `verify.sh` against a throwaway repository, and
CI runs it with actionlint and shellcheck.

## Which crates are published

Published: `weft-physical-type`, `weft-reduce`, `weft-line-protocol`,
`weft-arrow`, `weftdb`, `weft-arrow-store`, `weft-orchestration`.

`splimes` is released from its own repository
([basic-automation/splimes](https://github.com/basic-automation/splimes)) and consumed
here from crates.io. A WeftDB release that needs a splimes change waits for that
splimes release.

Not published (`publish = false`): `weft-server`, `weft-tui`, `weft-bench`. They are
binaries and ship as release artifacts instead.
