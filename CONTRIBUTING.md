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

A GPU is optional. Without a `wgpu` backend the engine falls back to SIMD/parallel CPU,
and the test suite is expected to pass either way — if a test only passes with a GPU,
that's a bug in the test.

## What CI checks

| Job | Command |
|-----|---------|
| `fmt` | `cargo +nightly fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D clippy::correctness -D clippy::suspicious -D clippy::perf` |
| `msrv` | `cargo +1.95.0 check --workspace --all-targets` |
| `test` | `cargo test --workspace -- --test-threads=4` on Linux, macOS and Windows |
| `docs` | `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS=-D warnings` |
| `package` | `cargo package --workspace --no-verify` |

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

## Pull requests

- One logical change per commit; keep mechanical churn (renames, reformatting) in
  commits of its own.
- Add a `CHANGELOG.md` entry under `## [Unreleased]` for anything user-visible.
- Performance claims need a benchmark in the repo that produces the number. This is a
  project rule, not a formality: the README links every figure to its harness.

## Releasing

See [`RELEASING.md`](RELEASING.md).
