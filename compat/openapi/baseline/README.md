# OpenAPI wire-compatibility baseline

This directory holds the **rc.1 wire-compatibility baseline**
(`weftdb.json`, a byte-identical copy of `../weftdb.json` as it was at
the rc.1 freeze cut).

`scripts/ci/check-openapi-breaking.py` compares the live committed spec
(`compat/openapi/weftdb.json`) against this baseline and classifies every
difference against the six compatibility rule families documented in
`docs/stability.md:27-40`.

## Baseline integrity

The baseline is a copy of the spec it polices; a PR must not move it
silently.  A committed SHA-256 digest (`weftdb.json.sha256`) is asserted
in CI (`claims` job) — every PR that changes the baseline must also update
the digest.

## Moving the baseline

A human moves the baseline deliberately by copying the current spec over
it in a PR that also bumps the version.  That PR must say which breaking
changes it accepts and why.

### Bump procedure

1. Edit the route definitions in `weft-server/src/openapi.rs`.
2. Regenerate the committed spec:
   ```
   WEFT_UPDATE_GOLDENS=1 cargo test -p weft-server --lib openapi
   ```
3. Copy the new spec onto the baseline:
   ```
   cp compat/openapi/weftdb.json compat/openapi/baseline/weftdb.json
   ```
4. Update the digest:
   ```
   sha256sum compat/openapi/baseline/weftdb.json > compat/openapi/baseline/weftdb.json.sha256
   ```
5. Bump the version in accordance with `docs/stability.md` (a breaking
   change requires a minor version bump pre-1.0).
6. Commit all touched files in the same PR, and update this directory's
   entry in `docs/stability.md` if the freeze-cut date has moved.