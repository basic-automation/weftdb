#!/usr/bin/env bash
#
# check-glibc.sh — fail a build whose ELF artifacts need a glibc newer than a stated floor.
#
# A "portable Linux" release is a claim about the oldest glibc that can start the binary, and the
# only thing that proves it is the versioned-symbol table the linker actually wrote. `ldd --version`
# on the build machine says nothing about the output; a single call into a newer symbol (a
# `posix_spawn`@2.24, a `statx`@2.28) silently raises the floor while the build still succeeds.
# This script reads the requirement out of the artifact itself.
#
# Usage:
#   scripts/check-glibc.sh <floor> <artifact> [<artifact> ...]     e.g. scripts/check-glibc.sh 2.28 target/release/weft-server
#   scripts/check-glibc.sh --verbose 2.28 dist/*.tar.gz            (archives are unpacked and scanned)
#
# Exit status: 0 every artifact's highest required GLIBC_* symbol version is <= <floor>; 1 one of
# them needs more, or an artifact is not a dynamic ELF we can read; 2 bad usage.
#
# Dependencies are binutils (`objdump`, with `readelf` as a fallback) and `tar`, all present on the
# ubuntu runners and in the manylinux images the release pipeline builds in.

set -euo pipefail

readonly prog="${0##*/}"

usage() {
	printf 'usage: %s [--verbose] <floor> <artifact> [<artifact> ...]\n' "$prog" >&2
	printf '       floor is a glibc version like 2.28\n' >&2
}

verbose=0
if [[ "${1:-}" == "--verbose" ]]; then
	verbose=1
	shift
fi

self_test=0
if [[ "${1:-}" == "--self-test" ]]; then
	self_test=1
	shift
fi

