# `compat/fixtures` — the trees a release wrote, frozen

Every subdirectory here is one fixture set, named for the release that wrote it, and every set is
**byte-frozen**: the `MANIFEST.sha256` inside it carries the sha256 of each file the set holds, and
`scripts/compat/releases.py verify` — the `release pins` CI job — fails on a file whose bytes moved,
on a file the manifest does not list, and on a listed file that is missing. A set is append-only: a
new release adds a sibling directory, it does not amend an old one, because the whole point of a
fixture is that the bytes a past release produced stop changing.

**So notes about a set go in this file, not inside it.** The README inside a set is itself one of
the hashed files: editing `v0.1.0/README.md` to explain something about the tree changes a file the
release did not write and turns `release pins` red with
`README.md does not match MANIFEST.sha256 (the fixture changed; a fixture set is append-only)`. That
job is what keeps these trees honest, and it is right to be the stricter party.

## The one thing a checkout cannot carry

`v0.1.0/legacy/v010_legacy/` is a rows-mode database, and rows-mode created an **empty
`dictionaries/` directory beside every `measurements.db`** — `create_dir_all(&dictionaries_path)` at
`v0.1.0:weftdb/src/types/aspect.rs:252`, on every aspect open; the set has three aspects, so three
such directories. An empty directory is the one artifact git has no representation for. It is also
the one thing `MANIFEST.sha256` cannot report: the manifest hashes files, so it lists no line to
miss, `release pins` stays green, and `git status` stays clean — nothing was deleted, the tree is
simply smaller in a checkout than it was on the disk that wrote it.

1636778 recovered the 97 files that the repo-wide store ignores (`*.db`, `*.weftseg`, …) had eaten
from these same trees, so a checkout holds every byte the release wrote. This is the residue of that
same gap one level of git down, and it was CI-only for the same reason: the readable leg of the
rows-mode work asserts the reader *names* `plant/price/dictionaries` among what it refuses to carry
forward, and only a machine that had run `gen_legacy_fixture` had a directory there to name.
`weftdb/tests/compat_v010.rs::restore_uncarried_directories` recreates them on the copy the test
reads, one beside every `measurements.db` it finds, so a test sees the tree the release wrote while
the committed bytes stay exactly as the release left them.

A future fixture whose evidence needs an empty directory cannot use this trick and should not have
to: carry a `.gitkeep`, add it to the set's `MANIFEST.sha256`, and say in that set's README what the
directory is for. It is then a file the release did not write, which is a real trade — and the trade
is the fixture's own to make, not the reader's.
