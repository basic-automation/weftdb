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
manual-only and requires the `crates-io` environment with a `CARGO_REGISTRY_TOKEN`
secret, so it can be put behind a required reviewer.

**Publishing is irreversible.** A bad version can be yanked but never replaced, and the
version number can never be reused. Do the dry run.

## 3. Tag, and ship the binaries

```bash
git tag -a v0.1.0 -m "WeftDB v0.1.0"
git push origin v0.1.0
```

The tag triggers `.github/workflows/release.yml`, which builds `weft-server`,
`weft-tui` and `weft-bench` natively on five targets — no cross toolchains:

| Target | Runner |
|--------|--------|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `x86_64-apple-darwin` | `macos-13` |
| `aarch64-apple-darwin` | `macos-latest` |
| `x86_64-pc-windows-msvc` | `windows-latest` |

Each is uploaded as a `.tar.gz` (or `.zip` on Windows) with a `.sha256` next to it.

The release is created as a **draft**: review the notes and the attached archives, then
publish it. If the workflow needs re-running against an existing tag, dispatch it
manually with the tag name as input.

## Which crates are published

Published: `splimes`, `weft-physical-type`, `weft-reduce`, `weft-line-protocol`,
`weft-arrow`, `weftdb`, `weft-arrow-store`, `weft-orchestration`.

Not published (`publish = false`): `weft-server`, `weft-tui`, `weft-bench`. They are
binaries and ship as release artifacts instead.
