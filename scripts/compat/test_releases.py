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
import tarfile
import tempfile
import tomllib
from io import BytesIO
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
		"legs": ["upgrade"],
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

	def test_a_beta_tag_is_accepted(self):
		blk = block(version="1.2.3-beta.1", tag="v1.2.3-beta.1", assets={"weftdb-v1.2.3-beta.1-x86_64-unknown-linux-gnu.tar.gz": "a" * 64})
		self.assertEqual(self.problems(blk), [])

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

	def test_a_beta_asset_name_is_accepted(self):
		blk = block(version="1.2.3-beta.1", tag="v1.2.3-beta.1", assets={"weftdb-v1.2.3-beta.1-x86_64-unknown-linux-gnu.tar.gz": "a" * 64})
		self.assertEqual(self.problems(blk), [])

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
		self.assertLess(releases.semver_key("1.0.0-beta.1"), releases.semver_key("1.0.0-rc.1"))
		self.assertLess(releases.semver_key("1.0.0-rc.1"), releases.semver_key("1.0.0"))
		self.assertLess(releases.semver_key("1.0.0-beta.1"), releases.semver_key("1.0.0"))
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
			pin.write_text(f'[[release]]\nversion = "0.1.0"\ntag = "v0.1.0"\ncommit = "{commit}"\npublished_at = "2026-10-09T00:56:59Z"\nbinaries = true\ncrates_io = false\nlegs = ["upgrade"]\nfixtures = "compat/fixtures/v0.1.0"\n\n[release.assets]\n"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz" = "' + "a" * 64 + '"\n')
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

	def test_pin_block_emits_only_legs_the_gate_recognises(self):
		"""A pin `pin --write` appends must pass the gate that reads it.

		`pin_block` still emitted `legs = ["upgrade", "rollback"]` after FRE-16
		removed `rollback` from the recognised legs, so the block it printed
		failed `check_block` with `unknown leg(s) ['rollback']` — and the
		leg-coverage gate would have failed it too. The generator's output has to
		be a block the gates accept, not just text shaped like one.
		"""
		sums = "a" * 64 + "  weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz\n"
		with tempfile.TemporaryDirectory() as tmp:
			env = dict(os.environ, PATH=stub_gh(Path(tmp), sums))
			out = subprocess.run([sys.executable, str(HERE / "releases.py"), "pin", "v0.1.0"], capture_output=True, text=True, env=env)
			self.assertEqual(out.returncode, 0, out.stderr)
			self.assertIn('legs = ["upgrade"]', out.stdout)
			emitted = tomllib.loads(out.stdout)["release"][0]
			self.assertEqual(releases.check_block(emitted, 1), [], "the emitted pin must pass the shape gate")

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
class Fetch(unittest.TestCase):
	"""`fetch` must check the bytes against the pin, and must not be reachable for an
	artifact the pin does not vouch for. A fake `gh` copies a file the test made; nothing
	in this class reaches GitHub."""

	def setUp(self):
		self.gh_calls = []
		self.real_gh = releases.gh

	def fake_gh(self, src: Path, copy: bool = True):
		"""Stand in for `gh release download … --pattern NAME --dir D`."""

		def gh(*args: str) -> str:
			self.gh_calls.append(args)
			pattern = args[args.index("--pattern") + 1] if "--pattern" in args else None
			dest = Path(args[args.index("--dir") + 1])
			dest.mkdir(parents=True, exist_ok=True)
			if copy and pattern:
				(dest / pattern).write_bytes(src.read_bytes())
			return ""

		releases.gh = gh

	def archive(self, tmp: Path, name: str = "weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz") -> Path:
		"""A release-shaped tarball holding an executable and a README."""
		payload = tmp / "stage"
		(payload / "bin").mkdir(parents=True)
		(payload / "bin" / "weft-server").write_text("#!/bin/sh\necho v0.1.0\n")
		(payload / "bin" / "weft-server").chmod(0o755)
		(payload / "README").write_text("v0.1.0\n")
		archive = tmp / name
		with tarfile.open(archive, "w:gz") as tar:
			tar.add(payload / "bin" / "weft-server", arcname="weftdb-v0.1.0-x86_64-unknown-linux-gnu/bin/weft-server")
			tar.add(payload / "README", arcname="weftdb-v0.1.0-x86_64-unknown-linux-gnu/README")
		return archive

	def tearDown(self):
		# Put the module's own `gh` back: a deleted attribute breaks the next test in the
		# class, which is the mistake this class made once already.
		releases.gh = self.real_gh

	def test_the_digest_of_the_bytes_arrived_is_the_digest_returned(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = self.archive(tmp)
			name = archive.name
			self.fake_gh(archive)
			pinned = block(assets={name: hashlib.sha256(archive.read_bytes()).hexdigest()})
			path, digest, unpacked = releases.fetch_asset(pinned, tmp / "out", host=("linux", "x86_64"))
			self.assertEqual(digest, hashlib.sha256(archive.read_bytes()).hexdigest())
			self.assertEqual(path, tmp / "out" / name)
			self.assertEqual(unpacked, [])

	def test_bytes_that_do_not_match_the_pin_are_refused_naming_both(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = self.archive(tmp)
			self.fake_gh(archive)
			pinned = block(assets={archive.name: "b" * 64})
			with self.assertRaises(releases.PinError) as caught:
				releases.fetch_asset(pinned, tmp / "out", host=("linux", "x86_64"))
			message = str(caught.exception)
			self.assertIn("b" * 64, message)
			self.assertIn(hashlib.sha256(archive.read_bytes()).hexdigest(), message)
			# The point of the sentence, not its punctuation: the pin is history.
			self.assertIn("append-only", message)

	def test_an_asset_the_pin_does_not_carry_is_refused_before_any_download(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = self.archive(tmp)
			self.fake_gh(archive)
			pinned = block(assets={archive.name: hashlib.sha256(archive.read_bytes()).hexdigest()})
			with self.assertRaises(releases.PinError) as caught:
				releases.fetch_asset(pinned, tmp / "out", asset="weftdb-v0.1.0-x86_64-unknown-linux-musl.tar.gz", host=("linux", "x86_64"))
			self.assertIn("x86_64-unknown-linux-gnu.tar.gz", str(caught.exception))
			self.assertEqual(self.gh_calls, [], "the pin was consulted before the network")

	def test_a_release_that_shipped_no_binaries_has_nothing_to_fetch(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			self.fake_gh(Path("nowhere"))
			with self.assertRaises(releases.PinError) as caught:
				releases.fetch_asset(block(binaries=False), tmp / "out", host=("linux", "x86_64"))
			self.assertIn("binaries = false", str(caught.exception))
			self.assertEqual(self.gh_calls, [])

	def test_gh_lying_about_success_is_caught(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = self.archive(tmp)
			self.fake_gh(archive, copy=False)
			pinned = block(assets={archive.name: hashlib.sha256(archive.read_bytes()).hexdigest()})
			with self.assertRaises(releases.PinError) as caught:
				releases.fetch_asset(pinned, tmp / "out", host=("linux", "x86_64"))
			self.assertIn("left no", str(caught.exception))

	def test_each_machine_gets_the_archive_named_for_its_triple(self):
		assets = {
			"weftdb-v0.1.0-aarch64-apple-darwin.tar.gz": "1" * 64,
			"weftdb-v0.1.0-aarch64-unknown-linux-gnu.tar.gz": "2" * 64,
			"weftdb-v0.1.0-x86_64-apple-darwin.tar.gz": "3" * 64,
			"weftdb-v0.1.0-x86_64-pc-windows-msvc.zip": "4" * 64,
			"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz": "5" * 64,
		}
		for host, expected in (
			(("linux", "x86_64"), "x86_64-unknown-linux-gnu.tar.gz"),
			(("linux", "aarch64"), "aarch64-unknown-linux-gnu.tar.gz"),
			(("darwin", "arm64"), "aarch64-apple-darwin.tar.gz"),
			(("darwin", "x86_64"), "x86_64-apple-darwin.tar.gz"),
			(("win32", "amd64"), "x86_64-pc-windows-msvc.zip"),
		):
			self.assertEqual(releases.pick_asset(assets, host=host).endswith(expected), True, f"{host} should get {expected}")

	def test_a_machine_with_no_pinned_archive_is_told_what_exists(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = self.archive(tmp)
			self.fake_gh(archive)
			pinned = block(assets={"weftdb-v0.1.0-aarch64-unknown-linux-gnu.tar.gz": "a" * 64})
			with self.assertRaises(releases.PinError) as caught:
				releases.fetch_asset(pinned, tmp / "out", host=("linux", "x86_64"))
			self.assertIn("x86_64-unknown-linux-gnu", str(caught.exception))
			self.assertIn("aarch64-unknown-linux-gnu.tar.gz", str(caught.exception))
			self.assertEqual(self.gh_calls, [])

	def test_an_unknown_machine_is_told_to_name_the_asset(self):
		assets = {"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz": "a" * 64}
		with self.assertRaises(releases.PinError) as caught:
			releases.pick_asset(assets, host=("plan9", "hollybush"))
		self.assertIn("--asset", str(caught.exception))

	def test_two_archives_for_one_triple_is_not_guessed(self):
		assets = {
			"weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz": "a" * 64,
			"weftdb-v0.1.0-x86_64-unknown-linux-gnu-static.tar.gz": "b" * 64,
		}
		with self.assertRaises(releases.PinError) as caught:
			releases.pick_asset(assets, host=("linux", "x86_64"))
		self.assertIn("--asset", str(caught.exception))

	def test_extract_lands_the_binaries_next_to_the_archive(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = self.archive(tmp)
			self.fake_gh(archive)
			pinned = block(assets={archive.name: hashlib.sha256(archive.read_bytes()).hexdigest()})
			path, digest, unpacked = releases.fetch_asset(pinned, tmp / "out", host=("linux", "x86_64"), extract=True)
			names = [str(entry.relative_to(tmp / "out")) for entry in unpacked]
			self.assertEqual(sorted(names), ["0.1.0/weftdb-v0.1.0-x86_64-unknown-linux-gnu/README", "0.1.0/weftdb-v0.1.0-x86_64-unknown-linux-gnu/bin/weft-server"])
			self.assertTrue((tmp / "out" / "0.1.0" / "weftdb-v0.1.0-x86_64-unknown-linux-gnu" / "bin" / "weft-server").is_file())

	def test_extract_refuses_a_zip_and_says_why(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = self.archive(tmp)
			self.fake_gh(archive)
			name = "weftdb-v0.1.0-x86_64-pc-windows-msvc.zip"
			pinned = block(assets={name: hashlib.sha256(archive.read_bytes()).hexdigest()})
			with self.assertRaises(releases.PinError) as caught:
				releases.fetch_asset(pinned, tmp / "out", asset=name, host=("win32", "amd64"), extract=True)
			self.assertIn("tar", str(caught.exception))

	def test_unpack_refuses_a_member_that_escapes_the_destination(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = tmp / "weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
			with tarfile.open(archive, "w:gz") as tar:
				info = tarfile.TarInfo("../escaped")
				info.size = 2
				tar.addfile(info, BytesIO(b"hi"))
			with self.assertRaises(releases.PinError) as caught:
				releases.unpack_tar(archive, tmp / "out")
			self.assertIn("outside", str(caught.exception))
			self.assertFalse((tmp / "escaped").exists())

	def test_unpack_refuses_a_link_member_because_it_bypasses_the_check(self):
		with tempfile.TemporaryDirectory() as raw:
			tmp = Path(raw)
			archive = tmp / "weftdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
			with tarfile.open(archive, "w:gz") as tar:
				info = tarfile.TarInfo("weftdb-v0.1.0/bin/weft-server")
				info.type = tarfile.SYMTYPE
				info.linkname = "../../../../etc/passwd"
				tar.addfile(info)
			with self.assertRaises(releases.PinError) as caught:
				releases.unpack_tar(archive, tmp / "out")
			self.assertIn("link", str(caught.exception))

	def test_fetch_on_the_committed_pin_names_the_archive_ci_downloads(self):
		"""The pin in the repository must name an archive this machine can select — the CI
		job runs with no --asset, so a pin that cannot be resolved by triple is a red job."""
		releases_ = releases.load_pin(REPO_ROOT / "compat" / "releases.toml")
		for entry in releases_:
			assets = dict(entry.get("assets") or {})
			if not entry.get("binaries"):
				continue
			names = [name for name in assets if "x86_64-unknown-linux-gnu" in name]
			self.assertEqual(len(names), 1, f"{entry['tag']} must carry exactly one linux x86_64 archive for the CI job, found {names}")


