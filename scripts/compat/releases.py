#!/usr/bin/env python3
"""The release pins in `compat/releases.toml`: read them, check them, add one.

The 1.0 compatibility gates (ROADMAP.md "Compatibility tests", "Upgrade/rollback procedure
exercised against the previous release") need a previous release to test against. That
release is named once, in `compat/releases.toml`: its tag, the commit the tag points at, the
fixture set it wrote, the compatibility legs it is the "previous release" for, and the
sha256 of every archive it published. A gate that reads the pin cannot drift from the
release it claims to test.

Subcommands:

    verify                 re-check every pin against git, against compat/fixtures/ and
                           against the shape of an asset name (offline; CI runs this)
    show                   print one line per pinned release
    path [--version V]     print the fixture directory of a pinned release (default: the
                           newest pinned release, which is what a gate runs against)
    verify-assets --tag T  download that release's SHA256SUMS asset from GitHub and compare
                           it with the pin (needs gh; the release workflow's job does this)
    pin <tag> [--write]    print the block for a released tag, built from git, the tag's
                           date and the release's SHA256SUMS asset; --write appends it

`verify` prints one `compat: <problem>` line per problem and exits 1 if there is any. A pin
is append-only history: changing a block after a gate has run against it means the artifact
that run measured no longer exists, which is exactly what `verify-assets` catches.
"""

from __future__ import annotations

import argparse
import datetime
import os
import hashlib
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
PIN_FILE = "compat/releases.toml"

# A release archive as scripts/release/stage.sh names it: weftdb-<tag>-<target>.(tar.gz|zip).
TARGET_RE = r"[a-z0-9_]+(?:-[a-z0-9_]+){2,3}"
ASSET_RE = re.compile(rf"^weftdb-(v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-rc\.[0-9]+)?)-({TARGET_RE})\.(tar\.gz|zip)$")
TAG_RE = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rc\.[1-9][0-9]*)?$")
VERSION_RE = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rc\.[1-9][0-9]*)?$")
DIGEST_RE = re.compile(r"^[0-9a-f]{64}$")
MANIFEST_NAME = "MANIFEST.sha256"
LEGS = {"upgrade", "rollback", "binary"}
# name -> (required, type) for a block's fields. `fixtures` is optional: a release is pinned
# when it is cut, and its fixture set lands later, so the field appears once it exists.
FIELDS = {
	"version": True,
	"tag": True,
	"commit": True,
	"published_at": True,
	"binaries": True,
	"crates_io": True,
	"legs": True,
	"assets": True,
	"fixtures": False,
}


class PinError(Exception):
	"""A pin, or the tree around it, is wrong. The message names what to fix."""


def display(path: Path, repo: Path) -> str:
	"""A path as a reader refers to it: under the repository when it is, absolute when not."""
	try:
		return str(path.relative_to(repo))
	except ValueError:
		return str(path)


def semver_key(version: str) -> tuple:
	"""Sort key putting 0.1.0 before 0.9.0 before 1.0.0-rc.1 before 1.0.0."""
	match = VERSION_RE.match(version)
	if not match:
		raise PinError(f"not a version weftdb releases: {version!r}")
	major, minor, patch, pre = match.groups()
	prerelease = (0, pre) if pre else (1, "")
	return (int(major), int(minor), int(patch), prerelease)


def load_pin(path: Path, repo: Path = REPO) -> list[dict]:
	"""The `[[release]]` blocks of a releases.toml, in file order."""
	if not path.is_file():
		raise PinError(f"{display(path, repo)} does not exist")
	with open(path, "rb") as fh:
		try:
			doc = tomllib.load(fh)
		except tomllib.TOMLDecodeError as error:
			raise PinError(f"{display(path, repo)} is not valid TOML: {error}") from error
	releases = doc.get("release")
	if not releases:
		raise PinError(f"{display(path, repo)} pins no release: it needs at least one [[release]] block")
	return releases


