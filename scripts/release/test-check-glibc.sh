#!/usr/bin/env bash
#
# test-check-glibc.sh — tests for scripts/check-glibc.sh.
#
# That script is the only thing standing between the build host's C library and the floor the
# release claims, and almost all of its value is on the failure path: a checker that cannot fail
# is worse than none, which is how an earlier version of it broke (it anchored `^GLIBC_` to a
# field, matched nothing, and reported every binary on the machine as needing no glibc). Two more
# failure paths came out of writing this suite: the file was not executable, which is how
# release.yml invokes it, and an artifact whose symbol table it could not read at all — because
# neither objdump nor readelf is installed — fell through to "requires no glibc symbol" and passed.
#
# A test cannot produce an artifact built against an older glibc, so the two binutils programs the
# checker shells out to are stubbed on PATH and the artifacts are files carrying the ELF magic and
# nothing else. Every version the checker reads then comes from a fixture, and each branch of its
# reporting gets exercised. A PATH directory is built per case, so "only readelf" and "no binutils
# at all" are real conditions: /usr/bin holds a real objdump, so a case that wants a tool missing
# cannot put /usr/bin on PATH, and bash is named by absolute path for the same reason.
#
# The numbers are the ones that matter: 2.28 is the floor the release promises, 2.34 and 2.39 are
# what a build on the runner image actually needed, and 2.9 against 2.10 is where a lexical compare
# rather than a numeric one goes wrong.
#
#   scripts/release/test-check-glibc.sh
#
# Exit status: 0 every case behaved, 1 at least one did not.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
check_script=$repo_root/scripts/check-glibc.sh
if [[ ! -f $check_script ]]; then
	printf 'test-check-glibc.sh: no checker at %s\n' "$check_script" >&2
	exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
# STUB_DIR is what the stubs read: one file per artifact holding the versions its symbol table
# names, and a ".libc" marker per artifact whose readelf -d shows a libc NEEDED entry.
stub_dir=$tmp/fixtures
mkdir -p "$tmp/stub" "$stub_dir"

cat >"$tmp/stub/objdump" <<'STUB'
#!/usr/bin/env bash
# objdump -T prints each versioned symbol with the version named after an "@".
file=${*: -1}
name=${file##*/}
if [[ -f $STUB_DIR/$name ]]; then
	while read -r v; do
		printf '    g libc       0000000000000000              x@GLIBC_%s\n' "$v"
	done <"$STUB_DIR/$name"
fi
STUB
cat >"$tmp/stub/readelf" <<'STUB'
#!/usr/bin/env bash
# readelf answers three questions here: -d whether the artifact links libc, -lW which interpreter
# loads it, and (with no flag but --dyn-syms) the symbol table itself.
file=""
mode=syms
for a in "$@"; do
	case $a in
	-d) mode=dynamic ;;
	-lW) mode=segments ;;
	-*) ;;
	*) file=$a ;;
	esac
done
name=${file##*/}
case $mode in
dynamic)
	[[ -f $STUB_DIR/$name.libc ]] || exit 0
	printf ' 0x0000000000000001 (NEEDED)   Shared library: [libc.so.6]\n'
	;;
segments)
	printf '      [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]\n'
	;;
syms)
	if [[ -f $STUB_DIR/$name ]]; then
		while read -r v; do
			printf '  12: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND x@GLIBC_%s\n' "$v"
		done <"$STUB_DIR/$name"
	fi
	;;
esac
STUB
chmod +x "$tmp/stub/objdump" "$tmp/stub/readelf"

