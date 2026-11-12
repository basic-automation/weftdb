# Stability and versioning policy

**FRE-15** · Part of the 1.0 release plan (roadmap W10). This document states what
WeftDB's users may rely on, what is experimental, and what is not yet guaranteed. Every
promise below is grounded in something that exists in the repository — a file, a test, a
CI job, a crate version, or a committed fixture set. Where the repo cannot yet support a
promise, that is named as a gap.

---

## Semver policy

This workspace follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, **minor version bumps may contain breaking changes**, as
stated in the preamble of [`CHANGELOG.md`](../CHANGELOG.md). A caller that pins to a
specific 0.x minor version will receive only patch releases within it; to track the
latest development, depend on `0.y` and read the changelog before each upgrade.

### What a change to the wire format counts as

The wire format (HTTP API) is the set of routes, request bodies, query parameters, and
response schemas documented in the [committed OpenAPI
spec](../compat/openapi/weftdb.json), which is [generated from `weft-server`'s real
`RouteTable` and drift-checked in CI](../weft-server/src/openapi.rs). Any change that would
require a client to modify a request or reinterpret a response is a **breaking change**
and requires a major (or, pre-1.0, a minor) version bump:

- Adding a new route is additive (non-breaking) as long as existing routes are unchanged.
- Adding an optional field to a request body or response object is non-breaking.
- Adding a new query parameter that is silently ignored when absent is non-breaking.
- Removing or renaming a route, changing a required field to optional, tightening or
  loosening a type constraint, or adding a required query parameter is breaking.
