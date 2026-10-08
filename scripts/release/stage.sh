#!/usr/bin/env bash
# Stage the release archives and their checksums.
#
#   scripts/release/stage.sh archive <target> <tag> <bin-dir> <src-dir> <out-dir>
#       Packs weft-server, weft-tui and weft-bench from <bin-dir>, with README.md and
#       LICENSE from <src-dir>, into <out-dir>/weftdb-<tag>-<target>.tar.gz (a .zip for
#       a Windows target), and writes <archive>.sha256 next to it.
#
#   scripts/release/stage.sh sums <dist-dir> <target>...
#       Requires exactly one archive per <target> in <dist-dir> and nothing else,
#       checks each archive against its .sha256, then writes <dist-dir>/SHA256SUMS over
#       all of them and checks that too.
#
# Runs on Linux, macOS and Windows (Git Bash). The checksum files use the
# "<sha256>  <file name>" format that `sha256sum -c` and `shasum -a 256 -c` read.
set -euo pipefail

TARGET_RE='^[a-z0-9_]+(-[a-z0-9_]+){2,3}$'
TAG_RE='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'
BINARIES=(weft-server weft-tui weft-bench)
DOCS=(README.md LICENSE)

fail() {
	if [[ ${GITHUB_ACTIONS:-} == true ]]; then
		printf '::error title=release stage::%s\n' "$*"
	else
		printf 'stage: %s\n' "$*" >&2
	fi
	exit 1
}

usage() {
	fail "usage: stage.sh archive <target> <tag> <bin-dir> <src-dir> <out-dir> | stage.sh sums <dist-dir> <target>..."
}

sha256_of() {
	local digest
	if command -v sha256sum >/dev/null 2>&1; then
		digest=$(sha256sum -- "$1")
	else
		digest=$(shasum -a 256 -- "$1")
	fi
	# Some sha256sum builds prefix the digest with "\" or the name with "*".
	digest=${digest#\\}
	printf '%s\n' "${digest%% *}"
}

# check_line <dir> <"digest  name" line>: the named file in <dir> must have that digest.
check_line() {
	local dir=$1 line=$2 want name got
	want=${line%% *}
	name=${line#*  }
	[[ $want =~ ^[0-9a-f]{64}$ && $name != "$line" && $name != */* ]] ||
		fail "malformed checksum line: $line"
	[[ -f $dir/$name ]] || fail "$name is listed but missing"
	got=$(sha256_of "$dir/$name")
	[[ $got == "$want" ]] || fail "$name: sha256 is $got, expected $want"
}

archive() {
	[[ $# -eq 5 ]] || usage
	local target=$1 tag=$2 bin_dir=$3 src_dir=$4 out_dir=$5 exe='' format name work f
	[[ $target =~ $TARGET_RE ]] || fail "not a target triple: $(printf '%q' "$target")"
	[[ $tag =~ $TAG_RE ]] || fail "not a release tag: $(printf '%q' "$tag")"
	[[ $target == *-windows-* ]] && exe=.exe
	format=tar.gz
	[[ $target == *-windows-* ]] && format=zip

	name="weftdb-$tag-$target"
	mkdir -p "$out_dir"
	out_dir=$(cd "$out_dir" && pwd)
	work=$(mktemp -d)
	# shellcheck disable=SC2064 # expand now: $work is local
	trap "rm -rf '$work'" EXIT
	mkdir "$work/$name"

	for f in "${BINARIES[@]}"; do
		[[ -f $bin_dir/$f$exe ]] || fail "missing binary: $bin_dir/$f$exe"
		cp "$bin_dir/$f$exe" "$work/$name/"
	done
	for f in "${DOCS[@]}"; do
		[[ -f $src_dir/$f ]] || fail "missing file: $src_dir/$f"
		cp "$src_dir/$f" "$work/$name/"
	done

	# Archive from inside the scratch dir with relative paths, so the archive holds a
	# single top-level directory and no Windows path translation is involved.
	if [[ $format == zip ]]; then
		command -v 7z >/dev/null 2>&1 || fail "7z is needed to build a zip"
		(cd "$work" && 7z a -tzip -bd -bso0 "$name.zip" "$name")
	else
		# COPYFILE_DISABLE keeps macOS tar from adding AppleDouble ._ entries.
		(cd "$work" && COPYFILE_DISABLE=1 tar -czf "$name.tar.gz" "$name")
	fi
	mv "$work/$name.$format" "$out_dir/"
	printf '%s  %s\n' "$(sha256_of "$out_dir/$name.$format")" "$name.$format" \
		>"$out_dir/$name.$format.sha256"

	echo "staged $out_dir/$name.$format"
	cat "$out_dir/$name.$format.sha256"
}

sums() {
	[[ $# -ge 2 ]] || usage
	local dist=$1 target line f
	shift
	[[ -d $dist ]] || fail "no such directory: $dist"
	local -a archives=() found=() all=()
	for target in "$@"; do
		[[ $target =~ $TARGET_RE ]] || fail "not a target triple: $(printf '%q' "$target")"
		shopt -s nullglob
		found=("$dist"/weftdb-v*-"$target".tar.gz "$dist"/weftdb-v*-"$target".zip)
		shopt -u nullglob
		[[ ${#found[@]} -eq 1 ]] ||
			fail "expected one archive for $target, found ${#found[@]}: ${found[*]:-none}"
		archives+=("${found[0]##*/}")
	done

	shopt -s nullglob
	all=("$dist"/*.tar.gz "$dist"/*.zip)
	shopt -u nullglob
	[[ ${#all[@]} -eq ${#archives[@]} ]] ||
		fail "found ${#all[@]} archives for ${#archives[@]} targets: ${all[*]##*/}"

	for f in "${archives[@]}"; do
		[[ -f $dist/$f.sha256 ]] || fail "missing $f.sha256"
		IFS= read -r line <"$dist/$f.sha256" || fail "empty $f.sha256"
		[[ ${line#*  } == "$f" ]] || fail "$f.sha256 names ${line#*  }"
		check_line "$dist" "$line"
	done

	: >"$dist/SHA256SUMS"
	while IFS= read -r f; do
		printf '%s  %s\n' "$(sha256_of "$dist/$f")" "$f" >>"$dist/SHA256SUMS"
	done < <(printf '%s\n' "${archives[@]}" | LC_ALL=C sort)

	while IFS= read -r line; do
		check_line "$dist" "$line"
	done <"$dist/SHA256SUMS"
	cat "$dist/SHA256SUMS"
}

[[ $# -ge 1 ]] || usage
command=$1
shift
case $command in
archive) archive "$@" ;;
sums) sums "$@" ;;
*) usage ;;
esac
