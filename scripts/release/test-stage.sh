#!/usr/bin/env bash
# Tests for scripts/release/stage.sh with stand-in binaries. Needs tar, and 7z for the
# Windows case (skipped without it); touches nothing outside a temp directory.
#
#   scripts/release/test-stage.sh
set -euo pipefail

stage="$(cd "$(dirname "$0")" && pwd)/stage.sh"
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
refuses() { # refuses <description> <command...>
	local description=$1
	shift
	if "$@" >"$tmp/log" 2>&1; then
		printf 'FAIL  %s: succeeded\n' "$description"
		failures=$((failures + 1))
	else
		printf 'ok    %s\n' "$description"
	fi
}

# sha256sum_c <dir> <checksum file>: what a user runs to check a download.
sha256sum_c() { (cd "$1" && sha256sum -c "$2"); }

src=$tmp/src
mkdir -p "$src"
echo readme >"$src/README.md"
echo license >"$src/LICENSE"

targets=(x86_64-unknown-linux-gnu aarch64-apple-darwin)
for target in "${targets[@]}"; do
	mkdir -p "$tmp/bin/$target"
	for b in weft-server weft-tui weft-bench; do
		printf '%s for %s\n' "$b" "$target" >"$tmp/bin/$target/$b"
	done
	check "archive $target" "$stage" archive "$target" v1.2.3 "$tmp/bin/$target" "$src" "$tmp/dist"
done

listing=$(tar -tzf "$tmp/dist/weftdb-v1.2.3-x86_64-unknown-linux-gnu.tar.gz" | LC_ALL=C sort | tr '\n' ' ')
want="weftdb-v1.2.3-x86_64-unknown-linux-gnu/ weftdb-v1.2.3-x86_64-unknown-linux-gnu/LICENSE weftdb-v1.2.3-x86_64-unknown-linux-gnu/README.md weftdb-v1.2.3-x86_64-unknown-linux-gnu/weft-bench weftdb-v1.2.3-x86_64-unknown-linux-gnu/weft-server weftdb-v1.2.3-x86_64-unknown-linux-gnu/weft-tui "
check "the archive holds one directory with the binaries and docs" test "$listing" = "$want"
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

refuses "archive with a missing binary" \
	"$stage" archive x86_64-unknown-linux-gnu v1.2.3 "$tmp/nowhere" "$src" "$tmp/other"
refuses "archive with a bad tag" \
	"$stage" archive x86_64-unknown-linux-gnu 'v1.2.3;id' "$tmp/bin/x86_64-unknown-linux-gnu" "$src" "$tmp/other"

check "sums over every target" "$stage" sums "$tmp/dist" "${targets[@]}"
check "SHA256SUMS verifies with sha256sum -c" sha256sum_c "$tmp/dist" SHA256SUMS
check "SHA256SUMS lists every archive" test "$(wc -l <"$tmp/dist/SHA256SUMS")" -eq 2

refuses "sums with a target missing" "$stage" sums "$tmp/dist" "${targets[@]}" x86_64-pc-windows-msvc
refuses "sums with an archive no target asked for" "$stage" sums "$tmp/dist" "${targets[0]}"

cp -r "$tmp/dist" "$tmp/corrupt"
printf 'x' >>"$tmp/corrupt/weftdb-v1.2.3-aarch64-apple-darwin.tar.gz"
refuses "sums over a corrupted archive" "$stage" sums "$tmp/corrupt" "${targets[@]}"

cp -r "$tmp/dist" "$tmp/nosum"
rm "$tmp/nosum/weftdb-v1.2.3-aarch64-apple-darwin.tar.gz.sha256"
refuses "sums with a .sha256 missing" "$stage" sums "$tmp/nosum" "${targets[@]}"

if ((failures > 0)); then
	echo "$failures failure(s)"
	exit 1
fi
echo "all passed"