def check_block(block: dict, position: int) -> list[str]:
	"""Every shape problem in one `[[release]]` block, as message lines."""
	problems: list[str] = []
	known = set(FIELDS)
	for extra in sorted(set(block) - known):
		problems.append(f"release #{position}: unknown field {extra!r} (known: {', '.join(sorted(known))})")
	for field, required in FIELDS.items():
		if required and field not in block:
			problems.append(f"release #{position}: missing required field {field!r}")
	version, tag = block.get("version"), block.get("tag")
	if isinstance(version, str) and not VERSION_RE.match(version):
		problems.append(f"release #{position}: version {version!r} is not X.Y.Z or X.Y.Z-rc.N")
	if isinstance(tag, str) and not TAG_RE.match(tag):
		problems.append(f"release #{position}: tag {tag!r} is not vX.Y.Z or vX.Y.Z-rc.N")
	if isinstance(version, str) and isinstance(tag, str) and tag != f"v{version}":
		problems.append(f"release #{position}: tag {tag!r} must be v{version}")
	if isinstance(block.get("commit"), str) and not re.fullmatch(r"[0-9a-f]{40}", block["commit"]):
		problems.append(f"release #{position}: commit must be a 40-hex commit id")
	for flag in ("binaries", "crates_io"):
		if flag in block and not isinstance(block[flag], bool):
			problems.append(f"release #{position}: {flag} must be a boolean")
	published = block.get("published_at")
	if isinstance(published, str):
		try:
			datetime.datetime.fromisoformat(published)
		except ValueError:
			problems.append(f"release #{position}: published_at {published!r} is not an ISO-8601 timestamp")
	elif published is not None and not isinstance(published, datetime.datetime):
		problems.append(f"release #{position}: published_at must be a quoted ISO-8601 timestamp")
	legs = block.get("legs")
	if isinstance(legs, list):
		unknown = [leg for leg in legs if leg not in LEGS]
		if unknown:
			problems.append(f"release #{position}: unknown leg(s) {unknown}; the legs are {', '.join(sorted(LEGS))}")
	elif legs is not None:
		problems.append(f"release #{position}: legs must be an array of strings")
	assets = block.get("assets")
	if isinstance(assets, dict):
		for name, digest in assets.items():
			match = ASSET_RE.match(name) if isinstance(name, str) else None
			if not match:
				problems.append(f"release #{position}: asset {name!r} is not named weftdb-<tag>-<target>.(tar.gz|zip)")
			elif isinstance(tag, str) and match.group(1) != tag:
				problems.append(f"release #{position}: asset {name!r} is named for {match.group(1)}, not for the pinned tag {tag}")
			if not isinstance(digest, str) or not DIGEST_RE.match(digest):
				problems.append(f"release #{position}: asset {name!r} needs a 64-hex sha256 digest")
	elif assets is not None:
		problems.append(f"release #{position}: assets must be a [release.assets] table")
	if isinstance(block.get("fixtures"), str) and not block["fixtures"].startswith("compat/fixtures/"):
		problems.append(f"release #{position}: fixtures must point under compat/fixtures/")
	return problems


def check_order(releases: list[dict]) -> list[str]:
	"""Pins are oldest-first and one per version, so "the newest pin" is the last block."""
	problems: list[str] = []
	seen: dict[str, int] = {}
	previous: tuple | None = None
	for index, block in enumerate(releases, start=1):
		version = block.get("version")
		if not isinstance(version, str) or not VERSION_RE.match(version):
			continue
		if version in seen:
			problems.append(f"release #{index}: version {version} is pinned twice (block {seen[version]})")
		seen[version] = index
		key = semver_key(version)
		if previous is not None and key <= previous:
			problems.append(f"release #{index}: version {version} is not newer than the block before it; pins are oldest first")
		previous = key
	return problems


def check_tag(repo: Path, block: dict, position: int) -> list[str]:
	"""The tag exists locally and points at exactly the pinned commit."""
	tag, commit = block.get("tag"), block.get("commit")
	if not (isinstance(tag, str) and isinstance(commit, str)):
		return []
	probe = subprocess.run(["git", "-C", str(repo), "rev-parse", "--verify", f"refs/tags/{tag}"], capture_output=True, text=True)
	if probe.returncode != 0:
		return [f"release #{position}: tag {tag} is not in this checkout; run git fetch --tags --force origin"]
	resolved = subprocess.run(["git", "-C", str(repo), "rev-list", "-n1", tag], capture_output=True, text=True).stdout.strip()
	if resolved != commit:
		return [f"release #{position}: tag {tag} points at {resolved}, but the pin says {commit}"]
	return []


def sha256_file(path: Path) -> str:
	digest = hashlib.sha256()
	with open(path, "rb") as fh:
		for chunk in iter(lambda: fh.read(1 << 20), b""):
			digest.update(chunk)
	return digest.hexdigest()


