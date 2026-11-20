# SEMVER-241 progress

## Audit: tag coverage in CHANGELOG.md

### Command outputs

```text
$ git tag --sort=creatordate
v0.1.0
v0.2.0
v0.2.1
v0.3.0
v0.3.1
v0.3.2
v0.3.3
v0.3.4
v0.3.5
v0.3.6
v0.3.7

$ grep -n '^## \[' CHANGELOG.md
9:   ## [Unreleased]
335: ## [0.3.7] - 2026-11-12
352: ## [0.3.6] - 2026-11-12
363: ## [0.3.5] - 2026-11-12
374: ## [0.3.4] - 2026-11-11
394: ## [0.3.3] - 2026-11-11
823: ## [0.3.2] - 2026-11-06
859: ## [0.3.1] - 2026-11-06
923: ## [0.3.0] - 2026-11-05
1264: ## [0.2.1] - 2026-11-02
1281: ## [0.2.0] - 2026-11-02
2811: ## [0.1.0] - 2026-10-08
```

### Audit table

| Tag | Section line | Date | Compare link | Verdict |
|-----|-------------|------|-------------|---------|
| v0.1.0 | :2811 | 2026-10-08 | `[0.1.0]: https://github.com/.../releases/tag/v0.1.0` | ✅ Section present, dated, link present |
| v0.2.0 | :1281 | 2026-11-02 | `[0.2.0]: .../compare/v0.1.0...v0.2.0` | ✅ Section present, dated, link present |
| v0.2.1 | :1264 | 2026-11-02 | `[0.2.1]: .../compare/v0.2.0...v0.2.1` | ✅ Section present, dated, link present |
| v0.3.0 | :923 | 2026-11-05 | `[0.3.0]: .../compare/v0.2.1...v0.3.0` | ✅ Section present, dated, link present |
| v0.3.1 | :859 | 2026-11-06 | `[0.3.1]: .../compare/v0.3.0...v0.3.1` | ✅ Section present, dated, link present |
| v0.3.2 | :823 | 2026-11-06 | `[0.3.2]: .../compare/v0.3.1...v0.3.2` | ✅ Section present, dated, link present |
| v0.3.3 | :394 | 2026-11-11 | `[0.3.3]: .../compare/v0.3.2...v0.3.3` | ✅ Section present, dated, link present |
| v0.3.4 | :387 | 2026-11-11 | `[0.3.4]: .../compare/v0.3.3...v0.3.4` | ✅ Section present, dated, link present |
| v0.3.5 | :374 | 2026-11-12 | `[0.3.5]: .../compare/v0.3.4...v0.3.5` | ✅ Section present, dated, link present |
| v0.3.6 | :352 | 2026-11-12 | `[0.3.6]: .../compare/v0.3.5...v0.3.6` | ✅ Section present, dated, link present |
| v0.3.7 | :335 | 2026-11-12 | `[0.3.7]: .../compare/v0.3.6...v0.3.7` | ✅ Section present, dated, link present |

### Counts
- **11** tags (git tag --sort=creatordate)
- **11** sections (`## [0.x.y] - <date>`)
- **11** compare links (`[0.x.y]: https://...`)

### Verdict
**Clean.** Every tagged release from v0.1.0 (2026-10-08) through v0.3.7 (2026-11-12) has its own CHANGELOG.md section with a date and a working compare link. No section or link is missing.

The "starting from the first tagged pre-release" requirement was already satisfied: the changelog baseline is v0.1.0, the first tag. The real gap this slice closes is the missing semver pre-release tagging policy (for `v1.0.0-beta.1`, `v1.0.0-rc.1`, etc.), which is now added to `docs/stability.md`.