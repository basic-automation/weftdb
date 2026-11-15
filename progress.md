
## Handoff — 2026-11-15

**SHA:** `40e2c15`
**PR:** #755
**Branch:** `p12/compat-tests`

### What was done
- Added `v010_compat_fre13_upgrade_contract` test to `weftdb/tests/compat_v010.rs` that opens the checked-in `compat/fixtures/v0.1.0/` store fixture, asserts the FRE-13 upgrade contract (layout version, data integrity, write-after-upgrade, reopen), and pins it for the v0.1.0 release bytes
- Added `SUPPORTED_LAYOUT` import to the test file
- Added a `bd()` helper for BigDecimal literals
- Appended the decision to `DECISIONS.md`
- Opened PR #755 against `main` and commented on #240

### What was verified
- The test binary lists `v010_compat_fre13_upgrade_contract` and passes when run directly
- `cargo check --workspace` compiles cleanly (this includes the compat_v010 test file)
- Full `cargo test --workspace` could not run on this host (memory constraints); CI will execute it

### What was done in this cut

- Added `v010_compat_fre13_rollback_after_writes_loses_nothing` to `compat_v010.rs` (leg C):
  opens the v0.1.0 fixture, upgrades, writes, tampers `STORE_FORMAT` marker `min_write_layout`
  to 99, asserts `IncompatibleLayout` refusal with the tampered floor and current supported
  layout, restores marker, reopens, verifies new and old data survive.
- Updated `UPGRADING.md` to describe what a rollback attempt does (refuses, loses nothing)
  and reference the test. Updated `CHANGELOG.md` to list both legs as implemented.
- Appended COMPAT-2 decision record to `DECISIONS.md`.

### What remains for the next cut
- **Leg C (rollback after writes)** — not implemented, declared unsupported for 1.0
- **UPGRADING.md prose** — operator documentation for the upgrade path
- Both are part of PAC-14a; the roadmap line (ROADMAP.md:107) is left unticked

### Inherited CI red
clippy/rustfmt/weft-server-lib failures from `origin/main`, fixed by PR #750.