if [[ $self_test -eq 0 && $# -lt 2 ]]; then
	usage
	exit 2
fi

floor="${1:-}"
if [[ $# -gt 0 ]]; then
	shift
fi

if [[ $self_test -eq 0 && ! "$floor" =~ ^[0-9]+(\.[0-9]+)*$ ]]; then
	printf '%s: floor %s is not a version like 2.28\n' "$prog" "$floor" >&2
	exit 2
fi

# Highest glibc version named in a versioned-symbol dump, empty if none is (static, musl, or a
# non-glibc libc: musl binaries carry no GLIBC_* version tags at all).
#
# The version arrives wrapped — objdump prints "(GLIBC_2.34)", readelf prints "GLIBC_2.34 (6)" — so
# grep the token out of whatever surrounds it rather than anchoring a field to it. An earlier
# version of this function matched `^GLIBC_` on a whole field, matched nothing, and reported every
# binary on the machine as needing no glibc at all: a check that cannot fail is worse than none.
highest_glibc_in_dump() {
	grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n 1
}

# True when the binary will be served by glibc at run time, so a dump naming no GLIBC_* symbol
# means we failed to read it rather than that the requirement is low.
needs_glibc() {
	readelf -d "$1" 2>/dev/null | grep -q 'Shared library: \[libc\.so'
}

# Versioned dynamic symbols of an ELF file, however we have to get them.
dump_dynamic_symbols() {
	local file="$1"
	if command -v objdump >/dev/null 2>&1; then
		objdump -T "$file" 2>/dev/null || true
	elif command -v readelf >/dev/null 2>&1; then
		readelf -W --dyn-syms "$file" 2>/dev/null || true
	else
		printf '%s: neither objdump nor readelf is installed\n' "$prog" >&2
		return 1
	fi
}

# One artifact: prints its requirement, returns 1 when it exceeds the floor.
check_one() {
	local file="$1" dump required interp

	if [[ ! -r "$file" ]]; then
		printf '%s: %s is not readable\n' "$prog" "$file" >&2
		return 1
	fi

	# A dynamic ELF starts with 0x7f 'E' 'L' 'F' and has a program header asking the dynamic
	# loader; a shell script or a tarball we failed to open would not, and must not pass silently.
	if [[ "$(od -An -tx1 -N4 "$file" | tr -d ' \n')" != "7f454c46" ]]; then
		printf '%s: %s is not an ELF file\n' "$prog" "$file" >&2
		return 1
	fi

	interp="$(readelf -lW "$file" 2>/dev/null | awk '/Requesting program interpreter/ { sub(/\]$/, ""); print $NF; exit }' || true)"

	# A failed dump is not an empty symbol table. Without either binutils the script used to
	# fall through to "requires no glibc symbol" and pass the artifact while saying nothing
	# useful on stderr, so the assignment's status is what decides here.
	if ! dump="$(dump_dynamic_symbols "$file")"; then
		printf '%s: FAIL %s could not be read\n' "$prog" "$file" >&2
		return 1
	fi
	required="$(printf '%s\n' "$dump" | highest_glibc_in_dump)"

	if [[ -z "$required" ]]; then
		# A glibc binary that names no glibc symbol was not read, it is not exempt. Only a binary
		# that does not link libc at all (static, musl) may legitimately report nothing.
		if needs_glibc "$file"; then
			printf '%s: FAIL %s links libc but no GLIBC_* symbol version could be read from it\n' "$prog" "$file" >&2
			return 1
		fi
		printf '%s: %s requires no glibc symbol (static or non-glibc libc)' "$prog" "$file"
		[[ $verbose -eq 1 ]] && printf ' interpreter=%s\n' "${interp:-none}" || printf '\n'
		return 0
	fi

	printf '%s: %s requires glibc %s (floor %s)' "$prog" "$file" "$required" "$floor"
	if [[ $verbose -eq 1 ]]; then
		printf ' interpreter=%s' "${interp:-none}"
	fi
	printf '\n'

	# sort -V puts the newer version last; equal versions collapse to one line.
	if [[ "$(printf '%s\n%s\n' "$floor" "$required" | sort -V | tail -n 1)" != "$floor" ]]; then
		printf '%s: FAIL %s needs glibc %s, above the %s baseline\n' "$prog" "$file" "$required" "$floor" >&2
		return 1
	fi
	return 0
}

# Archives (the release tarballs) are scanned by extracting them somewhere temporary, so the check
# covers exactly what a user would download rather than what happens to sit in the build directory.
scan_archive() {
	local archive="$1" dest status=0
	dest="$(mktemp -d)"
	# shellcheck disable=SC2064  # expand $dest now, not at trap time
	trap "rm -rf -- '$dest'" RETURN
	if ! tar -xzf "$archive" -C "$dest" 2>/dev/null; then
		printf '%s: %s is not a gzip tarball; refusing to call it portable\n' "$prog" "$archive" >&2
		return 1
	fi
	while IFS= read -r -d '' file; do
		check_one "$file" || status=1
	done < <(find "$dest" -type f -perm -u+x -print0)
	return "$status"
}

# The failure path is the whole value of this script, so prove it works before trusting a pass: a
# real glibc binary must yield a version, and a non-ELF file must be rejected. `--self-test` runs
# both against the runner's own /bin/true, so CI checks the checker. Usage:
#   scripts/check-glibc.sh --self-test
if [[ $self_test -eq 1 ]]; then
	probe="$(mktemp)"
	printf '#!/bin/sh\nexit 0\n' >"$probe"
	status=0
	highest="$(dump_dynamic_symbols /bin/true | highest_glibc_in_dump)"
	if [[ -z "$highest" ]]; then
		printf '%s: FAIL self-test — read no glibc version out of /bin/true\n' "$prog" >&2
		status=1
	fi
	if check_one "$probe" >/dev/null 2>&1; then
		printf '%s: FAIL self-test — accepted a shell script as an artifact\n' "$prog" >&2
		status=1
	fi
	rm -f -- "$probe"
	if [[ $status -eq 0 ]]; then
		printf '%s: OK — self-test read glibc %s from /bin/true and rejected a non-ELF file\n' "$prog" "$highest"
		exit 0
	fi
	exit 1
fi

overall=0

for artifact in "$@"; do
	case "$artifact" in
	*.tar.gz | *.tgz)
		scan_archive "$artifact" || overall=1
		;;
	*)
		check_one "$artifact" || overall=1
		;;
	esac
done

if [[ $overall -eq 0 ]]; then
	printf '%s: OK — every artifact starts on glibc %s or older\n' "$prog" "$floor"
fi
exit "$overall"