# The utilities the checker shells out to, other than the two binutils programs. `tar -xzf` needs
# gzip on PATH and `scan_archive` walks the extraction with find, so a case that hides a tool cannot
# put /usr/bin on PATH at all — /usr/bin holds a real objdump — and bash is named by absolute path
# for the same reason.
coreutils=(awk bash find grep gzip mktemp od rm sed sort tail tar tr)
path_dir() {
	local dir=$1 tool
	shift
	mkdir -p "$dir"
	for tool in "${coreutils[@]}"; do
		ln -s "$(command -v "$tool")" "$dir/$tool"
	done
	for tool in "$@"; do
		ln -s "$tmp/stub/$tool" "$dir/$tool"
	done
}
path_dir "$tmp/both" objdump readelf
path_dir "$tmp/ro" readelf
path_dir "$tmp/nobin"

passed=0
failed=0

# say <pass|refuse> <name> <expected-substring> <command…>
say() {
	local want=$1 name=$2 expect=$3
	shift 3
	local out rc=0
	if ! out=$("$@" 2>&1); then
		rc=1
	fi
	if [[ $want == pass && $rc -ne 0 ]] || [[ $want == refuse && $rc -eq 0 ]]; then
		local why="should have failed"
		[[ $want == pass ]] && why="should have passed"
		printf 'FAIL  %s: %s\n' "$name" "$why"
		printf '%s\n' "$out" | sed 's/^/      /'
		failed=$((failed + 1))
		return
	fi
	if [[ $expect != "" && $out != *"$expect"* ]]; then
		printf 'FAIL  %s: behaved as expected but did not say "%s"\n' "$name" "$expect"
		printf '%s\n' "$out" | sed 's/^/      /'
		failed=$((failed + 1))
		return
	fi
	printf 'ok    %s\n' "$name"
	passed=$((passed + 1))
}

check() {
	env -i PATH="$tmp/both" STUB_DIR="$stub_dir" "$check_script" "$@"
}

