#!/usr/bin/env bash
# Generate or check THIRD-PARTY-LICENSES.md, the license bundle every release archive
# carries (1.0 plan PAC-1b, absorbing EVI-20 so there is one bundle, not one per track).
#
#   scripts/licenses/generate.sh            rewrite THIRD-PARTY-LICENSES.md
#   scripts/licenses/generate.sh check      fail if it is not what the lockfile implies
#
# The license graph comes from Cargo.lock through about.toml (the five release targets,
# dev-dependencies excluded, the same license set deny.toml allows). `--fail` is passed so
# a dependency whose license cannot be identified stops the run: a silently missing crate
# is exactly the failure this file exists to prevent, and cargo-about only warns by default.
#
# The tool version is pinned, because the bundle is a committed artifact and an unpinned
# generator makes every upgrade an unexplained diff. Bump ABOUT_VERSION and the `cargo
# install --version` in .github/workflows/ci.yml together, and expect the file to change.
#
# Line endings are normalized to LF. A few crates publish their license text with CRLF, and
# .gitattributes (`* text=auto`) normalizes the committed bundle to LF, so a bundle kept as
# generated would disagree with itself: git would store one thing and the next run would
# produce another. Every CR in the generated output is part of a CRLF, so dropping CR is
# lossless here, and the archive carries one kind of line ending like every other text file.
#
# Runs on Linux and macOS. Needs the cargo toolchain (for `cargo metadata`) and network
# access the first time, to fetch crate sources whose license files are not yet cached.
set -euo pipefail
export LC_ALL=C

ABOUT_VERSION='0.9.2'
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

out='THIRD-PARTY-LICENSES.md'
mode=${1:-write}
[[ $# -eq 0 || $1 == write || $1 == check ]] || {
	echo "usage: generate.sh [write|check]" >&2
	exit 2
}

command -v cargo-about >/dev/null 2>&1 || {
	cat >&2 <<-EOF
		cargo-about is not installed:
		    cargo install cargo-about --version $ABOUT_VERSION --features cli
	EOF
	exit 1
}

got=$(cargo-about --version 2>/dev/null | awk '{print $2}')
[[ $got == "$ABOUT_VERSION" ]] || {
	echo "cargo-about $ABOUT_VERSION is required, found $got:" >&2
	echo "    cargo install cargo-about --version $ABOUT_VERSION --features cli" >&2
	exit 1
}

# Always written to a scratch file: `check` must not touch the tree, and `write` replaces the
# bundle only once generation has succeeded, so a failed run leaves the old file standing.
work=$(mktemp -d)
# shellcheck disable=SC2064 # expand now: $work is this run's own directory
trap "rm -rf '$work'" EXIT
bundle=$work/THIRD-PARTY-LICENSES.md

cargo-about -L warn generate \
	--config about.toml \
	--manifest-path Cargo.toml \
	--workspace --locked --fail \
	about.hbs -o "$bundle"
tr -d '\r' <"$bundle" >"$bundle.lf"
mv "$bundle.lf" "$bundle"

if [[ $mode == check ]]; then
	if cmp -s "$bundle" "$out"; then
		echo "$out matches Cargo.lock"
		exit 0
	fi
	echo "$out is stale; run scripts/licenses/generate.sh and commit the result" >&2
	diff -u --label "$out (committed)" --label "$out (generated)" "$out" "$bundle" |
		head -40 >&2 || true
	exit 1
fi

mv "$bundle" "$out"

# What the run produced, so a reviewer of the diff can see the shape of the change without
# opening 600 KB of license text.
crates=$(grep -cE '^  \* [a-z0-9._-]+ [0-9]' "$out" || true)
texts=$(grep -c '^--------------------------------------------------------------------------------$' "$out" || true)
printf '%s: %s license texts, %s crate entries\n' "$out" "$texts" "$crates"
