# P12-213 progress — restore_into HTTP route

## Status (2026-11-13 23:59)

### Done
- `progress.md` restored to `origin/main` content (not committed; PR stays single-purpose)
- `ROADMAP.md` line 90 ticked with parenthetical notes
- `CHANGELOG.md` one-liner under Unreleased
- `docs/roadmap-notes.md` 7.3 line updated: HTTP route landed
- `DECISIONS.md` section now states the three required points: (a) no adopt, (b) `.restoring-*` in STAGING_PREFIXES with 1-hour sweep limitation, (c) restore-base/backup-base containment
- **Commit** `b26db41` on `p12/restore-root-http` with DCO sign-off
- **Pushed** to origin
- **PR #739** created, Closes #213
- **Comment** on #213 linking the PR
- Assignment verified: `physics515`

### Not run (delegated to CI)
- `cargo test -p weft-server --test restore_into` — compiled but not executed
- `cargo clippy -p weft-server -- -D warnings`
- `cargo fmt --check`

### PR state
`gh pr view 739 --json mergeable,state` → MERGEABLE, OPEN