- Changing the semantics of an existing response field — including making a field that
  was always present become nullable for part of the response — is **wire-breaking**:
  a client that assumed `total` was always a number [must now handle `null`](#wire-compatibility-example).
- Adding a non-trivial default to a query parameter that previously had no default, if
  that default changes the behaviour of a request that omitted it, is breaking.

### What clients may assume

A client that pins to a specific minor version of `weft-server` (the release binary) may
assume that every route in that release's OpenAPI spec works as documented. The Rust SDK
([`weftdb-client`](../weft-client)) is versioned independently (currently `0.1.0`) and
maps one-to-one with the HTTP API; a client upgrade may lag the server by one minor
version, but the SDK's own changelog names any compatibility shifts.

---

## Surface stability table

### Query/ingest wire API

**Status: stable (pre-1.0).** The HTTP routes under `/api/` are the project's primary
interface. They are documented in the committed OpenAPI spec, which is generated from the
real route table and drift-checked in CI. The release-criteria line reads:

> **Release engineering · Public surface declared and frozen**
> *ROADMAP.md, "1.0 release criteria": a living check for the 1.0 freeze.*

The surface is declared (the OpenAPI spec covers every route) but **not yet frozen**: a
breaking change between pre-1.0 releases is permitted with a minor version bump. The
freeze is the 1.0 milestone — before it, treat additions as safe and removals or
reinterpretations as announced in the changelog.

**Basis:** [`compat/openapi/weftdb.json`](../compat/openapi/weftdb.json) and the drift
test [`generated_spec_matches_committed_file`](../weft-server/src/openapi.rs), which
the CI `test` job runs on every push and pull request (`cargo test --workspace`).

### `weftdb-client` (the Rust SDK, crate directory `weft-client/`)

**Status: stable (pre-1.0).** Every method matches one HTTP route. Breaking changes to
the Rust API (function signatures, error types, feature flags, MSRV) are announced in the
changelog and carried in a minor version bump. `f64` is statically refused as a stored
value (it does not implement `StoredValue`), and `BigDecimal` is the single lossless
carrier — both are compile-time guarantees that cannot drift.

**Basis:** [`weft-client/Cargo.toml`](../weft-client/Cargo.toml) (crate version 0.1.0,
API surface in `src/lib.rs`), [`weft-client/tests/fake.rs`](../weft-client/tests/fake.rs)
(client behaviour against a fake server), and the `compat` [release
pins](../compat/releases.toml) that name which server release the client targets.

### Storage format

**Status: stable (layout 2).** The on-disk format is versioned in the `STORE_FORMAT`
marker at the store root. A write floor (`min_write_layout`) protects the store from
being opened by a WeftDB that would break it. A read floor (`min_read_layout`) records
the oldest layout a future binary must know to read it. The current supported layout is
**2** ([`SUPPORTED_LAYOUT`](../weftdb/src/types/store_format.rs)); the legacy layout is
**1** ([`LEGACY_LAYOUT`](../weftdb/src/types/store_format.rs)), readable and migrated on
first open.

> **Durability & correctness · 7.4 Documented RPO/RTO and Upgrade/rollback procedure,
> exercised against the previous release**
> *ROADMAP.md, "1.0 release criteria": the ongoing work that makes the storage format a
> reliable promise for 1.0.*

The upgrade from layout 1 to layout 2 is tested by
[`tests/compat_v010.rs`](../weftdb/tests/compat_v010.rs) (opens a v0.1.0 fixture, migrates it,
and checks values survive). The `upgrade` leg in
[`compat/releases.toml`](../compat/releases.toml) names v0.1.0 as the previous release
for upgrade testing.

**Not yet guaranteed:**
- **Rollback after writes** (PAC-14a, roadmap W9). A store that has been written by a
  newer release cannot yet be opened by an older one. Rollback is unsupported for 1.0:
  a store written by the current layout is not readable by older binaries.
- **Layout-2 baseline fixtures** (FRE-18, roadmap W13). The freeze cut that writes the
  layout-2 fixture set that 1.0 and later upgrades run against is still in the future.

### CLI (`weft-server`)

**Status: experimental.** `weft-server` is a binary crate (`publish = false`) whose CLI
subcommands (serve, openapi, import-rows-mode, backup, etc.) are documented in the binary
but not versioned or compatibility-tested. Flags, subcommand names, and exit codes may
change between minor versions without notice.

**Basis:** [`weft-server/src/main.rs`](../weft-server/src/main.rs) dispatches the CLI.
No CLI-specific compatibility test or schema exists.

### Configuration file and environment variables

**Status: experimental.** Every `WEFT_*` environment variable is documented in
[`docs/reference.md`](reference.md#configuration) and
[`docs/config-schema.md`](../docs/config-schema.md). The document now carries a
`schema_version` marker and `config/schema.toml` holds a machine-readable copy;
`scripts/config/check_schema.py` validates that both agree and that every registered
variable is documented. A variable name, type, accepted range, or default may still change
in a minor release.

**Not yet guaranteed:** A formal configuration schema (JSON Schema, CUE, or similar) that
validates value shapes (types, ranges, accepted-patterns) across releases, rather than
only name coverage and version alignment. The current schema (PAC-5a) covers versioning
and completeness but does not enforce structural compatibility.

### Metrics line protocol (Prometheus `/metrics`)

**Status: experimental.** `weft-server` exposes Prometheus-format metrics. The metric
names, labels, and label values are not versioned and may change in a minor release.

**Basis:** The `/metrics` endpoint is documented in
[`docs/reference.md`](reference.md#metrics) and implemented in
`weft-server/src/metrics.rs`. No compatibility gate or version marker exists for the
metric surface.

### Documentation surfaces (README, docs/reference.md, CHANGELOG.md)

**Status: stable (informational).** The changelog format follows [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/). Breaking changes, added features, and
deprecations are recorded in `CHANGELOG.md` under the next release. The changelog is the
single source of truth for what changed between releases.

**Basis:** [`CHANGELOG.md`](../CHANGELOG.md) preamble and every release entry.

---

## What the compat suite actually tests

The compatibility suite lives in two places:

1. **`scripts/compat/releases.py`** and the pin file **`compat/releases.toml`**.
   Every pinned release carries:
   - The exact commit the tag points at.
   - The fixture set that release wrote (`compat/fixtures/<tag>/`).
   - The `upgrade` leg (the pinned release's store opens on the candidate).
   - The `upgrade` leg (the pinned release's store opens on the candidate), which
     is the only leg the compat suite actually executes.
   - The SHA-256 digest of every archive the release published.

   The [`compat` job in `ci.yml`](../.github/workflows/ci.yml) runs `releases.py verify`
   on every CI run. That command re-checks every pin against git and against
   `compat/fixtures/`, so a fixture that drifts fails CI. The leg that does run as code
   is the upgrade path, in `tests/compat_v010.rs`.

   Since [FRE-16](https://github.com/basic-automation/weftdb/issues/FRE-16), a
   dedicated gate (`scripts/compat/test_leg_coverage.py`) refuses to let a pin declare
   a leg that has no test runner — unknown or unexecuted legs are errors, not warnings.
   The only recognised leg is `upgrade`; `rollback` and `binary` are explicitly
   unsupported for 1.0.

2. **`tests/compat_v010.rs`** — the Rust integration test that opens a v0.1.0 fixture
   set, reads it under the current build, and checks that the migration is
   idempotent and the values match the `expected_reads.txt` the release wrote.

**What the suite covers today:**
- Forward migration: a v0.1.0 store opens and is readable on the current build.
- Idempotent migration: re-opening a migrated store produces the same reads.
- Fixture integrity: no committed fixture drifts from what its release wrote.

**What the suite does NOT cover today (gaps named in ROADMAP.md):**
- Rollback after writes (PAC-14a). Rollback is unsupported for 1.0: a store written
  by the current layout is not readable by older binaries.
- Binary compatibility. Explicitly unsupported for 1.0.
- Layout-2 baseline fixture (FRE-18, W13).

---

## Wire-compatibility example

PR [#649](https://github.com/basic-automation/weftdb/pull/649) (open as of this writing)
makes the `total` field in `StoredRangeResponse` nullable — it is always present on the
first page and absent on cursor-following pages. For the server this is additive (the
field is always present or becomes `null` rather than being removed), but for a client
that deserialises `total` as `usize` (non-optional), the change is **breaking**. Under
the policy above this is a wire-breaking change that requires a minor (pre-1.0) or major
(1.0+) version bump, and the changelog entry should name the `Option<usize>` / nullable
shift.

This is the exact class of change the policy must classify: a change that is additive for
the server but breaking for a strict client. The ruling is **major (breaking)**: the wire
contract changes, so the version that ships it must carry a breaking-change notification
for any SDK that cannot deserialise `null`.

---

## Promises the repo does not yet support

These are surfaces that the release criteria name but the repository cannot yet ground a
stability promise for:

| Surface | What the release criteria say | What is missing |
|---|---|---|
| Python client | ROADMAP mentions the Python SDK as shipped | Not on PyPI; no release pin, no CI compat job |
| R client | ROADMAP mentions the R SDK as shipped | Not on CRAN; no release pin, no CI compat job |
| Arrow Flight | `docs/reference.md` marks it **experimental** | No compat test, no version marker |
| Grafana datasource | `docs/reference.md` marks it **experimental** | No compat test, no version marker |
| Rollback after writes | ROADMAP W9: PAC-14a | Rollback is unsupported for 1.0 — a store written by the current layout is not readable by older binaries |
| Layout-2 baseline fixture | ROADMAP W13: FRE-18 | No freeze cut has been made; no layout-2 fixture set is committed |
| Binary compatibility | ROADMAP "Compatibility tests" | Binary compatibility is explicitly unsupported for 1.0 |
| Configuration schema | ROADMAP W8: PAC-5a | Version marker and name-coverage checker exist (PAC-5a). No formal JSON Schema / CUE that validates value shapes, types, or ranges across releases. |

Every one of these gaps is visible in the roadmap as an unchecked box. When a new check
is ticked, this document should be updated.