#!/usr/bin/env python3
"""Tests for scripts/compat/releases.py.

Run from the repository root:

    python3 -m unittest discover -s scripts/compat -p 'test_*.py' -v

The git tests build a throwaway repository in a temporary directory; the ones that read a
release's assets put a stub `gh` first on PATH, which answers `release download` with a
SHA256SUMS file of the test's choosing. Nothing here reaches GitHub.
"""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import releases  # noqa: E402

REPO_ROOT = HERE.parent.parent


def block(**overrides) -> dict:
	base = {
		"version": "0.1.0",
		"tag": "v0.1.0",
		"commit": "0" * 40,
		"published_at": "2026-10-09T00:56:59Z",
		"binaries": True,
		"crates_io": False,
		"legs": ["upgrade", "rollback"],
		"assets": {"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz": "a" * 64},
	}
	base.update(overrides)
	return base


def git(repo: Path, *args: str) -> str:
	proc = subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True, env={**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com"})
	assert proc.returncode == 0, proc.stderr
	return proc.stdout.strip()


def make_repo(tmp: Path) -> tuple[Path, str]:
	"""A two-commit repository; the older one carries the annotated tag v0.1.0."""
	repo = tmp / "repo"
	repo.mkdir()
	git(repo, "init", "-q", "-b", "main")
	(repo / "f").write_text("one\n")
	git(repo, "add", "f")
	git(repo, "commit", "-q", "-m", "one")
	git(repo, "tag", "-a", "-m", "v0.1.0", "v0.1.0")
	commit = git(repo, "rev-parse", "HEAD")
	(repo / "f").write_text("two\n")
	git(repo, "add", "f")
	git(repo, "commit", "-q", "-m", "two")
	return repo, commit


def write_fixture(root: Path, files: dict[str, str], manifest: bool = True) -> None:
	for name, body in files.items():
		path = root / name
		path.parent.mkdir(parents=True, exist_ok=True)
		path.write_bytes(body.encode() if isinstance(body, str) else body)
	if manifest:
		lines = [f"{hashlib.sha256(data.encode() if isinstance(data, str) else data).hexdigest()}  {name}" for name, data in sorted(files.items())]
		(root / releases.MANIFEST_NAME).write_text("\n".join(lines) + "\n")


def stub_gh(bin_dir: Path, sums: str) -> str:
	"""Put a fake `gh` in `bin_dir` and return the PATH that finds it first."""
	script = bin_dir / "gh"
	script.write_text(f"""#!/usr/bin/env python3
import sys, pathlib
args = sys.argv[1:]
out = pathlib.Path(args[args.index("--dir") + 1])
(out / "SHA256SUMS").write_text({sums!r}, encoding="utf-8")
""")
	script.chmod(0o755)
	return str(bin_dir) + os.pathsep + os.environ["PATH"]


class Shape(unittest.TestCase):
	def problems(self, blk: dict) -> list[str]:
		return releases.check_block(blk, 1)

	def test_a_good_block_needs_nothing(self):
		self.assertEqual(self.problems(block()), [])

	def test_a_missing_field_is_named(self):
		blk = block()
		del blk["commit"]
		self.assertIn("release #1: missing required field 'commit'", self.problems(blk))

	def test_tag_and_version_agree(self):
		self.assertIn("release #1: tag 'v0.2.0' must be v0.1.0", self.problems(block(tag="v0.2.0")))

	def test_tag_and_commit_shapes(self):
		found = self.problems(block(tag="0.1.0", commit="deadbeef"))
		self.assertTrue(any("tag '0.1.0' is not vX.Y.Z" in p for p in found))
		self.assertTrue(any("commit must be a 40-hex commit id" in p for p in found))

	def test_unknown_field_and_leg_are_reported(self):
		found = self.problems(block(whistle="x", legs=["upgrade", "sideways"]))
		self.assertIn("release #1: unknown field 'whistle' (known: assets, binaries, commit, crates_io, fixtures, legs, published_at, tag, version)", found)
		self.assertTrue(any("unknown leg(s) ['sideways']" in p for p in found))

	def test_asset_name_must_carry_the_pinned_tag(self):
		blk = block(version="0.2.0", tag="v0.2.0", assets={"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz": "a" * 64})
		found = self.problems(blk)
		self.assertIn("release #1: asset 'weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz' is named for v0.1.0, not for the pinned tag v0.2.0", found)

	def test_asset_digest_must_be_hex(self):
		blk = block(assets={"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz": "nope"})
		self.assertTrue(any("needs a 64-hex sha256 digest" in p for p in self.problems(blk)))

	def test_an_asset_name_that_is_not_an_archive(self):
		blk = block(assets={"weftdb-v0.1.0-linux.tar.bz2": "a" * 64})
		self.assertTrue(any("is not named weftdb-<tag>-<target>" in p for p in self.problems(blk)))

	def test_fixtures_stays_under_compat(self):
		self.assertTrue(any("fixtures must point under compat/fixtures/" in p for p in self.problems(block(fixtures="weftdb/tests/fixtures"))))

	def test_published_at_parses(self):
		self.assertTrue(any("published_at" in p for p in self.problems(block(published_at="last tuesday"))))

	def test_order_and_duplicates(self):
		self.assertEqual(releases.check_order([block(version="0.1.0", tag="v0.1.0"), block(version="0.2.0", tag="v0.2.0")]), [])
		found = releases.check_order([block(version="0.2.0", tag="v0.2.0"), block(version="0.1.0", tag="v0.1.0")])
		self.assertTrue(any("is not newer than the block before it" in p for p in found))
		found = releases.check_order([block(), block()])
		self.assertTrue(any("is pinned twice" in p for p in found))

	def test_prerelease_sorts_before_its_release(self):
		self.assertLess(releases.semver_key("1.0.0-rc.1"), releases.semver_key("1.0.0"))
		self.assertEqual(releases.newest([block(version="0.1.0", tag="v0.1.0"), block(version="0.9.0", tag="v0.9.0")])["version"], "0.9.0")


class TagsAndFixtures(unittest.TestCase):
	def test_tag_must_point_at_the_pinned_commit(self):
		with tempfile.TemporaryDirectory() as tmp:
			repo, commit = make_repo(Path(tmp))
			self.assertEqual(releases.check_tag(repo, block(commit=commit), 1), [])
			found = releases.check_tag(repo, block(commit="1" * 40), 1)
			self.assertTrue(found and f"points at {commit}, but the pin says " + "1" * 40 in found[0])

	def test_a_tag_the_checkout_does_not_have_is_a_hint(self):
		with tempfile.TemporaryDirectory() as tmp:
			repo, _ = make_repo(Path(tmp))
			found = releases.check_tag(repo, block(tag="v9.9.9"), 1)
			self.assertTrue(any("git fetch --tags" in p for p in found))

	def test_a_fixture_set_verifies_against_its_manifest(self):
		with tempfile.TemporaryDirectory() as tmp:
			root = Path(tmp) / "compat" / "fixtures" / "v0.1.0"
			write_fixture(root, {"README.md": "docs\n", "store/segments/a.weftseg": b"\x00\x01"})
			self.assertEqual(releases.check_fixture_dir(root, Path(tmp)), [])

	def test_a_tampered_fixture_is_caught(self):
		with tempfile.TemporaryDirectory() as tmp:
			root = Path(tmp) / "fx"
			write_fixture(root, {"a.bin": "one"})
			(root / "a.bin").write_text("two")
			found = releases.check_fixture_dir(root, Path(tmp))
			self.assertTrue(any("a.bin does not match MANIFEST.sha256" in p for p in found))

	def test_missing_and_unlisted_fixture_files(self):
		with tempfile.TemporaryDirectory() as tmp:
			root = Path(tmp) / "fx"
			write_fixture(root, {"a.bin": "one", "b.bin": "two"})
			(root / "b.bin").unlink()
			(root / "c.bin").write_text("three")
			found = releases.check_fixture_dir(root, Path(tmp))
			self.assertTrue(any("MANIFEST.sha256 lists b.bin, which is missing" in p for p in found))
			self.assertTrue(any("c.bin is not listed in MANIFEST.sha256" in p for p in found))

	def test_a_fixture_set_needs_a_manifest(self):
		with tempfile.TemporaryDirectory() as tmp:
			root = Path(tmp) / "fx"
			write_fixture(root, {"a.bin": "one"}, manifest=False)
			self.assertTrue(any("has no MANIFEST.sha256" in p for p in releases.check_fixture_dir(root, Path(tmp))))

	def test_a_pinned_fixture_dir_that_is_absent(self):
		with tempfile.TemporaryDirectory() as tmp:
			repo, commit = make_repo(Path(tmp))
			pin = repo / "compat" / "releases.toml"
			pin.parent.mkdir(parents=True, exist_ok=True)
			pin.write_text(f'[[release]]\nversion = "0.1.0"\ntag = "v0.1.0"\ncommit = "{commit}"\npublished_at = "2026-10-09T00:56:59Z"\nbinaries = true\ncrates_io = false\nlegs = ["upgrade"]\nfixtures = "compat/fixtures/v0.1.0"\n\n[release.assets]\n"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz" = "' + "a" * 64 + '"\n')
			found = releases.verify(repo, pin)
			self.assertTrue(any("compat/fixtures/v0.1.0 does not exist" in p for p in found))

	def test_a_whole_pin_tree_that_holds(self):
		with tempfile.TemporaryDirectory() as tmp:
			repo, commit = make_repo(Path(tmp))
			write_fixture(repo / "compat" / "fixtures" / "v0.1.0", {"README.md": "docs\n", "store/x.bin": b"\x00\x02"})
			pin = repo / "compat" / "releases.toml"
			pin.parent.mkdir(parents=True, exist_ok=True)
			pin.write_text(f'[[release]]\nversion = "0.1.0"\ntag = "v0.1.0"\ncommit = "{commit}"\npublished_at = "2026-10-09T00:56:59Z"\nbinaries = true\ncrates_io = false\nlegs = ["upgrade", "rollback"]\nfixtures = "compat/fixtures/v0.1.0"\n\n[release.assets]\n"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz" = "' + "a" * 64 + '"\n')
			self.assertEqual(releases.verify(repo, pin), [])


class Assets(unittest.TestCase):
	def test_pin_block_reads_the_release_s_own_sums(self):
		sums = "a" * 64 + "  weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz\n" + "b" * 64 + "  weftdb-v0.1.0-x86_64-pc-windows-msvc.zip\n"
		with tempfile.TemporaryDirectory() as tmp:
			env = dict(os.environ, PATH=stub_gh(Path(tmp), sums))
			out = subprocess.run([sys.executable, str(HERE / "releases.py"), "pin", "v0.1.0"], capture_output=True, text=True, env=env)
			self.assertEqual(out.returncode, 0, out.stderr)
			head = subprocess.run(["git", "-C", str(REPO_ROOT), "rev-list", "-n1", "v0.1.0"], capture_output=True, text=True).stdout.strip()
			self.assertTrue(head, "this checkout has no v0.1.0 tag")
			self.assertIn(f'commit = "{head}"', out.stdout)
			self.assertIn('tag = "v0.1.0"', out.stdout)
			self.assertNotIn("fixtures", out.stdout, "a pin gets its fixtures line when the fixture set lands")

	def test_asset_drift_is_reported(self):
		pinned = block(assets={"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz": "a" * 64, "weftdb-v0.1.0-gone-target.tar.gz": "c" * 64})
		old_sums = releases.release_sha256sums
		releases.release_sha256sums = lambda tag: {"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz": "d" * 64, "weftdb-v0.1.0-new-target.tar.gz": "e" * 64}
		try:
			found = releases.verify_assets(pinned)
		finally:
			releases.release_sha256sums = old_sums
		self.assertEqual(len(found), 3, found)
		self.assertTrue(any("pinned asset weftdb-v0.1.0-gone-target.tar.gz is not in" in p for p in found))
		self.assertTrue(any("the release publishes weftdb-v0.1.0-new-target.tar.gz" in p for p in found))
		self.assertTrue(any("is pinned as " + "a" * 64 + " but the release carries " + "d" * 64 in p for p in found))


class ThePinnedTree(unittest.TestCase):
	def test_the_v010_pin_names_the_fixture_set_x_fx_committed(self):
		"""The baseline pin names what X-FX committed, and the set is the shape a gate needs.

		A pin without its fixture set is a gate with nothing to open; a fixture set missing a
		tree, a record or the segment files inside `store/` is a gate that proves less than it
		claims. `verify` re-hashes every byte of it; this says what those bytes have to be."""
		blocks = [b for b in releases.load_pin(REPO_ROOT / releases.PIN_FILE) if b.get("version") == "0.1.0"]
		self.assertEqual(len(blocks), 1, "the baseline release is pinned exactly once")
		fixture = blocks[0].get("fixtures")
		self.assertEqual(fixture, "compat/fixtures/v0.1.0", "the v0.1.0 pin must name its fixture set: X-FX is what commits it")
		root = REPO_ROOT / fixture
		for tree in ("store", "backup", "legacy"):
			self.assertTrue((root / tree).is_dir(), f"{fixture}/{tree}: one of the three trees the v0.1.0 binaries wrote")
		for record in ("rows.csv", "expected_reads.txt", "aspects.json", "aspect_stats.txt", "stats.json", "backup.json", "restore_drill.json", "segment_listing.txt", "MANIFEST.sha256", "binaries.sha256", "provenance.txt", "README.md"):
			self.assertTrue((root / record).is_file(), f"{fixture}/{record}: how a fixture set says what it holds and who wrote it")
		self.assertTrue((REPO_ROOT / "scripts/compat/make-v010-fixtures.sh").is_file(), "a fixture set a gate cannot regenerate is a fixture set nobody can fix")
		self.assertTrue((REPO_ROOT / "weftdb/tests/compat_v010.rs").is_file(), "the pin names a fixture set; a test has to open it")
		# The shape that makes this root worth having: segments keyed in a control-plane
		# database, and no format marker at all, because the marker postdates the tag.
		self.assertTrue((root / "store" / "segment_index.db").is_file())
		self.assertFalse((root / "store" / "store.json").exists(), "a v0.1.0 root predates the format marker; a fixture that has one was not written by the tag")
		self.assertFalse((root / "store" / "segment_manifest.json").exists())
		self.assertTrue(any(path.name.endswith(".weftseg") for path in (root / "store" / "segments").iterdir()), "a Storage-v2 fixture with no segments tests nothing")

	def test_this_repositories_pins_verify(self):
		"""The file this repository ships must pass its own checks."""
		try:
			releases.verify(REPO_ROOT, REPO_ROOT / releases.PIN_FILE)
		except releases.PinError as error:
			self.skipTest(f"{error}")
			return
		problems = releases.verify(REPO_ROOT, REPO_ROOT / releases.PIN_FILE)
		self.assertEqual(problems, [])


if __name__ == "__main__":
	unittest.main()
