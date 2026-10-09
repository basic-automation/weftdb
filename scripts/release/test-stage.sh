#!/usr/bin/env bash
# Tests for scripts/release/stage.sh with stand-in binaries. The documents come from this
# repository's own tree, so a file stage.sh packs that the tree no longer has fails here,
# not in a release. Needs tar, and 7z for the Windows case (skipped without it); writes
# nothing outside a temp directory.
#
#   scripts/release/test-stage.sh
set -euo pipefail

stage="$(cd "$(dirname "$0")" && pwd)/stage.sh"
src="$(cd "$(dirname "$0")/../.." && pwd)"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
unset GITHUB_ACTIONS

failures=0
check() { # check <description> <command...>
	local description=$1
	shift
	if "$@" >"$tmp/log" 2>&1; then
		printf 'ok    %s\n' "$description"
	else
		printf 'FAIL  %s\n' "$description"
		sed 's/^/      /' "$tmp/log"
		failures=$((failures + 1))
	fi
}
refuses() { # refuses [reason:<text>] <description> <command...>: fails, saying <text>
	local reason=''
	if [[ $1 == reason:* ]]; then
		reason=${1#reason:}
		shift
	fi
	local description=$1
	shift
	if "$@" >"$tmp/log" 2>&1; then
		printf 'FAIL  %s: succeeded\n' "$description"
		failures=$((failures + 1))
	elif [[ -n $reason ]] && ! grep -qF -- "$reason" "$tmp/log"; then
		printf 'FAIL  %s: failed without saying "%s"\n' "$description" "$reason"
		sed 's/^/      /' "$tmp/log"
		failures=$((failures + 1))
	else
		printf 'ok    %s\n' "$description"
	fi
}

# sha256sum_c <dir> <checksum file>: what a user runs to check a download.
sha256sum_c() { (cd "$1" && sha256sum -c "$2"); }

targets=(x86_64-unknown-linux-gnu aarch64-apple-darwin)
for target in "${targets[@]}"; do
	mkdir -p "$tmp/bin/$target"
	for b in weft-server weft-tui weft-bench; do
		printf '%s for %s\n' "$b" "$target" >"$tmp/bin/$target/$b"
	done
	check "archive $target" "$stage" archive "$target" v1.2.3 "$tmp/bin/$target" "$src" "$tmp/dist"
done

dir=weftdb-v1.2.3-x86_64-unknown-linux-gnu
listing=$(tar -tzf "$tmp/dist/$dir.tar.gz" | LC_ALL=C sort | tr '\n' ' ')
want=""
for f in / /LICENSE-APACHE /LICENSE-MIT /NOTICE /README.md /THIRD-PARTY-NOTICES-weft-physical-type \
	/THIRD-PARTY-NOTICES-weft-reduce /weft-bench /weft-server /weft-tui; do
	want+="$dir$f "
done
check "the archive holds one directory with the binaries, licenses and notices" \
	test "$listing" = "$want"
mkdir "$tmp/unpacked"
tar -xzf "$tmp/dist/$dir.tar.gz" -C "$tmp/unpacked"
for crate in weft-physical-type weft-reduce; do
	check "$crate's THIRD-PARTY-NOTICES is packed under its own name" \
		cmp "$src/$crate/THIRD-PARTY-NOTICES" "$tmp/unpacked/$dir/THIRD-PARTY-NOTICES-$crate"
done
check "the .sha256 verifies with sha256sum -c" \
	sha256sum_c "$tmp/dist" weftdb-v1.2.3-x86_64-unknown-linux-gnu.tar.gz.sha256

if command -v 7z >/dev/null 2>&1; then
	mkdir -p "$tmp/bin/win"
	for b in weft-server weft-tui weft-bench; do echo "$b" >"$tmp/bin/win/$b.exe"; done
	check "archive a Windows target as a zip" \
		"$stage" archive x86_64-pc-windows-msvc v1.2.3 "$tmp/bin/win" "$src" "$tmp/windist"
else
	echo "skip  the Windows zip (no 7z)"
fi

mkdir -p "$tmp/partial"
cp "$src/README.md" "$src/LICENSE-MIT" "$src/LICENSE-APACHE" "$src/NOTICE" "$tmp/partial/"
refuses reason:"missing file: $tmp/partial/weft-physical-type/THIRD-PARTY-NOTICES" \
	"archive with a notice file missing" \
	"$stage" archive x86_64-unknown-linux-gnu v1.2.3 "$tmp/bin/x86_64-unknown-linux-gnu" "$tmp/partial" "$tmp/other"
refuses "archive with a missing binary" \
	"$stage" archive x86_64-unknown-linux-gnu v1.2.3 "$tmp/nowhere" "$src" "$tmp/other"
linux_bin=$tmp/bin/x86_64-unknown-linux-gnu
for bad in 'v1.2.3;id' v1.2.3-beta.1 v01.2.3 v1.2.3-dryrun-abc v1.2.3-dryrun-0123456789ab-release \
	v1.2.3-dryrun-0123456789AB; do
	refuses reason:"not a release tag or a dry-run label" "archive with the label $bad" \
		"$stage" archive x86_64-unknown-linux-gnu "$bad" "$linux_bin" "$src" "$tmp/other"
done

utf8=$(locale -a 2>/dev/null | grep -ixE 'en_US\.utf-?8' | head -n 1 || true)
LC_ALL=${utf8:-C.UTF-8} refuses reason:"not a release tag or a dry-run label" \
	"archive with a non-ASCII digit under ${utf8:-C.UTF-8}" \
	"$stage" archive x86_64-unknown-linux-gnu "v１.2.3" "$linux_bin" "$src" "$tmp/other"

# A dry run's archives carry its commit, and -dev for a dev-profile build.
for label in v1.2.3-dryrun-0123456789ab v1.2.3-dryrun-0123456789ab-dev v0.2.0-alpha.1-dryrun-0123456789ab; do
	check "archive with the dry-run label $label" \
		"$stage" archive x86_64-unknown-linux-gnu "$label" "$linux_bin" "$src" "$tmp/dry-$label"
	check "  is named for it" test -f "$tmp/dry-$label/weftdb-$label-x86_64-unknown-linux-gnu.tar.gz"
	check "  and sums over it" "$stage" sums "$tmp/dry-$label" "$label" x86_64-unknown-linux-gnu
done

check "sums over every target" "$stage" sums "$tmp/dist" v1.2.3 "${targets[@]}"
check "SHA256SUMS verifies with sha256sum -c" sha256sum_c "$tmp/dist" SHA256SUMS
check "SHA256SUMS lists every archive" test "$(wc -l <"$tmp/dist/SHA256SUMS")" -eq 2

refuses reason:"missing the archive" "sums with a target missing" \
	"$stage" sums "$tmp/dist" v1.2.3 "${targets[@]}" x86_64-pc-windows-msvc
refuses reason:"2 archives for 1 targets" "sums with an archive no target asked for" \
	"$stage" sums "$tmp/dist" v1.2.3 "${targets[0]}"
refuses reason:"missing the archive" "sums over archives named for another label" \
	"$stage" sums "$tmp/dist" v1.2.4 "${targets[@]}"
refuses reason:"missing the archive" "sums for a release over a dry run's archives" \
	"$stage" sums "$tmp/dry-v1.2.3-dryrun-0123456789ab" v1.2.3 x86_64-unknown-linux-gnu

cp -r "$tmp/dist" "$tmp/corrupt"
printf 'x' >>"$tmp/corrupt/weftdb-v1.2.3-aarch64-apple-darwin.tar.gz"
refuses reason:"sha256 is" "sums over a corrupted archive" "$stage" sums "$tmp/corrupt" v1.2.3 "${targets[@]}"

cp -r "$tmp/dist" "$tmp/nosum"
rm "$tmp/nosum/weftdb-v1.2.3-aarch64-apple-darwin.tar.gz.sha256"
refuses reason:"missing weftdb-v1.2.3-aarch64-apple-darwin.tar.gz.sha256" "sums with a .sha256 missing" \
	"$stage" sums "$tmp/nosum" v1.2.3 "${targets[@]}"

if ((failures > 0)); then
	echo "$failures failure(s)"
	exit 1
fi
echo "all passed"