# An artifact the dynamic loader will hand to glibc: ELF magic, executable, a libc NEEDED entry,
# and the versions its symbol table names. A static or musl artifact looks the same minus the
# marker, which is exactly the distinction the checker makes when a symbol table comes back empty.
artifact() {
	local name=$1
	shift
	printf '\177ELF\nplaceholder for a test artifact\n' >"$tmp/$name"
	chmod +x "$tmp/$name"
	printf 'libc\n' >"$stub_dir/$name.libc"
	if [[ $# -gt 0 ]]; then
		printf '%s\n' "$@" >"$stub_dir/$name"
	else
		: >"$stub_dir/$name"
	fi
}

static_artifact() {
	local name=$1
	shift
	artifact "$name" "$@"
	rm -f -- "$stub_dir/$name.libc"
}

pack() {
	local archive=$1
	shift
	rm -rf -- "$tmp/pack"
	mkdir -p "$tmp/pack"
	local n
	for n in "$@"; do
		cp -p "$tmp/$n" "$tmp/pack/$n"
	done
	tar -czf "$archive" -C "$tmp/pack" .
}

# The happy path, at the floor and under it.
artifact ok-server 2.2 2.16 2.28 2.3
say pass "a binary exactly at the floor passes" \
	"requires glibc 2.28 (floor 2.28)" check 2.28 "$tmp/ok-server"
say pass "the report says the whole set is fine" \
	"OK — every artifact starts on glibc 2.28 or older" check 2.28 "$tmp/ok-server"

artifact ok-cli 2.2 2.17
say pass "a binary below the floor passes" \
	"requires glibc 2.17 (floor 2.28)" check 2.28 "$tmp/ok-cli"

# The failure it exists for: a host-newer symbol, named, with the floor it broke.
artifact bad-tui 2.2 2.16 2.28 2.34
say refuse "a 2.34 symbol is refused at a 2.28 floor" \
	"above the 2.28 baseline" check 2.28 "$tmp/bad-tui"

# The highest version wins, whatever order the table lists them in: taking the first, or the last,
# is the other way this could have been wrong.
artifact peak-last 2.2 2.39 2.16
say refuse "the highest version wins, not the first" \
	"needs glibc 2.39" check 2.28 "$tmp/peak-last"

# Compared numerically: 2.10 is a later release than 2.9 and a string compare says the opposite.
artifact ten 2.10
say refuse "2.10 is above 2.9, not below it" \
	"above the 2.9 baseline" check 2.9 "$tmp/ten"
say pass "2.9 stays under 2.10" "" check 2.10 "$tmp/ten"
say refuse "a whole-number floor is compared, not truncated" \
	"above the 2 baseline" check 2 "$tmp/ten"

# An artifact naming no glibc symbol is only fine when it does not link libc at all.
static_artifact musl-bench
say pass "a static artifact passes" \
	"requires no glibc symbol" check 2.28 "$tmp/musl-bench"
artifact unreadable
say refuse "an libc-linked artifact with an unreadable symbol table fails" \
	"links libc but no GLIBC_* symbol" check 2.28 "$tmp/unreadable"

# Several artifacts: one over the floor fails the run, and the ones that passed still say so.
say refuse "one artifact over the floor fails the whole run" \
	"bad-tui" check 2.28 "$tmp/ok-cli" "$tmp/bad-tui" "$tmp/musl-bench"
say pass "the artifact that passed is still reported" \
	"ok-cli requires glibc 2.17" check 2.28 "$tmp/ok-cli" "$tmp/musl-bench"

# Archives are opened rather than trusted: a portable-looking tarball with a host-newer binary
# inside cannot be published, and a gzip file that is not an archive is an error, not a skip.
pack "$tmp/good.tar.gz" ok-cli musl-bench
say pass "a tarball of portable artifacts passes" \
	"OK — every artifact starts on glibc 2.28 or older" check 2.28 "$tmp/good.tar.gz"
pack "$tmp/bad.tar.gz" ok-cli bad-tui
say refuse "an over-floor artifact inside a tarball is found" \
	"bad-tui" check 2.28 "$tmp/bad.tar.gz"
printf '\037\213not a tarball\n' >"$tmp/fake.tar.gz"
say refuse "a gzip file that is not an archive is refused" \
	"refusing to call it portable" check 2.28 "$tmp/fake.tar.gz"

say pass "--verbose names the interpreter" \
	"interpreter=/lib64/ld-linux-x86-64.so.2" check --verbose 2.28 "$tmp/ok-cli"

# The paths where it could quietly do nothing.
say refuse "no arguments is a usage error" "usage:" check
say refuse "a floor with no artifact is a usage error" "usage:" check 2.28
say refuse "a floor that is not a version is refused" "is not a version like 2.28" \
	check two_point_28 "$tmp/ok-cli"
say refuse "a missing artifact is named" "is not readable" check 2.28 "$tmp/not-here"
printf '#!/bin/sh\nexit 0\n' >"$tmp/script.sh"
chmod +x "$tmp/script.sh"
say refuse "a shell script is not an artifact" "is not an ELF file" check 2.28 "$tmp/script.sh"

# Only readelf installed: the dump has to come from the fallback path, not from objdump.
say pass "the readelf fallback reads the same artifact" \
	"requires glibc 2.17" \
	env -i PATH="$tmp/ro" STUB_DIR="$stub_dir" "$check_script" 2.28 "$tmp/ok-cli"

# Neither installed. Reading nothing is not the same as reading nothing, and this used to pass the
# artifact on the strength of an empty symbol table.
say refuse "no binutils is an error, not a pass" "neither objdump nor readelf" \
	env -i PATH="$tmp/nobin" STUB_DIR="$stub_dir" "$check_script" 2.28 "$tmp/ok-cli"

# The self-test is the release workflow's own first step, and it has to work with whatever binutils
# the build image really has, so this one case runs without stubs.
say pass "--self-test passes with the real binutils" "self-test read glibc" \
	env -i PATH=/usr/bin:/bin "$check_script" --self-test

printf '\n%s passed, %s failed\n' "$passed" "$failed"
[[ $failed -eq 0 ]]
