# OpenAPI wire-compatibility baseline

This directory holds the **rc.1 wire-compatibility baseline**
(`weftdb.json`, a byte-identical copy of `../weftdb.json` as it was at
the rc.1 freeze cut).

`scripts/ci/check-openapi-breaking.py` compares the live committed spec
(`compat/openapi/weftdb.json`) against this baseline and classifies every
difference against the six compatibility rule families documented in
`docs/stability.md:27-40`.

## Moving the baseline

A human moves the baseline deliberately by copying the current spec over
it in a PR that also bumps the version.  That PR must say which breaking
changes it accepts and why.