def check_fixture_dir(fixture: Path, repo: Path = REPO) -> list[str]:
	"""A fixture set is byte-exact and self-describing: it carries a MANIFEST.sha256 whose
	listed digests match the files on disk, with nothing unlisted alongside them."""
	problems: list[str] = []
	if not fixture.is_dir():
		return [f"{display(fixture, repo)} does not exist: the fixture set a pin names must be committed with it"]
	manifest = fixture / MANIFEST_NAME
	if not manifest.is_file():
		return [f"{display(fixture, repo)} has no {MANIFEST_NAME}: a fixture set lists the sha256 of every file it holds"]
	listed: dict[str, str] = {}
	for line in manifest.read_text(encoding="utf-8").splitlines():
		line = line.strip()
		if not line:
			continue
		digest, _, name = line.partition("  ")
		if not name or not DIGEST_RE.match(digest):
			problems.append(f"{display(fixture, repo)}/{MANIFEST_NAME}: line {line!r} is not '<sha256>  <path>'")
			continue
		listed[name] = digest
	if not listed:
		problems.append(f"{display(fixture, repo)}/{MANIFEST_NAME} lists no file")
	for name, digest in sorted(listed.items()):
		path = fixture / name
		if not path.is_file():
			problems.append(f"{display(fixture, repo)}: {MANIFEST_NAME} lists {name}, which is missing")
		elif sha256_file(path) != digest:
			problems.append(f"{display(fixture, repo)}: {name} does not match {MANIFEST_NAME} (the fixture changed; a fixture set is append-only)")
	on_disk = {str(path.relative_to(fixture)) for path in fixture.rglob("*") if path.is_file()} - {MANIFEST_NAME}
	for name in sorted(on_disk - set(listed)):
		problems.append(f"{display(fixture, repo)}: {name} is not listed in {MANIFEST_NAME}")
	return problems


def verify(repo: Path, pin_path: Path) -> list[str]:
	"""Every problem with the pins, as message lines; empty means the pins hold."""
	releases = load_pin(pin_path)
	problems: list[str] = []
	for index, block in enumerate(releases, start=1):
		problems += check_block(block, index)
	problems += check_order(releases)
	for index, block in enumerate(releases, start=1):
		problems += check_tag(repo, block, index)
		fixture = block.get("fixtures")
		if isinstance(fixture, str):
			problems += check_fixture_dir(repo / fixture, repo)
	return problems


def newest(releases: list[dict]) -> dict:
	return max(releases, key=lambda block: semver_key(str(block.get("version"))))


def gh(*args: str) -> str:
	if shutil.which("gh") is None:
		raise PinError("gh is not on PATH")
	proc = subprocess.run(["gh", *args], capture_output=True, text=True)
	if proc.returncode != 0:
		raise PinError(f"gh {' '.join(args)}: {proc.stderr.strip() or proc.stdout.strip()}")
	return proc.stdout


def release_sha256sums(tag: str) -> dict[str, str]:
	"""The asset digests the release itself publishes, read from its SHA256SUMS asset.

	The repository comes from `GITHUB_REPOSITORY` (set by every Actions job) so a fork's run
	or a test's stub resolves against the release it means, exactly as dco_check.py reads it."""
	repository = os.environ.get("GITHUB_REPOSITORY", "basic-automation/weftdb")
	with tempfile.TemporaryDirectory() as tmp:
		gh("release", "download", tag, "--repo", repository, "--pattern", "SHA256SUMS", "--clobber", "--dir", tmp)
		body = Path(tmp, "SHA256SUMS").read_text(encoding="utf-8")
	sums: dict[str, str] = {}
	for line in body.splitlines():
		digest, _, name = line.strip().partition("  ")
		if DIGEST_RE.match(digest) and name:
			sums[name] = digest
	if not sums:
		raise PinError(f"{tag}: its SHA256SUMS asset holds no recognizable line")
	return sums


def release_published_at(tag: str) -> str | None:
	"""When a release went public, from GitHub; None when gh cannot say (offline, no token)."""
	try:
		return gh("release", "view", tag, "--repo", os.environ.get("GITHUB_REPOSITORY", "basic-automation/weftdb"), "--json", "publishedAt", "-q", ".publishedAt").strip() or None
	except PinError:
		return None


def pin_block(tag: str) -> str:
	"""The TOML block for a released tag, built from git and the release's own asset sums.

	It writes no `fixtures` line: the fixture set a release wrote lands in a later PR, which
	adds the line when it commits `compat/fixtures/<tag>/`."""
	if not TAG_RE.match(tag):
		raise PinError(f"{tag} is not a release tag (vX.Y.Z or vX.Y.Z-rc.N)")
	version = tag[1:]
	commit = subprocess.run(["git", "-C", str(REPO), "rev-list", "-n1", tag], capture_output=True, text=True).stdout.strip()
	if not re.fullmatch(r"[0-9a-f]{40}", commit):
		raise PinError(f"tag {tag} is not in this checkout; run git fetch --tags --force origin")
	published = release_published_at(tag) or subprocess.run(["git", "-C", str(REPO), "for-each-ref", f"--format=%(creatordate:iso-strict)", f"refs/tags/{tag}"], capture_output=True, text=True).stdout.strip()
	lines = [
		"[[release]]",
		f'version = "{version}"',
		f'tag = "{tag}"',
		f'commit = "{commit}"',
		f'published_at = "{published}"',
		"binaries = true",
		"crates_io = false",
		'legs = ["upgrade", "rollback"]',
		"",
		"[release.assets]",
	]
	lines += [f'"{name}" = "{digest}"' for name, digest in sorted(release_sha256sums(tag).items())]
	return "\n".join(lines) + "\n"


def verify_assets(block: dict) -> list[str]:
	"""Compare the pin's asset digests with what the release publishes today."""
	tag = str(block["tag"])
	pinned = dict(block.get("assets") or {})
	live = release_sha256sums(tag)
	problems: list[str] = []
	for name in sorted(set(pinned) - set(live)):
		problems.append(f"{tag}: pinned asset {name} is not in the release's SHA256SUMS")
	for name in sorted(set(live) - set(pinned)):
		problems.append(f"{tag}: the release publishes {name}, which the pin does not carry")
	for name in sorted(set(pinned) & set(live)):
		if pinned[name] != live[name]:
			problems.append(f"{tag}: asset {name} is pinned as {pinned[name]} but the release carries {live[name]}")
	return problems


def main(argv: list[str] | None = None) -> int:
	parser = argparse.ArgumentParser(prog="releases.py", description=__doc__.splitlines()[0])
	sub = parser.add_subparsers(dest="command", required=True)
	sub.add_parser("verify")
	sub.add_parser("show")
	path = sub.add_parser("path")
	path.add_argument("--version", help="pinned version to name (default: the newest pin)")
	assets = sub.add_parser("verify-assets")
	assets.add_argument("--tag", help="pinned tag to check against GitHub (default: every pin)")
	pin = sub.add_parser("pin")
	pin.add_argument("tag")
	pin.add_argument("--write", action="store_true", help="append the block to compat/releases.toml")
	args = parser.parse_args(argv)

	try:
		if args.command == "pin":
			block = pin_block(args.tag)
			if args.write:
				text = (REPO / PIN_FILE).read_text(encoding="utf-8").rstrip("\n")
				(REPO / PIN_FILE).write_text(text + "\n\n" + block, encoding="utf-8")
				print(f"{PIN_FILE}: pinned {args.tag}")
			else:
				sys.stdout.write(block)
			return 0
		releases = load_pin(REPO / PIN_FILE)
		if args.command == "show":
			for block in releases:
				print(f"{block['tag']} {block['commit'][:12]} fixtures={block.get('fixtures', '-')} legs={','.join(block.get('legs', []))} assets={len(block.get('assets', {}))}")
			return 0
		if args.command == "path":
			if args.version:
				match = [b for b in releases if b.get("version") == args.version]
				if not match:
					print(f"compat: no pin for version {args.version}", file=sys.stderr)
					return 1
				block = match[0]
			else:
				block = newest(releases)
			fixture = block.get("fixtures")
			if not fixture:
				print(f"compat: {block['tag']} pins no fixture set yet", file=sys.stderr)
				return 1
			print(REPO / fixture)
			return 0
		if args.command == "verify-assets":
			target = releases if not args.tag else [b for b in releases if b.get("tag") == args.tag]
			if not target:
				print(f"compat: {args.tag} is not pinned", file=sys.stderr)
				return 1
			problems = [problem for block in target for problem in verify_assets(block)]
			for problem in problems:
				print(f"compat: {problem}")
			if not problems:
				print("compat: every pinned release's assets match its SHA256SUMS")
			return 1 if problems else 0
		problems = verify(REPO, REPO / PIN_FILE)
		for problem in problems:
			print(f"compat: {problem}")
		if not problems:
			print(f"compat: {len(releases)} pinned release(s) verified")
		return 1 if problems else 0
	except PinError as error:
		print(f"compat: {error}", file=sys.stderr)
		return 2


if __name__ == "__main__":
	raise SystemExit(main())
